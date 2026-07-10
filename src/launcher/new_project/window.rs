/*
File: src/launcher/new_project/window.rs

Purpose:
Standalone "New Project" launcher window that mirrors the legacy Python layout.

Main responsibilities:
- render the import/download/stitch/save control column from the Python window;
- render a separate native egui viewport when supported, with embedded fallback;
- react to ribbon/source events produced by sibling logic modules.

Notes:
The folder/file import flow, quick downloader, and stitch/split workflow are implemented here.
Long-running image processing is delegated to background controllers to keep the egui window
responsive while the ribbon updates.
*/

use egui::{
    Align, Button, CentralPanel, ComboBox, Frame, Layout, ProgressBar, RichText, ScrollArea,
    Slider, Stroke, TextEdit, TextureHandle, TextureOptions, Ui, ViewportClass, Window,
    WindowLevel,
};
use image::{DynamicImage, RgbaImage};
#[cfg(not(target_arch = "wasm32"))]
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use ms_thread as thread;
use web_time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config;
use crate::launcher::new_project::advanced_download::{
    AdvancedAutoCandidateSet, AdvancedBrowserBackend, AdvancedDownloadController,
    AdvancedDownloadEvent, InterceptCounts, advanced_downloader_version_warning_message,
    build_pages_from_auto_candidates,
};
use crate::launcher::new_project::open_source::{
    OpenSourceKind, SourceImportController, SourceImportOptions, SourceLoadEvent,
};
use crate::launcher::new_project::project_io::{
    ProjectCatalogController, ProjectCatalogEvent, ProjectCatalogSnapshot, ProjectSaveController,
    ProjectSaveEvent, ProjectSaveImage, ProjectSaveRequest, ProjectSaveTarget, chapters_for_title,
    dir_has_entries,
};
use crate::launcher::new_project::quick_download::{
    QuickDownloadController, QuickDownloadEvent, supported_sites_tooltip,
};
use crate::launcher::new_project::reline::{
    RelineController, RelineCvtColorOptions, RelineEvent, RelineHalftoneOptions, RelineInputImage,
    RelineLevelOptions, RelineModelCatalogController, RelineModelCatalogEntry,
    RelineModelCatalogEvent, RelineOptions, RelineResizeOptions, RelineSharpOptions,
    RelineUpscaleOptions,
};
use crate::launcher::new_project::ribbon::{
    ImportedImage, RibbonCrop, RibbonMergeError, RibbonPage, RibbonState, RibbonTile,
    build_ribbon_pages, build_ribbon_tiles,
};
use crate::launcher::new_project::stitching::{
    ManualCutGuide, StitchController, StitchEvent, StitchInputImage, StitchOptions, StitchRequest,
    StitchSplitMode, StitchSuccessKind,
};
use crate::launcher::new_project::waifu2x::{
    Waifu2xController, Waifu2xEvent, Waifu2xInputImage, Waifu2xOptions,
};
#[cfg(feature = "tutorial")]
use crate::launcher::new_project::tutorial::{self, NpTutorialCommand, NpTutorialCtx};
use crate::launcher::state::OpenProjectSelection;
use crate::paste_image;
#[cfg(feature = "tutorial")]
use crate::tutorial::{TutorialController, TutorialId, TutorialProgressHandle, TutorialStep};
use crate::screen_capture::{self, ScreenRect};
use crate::widgets::{
    ArrowStyle, EditableComboBox, GutterItem, MarkedScrollArea, MarkedScrollOutput, ScrollSpan,
    arrow,
};

const LEFT_PANEL_WIDTH: f32 = 444.0;
const SECTION_SPACING: f32 = 14.0;
const ACTION_BUTTON_SIZE: egui::Vec2 = egui::vec2(170.0, 34.0);
const SMALL_BUTTON_SIZE: egui::Vec2 = egui::vec2(92.0, 32.0);
const VIEWER_MIN_HEIGHT: f32 = 560.0;
const RIBBON_PREVIEW_SPACING: f32 = 10.0;
const RIBBON_DELETE_BUTTON_SIZE: f32 = 26.0;
const RIBBON_CROP_BUTTON_WIDTH: f32 = 116.0;
const RIBBON_IMAGE_CONTROL_GAP: f32 = 6.0;
const MANUAL_CUT_HANDLE_WIDTH: f32 = 116.0;
const MANUAL_CUT_HANDLE_HEIGHT: f32 = 24.0;
const MANUAL_CUT_APPLY_BUTTON_SIZE: egui::Vec2 = egui::vec2(128.0, 30.0);
const MANUAL_CUT_DELETE_BUTTON_SIZE: f32 = 22.0;
const MANUAL_CUT_MIN_EDGE_DISTANCE_PX: usize = 100;
const MANUAL_CUT_SCROLL_ARROW_WIDTH: f32 = 30.0;
const MANUAL_CUT_SCROLL_ARROW_HEIGHT: f32 = 18.0;
const PAGE_BOUNDARY_SCROLL_ARROW_WIDTH: f32 = MANUAL_CUT_SCROLL_ARROW_WIDTH / 3.0;
const PAGE_BOUNDARY_SCROLL_ARROW_HEIGHT: f32 = MANUAL_CUT_SCROLL_ARROW_HEIGHT / 3.0;
const CROP_WINDOW_MIN_SIZE: egui::Vec2 = egui::vec2(560.0, 440.0);
const CROP_HANDLE_SIZE: f32 = 14.0;
const CROP_MIN_SIDE: usize = 32;
const SCREEN_CAPTURE_VIEWPORT_ID_SALT: &str = "launcher_new_project_screen_capture";
const SCREEN_CAPTURE_MIN_SIDE: usize = 48;
const SCREEN_CAPTURE_CONFIRM_DELAY_MS: u64 = 140;
const SCREEN_CAPTURE_UI_ENABLED: bool = false;
#[cfg(not(target_arch = "wasm32"))]
const TEST_CHAPTER_SITE_CHECK_TIMEOUT: Duration = Duration::from_secs(12);
const AUTO_REVIEW_CARD_SIDE: f32 = 230.0;
const AUTO_REVIEW_CARD_MIN_SIDE: f32 = 168.0;
const AUTO_REVIEW_CARD_GAP: f32 = 10.0;
const AUTO_REVIEW_CARD_MARGIN: f32 = 8.0;
const AUTO_REVIEW_CARD_HEADER_HEIGHT: f32 = 24.0;
const AUTO_REVIEW_CARD_FOOTER_HEIGHT: f32 = 34.0;

#[derive(Clone)]
struct OperationProgress {
    operation: &'static str,
    stage: String,
    current: usize,
    total: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SaveMode {
    ProjectBase,
    AltVersion,
    Independent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeftPanelMode {
    Simple,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SimpleModeStep {
    ImportDownload,
    StitchCut,
    Denoise,
    Save,
}

impl SimpleModeStep {
    const ALL: [Self; 4] = [
        Self::ImportDownload,
        Self::StitchCut,
        Self::Denoise,
        Self::Save,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::ImportDownload => t!("launcher.new_project.step_import"),
            Self::StitchCut => t!("launcher.new_project.step_stitch"),
            Self::Denoise => t!("launcher.new_project.step_denoise"),
            Self::Save => t!("launcher.new_project.step_save"),
        }
    }

    fn number(self) -> usize {
        match self {
            Self::ImportDownload => 1,
            Self::StitchCut => 2,
            Self::Denoise => 3,
            Self::Save => 4,
        }
    }

    fn previous(self) -> Option<Self> {
        match self {
            Self::ImportDownload => None,
            Self::StitchCut => Some(Self::ImportDownload),
            Self::Denoise => Some(Self::StitchCut),
            Self::Save => Some(Self::Denoise),
        }
    }

    fn next(self) -> Option<Self> {
        match self {
            Self::ImportDownload => Some(Self::StitchCut),
            Self::StitchCut => Some(Self::Denoise),
            Self::Denoise => Some(Self::Save),
            Self::Save => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageProcessor {
    Waifu2x,
    Reline,
}

impl ImageProcessor {
    fn as_index(self) -> usize {
        match self {
            Self::Waifu2x => 0,
            Self::Reline => 1,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Reline,
            _ => Self::Waifu2x,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Waifu2x => "waifu2x",
            Self::Reline => "Reline",
        }
    }
}

/// Display mode of the Reline section: a guided simplified UI or the full expert UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelineUiMode {
    Simple,
    Full,
}

impl RelineUiMode {
    /// Combo index used by the simple/full toggle (0 = simple, 1 = full).
    fn as_index(self) -> usize {
        match self {
            Self::Simple => 0,
            Self::Full => 1,
        }
    }

    /// Map a combo index back to a mode; any out-of-range value falls back to `Simple`.
    fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Full,
            _ => Self::Simple,
        }
    }

    /// Stable string used for config persistence.
    fn as_config_str(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Full => "full",
        }
    }

    /// Parse a persisted config string; unknown values fall back to `Simple`.
    fn from_config_str(value: &str) -> Self {
        match value {
            "full" => Self::Full,
            _ => Self::Simple,
        }
    }
}

/// Guided post-processing preset for the simplified Reline UI.
///
/// Each preset expands into a fixed set of Reline pipeline nodes in
/// `build_reline_simple_options`, so the user picks intent instead of raw parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelineSimplePreset {
    /// Only run the selected model (clean upscale/restore, no extra nodes).
    ModelOnly,
    /// Model plus a light sharpness/level cleanup pass.
    RestoreLightSharp,
    /// Model tuned for removing printed halftone screen (descreen).
    Descreen,
    /// Model plus halftone (screentone) synthesis.
    AddHalftone,
}

impl RelineSimplePreset {
    /// All presets in display order. Keep in sync with `from_index`/`as_index`.
    const ALL: [RelineSimplePreset; 4] = [
        Self::ModelOnly,
        Self::RestoreLightSharp,
        Self::Descreen,
        Self::AddHalftone,
    ];

    fn as_index(self) -> usize {
        match self {
            Self::ModelOnly => 0,
            Self::RestoreLightSharp => 1,
            Self::Descreen => 2,
            Self::AddHalftone => 3,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            1 => Self::RestoreLightSharp,
            2 => Self::Descreen,
            3 => Self::AddHalftone,
            _ => Self::ModelOnly,
        }
    }

    /// Short label shown on the preset selector.
    fn label(self) -> &'static str {
        match self {
            Self::ModelOnly => t!("launcher.new_project.processor_model_only"),
            Self::RestoreLightSharp => t!("launcher.new_project.processor_restore_light_sharp"),
            Self::Descreen => t!("launcher.new_project.processor_remove_screen"),
            Self::AddHalftone => t!("launcher.new_project.processor_add_screentone"),
        }
    }

    /// One-line recommendation shown under the selected preset.
    fn hint(self) -> &'static str {
        match self {
            Self::ModelOnly => t!("launcher.new_project.processor_model_only_desc"),
            Self::RestoreLightSharp => {
                t!("launcher.new_project.processor_restore_light_sharp_desc")
            }
            Self::Descreen => t!("launcher.new_project.processor_remove_screen_desc"),
            Self::AddHalftone => t!("launcher.new_project.processor_add_screentone_desc"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AdvancedDownloadMode {
    PatternLinkSearch,
    CanvasDownload,
    DeepCapture,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AdvancedLinkSourceMode {
    Pattern,
    AutoReview,
}

struct AdvancedAutoReviewState {
    candidates: AdvancedAutoCandidateSet,
    group_view: bool,
    removed_items: HashSet<usize>,
    removed_groups: HashSet<usize>,
    thumb_textures: HashMap<usize, TextureHandle>,
    expanded_item: Option<usize>,
    expanded_texture: Option<(usize, TextureHandle)>,
    open: bool,
}

impl AdvancedAutoReviewState {
    fn new(candidates: AdvancedAutoCandidateSet) -> Self {
        let removed_groups = auto_review_default_removed_groups(&candidates);
        let removed_items = auto_review_default_removed_items(&candidates);
        Self {
            candidates,
            group_view: true,
            removed_items,
            removed_groups,
            thumb_textures: HashMap::new(),
            expanded_item: None,
            expanded_texture: None,
            open: true,
        }
    }

    fn retained_count(&self) -> usize {
        self.candidates
            .items
            .iter()
            .filter(|item| {
                !self.removed_items.contains(&item.id)
                    && !self.removed_groups.contains(&item.group_id)
            })
            .count()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImportMode {
    ReplaceRibbon,
    AddToStart,
    AddToEnd,
    AddBeforeCurrent,
    AddAfterCurrent,
}

impl ImportMode {
    fn as_index(self) -> usize {
        match self {
            Self::ReplaceRibbon => 0,
            Self::AddToStart => 1,
            Self::AddToEnd => 2,
            Self::AddBeforeCurrent => 3,
            Self::AddAfterCurrent => 4,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::ReplaceRibbon,
            1 => Self::AddToStart,
            2 => Self::AddToEnd,
            3 => Self::AddBeforeCurrent,
            4 => Self::AddAfterCurrent,
            _ => Self::ReplaceRibbon,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RibbonImageControlAction {
    Crop,
    MoveUp,
    MoveDown,
    Delete,
    MergeWithPrevious,
    MergeWithNext,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum CropHandle {
    Move,
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Clone, Copy)]
struct CropDragState {
    handle: CropHandle,
    start_rect: RibbonCrop,
    start_pointer_px: egui::Pos2,
}

struct CropEditorState {
    page_index: usize,
    page_name: String,
    source_size: [usize; 2],
    crop_rect: RibbonCrop,
    tiles: Vec<RibbonTile>,
    drag_state: Option<CropDragState>,
    window_rect: Option<egui::Rect>,
}

struct ClipboardPasteResult {
    image: Option<ImportedImage>,
    error: Option<String>,
}

struct ScreenCaptureResult {
    image: Option<ImportedImage>,
    error: Option<String>,
}

struct TestChapterAvailabilityResult {
    available: bool,
    chapter_url: String,
    log_message: Option<String>,
}

struct ScreenCaptureOverlayState {
    desktop_bounds: ScreenRect,
    selection: RibbonCrop,
    drag_state: Option<CropDragState>,
}

struct PendingScreenCapture {
    requested_at: Instant,
    region: ScreenRect,
}

pub struct NewProjectWindowState {
    left_panel_mode: LeftPanelMode,
    simple_mode_step: SimpleModeStep,
    simple_import_show_advanced: bool,
    simple_stitch_done: bool,
    simple_manual_cut_preview_active: bool,
    test_chapter_check_rx: Option<Receiver<TestChapterAvailabilityResult>>,
    filter_same_width: bool,
    import_mode: ImportMode,
    import_extra_names: String,
    quick_link: String,
    advanced_page_url: String,
    selected_advanced_backend: AdvancedBrowserBackend,
    selected_browser: usize,
    selected_site: usize,
    browser_names: Vec<String>,
    site_presets: Vec<(String, String)>,
    advanced_mode: AdvancedDownloadMode,
    advanced_link_source_mode: AdvancedLinkSourceMode,
    advanced_link_collect_active: bool,
    advanced_link_collect_found_links: usize,
    advanced_link_collect_last_poll_at: Instant,
    advanced_intercept_active: bool,
    advanced_intercept_counts: InterceptCounts,
    advanced_intercept_last_poll_at: Instant,
    advanced_downloader_version_warning_open: bool,
    advanced_downloader_version_warning_dismissed: bool,
    advanced_downloader_version_warning_message: String,
    site_name: String,
    image_prefix: String,
    advanced_fetch_parallelism: usize,
    advanced_auto_review: Option<AdvancedAutoReviewState>,
    stitch_parts: String,
    stitch_target_height: String,
    stitch_band_rows: String,
    stitch_tolerance: String,
    stitch_search_radius: String,
    stitch_prefer_up: bool,
    cut_as_chapter_enabled: bool,
    cut_title: usize,
    cut_chapter: usize,
    image_processor: ImageProcessor,
    waifu_noise: usize,
    waifu_scale: usize,
    waifu_tile_size: String,
    reline_reader_mode: usize,
    reline_upscale_enabled: bool,
    reline_model_name: String,
    reline_model_catalog_requested: bool,
    reline_model_catalog_error: Option<String>,
    reline_model_catalog_entries: Vec<RelineModelCatalogEntry>,
    reline_model_path: String,
    reline_model_url: String,
    reline_tiler: usize,
    reline_target_scale: String,
    reline_dtype: usize,
    reline_exact_tiler_size: String,
    reline_allow_cpu_upscale: bool,
    reline_sharp_enabled: bool,
    reline_sharp_low_input: String,
    reline_sharp_high_input: String,
    reline_sharp_gamma: String,
    reline_sharp_diapason_white: String,
    reline_sharp_diapason_black: String,
    reline_sharp_canny: bool,
    reline_sharp_canny_type: usize,
    reline_halftone_enabled: bool,
    reline_halftone_dot_size: String,
    reline_halftone_angle: String,
    reline_halftone_dot_type: usize,
    reline_halftone_mode: usize,
    reline_halftone_ssaa_scale: String,
    reline_halftone_ssaa_filter: usize,
    reline_halftone_disable_auto_dot: bool,
    reline_resize_enabled: bool,
    reline_resize_height: String,
    reline_resize_width: String,
    reline_resize_percent: String,
    reline_resize_filter: usize,
    reline_resize_gamma_correction: bool,
    reline_resize_spread: bool,
    reline_resize_spread_size: String,
    reline_level_enabled: bool,
    reline_level_low_input: String,
    reline_level_high_input: String,
    reline_level_low_output: String,
    reline_level_high_output: String,
    reline_level_gamma: String,
    reline_cvt_color_enabled: bool,
    reline_cvt_color_type: usize,
    // Simplified Reline UI state (guided mode shown by default).
    reline_ui_mode: RelineUiMode,
    reline_simple_preset: usize,
    reline_simple_sharp: usize,
    reline_simple_scale: usize,
    reline_simple_resize_enabled: bool,
    reline_simple_resize_target: String,
    reline_model_picker_open: bool,
    save_title: usize,
    save_title_input: String,
    save_title_combo: EditableComboBox,
    save_chapter: String,
    save_mode: SaveMode,
    /// Last folder chosen for an independent save during this session. The next
    /// independent-save dialog opens at this folder's parent so a "chapter"
    /// pick (`.../16`) reopens at the "title" level (`.../`).
    last_independent_save_dir: Option<PathBuf>,
    alt_title: usize,
    alt_chapter: usize,
    alt_name: String,
    project_catalog_error: Option<String>,
    import_status: String,
    last_error: Option<String>,
    source_import: SourceImportController,
    project_catalog: ProjectCatalogController,
    advanced_download: AdvancedDownloadController,
    quick_download: QuickDownloadController,
    stitch: StitchController,
    save: ProjectSaveController,
    waifu2x: Waifu2xController,
    reline: RelineController,
    reline_model_catalog: RelineModelCatalogController,
    ribbon: RibbonState,
    selected_ribbon_page: Option<usize>,
    clipboard_paste_rx: Option<Receiver<ClipboardPasteResult>>,
    clipboard_paste_in_flight: bool,
    screen_capture_overlay: Option<ScreenCaptureOverlayState>,
    screen_capture_pending: Option<PendingScreenCapture>,
    screen_capture_rx: Option<Receiver<ScreenCaptureResult>>,
    screen_capture_in_flight: bool,
    crop_editor: Option<CropEditorState>,
    manual_cut_guides: Vec<ManualCutGuide>,
    manual_cut_context_guide: Option<ManualCutGuide>,
    active_progress: Option<OperationProgress>,
    project_catalog_snapshot: ProjectCatalogSnapshot,
    open_after_save_requested: bool,
    return_to_import_after_save_requested: bool,
    pending_open_selection: Option<OpenProjectSelection>,
    pending_open_wiki_guide: bool,
    batch_processing_window_open: bool,
    batch_processing: crate::launcher::new_project::batch_processing::BatchProcessingWindowState,
    /// Onboarding overlay for THIS viewport (its own controller so the overlay
    /// renders in the new-project window, not the launcher root). Drives the
    /// pipeline via commands drained after `sync` (see `tutorial.rs`). Gated
    /// behind the `tutorial` feature (off by default).
    #[cfg(feature = "tutorial")]
    tutorial: TutorialController<NpTutorialCtx>,
    /// True until the first frame after the window opens; used to edge-trigger
    /// autoplay. Reset when the window closes so re-opening re-fires the edge.
    #[cfg(feature = "tutorial")]
    tutorial_first_frame: bool,
}

impl NewProjectWindowState {
    pub fn new(
        projects_root: PathBuf,
        #[cfg(feature = "tutorial")] tutorial_progress: TutorialProgressHandle,
    ) -> Self {
        let advanced_download = AdvancedDownloadController::new();
        let browser_names = advanced_download.available_browsers().to_vec();
        let site_presets = load_image_url_presets();
        let default_prefix = AdvancedDownloadController::default_link_prefix().to_string();
        let selected_site = site_presets
            .iter()
            .position(|(_, prefix)| *prefix == default_prefix)
            .unwrap_or(0);
        let mut state = Self {
            left_panel_mode: LeftPanelMode::Simple,
            simple_mode_step: SimpleModeStep::ImportDownload,
            simple_import_show_advanced: false,
            simple_stitch_done: false,
            simple_manual_cut_preview_active: false,
            test_chapter_check_rx: None,
            filter_same_width: true,
            import_mode: ImportMode::ReplaceRibbon,
            import_extra_names: "resource, resource(*), scan*.*, page????, img[0-9]*.dat"
                .to_string(),
            quick_link: String::new(),
            advanced_page_url: String::new(),
            selected_advanced_backend: advanced_download.backend(),
            selected_browser: 0,
            selected_site,
            browser_names,
            site_presets,
            // Deep capture is the default advanced mode; it pairs with the Cloak
            // backend default and drives the simple-mode auto-capture section.
            advanced_mode: AdvancedDownloadMode::DeepCapture,
            advanced_link_source_mode: AdvancedLinkSourceMode::Pattern,
            advanced_link_collect_active: false,
            advanced_link_collect_found_links: 0,
            advanced_link_collect_last_poll_at: Instant::now(),
            advanced_intercept_active: false,
            advanced_intercept_counts: InterceptCounts::default(),
            advanced_intercept_last_poll_at: Instant::now(),
            advanced_downloader_version_warning_open: false,
            advanced_downloader_version_warning_dismissed: false,
            advanced_downloader_version_warning_message: String::new(),
            site_name: String::new(),
            image_prefix: default_prefix,
            advanced_fetch_parallelism: 4,
            advanced_auto_review: None,
            stitch_parts: String::new(),
            stitch_target_height: "19000".to_string(),
            stitch_band_rows: "4".to_string(),
            stitch_tolerance: "15".to_string(),
            stitch_search_radius: "5500".to_string(),
            stitch_prefer_up: true,
            cut_as_chapter_enabled: false,
            cut_title: 0,
            cut_chapter: 0,
            image_processor: ImageProcessor::Waifu2x,
            waifu_noise: 4,
            waifu_scale: 0,
            waifu_tile_size: "384".to_string(),
            reline_reader_mode: 0,
            reline_upscale_enabled: true,
            reline_model_name: "1x-MangaJPEGHQ".to_string(),
            reline_model_catalog_requested: false,
            reline_model_catalog_error: None,
            reline_model_catalog_entries: Vec::new(),
            reline_model_path: String::new(),
            reline_model_url: String::new(),
            reline_tiler: 0,
            reline_target_scale: "1".to_string(),
            reline_dtype: 0,
            reline_exact_tiler_size: "800".to_string(),
            reline_allow_cpu_upscale: true,
            reline_sharp_enabled: false,
            reline_sharp_low_input: "0".to_string(),
            reline_sharp_high_input: "255".to_string(),
            reline_sharp_gamma: "1.0".to_string(),
            reline_sharp_diapason_white: "-1".to_string(),
            reline_sharp_diapason_black: "-1".to_string(),
            reline_sharp_canny: false,
            reline_sharp_canny_type: 1,
            reline_halftone_enabled: false,
            reline_halftone_dot_size: "7".to_string(),
            reline_halftone_angle: "0".to_string(),
            reline_halftone_dot_type: 4,
            reline_halftone_mode: 0,
            reline_halftone_ssaa_scale: String::new(),
            reline_halftone_ssaa_filter: 10,
            reline_halftone_disable_auto_dot: false,
            reline_resize_enabled: false,
            reline_resize_height: String::new(),
            reline_resize_width: String::new(),
            reline_resize_percent: String::new(),
            reline_resize_filter: 13,
            reline_resize_gamma_correction: false,
            reline_resize_spread: false,
            reline_resize_spread_size: "2800".to_string(),
            reline_level_enabled: false,
            reline_level_low_input: "0".to_string(),
            reline_level_high_input: "255".to_string(),
            reline_level_low_output: "0".to_string(),
            reline_level_high_output: "255".to_string(),
            reline_level_gamma: "1.0".to_string(),
            reline_cvt_color_enabled: false,
            reline_cvt_color_type: 0,
            reline_ui_mode: load_reline_ui_mode(),
            reline_simple_preset: RelineSimplePreset::RestoreLightSharp.as_index(),
            reline_simple_sharp: 1,
            reline_simple_scale: 0,
            reline_simple_resize_enabled: false,
            reline_simple_resize_target: "2800".to_string(),
            reline_model_picker_open: false,
            save_title: 0,
            save_title_input: "Title A".to_string(),
            save_title_combo: EditableComboBox::new("launcher_new_project_save_title")
                .with_hint_text(t!("launcher.new_project.title_combo_hint")),
            save_chapter: String::new(),
            save_mode: SaveMode::ProjectBase,
            last_independent_save_dir: None,
            alt_title: 0,
            alt_chapter: 0,
            alt_name: String::new(),
            project_catalog_error: None,
            import_status: t!("launcher.new_project.no_images_loaded_hint").to_string(),
            last_error: None,
            source_import: SourceImportController::new(),
            project_catalog: ProjectCatalogController::new(projects_root.clone()),
            advanced_download,
            quick_download: QuickDownloadController::new(),
            stitch: StitchController::new(),
            save: ProjectSaveController::new(projects_root),
            waifu2x: Waifu2xController::new(),
            reline: RelineController::new(),
            reline_model_catalog: RelineModelCatalogController::new(),
            ribbon: RibbonState::new(),
            selected_ribbon_page: None,
            clipboard_paste_rx: None,
            clipboard_paste_in_flight: false,
            screen_capture_overlay: None,
            screen_capture_pending: None,
            screen_capture_rx: None,
            screen_capture_in_flight: false,
            crop_editor: None,
            manual_cut_guides: Vec::new(),
            manual_cut_context_guide: None,
            active_progress: None,
            project_catalog_snapshot: ProjectCatalogSnapshot {
                titles: Vec::new(),
                chapters_by_title: HashMap::new(),
            },
            open_after_save_requested: false,
            return_to_import_after_save_requested: false,
            pending_open_selection: None,
            pending_open_wiki_guide: false,
            batch_processing_window_open: false,
            batch_processing:
                crate::launcher::new_project::batch_processing::BatchProcessingWindowState::new(),
            #[cfg(feature = "tutorial")]
            tutorial: TutorialController::new(
                tutorial_progress,
                vec![(
                    TutorialId::NewProject,
                    tutorial::steps as fn() -> Vec<TutorialStep<NpTutorialCtx>>,
                )],
            ),
            #[cfg(feature = "tutorial")]
            tutorial_first_frame: true,
        };
        state.project_catalog.refresh();
        state
    }

    /// Execute a tutorial-requested action on the whole window state. Called from
    /// `show` after `sync` drains the command queue, so `self.tutorial` is no
    /// longer borrowed. Every action is idempotent enough to survive a "Назад"
    /// re-entry (the pipeline triggers self-guard on in-flight ops).
    #[cfg(feature = "tutorial")]
    fn apply_tutorial_command(&mut self, command: NpTutorialCommand) {
        match command {
            NpTutorialCommand::SwitchToSimple => self.left_panel_mode = LeftPanelMode::Simple,
            NpTutorialCommand::SwitchToFull => self.left_panel_mode = LeftPanelMode::Full,
            NpTutorialCommand::StartTestDownload => self.start_test_chapter_download(),
            NpTutorialCommand::StartStitchAutoCut => {
                self.start_stitch_split(StitchSplitMode::AutoCut);
            }
            NpTutorialCommand::StartWaifu2x => self.start_waifu2x(),
        }
    }

    /// Build this frame's tutorial context snapshot (state the gates read).
    #[cfg(feature = "tutorial")]
    fn tutorial_snapshot(&self) -> NpTutorialCtx {
        NpTutorialCtx {
            busy: self.active_progress.is_some(),
            ribbon_has_pages: !self.ribbon.pages().is_empty(),
            waifu_available: self.waifu2x.unavailable_reason().is_none(),
            commands: Vec::new(),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, viewport_class: ViewportClass) -> bool {
        // The viewport callback hands us a `Ui`; derive its `Context` (cheap Arc clone)
        // so the worker polling, sub-windows and global-style swap below stay unchanged.
        let ctx_owned = ui.ctx().clone();
        let ctx = &ctx_owned;
        self.poll_clipboard_paste();
        self.poll_screen_capture();
        self.maybe_start_pending_screen_capture();
        self.poll_folder_load(ctx);
        self.poll_project_catalog(ctx);
        self.poll_advanced_download(ctx);
        self.poll_test_chapter_check(ctx);
        self.poll_quick_download(ctx);
        self.poll_stitch(ctx);
        self.poll_save(ctx);
        self.poll_waifu2x(ctx);
        self.poll_reline(ctx);
        self.poll_reline_model_catalog(ctx);

        // --- Tutorial: autoplay on the open edge, then drive one tick. ---
        // The whole tutorial is gated behind the `tutorial` feature (off by
        // default); the controller and its `mark` sites stay compiled but inert.
        #[cfg(feature = "tutorial")]
        {
            // Autoplay only fires once per open (and only if enabled & not completed).
            if std::mem::take(&mut self.tutorial_first_frame) {
                self.tutorial.maybe_autoplay(TutorialId::NewProject);
            }
            // `sync` runs the current step's `on_enter` (which pushes commands) and
            // evaluates its gate against this snapshot. The snapshot is built before
            // `sync` (borrows self read-only); commands are drained AFTER `sync`
            // returns, so `apply_tutorial_command` gets `&mut self` without aliasing
            // `self.tutorial`. Executing them here (before the panels render) lets a
            // mode switch take effect the same frame the step is entered.
            let mut tutorial_ctx = self.tutorial_snapshot();
            self.tutorial.sync(&mut tutorial_ctx);
            self.tutorial.begin_frame();
            for command in std::mem::take(&mut tutorial_ctx.commands) {
                self.apply_tutorial_command(command);
            }
        }

        // A native window is its own viewport but shares the launcher's single egui Context,
        // so its style is global. Switch to this window's dark style for the duration of its
        // rendering and restore the previous (launcher) style afterwards, so it never leaks
        // back and leaves the launcher's combo boxes / text fields unstyled. The embedded path
        // scopes its style via `ui.set_style` instead and needs no global change.
        let restore_style = (!matches!(viewport_class, ViewportClass::EmbeddedWindow)).then(|| {
            let previous = ctx.global_style();
            ctx.set_global_style(standard_dark_style());
            previous
        });
        let keep_open = match viewport_class {
            ViewportClass::EmbeddedWindow => self.show_embedded(ctx),
            _ => self.show_native(ui),
        };
        self.show_crop_editor_window(ctx);
        self.show_advanced_downloader_version_warning(ctx);
        self.show_advanced_auto_review_window(ctx);
        self.show_screen_capture_overlay(ctx);
        self.show_batch_processing_window(ctx);
        if self.source_import.is_loading()
            || self.project_catalog.is_loading()
            || self.advanced_download.is_loading()
            || self.quick_download.is_loading()
            || self.test_chapter_check_rx.is_some()
            || self.stitch.is_loading()
            || self.save.is_loading()
            || self.waifu2x.is_loading()
            || self.reline.is_loading()
            || self.clipboard_paste_in_flight
            || self.screen_capture_overlay.is_some()
            || self.screen_capture_pending.is_some()
            || self.screen_capture_in_flight
            || self.crop_editor.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        // Overlay last so it draws on top within THIS viewport (its own ctx).
        #[cfg(feature = "tutorial")]
        self.tutorial.render(ctx);

        if !keep_open {
            self.handle_window_closed();
            // Re-arm the open edge so re-opening the window can autoplay again.
            #[cfg(feature = "tutorial")]
            {
                self.tutorial_first_frame = true;
            }
        }
        if let Some(previous_style) = restore_style {
            ctx.set_global_style(previous_style);
        }
        keep_open
    }

    pub fn take_open_project_selection(&mut self) -> Option<OpenProjectSelection> {
        self.pending_open_selection.take()
    }

    pub fn take_open_wiki_guide_requested(&mut self) -> bool {
        std::mem::take(&mut self.pending_open_wiki_guide)
    }

    pub fn set_projects_root(&mut self, projects_root: PathBuf) {
        self.project_catalog = ProjectCatalogController::new(projects_root.clone());
        self.save = ProjectSaveController::new(projects_root);
        self.project_catalog_snapshot = ProjectCatalogSnapshot {
            titles: Vec::new(),
            chapters_by_title: HashMap::new(),
        };
        self.project_catalog_error = None;
        self.project_catalog.refresh();
    }

    fn show_native(&mut self, ui: &mut egui::Ui) -> bool {
        if ui.ctx().input(|input| input.viewport().close_requested()) {
            return false;
        }

        CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(24, 24, 27))
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ui, |ui| {
                // An immediate viewport's root ui snapshots `ctx.style()` at
                // creation (the launcher style), so the global-style swap in
                // `show()` reaches `ctx`-based sub-windows but NOT these panel
                // widgets. Set this window's dark style directly on the content
                // ui, mirroring the embedded path, so the launcher theme cannot
                // leak into the new-project window.
                ui.set_style(standard_dark_style());
                self.show_contents(ui, false);
            });
        true
    }

    fn show_embedded(&mut self, ctx: &egui::Context) -> bool {
        let mut keep_open = true;
        Window::new(t!("launcher.new_project.window_title")).id(egui::Id::new("launcher.new_project.window_title"))
            .open(&mut keep_open)
            .default_size(egui::vec2(1180.0, 760.0))
            .min_width(1000.0)
            .min_height(680.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.scope(|ui| {
                    ui.set_style(standard_dark_style());
                    self.show_contents(ui, true);
                });
            });
        keep_open
    }

    fn show_contents(&mut self, ui: &mut Ui, embedded: bool) {
        if embedded {
            ui.add_space(4.0);
        }

        ui.columns(2, |columns| {
            columns[0].set_width(LEFT_PANEL_WIDTH);
            self.show_left_panel(&mut columns[0]);
            self.show_viewer_panel(&mut columns[1]);
        });
    }

    fn show_left_panel(&mut self, ui: &mut Ui) {
        let any_loading = self.source_import.is_loading()
            || self.advanced_download.is_loading()
            || self.quick_download.is_loading()
            || self.test_chapter_check_rx.is_some()
            || self.stitch.is_loading()
            || self.save.is_loading()
            || self.waifu2x.is_loading()
            || self.reline.is_loading()
            || self.clipboard_paste_in_flight
            || self.screen_capture_in_flight;
        ui.set_width(LEFT_PANEL_WIDTH);
        ui.vertical(|ui| {
            ui.label(RichText::new(t!("launcher.new_project.overall_progress_label")).small());
            ui.label(RichText::new(&self.import_status).small().weak());
            let global_progress = self.current_progress(any_loading);
            ui.horizontal(|ui| {
                ui.add(
                    ProgressBar::new(global_progress.fraction)
                        .animate(any_loading)
                        .desired_width((LEFT_PANEL_WIDTH - 86.0).max(180.0))
                        .text(global_progress.label),
                );
                if button_sized(ui, t!("launcher.new_project.guide_button"), egui::vec2(64.0, 22.0), true).clicked() {
                    self.pending_open_wiki_guide = true;
                }
            });
            if button_sized(
                ui,
                t!("launcher.new_project.batch_processing_button"),
                egui::vec2(LEFT_PANEL_WIDTH - 4.0, 34.0),
                true,
            )
            .clicked()
            {
                self.batch_processing_window_open = true;
            }
            ui.add_space(10.0);
            self.show_left_panel_mode_tabs(ui);
            ui.add_space(10.0);
            match self.left_panel_mode {
                LeftPanelMode::Simple => self.show_simple_panel(ui),
                LeftPanelMode::Full => self.show_full_panel(ui),
            }
        });
    }

    fn show_left_panel_mode_tabs(&mut self, ui: &mut Ui) {
        // `_row` (leading underscore) is only consumed by the feature-gated `mark`
        // below; the underscore keeps it warning-free when `tutorial` is off.
        let _row = ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.left_panel_mode,
                LeftPanelMode::Simple,
                t!("launcher.new_project.mode_simple"),
            );
            ui.selectable_value(
                &mut self.left_panel_mode,
                LeftPanelMode::Full,
                t!("launcher.new_project.mode_full"),
            );
        });
        #[cfg(feature = "tutorial")]
        self.tutorial.mark(tutorial::TARGET_MODE_TABS, _row.response.rect);
    }

    fn show_full_panel(&mut self, ui: &mut Ui) {
        ScrollArea::vertical()
            .id_salt("launcher_new_project_left_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                section_group(ui, t!("launcher.new_project.section_import"), |ui| self.show_import_section(ui));
                ui.add_space(SECTION_SPACING);
                section_group(ui, t!("launcher.new_project.section_quick_dl"), |ui| {
                    self.show_quick_downloader(ui)
                });
                ui.add_space(SECTION_SPACING);
                section_group(ui, t!("launcher.new_project.section_advanced_dl"), |ui| {
                    self.show_advanced_downloader(ui)
                });
                ui.add_space(SECTION_SPACING);
                section_group(ui, t!("launcher.new_project.section_stitch"), |ui| {
                    self.show_stitch_section(ui)
                });
                ui.add_space(SECTION_SPACING);
                section_group(ui, t!("launcher.new_project.section_processing"), |ui| {
                    self.show_image_processing_section(ui)
                });
                ui.add_space(SECTION_SPACING);
                section_group(ui, t!("launcher.new_project.step_save"), |ui| self.show_save_section(ui));
            });
    }

    fn show_simple_panel(&mut self, ui: &mut Ui) {
        self.show_simple_step_tabs(ui);
        ui.add_space(10.0);
        ScrollArea::vertical()
            .id_salt("launcher_new_project_simple_left_scroll")
            .auto_shrink([false, false])
            .max_height((ui.available_height() - 54.0).max(160.0))
            .show(ui, |ui| match self.simple_mode_step {
                SimpleModeStep::ImportDownload => self.show_simple_import_step(ui),
                SimpleModeStep::StitchCut => self.show_simple_stitch_step(ui),
                SimpleModeStep::Denoise => self.show_simple_denoise_step(ui),
                SimpleModeStep::Save => self.show_simple_save_step(ui),
            });
        ui.add_space(10.0);
        self.show_simple_step_navigation(ui);
    }

    fn show_simple_step_tabs(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            for step in SimpleModeStep::ALL {
                let label = format!("{}. {}", step.number(), step.title());
                ui.selectable_value(&mut self.simple_mode_step, step, label);
            }
        });
    }

    fn show_simple_import_step(&mut self, ui: &mut Ui) {
        let file_button_enabled =
            !self.source_import.is_loading() && !self.clipboard_paste_in_flight;
        let folder_button_enabled = !self.source_import.is_loading();
        let clipboard_button_enabled = !self.source_import.is_loading()
            && !self.clipboard_paste_in_flight
            && !self.screen_capture_in_flight;
        let quick_download_enabled = !self.source_import.is_loading()
            && !self.quick_download.is_loading()
            && self.test_chapter_check_rx.is_none();
        let advanced_button_enabled = !self.advanced_download.is_loading()
            && !self.advanced_link_collect_active
            && !self.advanced_intercept_active;

        section_group(
            ui,
            &tf!("launcher.new_project.simple_step_header", arg = self.simple_mode_step.number(), arg_2 = self.simple_mode_step.title()),
            |ui| {
                ui.label(
                    RichText::new(
                        t!("launcher.new_project.simple_intro_text"),
                    )
                    .color(egui::Color32::WHITE)
                    .strong(),
                );
                ui.add_space(SECTION_SPACING);

                if self.simple_import_show_advanced {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(
                                t!("launcher.new_project.simple_adv_dl_open_notice"),
                            )
                            .small()
                            .weak(),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if button_sized(
                                ui,
                                t!("launcher.new_project.back_to_options_button"),
                                egui::vec2(176.0, 28.0),
                                true,
                            )
                            .clicked()
                            {
                                self.simple_import_show_advanced = false;
                            }
                        });
                    });
                    ui.add_space(10.0);
                    self.show_advanced_downloader(ui);
                    return;
                }

                sub_group(ui, t!("launcher.new_project.source_question"), |ui| {
                    ui.label(
                        RichText::new(t!("launcher.new_project.already_downloaded_hint")).strong(),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if button_sized(
                            ui,
                            t!("launcher.new_project.open_archive_image_button"),
                            egui::vec2(198.0, 34.0),
                            file_button_enabled,
                        )
                        .clicked()
                        {
                            self.open_file_dialog();
                        }
                        if button_sized(
                            ui,
                            t!("launcher.new_project.open_folder_button"),
                            egui::vec2(146.0, 34.0),
                            folder_button_enabled,
                        )
                        .clicked()
                        {
                            self.open_folder_dialog();
                        }
                    });
                    if button_sized(
                        ui,
                        t!("launcher.new_project.paste_from_clipboard_button"),
                        egui::vec2(198.0, 34.0),
                        clipboard_button_enabled,
                    )
                    .clicked()
                    {
                        self.start_clipboard_paste();
                    }
                    self.show_operation_progress(ui, "source");
                });

                ui.add_space(SECTION_SPACING);
                sub_group(
                    ui,
                    t!("launcher.new_project.saved_webpage_hint"),
                    |ui| {
                        ui.horizontal(|ui| {
                            if button_sized(
                                ui,
                                t!("launcher.new_project.open_page_folder_button"),
                                egui::vec2(198.0, 34.0),
                                folder_button_enabled,
                            )
                            .clicked()
                            {
                                self.open_folder_dialog();
                            }
                            if button_sized(
                                ui,
                                t!("launcher.new_project.open_page_html_button"),
                                egui::vec2(182.0, 34.0),
                                file_button_enabled,
                            )
                            .clicked()
                            {
                                self.open_file_dialog();
                            }
                        });
                    },
                );

                ui.add_space(SECTION_SPACING);
                sub_group(
                    ui,
                    t!("launcher.new_project.free_chapter_sites_hint"),
                    |ui| {
                        egui::CollapsingHeader::new(t!("launcher.new_project.supported_sites_header")).id_salt("launcher.new_project.supported_sites_header")
                            .default_open(false)
                            .show(ui, |ui| {
                                for site in supported_quick_download_sites() {
                                    ui.label(RichText::new(site).small().weak());
                                }
                            });
                        ui.add_space(8.0);
                        ui.add(
                            TextEdit::singleline(&mut self.quick_link)
                                .desired_width(fill_width(ui))
                                .hint_text(t!("launcher.new_project.paste_chapter_link_placeholder")),
                        );
                        ui.add_space(8.0);
                        if button_sized(
                            ui,
                            t!("launcher.new_project.download_button"),
                            egui::vec2(140.0, 34.0),
                            quick_download_enabled,
                        )
                        .clicked()
                        {
                            self.start_quick_download();
                        }
                        self.show_operation_progress(ui, "quick_download");
                    },
                );

                ui.add_space(SECTION_SPACING);
                sub_group(
                    ui,
                    t!("launcher.new_project.test_chapter_intro"),
                    |ui| {
                        ui.label(
                            RichText::new(
                                t!("launcher.new_project.test_chapter_hint"),
                            )
                            .small()
                            .weak(),
                        );
                        ui.add_space(8.0);
                        let test_dl = button_sized(
                            ui,
                            t!("launcher.new_project.download_test_chapter_button"),
                            egui::vec2(220.0, 34.0),
                            quick_download_enabled,
                        );
                        #[cfg(feature = "tutorial")]
                        self.tutorial
                            .mark(tutorial::TARGET_TEST_DOWNLOAD, test_dl.rect);
                        if test_dl.clicked() {
                            self.start_test_chapter_download();
                        }
                    },
                );

                ui.add_space(SECTION_SPACING);
                sub_group(
                    ui,
                    t!("launcher.new_project.auto_intercept_title"),
                    |ui| {
                        ui.label(
                            RichText::new(
                                t!("launcher.new_project.auto_intercept_hint"),
                            )
                            .small()
                            .weak(),
                        );
                        ui.add_space(8.0);
                        ui.add(
                            TextEdit::singleline(&mut self.advanced_page_url)
                                .desired_width(fill_width(ui))
                                .hint_text(t!("launcher.new_project.paste_chapter_link_placeholder")),
                        );
                        ui.add_space(8.0);

                        // Deep capture cannot run while another advanced command
                        // is in flight; the URL is only needed to open the page.
                        let capture_busy = self.advanced_download.is_loading();
                        let url_ready = !self.advanced_page_url.trim().is_empty();

                        if button_sized(
                            ui,
                            t!("launcher.new_project.open_in_browser_button"),
                            egui::vec2(220.0, 34.0),
                            !capture_busy && !self.advanced_intercept_active && url_ready,
                        )
                        .clicked()
                        {
                            self.prepare_simple_deep_capture();
                            self.start_advanced_open();
                        }
                        ui.add_space(8.0);

                        if self.advanced_intercept_active {
                            let counts = self.advanced_intercept_counts;
                            ui.label(
                                RichText::new(tf!("launcher.new_project.intercept_counts_status", counts = counts.canvases, counts_2 = counts.images))
                                .color(egui::Color32::from_rgb(76, 175, 80))
                                .strong(),
                            );
                            ui.add_space(8.0);
                        }

                        if button_sized(
                            ui,
                            t!("launcher.new_project.start_intercept_button"),
                            egui::vec2(220.0, 34.0),
                            !capture_busy && !self.advanced_intercept_active,
                        )
                        .clicked()
                        {
                            self.prepare_simple_deep_capture();
                            self.start_advanced_deep_intercept();
                        }
                        if button_sized(
                            ui,
                            t!("launcher.new_project.finish_intercept_button"),
                            egui::vec2(220.0, 34.0),
                            !capture_busy && self.advanced_intercept_active,
                        )
                        .clicked()
                        {
                            self.finish_advanced_deep_intercept();
                        }
                        self.show_operation_progress(ui, "advanced_download");
                    },
                );

                ui.add_space(SECTION_SPACING);
                sub_group(
                    ui,
                    t!("launcher.new_project.all_methods_failed_hint"),
                    |ui| {
                        ui.label(
                            RichText::new(t!("launcher.new_project.html_dig_hint"))
                                .small()
                                .weak(),
                        );
                        ui.add_space(8.0);
                        if button_sized(
                            ui,
                            t!("launcher.new_project.open_advanced_dl_button"),
                            egui::vec2(260.0, 34.0),
                            advanced_button_enabled,
                        )
                        .clicked()
                        {
                            self.simple_import_show_advanced = true;
                        }
                    },
                );
            },
        );
    }

    fn show_simple_stitch_step(&mut self, ui: &mut Ui) {
        let can_start = self.can_start_stitch();
        section_group(
            ui,
            &tf!("launcher.new_project.simple_step_header", arg = self.simple_mode_step.number(), arg_2 = self.simple_mode_step.title()),
            |ui| {
                ui.label(
                    t!("launcher.new_project.vertical_or_paged_question"),
                );
                ui.add_space(8.0);
                if button_sized(ui, t!("launcher.new_project.skip_stitch_button"), egui::vec2(220.0, 34.0), true).clicked()
                {
                    self.simple_stitch_done = true;
                    self.simple_manual_cut_preview_active = false;
                    self.simple_mode_step = SimpleModeStep::Denoise;
                }

                ui.add_space(SECTION_SPACING * 1.6);
                ui.label(RichText::new(t!("launcher.new_project.if_webtoon_cut_hint")).strong());
                ui.add_space(8.0);
                if button_sized(
                    ui,
                    t!("launcher.new_project.stitch_cut_auto_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    can_start,
                )
                .clicked()
                {
                    self.simple_stitch_done = false;
                    self.simple_manual_cut_preview_active = false;
                    self.start_stitch_split(StitchSplitMode::AutoCut);
                }
                if button_sized(
                    ui,
                    t!("launcher.new_project.stitch_show_cuts_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    can_start,
                )
                .clicked()
                {
                    self.simple_stitch_done = false;
                    self.simple_manual_cut_preview_active = false;
                    self.start_stitch_split(StitchSplitMode::ManualCutPreview);
                }
                self.show_operation_progress(ui, "stitch");

                if self.simple_manual_cut_preview_active {
                    ui.add_space(SECTION_SPACING);
                    ui.label(
                        RichText::new(
                            t!("launcher.new_project.auto_cuts_review_hint"),
                        )
                        .color(egui::Color32::WHITE)
                        .strong(),
                    );
                }

                if self.simple_stitch_done {
                    ui.add_space(SECTION_SPACING);
                    ui.label(
                        RichText::new(
                            t!("launcher.new_project.extra_cuts_hint"),
                        )
                        .color(egui::Color32::WHITE)
                        .strong(),
                    );
                    ui.add_space(10.0);
                    if button_sized(ui, t!("launcher.common.next_button"), egui::vec2(124.0, 34.0), true).clicked() {
                        self.simple_mode_step = SimpleModeStep::Denoise;
                    }
                }
            },
        );
    }

    fn show_simple_denoise_step(&mut self, ui: &mut Ui) {
        section_group(
            ui,
            &tf!("launcher.new_project.simple_step_header", arg = self.simple_mode_step.number(), arg_2 = self.simple_mode_step.title()),
            |ui| {
                ui.label(
                    RichText::new(
                        t!("launcher.new_project.denoise_optional_hint"),
                    )
                    .color(egui::Color32::WHITE)
                    .strong(),
                );
                self.show_operation_progress(ui, self.current_image_processing_operation());

                ui.add_space(SECTION_SPACING * 1.6);
                if button_sized(
                    ui,
                    &tf!("launcher.new_project.process_with_button", arg = self.image_processor.title()),
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    self.can_start_image_processing(),
                )
                .clicked()
                {
                    self.start_image_processing();
                    self.simple_mode_step = SimpleModeStep::Save;
                }
                if button_sized(ui, t!("launcher.new_project.skip_button"), egui::vec2(140.0, 34.0), true).clicked()
                {
                    self.simple_mode_step = SimpleModeStep::Save;
                }
            },
        );
    }

    fn show_simple_save_step(&mut self, ui: &mut Ui) {
        let titles = self.project_catalog_snapshot.titles.clone();
        let can_refresh_catalog = !self.project_catalog.is_loading() && !self.save.is_loading();
        let can_save = self.can_start_save();

        section_group(
            ui,
            &tf!("launcher.new_project.simple_step_header", arg = self.simple_mode_step.number(), arg_2 = self.simple_mode_step.title()),
            |ui| {
                let mut save_to_project = self.save_mode != SaveMode::Independent;
                if ui
                    .checkbox(&mut save_to_project, t!("launcher.new_project.save_to_projects_button"))
                    .changed()
                    && save_to_project
                {
                    self.save_mode = SaveMode::ProjectBase;
                }

                let mut save_to_folder = self.save_mode == SaveMode::Independent;
                if ui
                    .checkbox(&mut save_to_folder, t!("launcher.new_project.save_to_other_folder_button"))
                    .changed()
                    && save_to_folder
                {
                    self.save_mode = SaveMode::Independent;
                }
                ui.label(
                    RichText::new(t!("launcher.new_project.just_downloading_hint"))
                        .small()
                        .weak(),
                );

                ui.add_space(SECTION_SPACING);
                if self.project_catalog.is_loading() {
                    ui.label(
                        RichText::new(t!("launcher.new_project.refreshing_titles_status"))
                            .small()
                            .weak(),
                    );
                } else if let Some(error) = &self.project_catalog_error {
                    ui.colored_label(egui::Color32::from_rgb(255, 120, 120), error);
                }

                if self.save_mode == SaveMode::Independent {
                    if button_sized(
                        ui,
                        t!("launcher.new_project.choose_save_folder_button"),
                        egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                        can_save,
                    )
                    .clicked()
                    {
                        self.start_save_to_folder();
                    }
                    self.show_operation_progress(ui, "save");
                    return;
                }

                ui.label(t!("launcher.new_project.title_input_hint"));
                let save_title_response =
                    self.save_title_combo
                        .draw(ui, &mut self.save_title_input, titles.as_slice());
                if save_title_response.changed {
                    self.sync_save_title_from_input();
                }
                if button_sized(ui, t!("launcher.common.refresh_button"), SMALL_BUTTON_SIZE, can_refresh_catalog).clicked()
                {
                    self.refresh_project_catalog();
                }

                ui.add_space(8.0);
                ui.label(t!("launcher.new_project.chapter_input_hint"));
                ui.add(TextEdit::singleline(&mut self.save_chapter).desired_width(fill_width(ui)));

                ui.add_space(SECTION_SPACING);
                if button_sized(
                    ui,
                    t!("launcher.new_project.save_and_open_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    can_save,
                )
                .clicked()
                {
                    self.start_save_to_project(true);
                }
                if button_sized(
                    ui,
                    t!("launcher.new_project.save_and_download_more_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    can_save,
                )
                .clicked()
                    && self.start_save_to_project(false)
                {
                    self.return_to_import_after_save_requested = true;
                }

                ui.add_space(SECTION_SPACING);
                ui.label(
                    RichText::new(
                        t!("launcher.new_project.projects_location_hint"),
                    )
                    .small()
                    .weak(),
                );
                self.show_operation_progress(ui, "save");
            },
        );
    }

    fn show_simple_step_navigation(&mut self, ui: &mut Ui) {
        let previous_step = self.simple_mode_step.previous();
        let next_step = self.simple_mode_step.next();
        let can_go_next = next_step.is_some()
            && (self.simple_mode_step != SimpleModeStep::StitchCut || self.simple_stitch_done);
        let next_label = if self.simple_mode_step == SimpleModeStep::StitchCut {
            t!("launcher.common.next_button")
        } else {
            t!("launcher.common.forward_button")
        };
        ui.horizontal(|ui| {
            if button_sized(
                ui,
                t!("launcher.common.back_button"),
                egui::vec2(124.0, 34.0),
                previous_step.is_some(),
            )
            .clicked()
                && let Some(step) = previous_step
            {
                self.simple_mode_step = step;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button_sized(ui, next_label, egui::vec2(124.0, 34.0), can_go_next).clicked()
                    && let Some(step) = next_step
                {
                    self.simple_mode_step = step;
                }
            });
        });
    }

    fn advance_simple_import_step_after_success(&mut self) {
        if self.left_panel_mode == LeftPanelMode::Simple
            && self.simple_mode_step == SimpleModeStep::ImportDownload
        {
            self.simple_import_show_advanced = false;
            self.simple_mode_step = SimpleModeStep::StitchCut;
        }
    }

    fn show_import_section(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let open_folder = button_sized(
                ui,
                t!("launcher.new_project.open_folder_menu"),
                ACTION_BUTTON_SIZE,
                !self.source_import.is_loading(),
            );
            #[cfg(feature = "tutorial")]
            self.tutorial.mark(tutorial::TARGET_IMPORT, open_folder.rect);
            if open_folder.clicked() {
                self.open_folder_dialog();
            }
            if button_sized(
                ui,
                t!("launcher.new_project.open_file_menu"),
                ACTION_BUTTON_SIZE,
                !self.source_import.is_loading() && !self.clipboard_paste_in_flight,
            )
            .clicked()
            {
                self.open_file_dialog();
            }
        });
        if button_sized(
            ui,
            t!("launcher.new_project.paste_from_clipboard_button"),
            ACTION_BUTTON_SIZE,
            !self.source_import.is_loading()
                && !self.clipboard_paste_in_flight
                && !self.screen_capture_in_flight,
        )
        .clicked()
        {
            self.start_clipboard_paste();
        }
        let capture_button_label = if self.is_screen_capture_mode_enabled() {
            t!("launcher.new_project.exit_capture_mode_button")
        } else {
            t!("launcher.new_project.capture_mode_button")
        };
        if button_sized(
            ui,
            capture_button_label,
            ACTION_BUTTON_SIZE,
            SCREEN_CAPTURE_UI_ENABLED
                && !self.source_import.is_loading()
                && !self.clipboard_paste_in_flight
                && !self.screen_capture_in_flight,
        )
        .clicked()
        {
            if self.is_screen_capture_mode_enabled() {
                self.stop_screen_capture_mode();
            } else {
                self.start_screen_capture_mode(ui.ctx());
            }
        }
        if !SCREEN_CAPTURE_UI_ENABLED {
            ui.label(
                RichText::new(t!("launcher.new_project.capture_mode_disabled_notice"))
                    .small()
                    .weak(),
            );
        }
        field_label(ui, t!("launcher.new_project.import_mode_label"));
        let mut import_mode_index = self.import_mode.as_index();
        combo_index(
            ui,
            "launcher_new_project_import_mode",
            &[
                t!("launcher.new_project.import_mode_replace"),
                t!("launcher.new_project.import_mode_prepend"),
                t!("launcher.new_project.import_mode_append"),
                t!("launcher.new_project.import_mode_before_current"),
                t!("launcher.new_project.import_mode_after_current"),
            ],
            &mut import_mode_index,
        );
        self.import_mode = ImportMode::from_index(import_mode_index);
        ui.checkbox(
            &mut self.filter_same_width,
            t!("launcher.new_project.filter_same_width_check"),
        );
        field_label(ui, t!("launcher.new_project.extra_filenames_hint"));
        ui.add(
            TextEdit::singleline(&mut self.import_extra_names)
                .desired_width(fill_width(ui))
                .hint_text("resource, scan*.*, page????"),
        );
        self.show_operation_progress(ui, "source");
    }

    fn show_quick_downloader(&mut self, ui: &mut Ui) {
        field_label(ui, t!("launcher.new_project.chapter_link_label"));
        ui.add(
            TextEdit::singleline(&mut self.quick_link)
                .desired_width(fill_width(ui))
                .hint_text(t!("launcher.new_project.chapter_link_placeholder")),
        );
        ui.add_space(6.0);
        let can_start_download =
            !self.source_import.is_loading() && !self.quick_download.is_loading();
        let response = button_sized(
            ui,
            t!("launcher.new_project.load_chapters_from_link_button"),
            egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
            can_start_download,
        );
        let response = response.on_hover_text(supported_sites_tooltip());
        #[cfg(feature = "tutorial")]
        self.tutorial.mark(tutorial::TARGET_QUICK, response.rect);
        if response.clicked() {
            self.start_quick_download();
        }
        self.show_operation_progress(ui, "quick_download");
    }

    fn start_clipboard_paste(&mut self) {
        if self.clipboard_paste_in_flight {
            return;
        }
        let (tx, rx) = mpsc::channel::<ClipboardPasteResult>();
        self.clipboard_paste_rx = Some(rx);
        self.clipboard_paste_in_flight = true;
        self.active_progress = None;
        self.last_error = None;
        self.import_status = t!("launcher.new_project.reading_clipboard_status").to_string();
        let page_name = self.next_clipboard_page_name();
        thread::spawn(move || {
            let result = match paste_image::read_image_from_clipboard() {
                Ok(image) => {
                    let rgba = image::RgbaImage::from_raw(
                        u32::try_from(image.width).unwrap_or(u32::MAX),
                        u32::try_from(image.height).unwrap_or(u32::MAX),
                        image.rgba,
                    );
                    match rgba {
                        Some(rgba) => ClipboardPasteResult {
                            image: Some(ImportedImage {
                                name: page_name,
                                image: DynamicImage::ImageRgba8(rgba),
                            }),
                            error: None,
                        },
                        None => ClipboardPasteResult {
                            image: None,
                            error: Some(
                                t!("launcher.new_project.clipboard_unsupported_size_error")
                                    .to_string(),
                            ),
                        },
                    }
                }
                Err(error) => ClipboardPasteResult {
                    image: None,
                    error: Some(error),
                },
            };
            let _ = tx.send(result);
        });
    }

    fn poll_clipboard_paste(&mut self) {
        let Some(rx) = self.clipboard_paste_rx.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(ClipboardPasteResult {
                image: None,
                error: Some(t!("launcher.new_project.clipboard_read_interrupted_error").to_string()),
            }),
        };
        let Some(result) = result else {
            return;
        };
        self.clipboard_paste_in_flight = false;
        self.clipboard_paste_rx = None;

        if let Some(image) = result.image {
            self.apply_source_import_result(
                PathBuf::from("[clipboard]"),
                build_ribbon_pages(vec![image]),
            );
            self.crop_editor = None;
            self.manual_cut_guides.clear();
            self.import_status = t!("launcher.new_project.clipboard_image_pasted_status").to_string();
            self.last_error = None;
            self.advance_simple_import_step_after_success();
            crate::runtime_log::log_info("[new-project] ribbon image pasted from clipboard");
        } else if let Some(error) = result.error {
            self.import_status = t!("launcher.new_project.clipboard_paste_error").to_string();
            self.last_error = Some(error.clone());
            crate::runtime_log::log_error(format!("[new-project] clipboard paste failed: {error}"));
        }
    }

    fn next_clipboard_page_name(&self) -> String {
        let next_index = self.ribbon.pages().len().saturating_add(1);
        format!("clipboard_{next_index:03}.png")
    }

    fn is_screen_capture_mode_enabled(&self) -> bool {
        self.screen_capture_overlay.is_some()
            || self.screen_capture_pending.is_some()
            || self.screen_capture_in_flight
    }

    /// Enables the screen-capture overlay for a fresh selection.
    ///
    /// `ctx` is the launcher viewport context; on macOS it supplies the monitor
    /// size in logical points used to size and position the overlay. See
    /// `resolve_capture_desktop_bounds` for why the bounds source is
    /// platform-dependent.
    fn start_screen_capture_mode(&mut self, ctx: &egui::Context) {
        match resolve_capture_desktop_bounds(ctx) {
            Ok(desktop_bounds) => {
                self.screen_capture_overlay = Some(ScreenCaptureOverlayState {
                    selection: default_screen_capture_selection(desktop_bounds),
                    desktop_bounds,
                    drag_state: None,
                });
                self.screen_capture_pending = None;
                self.screen_capture_rx = None;
                self.screen_capture_in_flight = false;
                self.import_status =
                    t!("launcher.new_project.capture_active_hint").to_string();
                self.last_error = None;
                crate::runtime_log::log_info(format!(
                    "[new-project] screen capture mode enabled for desktop {}x{} at {},{}",
                    desktop_bounds.width, desktop_bounds.height, desktop_bounds.x, desktop_bounds.y
                ));
            }
            Err(error) => {
                self.import_status = t!("launcher.new_project.capture_enable_error").to_string();
                self.last_error = Some(error.clone());
                crate::runtime_log::log_error(format!(
                    "[new-project] failed to enable screen capture mode: {error}"
                ));
            }
        }
    }

    fn stop_screen_capture_mode(&mut self) {
        self.screen_capture_overlay = None;
        self.screen_capture_pending = None;
        self.screen_capture_rx = None;
        self.screen_capture_in_flight = false;
        self.import_status = t!("launcher.new_project.capture_disabled_status").to_string();
        self.last_error = None;
        crate::runtime_log::log_info("[new-project] screen capture mode disabled");
    }

    fn queue_screen_capture(&mut self) {
        let Some(overlay) = self.screen_capture_overlay.as_ref() else {
            return;
        };
        if self.screen_capture_pending.is_some() || self.screen_capture_in_flight {
            return;
        }
        self.screen_capture_pending = Some(PendingScreenCapture {
            requested_at: Instant::now(),
            region: screen_capture_selection_to_global_rect(
                overlay.desktop_bounds,
                overlay.selection,
            ),
        });
        self.import_status =
            t!("launcher.new_project.capture_preparing_status").to_string();
        self.last_error = None;
    }

    fn maybe_start_pending_screen_capture(&mut self) {
        let Some(pending) = self.screen_capture_pending.as_ref() else {
            return;
        };
        if pending.requested_at.elapsed() < Duration::from_millis(SCREEN_CAPTURE_CONFIRM_DELAY_MS) {
            return;
        }
        if self.screen_capture_in_flight {
            return;
        }

        let region = pending.region;
        let page_name = self.next_screen_capture_page_name();
        let (tx, rx) = mpsc::channel::<ScreenCaptureResult>();
        self.screen_capture_rx = Some(rx);
        self.screen_capture_in_flight = true;
        self.screen_capture_pending = None;
        self.import_status = t!("launcher.new_project.capture_snapping_status").to_string();

        thread::spawn(move || {
            let result = match screen_capture::capture_screen_rect(region) {
                Ok(image) => ScreenCaptureResult {
                    image: Some(ImportedImage {
                        name: page_name,
                        image: DynamicImage::ImageRgba8(image),
                    }),
                    error: None,
                },
                Err(error) => ScreenCaptureResult {
                    image: None,
                    error: Some(error),
                },
            };
            let _ = tx.send(result);
        });
    }

    fn poll_screen_capture(&mut self) {
        let Some(rx) = self.screen_capture_rx.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(ScreenCaptureResult {
                image: None,
                error: Some(t!("launcher.new_project.capture_interrupted_error").to_string()),
            }),
        };
        let Some(result) = result else {
            return;
        };
        self.screen_capture_rx = None;
        self.screen_capture_in_flight = false;

        if let Some(image) = result.image {
            self.apply_source_import_result(
                PathBuf::from("[screen-capture]"),
                build_ribbon_pages(vec![image]),
            );
            self.crop_editor = None;
            self.manual_cut_guides.clear();
            self.import_status = t!("launcher.new_project.capture_added_status").to_string();
            self.last_error = None;
            self.advance_simple_import_step_after_success();
            crate::runtime_log::log_info("[new-project] screen capture inserted into ribbon");
        } else if let Some(error) = result.error {
            self.import_status = t!("launcher.new_project.capture_failed_error").to_string();
            self.last_error = Some(error.clone());
            crate::runtime_log::log_error(format!("[new-project] screen capture failed: {error}"));
        }
    }

    fn next_screen_capture_page_name(&self) -> String {
        let next_index = self.ribbon.pages().len().saturating_add(1);
        format!("capture_{next_index:03}.png")
    }

    fn show_screen_capture_overlay(&mut self, ctx: &egui::Context) {
        if self.screen_capture_pending.is_some() || self.screen_capture_in_flight {
            return;
        }
        let Some(overlay) = self.screen_capture_overlay.as_mut() else {
            return;
        };

        let viewport_id = egui::ViewportId::from_hash_of(SCREEN_CAPTURE_VIEWPORT_ID_SALT);
        let mut keep_open = true;
        let mut capture_requested = false;
        let builder = crate::launcher::apply_launcher_window_metadata(
            egui::ViewportBuilder::default()
                .with_title(t!("launcher.new_project.capture_mode_button"))
                .with_app_id(crate::launcher::launcher_app_id(false))
                .with_position(egui::pos2(
                    overlay.desktop_bounds.x as f32,
                    overlay.desktop_bounds.y as f32,
                ))
                .with_inner_size(egui::vec2(
                    overlay.desktop_bounds.width as f32,
                    overlay.desktop_bounds.height as f32,
                ))
                .with_resizable(false)
                .with_transparent(true)
                .with_decorations(false)
                .with_clamp_size_to_monitor_size(false)
                .with_window_level(WindowLevel::AlwaysOnTop)
                .with_mouse_passthrough(false)
                .with_close_button(false)
                .with_minimize_button(false)
                .with_maximize_button(false)
                .with_active(true),
        );

        ctx.show_viewport_immediate(viewport_id, builder, |ui, _class| {
            let ctx = ui.ctx().clone();
            keep_open = !ctx.input(|input| input.viewport().close_requested());
            if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
                keep_open = false;
            }
            if ctx.input(|input| input.key_pressed(egui::Key::S)) {
                capture_requested = true;
            }
            ctx.request_repaint_after(Duration::from_millis(16));

            CentralPanel::default()
                .frame(egui::Frame::NONE.fill(egui::Color32::TRANSPARENT))
                .show(ui, |ui| {
                    render_screen_capture_overlay(ui, overlay);
                    if show_screen_capture_overlay_controls(ui, overlay.selection) {
                        capture_requested = true;
                    }
                });
        });

        if capture_requested {
            self.queue_screen_capture();
        }
        if !keep_open {
            self.stop_screen_capture_mode();
        }
    }

    fn show_advanced_downloader(&mut self, ui: &mut Ui) {
        field_label(ui, t!("launcher.new_project.page_link_label"));
        ui.add(
            TextEdit::singleline(&mut self.advanced_page_url)
                .desired_width(fill_width(ui))
                .hint_text(t!("launcher.new_project.open_page_in_browser_hint")),
        );

        field_label(ui, t!("launcher.new_project.browser_engine_label"));
        ui.add_enabled_ui(
            !self.advanced_download.is_loading()
                && !self.advanced_link_collect_active
                && !self.advanced_intercept_active,
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    for backend in AdvancedBrowserBackend::ALL {
                        ui.selectable_value(
                            &mut self.selected_advanced_backend,
                            backend,
                            backend.label(),
                        );
                    }
                });
            },
        );
        self.advanced_download
            .set_backend(self.selected_advanced_backend);

        field_label(ui, t!("launcher.new_project.browser_label"));
        self.clamp_advanced_indexes();
        if self.selected_advanced_backend == AdvancedBrowserBackend::Cloak {
            ui.label(RichText::new("CloakBrowser").small());
            ui.label(
                RichText::new(t!("launcher.new_project.cloak_profile_hint"))
                    .small()
                    .weak(),
            );
        } else if self.browser_names.is_empty() {
            ui.label(
                RichText::new(t!("launcher.new_project.no_supported_browsers_hint"))
                    .small()
                    .weak(),
            );
        } else {
            combo_index_owned(
                ui,
                "launcher_new_project_browser",
                &self.browser_names,
                &mut self.selected_browser,
            );
        }

        let can_open_browser = !self.advanced_download.is_loading()
            && !self.advanced_link_collect_active
            && !self.advanced_intercept_active
            && self.advanced_browser_available()
            && !self.advanced_page_url.trim().is_empty();
        if button_sized(
            ui,
            t!("launcher.new_project.open_in_browser_button"),
            egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
            can_open_browser,
        )
        .clicked()
        {
            self.start_advanced_open();
        }
        ui.label(
            RichText::new(t!("launcher.new_project.ensure_images_loaded_hint"))
                .small()
                .weak(),
        );

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        if self.selected_advanced_backend != AdvancedBrowserBackend::Cloak
            && self.advanced_mode == AdvancedDownloadMode::DeepCapture
        {
            self.advanced_mode = AdvancedDownloadMode::PatternLinkSearch;
        }

        field_label(ui, t!("launcher.new_project.mode_label"));
        ui.add_enabled_ui(
            !self.advanced_link_collect_active && !self.advanced_intercept_active,
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(
                        &mut self.advanced_mode,
                        AdvancedDownloadMode::PatternLinkSearch,
                        t!("launcher.new_project.mode_pattern_search"),
                    );
                    ui.selectable_value(
                        &mut self.advanced_mode,
                        AdvancedDownloadMode::CanvasDownload,
                        t!("launcher.new_project.mode_canvas_download"),
                    );
                    ui.add_enabled_ui(
                        self.selected_advanced_backend == AdvancedBrowserBackend::Cloak,
                        |ui| {
                            ui.selectable_value(
                                &mut self.advanced_mode,
                                AdvancedDownloadMode::DeepCapture,
                                t!("launcher.new_project.mode_deep_intercept"),
                            );
                        },
                    );
                });
            },
        );
        ui.add_space(8.0);

        if self.advanced_mode == AdvancedDownloadMode::PatternLinkSearch {
            ui.add_enabled_ui(
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active,
                |ui| {
                    field_label(ui, t!("launcher.new_project.link_search_type_label"));
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(
                            &mut self.advanced_link_source_mode,
                            AdvancedLinkSourceMode::Pattern,
                            t!("launcher.new_project.link_search_normal"),
                        );
                        ui.selectable_value(
                            &mut self.advanced_link_source_mode,
                            AdvancedLinkSourceMode::AutoReview,
                            t!("launcher.new_project.link_search_autofind"),
                        );
                    });
                },
            );
            ui.add_space(6.0);

            if self.advanced_link_source_mode == AdvancedLinkSourceMode::Pattern {
                field_label(
                    ui,
                    t!("launcher.new_project.site_preset_prefix_hint"),
                );
                let previous_site = self.selected_site;
                combo_index_pairs(
                    ui,
                    "launcher_new_project_site",
                    &self.site_presets,
                    &mut self.selected_site,
                );
                if previous_site != self.selected_site
                    && let Some((_, prefix)) = self.site_presets.get(self.selected_site)
                {
                    self.image_prefix = prefix.clone();
                }

                field_label(ui, t!("launcher.new_project.prefix_label"));
                ui.add(TextEdit::singleline(&mut self.image_prefix).desired_width(fill_width(ui)));

                field_label(ui, t!("launcher.new_project.new_site_name_label"));
                ui.add(
                    TextEdit::singleline(&mut self.site_name)
                        .desired_width(fill_width(ui))
                        .hint_text(t!("launcher.new_project.save_name_placeholder")),
                );

                if button_sized(
                    ui,
                    t!("launcher.new_project.save_prefix_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    !self.advanced_download.is_loading()
                        && !self.advanced_link_collect_active
                        && !self.advanced_intercept_active,
                )
                .clicked()
                {
                    self.save_advanced_prefix();
                }
            } else {
                ui.label(
                    RichText::new(
                        t!("launcher.new_project.autofind_hint"),
                    )
                    .small()
                    .weak(),
                );
            }

            field_label(ui, t!("launcher.new_project.download_threads_label"));
            ui.add_enabled(
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active,
                Slider::new(&mut self.advanced_fetch_parallelism, 1..=8).text(t!("launcher.new_project.threads_suffix")),
            );

            sub_group(ui, t!("launcher.new_project.collect_and_download_button"), |ui| {
                if self.advanced_link_collect_active {
                    ui.label(
                        RichText::new(tf!("launcher.new_project.collected_links_status", arg = self.advanced_link_collect_found_links))
                        .color(egui::Color32::from_rgb(76, 175, 80))
                        .strong(),
                    );
                    ui.add_space(8.0);
                }
                if button_sized(
                    ui,
                    t!("launcher.new_project.download_now_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 74.0, 34.0),
                    !self.advanced_download.is_loading()
                        && !self.advanced_link_collect_active
                        && !self.advanced_intercept_active
                        && self.advanced_browser_available(),
                )
                .clicked()
                {
                    self.start_advanced_fetch();
                }
                if self.advanced_link_source_mode == AdvancedLinkSourceMode::AutoReview
                    && button_sized(
                        ui,
                        t!("launcher.new_project.stop_download_button"),
                        egui::vec2(LEFT_PANEL_WIDTH - 74.0, 34.0),
                        self.advanced_download.can_cancel_current_auto_fetch(),
                    )
                    .clicked()
                {
                    self.stop_advanced_auto_fetch();
                }
                if button_sized(
                    ui,
                    t!("launcher.new_project.start_collect_links_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 74.0, 34.0),
                    !self.advanced_download.is_loading()
                        && !self.advanced_link_collect_active
                        && !self.advanced_intercept_active
                        && self.advanced_browser_available(),
                )
                .clicked()
                {
                    self.start_advanced_link_collect();
                }
                if button_sized(
                    ui,
                    t!("launcher.new_project.stop_collect_links_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 74.0, 34.0),
                    !self.advanced_download.is_loading()
                        && self.advanced_link_collect_active
                        && self.advanced_browser_available(),
                )
                .clicked()
                {
                    self.finish_advanced_link_collect();
                }
            });
        } else if self.advanced_mode == AdvancedDownloadMode::CanvasDownload {
            if self.advanced_intercept_active {
                ui.label(
                    RichText::new(tf!("launcher.new_project.found_pages_status", arg = self.advanced_intercept_counts.total))
                    .color(egui::Color32::from_rgb(76, 175, 80))
                    .strong(),
                );
                ui.add_space(8.0);
            }
            if button_sized(
                ui,
                t!("launcher.new_project.download_now_button"),
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active
                    && self.advanced_browser_available(),
            )
            .clicked()
            {
                self.start_advanced_canvas_fetch();
            }
            if button_sized(
                ui,
                t!("launcher.new_project.start_intercept_button"),
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active
                    && self.advanced_browser_available(),
            )
            .clicked()
            {
                self.start_advanced_canvas_intercept();
            }
            if button_sized(
                ui,
                t!("launcher.new_project.finish_intercept_button"),
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && self.advanced_intercept_active
                    && self.advanced_browser_available(),
            )
            .clicked()
            {
                self.finish_advanced_canvas_intercept();
            }
        } else {
            if self.advanced_intercept_active {
                let counts = self.advanced_intercept_counts;
                ui.label(
                    RichText::new(tf!("launcher.new_project.intercept_counts_status", counts = counts.canvases, counts_2 = counts.images))
                    .color(egui::Color32::from_rgb(76, 175, 80))
                    .strong(),
                );
                ui.add_space(8.0);
            }
            ui.label(
                RichText::new(
                    t!("launcher.new_project.cloak_deep_intercept_hint"),
                )
                .small()
                .weak(),
            );
            ui.add_space(8.0);
            if button_sized(
                ui,
                t!("launcher.new_project.start_deep_intercept_button"),
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active
                    && self.selected_advanced_backend == AdvancedBrowserBackend::Cloak,
            )
            .clicked()
            {
                self.start_advanced_deep_intercept();
            }
            if button_sized(
                ui,
                t!("launcher.new_project.finish_deep_intercept_button"),
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && self.advanced_intercept_active
                    && self.selected_advanced_backend == AdvancedBrowserBackend::Cloak,
            )
            .clicked()
            {
                self.finish_advanced_deep_intercept();
            }
        }
        self.show_operation_progress(ui, "advanced_download");
    }

    fn show_advanced_auto_review_window(&mut self, ctx: &egui::Context) {
        let mut apply_clicked = false;
        let mut close_review = false;
        if let Some(review) = self.advanced_auto_review.as_mut() {
            let mut open = review.open;
            Window::new(t!("launcher.new_project.autofind_review_title")).id(egui::Id::new("launcher.new_project.autofind_review_title"))
                .open(&mut open)
                .resizable(true)
                .default_width(980.0)
                .default_height(720.0)
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(&mut review.group_view, t!("launcher.new_project.split_by_groups_check"));
                        ui.separator();
                        ui.label(tf!("launcher.new_project.review_images_count", review = review.retained_count(), review_2 = review.candidates.items.len()));
                        ui.label(tf!("launcher.new_project.review_groups_count", review = review.candidates.groups.len()));
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(t!("launcher.new_project.add_to_ribbon_button")).clicked() {
                            apply_clicked = true;
                        }
                        if ui.button(t!("launcher.common.close_button")).clicked() {
                            close_review = true;
                        }
                    });
                    ui.separator();
                    ScrollArea::vertical()
                        .id_salt("advanced_auto_review_scroll")
                        .max_height(ui.available_height().max(160.0))
                        .show(ui, |ui| {
                            if review.group_view {
                                Self::show_auto_review_groups(ui, review);
                                ui.separator();
                                ui.heading(t!("launcher.new_project.final_order_label"));
                            }
                            Self::show_auto_review_order(ui, review);
                        });
                });
            review.open = open;
            if !review.open {
                close_review = true;
            }
        }

        if let Some(review) = self.advanced_auto_review.as_mut() {
            Self::show_auto_candidate_preview(ctx, review);
        }
        if apply_clicked {
            self.apply_advanced_auto_review();
        } else if close_review {
            self.advanced_auto_review = None;
        }
    }

    fn show_auto_review_groups(ui: &mut Ui, review: &mut AdvancedAutoReviewState) {
        let groups = review
            .candidates
            .groups
            .iter()
            .map(|group| (group.id, group.signature.clone(), group.item_ids.clone()))
            .collect::<Vec<_>>();
        for (group_id, signature, item_ids) in groups {
            let color = advanced_group_color(group_id);
            let removed = review.removed_groups.contains(&group_id);
            Frame::group(ui.style())
                .stroke(Stroke::new(2.0, color))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(tf!("launcher.new_project.group_images_count", group_id = group_id + 1, item_ids = item_ids.len()))
                            .strong(),
                        );
                        ui.label(RichText::new(signature).small().weak());
                        let label = if removed {
                            t!("launcher.new_project.restore_group_button")
                        } else {
                            t!("launcher.new_project.delete_group_button")
                        };
                        if ui.button(label).clicked() {
                            if removed {
                                review.removed_groups.remove(&group_id);
                            } else {
                                review.removed_groups.insert(group_id);
                            }
                        }
                    });
                    ui.add_space(6.0);
                    Self::show_auto_candidate_cards(ui, review, &item_ids);
                });
            ui.add_space(8.0);
        }
    }

    fn show_auto_review_order(ui: &mut Ui, review: &mut AdvancedAutoReviewState) {
        let mut item_ids = review
            .candidates
            .items
            .iter()
            .map(|item| (item.order_index, item.id))
            .collect::<Vec<_>>();
        item_ids.sort_by_key(|(order_index, _)| *order_index);
        let ids = item_ids
            .into_iter()
            .map(|(_, item_id)| item_id)
            .collect::<Vec<_>>();
        Self::show_auto_candidate_cards(ui, review, &ids);
    }

    fn show_auto_candidate_cards(
        ui: &mut Ui,
        review: &mut AdvancedAutoReviewState,
        item_ids: &[usize],
    ) {
        let (columns, card_side) = auto_review_card_layout(ui);
        let card_step = card_side + AUTO_REVIEW_CARD_GAP;
        for (row_index, row) in item_ids.chunks(columns).enumerate() {
            let row_width = row
                .iter()
                .enumerate()
                .fold(0.0_f32, |width, (column_index, _)| {
                    if column_index == 0 {
                        card_side
                    } else {
                        width + card_step
                    }
                });
            let (row_rect, _) =
                ui.allocate_exact_size(egui::vec2(row_width, card_side), egui::Sense::hover());
            for (column_index, &item_id) in row.iter().enumerate() {
                let x = row_rect.left() + card_step * auto_review_index_as_f32(column_index);
                let rect = egui::Rect::from_min_size(
                    egui::pos2(x, row_rect.top()),
                    egui::vec2(card_side, card_side),
                );
                let card_id =
                    ui.id()
                        .with(("advanced_auto_card", row_index, column_index, item_id));
                Self::show_auto_candidate_card(ui, review, item_id, rect, card_id);
            }
            ui.add_space(AUTO_REVIEW_CARD_GAP);
        }
    }

    fn show_auto_candidate_card(
        ui: &mut Ui,
        review: &mut AdvancedAutoReviewState,
        item_id: usize,
        rect: egui::Rect,
        card_id: egui::Id,
    ) {
        let Some((group_id, order_index, width, height, url)) = review
            .candidates
            .items
            .iter()
            .find(|item| item.id == item_id)
            .map(|item| {
                (
                    item.group_id,
                    item.order_index,
                    item.width,
                    item.height,
                    item.url.clone(),
                )
            })
        else {
            return;
        };

        let removed =
            review.removed_items.contains(&item_id) || review.removed_groups.contains(&group_id);
        let stroke_color = if removed {
            egui::Color32::from_gray(80)
        } else {
            advanced_group_color(group_id)
        };
        let fill = ui.visuals().widgets.noninteractive.bg_fill;
        let response = ui.interact(rect, card_id, egui::Sense::click());
        let parent_clip = ui.clip_rect();
        let card_clip = parent_clip.intersect(rect);
        ui.painter().with_clip_rect(card_clip).rect(
            rect,
            egui::CornerRadius::same(4),
            fill,
            Stroke::new(2.0, stroke_color),
            egui::StrokeKind::Inside,
        );
        if response.clicked() && !removed {
            review.expanded_item = Some(item_id);
        }

        let inner = rect.shrink(AUTO_REVIEW_CARD_MARGIN);
        let header_rect = egui::Rect::from_min_size(
            inner.min,
            egui::vec2(inner.width(), AUTO_REVIEW_CARD_HEADER_HEIGHT),
        );
        let footer_rect = egui::Rect::from_min_size(
            egui::pos2(
                inner.left(),
                inner.bottom() - AUTO_REVIEW_CARD_FOOTER_HEIGHT,
            ),
            egui::vec2(inner.width(), AUTO_REVIEW_CARD_FOOTER_HEIGHT),
        );
        let image_rect = egui::Rect::from_min_max(
            egui::pos2(inner.left(), header_rect.bottom() + 4.0),
            egui::pos2(inner.right(), footer_rect.top() - 6.0),
        );

        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(header_rect)
                .layout(Layout::left_to_right(Align::Center)),
            |ui| {
                ui.set_clip_rect(parent_clip.intersect(header_rect));
                ui.label(RichText::new(format!("#{}", order_index + 1)).small());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let label = if review.removed_items.contains(&item_id) {
                        t!("launcher.new_project.restore_button")
                    } else {
                        "×"
                    };
                    if ui.small_button(label).clicked() {
                        if review.removed_items.contains(&item_id) {
                            review.removed_items.remove(&item_id);
                        } else {
                            review.removed_items.insert(item_id);
                        }
                    }
                });
            },
        );

        if let Some(texture) = Self::auto_thumb_texture(ui, review, item_id) {
            let response = ui.put(
                image_rect,
                egui::Image::new((texture.id(), image_rect.size())).sense(egui::Sense::click()),
            );
            if response.clicked() && !removed {
                review.expanded_item = Some(item_id);
            }
        }
        if removed {
            ui.painter()
                .with_clip_rect(parent_clip.intersect(image_rect))
                .rect_filled(
                    image_rect,
                    egui::CornerRadius::same(3),
                    egui::Color32::from_black_alpha(150),
                );
        }

        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(footer_rect)
                .layout(Layout::top_down(Align::Min)),
            |ui| {
                ui.set_clip_rect(parent_clip.intersect(footer_rect));
                ui.label(RichText::new(format!("{width}×{height}")).small());
                ui.label(RichText::new(shorten_url(&url, 31)).small().weak());
            },
        );
    }

    fn auto_thumb_texture(
        ui: &mut Ui,
        review: &mut AdvancedAutoReviewState,
        item_id: usize,
    ) -> Option<TextureHandle> {
        if let Some(texture) = review.thumb_textures.get(&item_id) {
            return Some(texture.clone());
        }
        let thumbnail = review
            .candidates
            .items
            .iter()
            .find(|item| item.id == item_id)
            .map(|item| item.thumbnail.clone())?;
        let texture = ui.ctx().load_texture(
            format!("advanced-auto-thumb-{item_id}"),
            thumbnail,
            TextureOptions::LINEAR,
        );
        review.thumb_textures.insert(item_id, texture.clone());
        Some(texture)
    }

    fn show_auto_candidate_preview(ctx: &egui::Context, review: &mut AdvancedAutoReviewState) {
        let Some(item_id) = review.expanded_item else {
            return;
        };
        let Some((width, height, url, image)) = review
            .candidates
            .items
            .iter()
            .find(|item| item.id == item_id)
            .map(|item| {
                (
                    item.width,
                    item.height,
                    item.url.clone(),
                    item.image.clone(),
                )
            })
        else {
            review.expanded_item = None;
            return;
        };
        if review
            .expanded_texture
            .as_ref()
            .is_none_or(|(texture_item_id, _)| *texture_item_id != item_id)
        {
            let preview = dynamic_image_preview(&image, 900, 680);
            let texture = ctx.load_texture(
                format!("advanced-auto-preview-{item_id}"),
                preview,
                TextureOptions::LINEAR,
            );
            review.expanded_texture = Some((item_id, texture));
        }
        let mut open = true;
        Window::new(t!("launcher.new_project.image_view_title")).id(egui::Id::new("launcher.new_project.image_view_title"))
            .open(&mut open)
            .resizable(true)
            .default_width(960.0)
            .default_height(760.0)
            .show(ctx, |ui| {
                ui.label(format!("{width}×{height}"));
                ui.label(RichText::new(url).small().weak());
                ui.horizontal(|ui| {
                    if ui.button(t!("launcher.common.delete_button")).clicked() {
                        review.removed_items.insert(item_id);
                    }
                    if ui.button(t!("launcher.new_project.keep_button")).clicked() {
                        review.removed_items.remove(&item_id);
                    }
                });
                ui.separator();
                if let Some((_, texture)) = &review.expanded_texture {
                    let size = texture.size_vec2();
                    ScrollArea::both()
                        .id_salt("advanced_auto_preview_scroll")
                        .show(ui, |ui| {
                            ui.add(egui::Image::new((texture.id(), size)));
                        });
                }
            });
        if !open {
            review.expanded_item = None;
            review.expanded_texture = None;
        }
    }

    fn apply_advanced_auto_review(&mut self) {
        let Some(review) = self.advanced_auto_review.as_ref() else {
            return;
        };
        let pages = match build_pages_from_auto_candidates(
            &review.candidates,
            &review.removed_items,
            &review.removed_groups,
        ) {
            Ok(pages) => pages,
            Err(message) => {
                self.last_error = Some(message);
                self.import_status = t!("launcher.new_project.autofind_no_images_for_ribbon").to_string();
                return;
            }
        };
        let source_url = review.candidates.source_url.clone();
        let page_count = pages.len();
        self.ribbon
            .replace_source(PathBuf::from(&source_url), pages);
        self.selected_ribbon_page = self.default_selected_page();
        self.crop_editor = None;
        self.manual_cut_guides.clear();
        self.active_progress = None;
        self.last_error = None;
        self.simple_stitch_done = false;
        self.simple_manual_cut_preview_active = false;
        self.advance_simple_import_step_after_success();
        self.import_status =
            tf!("launcher.new_project.autofind_added_status", page_count = page_count, source_url = source_url);
        self.advanced_auto_review = None;
    }

    fn show_stitch_section(&mut self, ui: &mut Ui) {
        let cut_titles = self.project_catalog_snapshot.titles.clone();
        let cut_chapters = self.current_cut_chapter_options().to_vec();
        let can_start = self.can_start_stitch();
        let can_restore = self.ribbon.can_restore_original() || self.has_manual_cut_preview();
        let can_apply_manual = self.has_manual_cut_preview() && !self.stitch.is_loading();
        let can_refresh_catalog = !self.project_catalog.is_loading() && !self.stitch.is_loading();
        let can_take_chapter = self.cut_as_chapter_enabled
            && !self.stitch.is_loading()
            && self.current_cut_title_name().is_some()
            && self.current_cut_chapter_name().is_some()
            && !self.ribbon.pages().is_empty();
        let can_pick_folder = self.cut_as_chapter_enabled
            && !self.stitch.is_loading()
            && !self.ribbon.pages().is_empty();

        field_label(ui, t!("launcher.new_project.stitch_k_label"));
        ui.add(
            TextEdit::singleline(&mut self.stitch_parts)
                .desired_width(160.0)
                .hint_text(t!("launcher.new_project.stitch_k_placeholder")),
        );

        field_label(ui, t!("launcher.new_project.stitch_hmax_label"));
        ui.add(TextEdit::singleline(&mut self.stitch_target_height).desired_width(160.0));

        field_label(ui, t!("launcher.new_project.stitch_band_rows_label"));
        ui.add(TextEdit::singleline(&mut self.stitch_band_rows).desired_width(160.0));

        field_label(ui, t!("launcher.new_project.stitch_tol_label"));
        ui.add(TextEdit::singleline(&mut self.stitch_tolerance).desired_width(160.0));

        field_label(ui, "search_radius (px)");
        ui.add(TextEdit::singleline(&mut self.stitch_search_radius).desired_width(160.0));

        ui.checkbox(&mut self.stitch_prefer_up, t!("launcher.new_project.stitch_up_first_check"));

        let wide_button = egui::vec2(LEFT_PANEL_WIDTH - 52.0, 32.0);
        if button_sized(ui, t!("launcher.new_project.stitch_ribbon_button"), wide_button, can_start).clicked() {
            self.start_stitch_split(StitchSplitMode::StitchOnly);
        }
        if button_sized(ui, t!("launcher.new_project.stitch_set_cut_lines_button"), wide_button, can_start).clicked()
        {
            self.start_stitch_split(StitchSplitMode::ManualCutPreview);
        }
        let stitch_auto = button_sized(ui, t!("launcher.new_project.stitch_cut_auto_button"), wide_button, can_start);
        #[cfg(feature = "tutorial")]
        self.tutorial.mark(tutorial::TARGET_STITCH, stitch_auto.rect);
        if stitch_auto.clicked() {
            self.start_stitch_split(StitchSplitMode::AutoCut);
        }
        if button_sized(
            ui,
            t!("launcher.new_project.stitch_only_uneven_button"),
            wide_button,
            can_start,
        )
        .clicked()
        {
            self.start_stitch_split(StitchSplitMode::HeterogeneousBottoms);
        }
        if button_sized(ui, t!("launcher.new_project.restore_original_button"), ACTION_BUTTON_SIZE, can_restore).clicked()
        {
            self.restore_original_pages();
        }
        if can_apply_manual
            && button_sized(ui, t!("launcher.new_project.apply_manual_cut_button"), wide_button, true).clicked()
        {
            self.apply_manual_cut_guides();
        }

        ui.add_space(8.0);
        self.show_operation_progress(ui, "stitch");
        sub_group(ui, t!("launcher.new_project.cut_as_chapter_button"), |ui| {
            ui.checkbox(
                &mut self.cut_as_chapter_enabled,
                t!("launcher.new_project.enable_cut_by_chapter_check"),
            );
            ui.add_enabled_ui(self.cut_as_chapter_enabled, |ui| {
                field_label(ui, t!("launcher.common.title_label"));
                let previous_cut_title = self.cut_title;
                combo_index_owned(
                    ui,
                    "launcher_new_project_cut_title",
                    cut_titles.as_slice(),
                    &mut self.cut_title,
                );
                if previous_cut_title != self.cut_title {
                    self.cut_chapter = 0;
                }
                field_label(ui, t!("launcher.common.chapter_label"));
                combo_index_owned(
                    ui,
                    "launcher_new_project_cut_chapter",
                    cut_chapters.as_slice(),
                    &mut self.cut_chapter,
                );
                ui.horizontal(|ui| {
                    if button_sized(ui, t!("launcher.common.refresh_button"), SMALL_BUTTON_SIZE, can_refresh_catalog)
                        .clicked()
                    {
                        self.refresh_project_catalog();
                    }
                    if button_sized(ui, t!("launcher.new_project.take_this_chapter_button"), ACTION_BUTTON_SIZE, can_take_chapter)
                        .clicked()
                    {
                        self.start_cut_like_project_chapter();
                    }
                });
                if button_sized(
                    ui,
                    t!("launcher.new_project.choose_folder_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 84.0, 32.0),
                    can_pick_folder,
                )
                .clicked()
                {
                    self.start_cut_like_folder();
                }
            });
        });
    }

    fn show_image_processing_section(&mut self, ui: &mut Ui) {
        let processor_labels = ["waifu2x", "Reline"];
        let mut processor_index = self.image_processor.as_index();
        field_label(ui, t!("launcher.new_project.waifu2x_engine_label"));
        combo_index(
            ui,
            "launcher_new_project_image_processor",
            &processor_labels,
            &mut processor_index,
        );
        self.image_processor = ImageProcessor::from_index(processor_index);
        ui.add_space(8.0);
        match self.image_processor {
            ImageProcessor::Waifu2x => self.show_waifu_section(ui),
            ImageProcessor::Reline => self.show_reline_section(ui),
        }
    }

    fn show_waifu_section(&mut self, ui: &mut Ui) {
        let noise_levels = ["-1", "0", "1", "2", "3"];
        let scale_levels = ["1", "2", "4", "8", "16", "32"];

        field_label(ui, t!("launcher.new_project.waifu2x_backend_path_label"));
        let mut backend = self.waifu_backend_path_display();
        ui.add_enabled(
            false,
            TextEdit::singleline(&mut backend).desired_width(fill_width(ui)),
        );
        if let Some(reason) = self.waifu2x.unavailable_reason() {
            ui.colored_label(egui::Color32::from_rgb(236, 112, 99), reason);
        }

        field_label(ui, t!("launcher.new_project.waifu2x_noise_label"));
        combo_index(
            ui,
            "launcher_new_project_w2x_noise",
            &noise_levels,
            &mut self.waifu_noise,
        );

        field_label(ui, t!("launcher.new_project.waifu2x_scale_label"));
        combo_index(
            ui,
            "launcher_new_project_w2x_scale",
            &scale_levels,
            &mut self.waifu_scale,
        );

        field_label(ui, "Tile size -t (>=32, 0=auto)");
        ui.add(TextEdit::singleline(&mut self.waifu_tile_size).desired_width(160.0));

        let waifu_run = button_sized(
            ui,
            t!("launcher.new_project.run_waifu2x_button"),
            egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
            self.can_start_waifu2x(),
        );
        #[cfg(feature = "tutorial")]
        self.tutorial.mark(tutorial::TARGET_WAIFU, waifu_run.rect);
        if waifu_run.clicked() {
            self.start_waifu2x();
        }
        self.show_operation_progress(ui, "waifu2x");
    }

    /// Reline section entry point: draws the simple/full toggle and dispatches to the active mode.
    ///
    /// The mode is persisted in user config; toggling writes the new mode once (not per frame).
    fn show_reline_section(&mut self, ui: &mut Ui) {
        let mut mode_index = self.reline_ui_mode.as_index();
        field_label(ui, t!("launcher.new_project.reline_interface_label"));
        combo_index(
            ui,
            "launcher_new_project_reline_ui_mode",
            &[t!("launcher.new_project.reline_interface_simple"), t!("launcher.new_project.reline_interface_full")],
            &mut mode_index,
        );
        let new_mode = RelineUiMode::from_index(mode_index);
        if new_mode != self.reline_ui_mode {
            self.reline_ui_mode = new_mode;
            save_reline_ui_mode(new_mode);
        }
        ui.add_space(8.0);

        match self.reline_ui_mode {
            RelineUiMode::Simple => self.show_reline_simple(ui),
            RelineUiMode::Full => self.show_reline_full(ui),
        }
    }

    /// Guided Reline UI: a categorized model gallery plus a small set of high-level controls.
    ///
    /// Hidden parameters take safe defaults; the resulting `RelineOptions` are built by
    /// `build_reline_simple_options`.
    fn show_reline_simple(&mut self, ui: &mut Ui) {
        self.ensure_reline_model_catalog_requested();

        field_label(ui, t!("launcher.new_project.reline_model_label"));
        self.show_reline_model_gallery(ui);
        ui.add_space(10.0);

        sub_group(ui, t!("launcher.new_project.reline_process_mode_label"), |ui| {
            for preset in RelineSimplePreset::ALL {
                let mut selected = self.reline_simple_preset == preset.as_index();
                if ui.radio(selected, preset.label()).clicked() {
                    selected = true;
                }
                if selected {
                    self.reline_simple_preset = preset.as_index();
                }
            }
            let active = RelineSimplePreset::from_index(self.reline_simple_preset);
            ui.add_space(4.0);
            ui.label(RichText::new(active.hint()).small().weak());
        });
        ui.add_space(8.0);

        field_label(ui, t!("launcher.new_project.reline_sharpness_label"));
        combo_index(
            ui,
            "launcher_new_project_reline_simple_sharp",
            &[t!("launcher.new_project.reline_sharpness_none"), t!("launcher.new_project.reline_sharpness_weak"), t!("launcher.new_project.reline_sharpness_strong")],
            &mut self.reline_simple_sharp,
        );

        field_label(ui, t!("launcher.new_project.reline_target_scale_label"));
        combo_index(
            ui,
            "launcher_new_project_reline_simple_scale",
            &[t!("launcher.new_project.reline_scale_auto"), "×2", "×4"],
            &mut self.reline_simple_scale,
        );

        ui.add_space(4.0);
        ui.checkbox(
            &mut self.reline_simple_resize_enabled,
            t!("launcher.new_project.reline_result_height_label"),
        );
        if self.reline_simple_resize_enabled {
            ui.add(
                TextEdit::singleline(&mut self.reline_simple_resize_target).desired_width(160.0),
            );
        }

        ui.add_space(10.0);
        if button_sized(
            ui,
            t!("launcher.new_project.run_reline_button"),
            egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
            self.can_start_reline(),
        )
        .clicked()
        {
            self.start_reline();
        }
        self.show_operation_progress(ui, "reline");
    }

    /// Categorized model picker used by the simplified Reline UI.
    ///
    /// Joins the fetched catalog (`reline_model_catalog_entries`) with offline classification
    /// from `reline_models::classify`. The full categorized list lives inside a collapsible area
    /// toggled by the "Выбрать модель" button; when collapsed only the selected model's card is
    /// shown. The expanded list grows inline (no inner scroll) — the surrounding page scrolls.
    fn show_reline_model_gallery(&mut self, ui: &mut Ui) {
        use crate::launcher::new_project::reline_models::{ModelCategory, classify};

        if self.reline_model_catalog.is_loading() {
            ui.label(
                RichText::new(t!("launcher.new_project.reline_loading_models_status"))
                    .small()
                    .weak(),
            );
        }
        if let Some(error) = &self.reline_model_catalog_error {
            ui.colored_label(egui::Color32::from_rgb(236, 112, 99), error.clone());
        }

        ui.horizontal(|ui| {
            let toggle_label = if self.reline_model_picker_open {
                t!("launcher.new_project.reline_hide_list_button")
            } else {
                t!("launcher.new_project.reline_choose_model_button")
            };
            if ui.button(toggle_label).clicked() {
                self.reline_model_picker_open = !self.reline_model_picker_open;
            }
            let refresh_label = if self.reline_model_catalog.is_loading() {
                t!("launcher.common.loading")
            } else {
                t!("launcher.common.refresh_button")
            };
            if ui
                .add_enabled(
                    !self.reline_model_catalog.is_loading(),
                    Button::new(refresh_label),
                )
                .on_hover_text(t!("launcher.new_project.reline_refresh_models_tooltip"))
                .clicked()
            {
                self.refresh_reline_model_catalog();
            }
        });

        if self.reline_model_catalog_entries.is_empty() {
            ui.label(
                RichText::new(t!("launcher.new_project.reline_models_empty_hint"))
                    .small()
                    .weak(),
            );
            return;
        }

        // Collapsed: show only the currently selected model's title and description.
        if !self.reline_model_picker_open {
            if self.reline_model_name.trim().is_empty() {
                ui.label(
                    RichText::new(t!("launcher.new_project.reline_no_model_selected_hint"))
                        .small()
                        .weak(),
                );
            } else {
                let meta = classify(&self.reline_model_name);
                ui.label(RichText::new(meta.display_title()).strong());
                ui.label(RichText::new(&self.reline_model_name).small().weak());
                ui.label(RichText::new(meta.description).small());
            }
            return;
        }

        // Expanded: full categorized list, grown inline (no inner ScrollArea).
        // Group entries by classified category, preserving catalog order within each group.
        let classified: Vec<(usize, ModelCategory)> = self
            .reline_model_catalog_entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (index, classify(&entry.name).category))
            .collect();

        for category in ModelCategory::ALL {
            let group: Vec<usize> = classified
                .iter()
                .filter(|(_, cat)| *cat == category)
                .map(|(index, _)| *index)
                .collect();
            if group.is_empty() {
                continue;
            }
            egui::CollapsingHeader::new(format!("{} ({})", category.title(), group.len()))
                .default_open(category.order() == 0)
                .id_salt(("reline_gallery", category.order()))
                .show(ui, |ui| {
                    for index in group {
                        let entry = &self.reline_model_catalog_entries[index];
                        let name = entry.name.clone();
                        let downloaded = entry.downloaded;
                        let meta = classify(&name);
                        let selected = self.reline_model_name == name;

                        let header = if downloaded {
                            tf!("launcher.new_project.reline_model_downloaded_marker", meta = meta.display_title())
                        } else {
                            meta.display_title()
                        };
                        let response =
                            ui.selectable_label(selected, RichText::new(header).strong());
                        ui.label(RichText::new(&name).small().weak());
                        ui.label(RichText::new(&meta.description).small());
                        if let Some(recommendation) = &meta.recommendation {
                            ui.label(
                                RichText::new(recommendation)
                                    .small()
                                    .color(egui::Color32::from_rgb(120, 190, 120)),
                            );
                        }
                        if response.clicked() {
                            // Pick the model and collapse the list back to the compact card.
                            self.reline_model_name = name;
                            self.reline_model_picker_open = false;
                        }
                        ui.add_space(6.0);
                    }
                });
        }
    }

    /// Full (expert) Reline UI: every pipeline node with raw parameters. Kept behind the toggle.
    fn show_reline_full(&mut self, ui: &mut Ui) {
        const READER_MODES: [&str; 3] = ["rgb", "gray", "dynamic"];
        const TILERS: [&str; 3] = ["exact", "max", "no_tiling"];
        const DTYPES: [&str; 3] = ["F32", "F16", "BF16"];
        const CANNY_TYPES: [&str; 3] = ["invert", "normal", "unsharp"];
        const DOT_TYPES: [&str; 5] = ["line", "cross", "ellipse", "invline", "circle"];
        const HALFTONE_MODES: [&str; 4] = ["gray", "rgb", "hsv", "cmyk"];
        const HALFTONE_FILTERS: [&str; 22] = [
            "nearest",
            "box",
            "sbox4",
            "sbox8",
            "linear",
            "slinear4",
            "slinear8",
            "hamming",
            "shamming4",
            "shamming8",
            "catmullrom",
            "scatmullrom4",
            "scatmullrom8",
            "mitchell",
            "smitchell4",
            "smitchell8",
            "lanczos",
            "slanczos4",
            "slanczos8",
            "gauss",
            "sgauss4",
            "sgauss8",
        ];
        const RESIZE_FILTERS: [&str; 33] = [
            "nearest",
            "box",
            "sbox4",
            "sbox8",
            "ibox",
            "linear",
            "slinear4",
            "slinear8",
            "ilinear",
            "hamming",
            "shamming4",
            "shamming8",
            "ihamming",
            "catmullrom",
            "scatmullrom4",
            "scatmullrom8",
            "icatmullrom",
            "mitchell",
            "smitchell4",
            "smitchell8",
            "imitchell",
            "lanczos",
            "slanczos4",
            "slanczos8",
            "ilanczos",
            "gauss",
            "sgauss4",
            "sgauss8",
            "igauss",
            "dpid_0.25",
            "dpid_0.5",
            "dpid_0.75",
            "dpid_1",
        ];
        const CVT_TYPES: [&str; 4] = ["RGB2Gray2020", "RGB2Gray709", "RGB2Gray", "Gray2RGB"];

        self.ensure_reline_model_catalog_requested();

        field_label_hover(ui, t!("launcher.new_project.reline_read_mode_label"), t!("launcher.new_project.reline_read_mode_tooltip"));
        combo_index(
            ui,
            "launcher_new_project_reline_reader_mode",
            &READER_MODES,
            &mut self.reline_reader_mode,
        );

        let response = egui::CollapsingHeader::new(t!("launcher.new_project.reline_section_restore")).id_salt("launcher.new_project.reline_section_restore")
            .default_open(true)
            .show(ui, |ui| {
                ui.checkbox(
                    &mut self.reline_upscale_enabled,
                    t!("launcher.new_project.reline_enable_restore"),
                );
                field_label(ui, t!("launcher.new_project.reline_model_from_catalog"));
                self.show_reline_model_combo(ui);
                field_label(ui, t!("launcher.new_project.reline_local_model_path"));
                ui.add(
                    TextEdit::singleline(&mut self.reline_model_path).desired_width(fill_width(ui)),
                );
                field_label(ui, t!("launcher.new_project.reline_direct_url"));
                ui.add(
                    TextEdit::singleline(&mut self.reline_model_url).desired_width(fill_width(ui)),
                );
                field_label(ui, t!("launcher.new_project.reline_tiling"));
                combo_index(
                    ui,
                    "launcher_new_project_reline_tiler",
                    &TILERS,
                    &mut self.reline_tiler,
                );
                field_label(ui, t!("launcher.new_project.reline_target_scale_field_hint"));
                ui.add(TextEdit::singleline(&mut self.reline_target_scale).desired_width(160.0));
                field_label(ui, t!("launcher.new_project.reline_compute_type"));
                combo_index(
                    ui,
                    "launcher_new_project_reline_dtype",
                    &DTYPES,
                    &mut self.reline_dtype,
                );
                field_label(ui, t!("launcher.new_project.reline_exact_tile_size"));
                ui.add(
                    TextEdit::singleline(&mut self.reline_exact_tiler_size).desired_width(160.0),
                );
                ui.checkbox(
                    &mut self.reline_allow_cpu_upscale,
                    t!("launcher.new_project.reline_allow_cpu"),
                );
            });
        response.header_response.on_hover_text(t!("launcher.new_project.reline_restore_tooltip"));

        let response = egui::CollapsingHeader::new(t!("launcher.new_project.reline_section_sharpen")).id_salt("launcher.new_project.reline_section_sharpen").show(ui, |ui| {
            ui.checkbox(&mut self.reline_sharp_enabled, t!("launcher.new_project.reline_enable_sharpen"));
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_input_low_level"),
                &mut self.reline_sharp_low_input,
            );
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_input_high_level"),
                &mut self.reline_sharp_high_input,
            );
            numeric_text_field(ui, t!("launcher.new_project.reline_gamma"), &mut self.reline_sharp_gamma);
            numeric_text_field(ui, t!("launcher.new_project.reline_white_range"), &mut self.reline_sharp_diapason_white);
            numeric_text_field(ui, t!("launcher.new_project.reline_black_range"), &mut self.reline_sharp_diapason_black);
            ui.checkbox(&mut self.reline_sharp_canny, t!("launcher.new_project.reline_canny_contour"));
            field_label(ui, t!("launcher.new_project.reline_canny_mode"));
            combo_index(
                ui,
                "launcher_new_project_reline_canny_type",
                &CANNY_TYPES,
                &mut self.reline_sharp_canny_type,
            );
        });
        response.header_response.on_hover_text(t!("launcher.new_project.reline_sharpen_tooltip"));

        let response = egui::CollapsingHeader::new(t!("launcher.new_project.reline_section_halftone")).id_salt("launcher.new_project.reline_section_halftone").show(ui, |ui| {
            ui.checkbox(&mut self.reline_halftone_enabled, t!("launcher.new_project.reline_enable_halftone"));
            numeric_text_field(ui, t!("launcher.new_project.reline_dot_size"), &mut self.reline_halftone_dot_size);
            numeric_text_field(ui, t!("launcher.new_project.reline_angle"), &mut self.reline_halftone_angle);
            field_label(ui, t!("launcher.new_project.reline_dot_type"));
            combo_index(
                ui,
                "launcher_new_project_reline_dot_type",
                &DOT_TYPES,
                &mut self.reline_halftone_dot_type,
            );
            field_label(ui, t!("launcher.new_project.reline_halftone_color_mode"));
            combo_index(
                ui,
                "launcher_new_project_reline_halftone_mode",
                &HALFTONE_MODES,
                &mut self.reline_halftone_mode,
            );
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_ssaa_scale"),
                &mut self.reline_halftone_ssaa_scale,
            );
            field_label(ui, t!("launcher.new_project.reline_ssaa_filter"));
            combo_index(
                ui,
                "launcher_new_project_reline_halftone_filter",
                &HALFTONE_FILTERS,
                &mut self.reline_halftone_ssaa_filter,
            );
            ui.checkbox(
                &mut self.reline_halftone_disable_auto_dot,
                t!("launcher.new_project.reline_disable_auto_dot"),
            );
        });
        response.header_response.on_hover_text(t!("launcher.new_project.reline_halftone_tooltip"));

        let response = egui::CollapsingHeader::new(t!("launcher.new_project.reline_section_resize")).id_salt("launcher.new_project.reline_section_resize").show(ui, |ui| {
            ui.checkbox(
                &mut self.reline_resize_enabled,
                t!("launcher.new_project.reline_enable_resize"),
            );
            numeric_text_field(ui, t!("launcher.new_project.reline_height"), &mut self.reline_resize_height);
            numeric_text_field(ui, t!("launcher.new_project.reline_width"), &mut self.reline_resize_width);
            numeric_text_field(ui, t!("launcher.new_project.reline_percent"), &mut self.reline_resize_percent);
            field_label(ui, t!("launcher.new_project.reline_filter"));
            combo_index(
                ui,
                "launcher_new_project_reline_resize_filter",
                &RESIZE_FILTERS,
                &mut self.reline_resize_filter,
            );
            ui.checkbox(&mut self.reline_resize_gamma_correction, t!("launcher.new_project.reline_gamma_correction"));
            ui.checkbox(&mut self.reline_resize_spread, t!("launcher.new_project.reline_spread"));
            numeric_text_field(ui, t!("launcher.new_project.reline_spread_size"), &mut self.reline_resize_spread_size);
        });
        response.header_response.on_hover_text(t!("launcher.new_project.reline_resize_tooltip"));

        let response = egui::CollapsingHeader::new(t!("launcher.new_project.reline_section_levels")).id_salt("launcher.new_project.reline_section_levels").show(ui, |ui| {
            ui.checkbox(&mut self.reline_level_enabled, t!("launcher.new_project.reline_enable_levels"));
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_input_low_level"),
                &mut self.reline_level_low_input,
            );
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_input_high_level"),
                &mut self.reline_level_high_input,
            );
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_output_low_level"),
                &mut self.reline_level_low_output,
            );
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_output_high_level"),
                &mut self.reline_level_high_output,
            );
            numeric_text_field(ui, t!("launcher.new_project.reline_gamma"), &mut self.reline_level_gamma);
        });
        response.header_response.on_hover_text(t!("launcher.new_project.reline_levels_tooltip"));

        let response = egui::CollapsingHeader::new(t!("launcher.new_project.reline_section_colorconv")).id_salt("launcher.new_project.reline_section_colorconv").show(ui, |ui| {
            ui.checkbox(
                &mut self.reline_cvt_color_enabled,
                t!("launcher.new_project.reline_enable_colorconv"),
            );
            field_label(ui, t!("launcher.new_project.reline_conv_type"));
            combo_index(
                ui,
                "launcher_new_project_reline_cvt_type",
                &CVT_TYPES,
                &mut self.reline_cvt_color_type,
            );
        });
        response.header_response.on_hover_text(t!("launcher.new_project.reline_colorconv_tooltip"));

        if button_sized(
            ui,
            t!("launcher.new_project.run_reline_button"),
            egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
            self.can_start_reline(),
        )
        .clicked()
        {
            self.start_reline();
        }
        self.show_operation_progress(ui, "reline");
    }

    fn ensure_reline_model_catalog_requested(&mut self) {
        if self.reline_model_catalog_requested {
            return;
        }
        self.reline_model_catalog_requested = true;
        self.reline_model_catalog_error = None;
        self.reline_model_catalog.begin();
    }

    fn refresh_reline_model_catalog(&mut self) {
        self.reline_model_catalog_requested = true;
        self.reline_model_catalog_error = None;
        self.reline_model_catalog.begin();
    }

    fn show_reline_model_combo(&mut self, ui: &mut Ui) {
        let mut options: Vec<(String, String, String)> = self
            .reline_model_catalog_entries
            .iter()
            .map(|entry| {
                let label = if entry.downloaded {
                    tf!("launcher.new_project.reline_entry_downloaded_marker", entry = entry.name)
                } else {
                    entry.name.clone()
                };
                (entry.name.clone(), label, entry.filename.clone())
            })
            .collect();

        let current_model = self.reline_model_name.trim();
        if !current_model.is_empty() && !options.iter().any(|(name, _, _)| name == current_model) {
            options.insert(
                0,
                (
                    current_model.to_string(),
                    tf!("launcher.new_project.reline_current_value_marker", current_model = current_model),
                    String::new(),
                ),
            );
        }

        ui.horizontal(|ui| {
            let refresh_width = 96.0;
            let combo_width =
                (ui.available_width() - refresh_width - ui.spacing().item_spacing.x).max(150.0);
            ComboBox::from_id_salt("launcher_new_project_reline_model_name")
                .width(combo_width)
                .selected_text(if self.reline_model_name.trim().is_empty() {
                    t!("launcher.new_project.reline_not_selected")
                } else {
                    self.reline_model_name.as_str()
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.reline_model_name,
                        String::new(),
                        t!("launcher.new_project.reline_dont_pick_from_catalog"),
                    )
                    .on_hover_text(t!("launcher.new_project.reline_use_local_or_url_hint"));
                    if options.is_empty() {
                        ui.label(t!("launcher.new_project.reline_models_not_loaded"));
                    } else {
                        for (name, label, filename) in &options {
                            let response = ui.selectable_value(
                                &mut self.reline_model_name,
                                name.clone(),
                                label,
                            );
                            if !filename.trim().is_empty() {
                                response.on_hover_text(tf!("launcher.new_project.reline_file_label", filename = filename));
                            }
                        }
                    }
                });

            let refresh_label = if self.reline_model_catalog.is_loading() {
                t!("launcher.common.loading")
            } else {
                t!("launcher.common.refresh_button")
            };
            if ui
                .add_enabled(
                    !self.reline_model_catalog.is_loading(),
                    Button::new(refresh_label).min_size(egui::vec2(refresh_width, 28.0)),
                )
                .on_hover_text(t!("launcher.new_project.reline_refresh_models_tooltip"))
                .clicked()
            {
                self.refresh_reline_model_catalog();
            }
        });

        if self.reline_model_catalog.is_loading() {
            ui.label(
                RichText::new(t!("launcher.new_project.reline_loading_models_status"))
                    .small()
                    .weak(),
            );
        }
        if let Some(error) = &self.reline_model_catalog_error {
            ui.colored_label(egui::Color32::from_rgb(236, 112, 99), error);
        }
    }

    fn show_save_section(&mut self, ui: &mut Ui) {
        let titles = self.project_catalog_snapshot.titles.clone();
        let alt_chapters = self.current_alt_chapter_options().to_vec();
        let can_refresh_catalog = !self.project_catalog.is_loading() && !self.save.is_loading();
        let can_save = self.can_start_save();

        ui.vertical(|ui| {
            let mut project_base = self.save_mode == SaveMode::ProjectBase;
            if ui
                .checkbox(&mut project_base, t!("launcher.new_project.save_as_project_base_button"))
                .changed()
                && project_base
            {
                self.save_mode = SaveMode::ProjectBase;
            }

            let mut alt_version = self.save_mode == SaveMode::AltVersion;
            if ui
                .checkbox(&mut alt_version, t!("launcher.new_project.save_as_alt_version_button"))
                .changed()
                && alt_version
            {
                self.save_mode = SaveMode::AltVersion;
            }

            let mut independent = self.save_mode == SaveMode::Independent;
            if ui
                .checkbox(&mut independent, t!("launcher.new_project.save_independent_button"))
                .changed()
                && independent
            {
                self.save_mode = SaveMode::Independent;
            }
        });
        ui.add_space(6.0);
        ui.label(
            RichText::new(tf!("launcher.new_project.projects_folder_label", arg = self.project_catalog.projects_root().display()))
            .small()
            .weak(),
        );
        if self.project_catalog.is_loading() {
            ui.label(
                RichText::new(t!("launcher.new_project.refreshing_titles_status"))
                    .small()
                    .weak(),
            );
        } else if let Some(error) = &self.project_catalog_error {
            ui.colored_label(egui::Color32::from_rgb(255, 120, 120), error);
        }

        if self.save_mode == SaveMode::ProjectBase {
            ui.add_space(10.0);
            sub_group(
                ui,
                t!("launcher.new_project.save_as_project_base_button"),
                |ui| {
                    field_label(ui, t!("launcher.common.title_label"));
                    let save_title_response = self.save_title_combo.draw(
                        ui,
                        &mut self.save_title_input,
                        titles.as_slice(),
                    );
                    if save_title_response.changed {
                        self.sync_save_title_from_input();
                    }
                    if button_sized(ui, t!("launcher.common.refresh_button"), SMALL_BUTTON_SIZE, can_refresh_catalog)
                        .clicked()
                    {
                        self.refresh_project_catalog();
                    }
                    field_label(ui, t!("launcher.new_project.chapter_name_label"));
                    ui.add(
                        TextEdit::singleline(&mut self.save_chapter).desired_width(fill_width(ui)),
                    );
                    if button_sized(
                        ui,
                        t!("launcher.common.save_and_open_button"),
                        egui::vec2(LEFT_PANEL_WIDTH - 84.0, 32.0),
                        can_save,
                    )
                    .clicked()
                    {
                        self.start_save_to_project(true);
                    }
                    if button_sized(
                        ui,
                        t!("launcher.new_project.save_to_project_button"),
                        egui::vec2(LEFT_PANEL_WIDTH - 84.0, 32.0),
                        can_save,
                    )
                    .clicked()
                    {
                        self.start_save_to_project(false);
                    }
                },
            );
        }

        if self.save_mode == SaveMode::AltVersion {
            ui.add_space(10.0);
            sub_group(
                ui,
                t!("launcher.new_project.save_as_alt_version_button"),
                |ui| {
                    field_label(ui, t!("launcher.common.title_label"));
                    let previous_alt_title = self.alt_title;
                    combo_index_owned(
                        ui,
                        "launcher_new_project_alt_title",
                        titles.as_slice(),
                        &mut self.alt_title,
                    );
                    if previous_alt_title != self.alt_title {
                        self.alt_chapter = 0;
                    }
                    if button_sized(ui, t!("launcher.common.refresh_button"), SMALL_BUTTON_SIZE, can_refresh_catalog)
                        .clicked()
                    {
                        self.refresh_project_catalog();
                    }

                    field_label(ui, t!("launcher.common.chapter_label"));
                    combo_index_owned(
                        ui,
                        "launcher_new_project_alt_chapter",
                        alt_chapters.as_slice(),
                        &mut self.alt_chapter,
                    );
                    if button_sized(ui, t!("launcher.common.refresh_button"), SMALL_BUTTON_SIZE, can_refresh_catalog)
                        .clicked()
                    {
                        self.refresh_project_catalog();
                    }

                    field_label(ui, t!("launcher.new_project.alt_version_name_label"));
                    ui.add(TextEdit::singleline(&mut self.alt_name).desired_width(fill_width(ui)));
                    if button_sized(
                        ui,
                        t!("launcher.new_project.save_as_alt_version_short_button"),
                        egui::vec2(LEFT_PANEL_WIDTH - 84.0, 32.0),
                        can_save,
                    )
                    .clicked()
                    {
                        self.start_save_alt_version();
                    }
                },
            );
        }

        if self.save_mode == SaveMode::Independent {
            ui.add_space(10.0);
            sub_group(ui, t!("launcher.new_project.save_independent_button"), |ui| {
                if button_sized(
                    ui,
                    t!("launcher.new_project.save_to_folder_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 84.0, 32.0),
                    can_save,
                )
                .clicked()
                {
                    self.start_save_to_folder();
                }
            });
        }
        self.show_operation_progress(ui, "save");
    }

    fn show_viewer_panel(&mut self, ui: &mut Ui) {
        ui.add_space(2.0);
        Frame::group(ui.style())
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                ui.set_min_height(VIEWER_MIN_HEIGHT);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(t!("launcher.new_project.pages_preview_title")).size(20.0).strong());
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new(tf!("launcher.new_project.pages_on_ribbon_count", arg = self.ribbon.pages().len()))
                            .small()
                            .weak(),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if self.has_manual_cut_preview() {
                                let button = Button::new(
                                    RichText::new(t!("launcher.new_project.cut_button"))
                                        .size(14.0)
                                        .strong()
                                        .color(egui::Color32::WHITE),
                                )
                                .fill(egui::Color32::from_rgb(183, 28, 28))
                                .stroke(Stroke::new(1.0, egui::Color32::from_rgb(255, 121, 121)))
                                .corner_radius(999.0);
                                if ui
                                    .add_enabled(
                                        !self.stitch.is_loading(),
                                        button.min_size(MANUAL_CUT_APPLY_BUTTON_SIZE),
                                    )
                                    .clicked()
                                {
                                    self.apply_manual_cut_guides();
                                }
                                ui.add_space(8.0);
                            }
                            if let Some(folder) = self.ribbon.loaded_source() {
                                ui.label(
                                    RichText::new(folder.display().to_string()).small().weak(),
                                );
                            } else {
                                ui.label(RichText::new(t!("launcher.new_project.folder_not_selected")).small().weak());
                            }
                        });
                    });

                    ui.add_space(6.0);
                    ui.label(RichText::new(&self.import_status).small());
                    if let Some(error) = &self.last_error {
                        ui.add_space(4.0);
                        ui.colored_label(egui::Color32::from_rgb(255, 120, 120), error);
                    }

                    ui.add_space(10.0);
                    self.show_ribbon(ui);
                });
            });
    }

    fn show_ribbon(&mut self, ui: &mut Ui) {
        let available_height = safe_dimension(ui.available_size_before_wrap().y, VIEWER_MIN_HEIGHT);
        Frame::new()
            .fill(egui::Color32::from_rgba_premultiplied(18, 18, 22, 160))
            .stroke(Stroke::new(
                1.0,
                ui.visuals().widgets.noninteractive.bg_stroke.color,
            ))
            .corner_radius(12.0)
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.set_min_height((available_height - 8.0).max(220.0));
                let mut cut_marker_screen_positions = Vec::new();
                let mut page_boundary_screen_positions = Vec::new();
                let scroll_output = MarkedScrollArea::vertical("launcher_new_project_ribbon_scroll")
                    .floating(false)
                    .gutter_width(MANUAL_CUT_SCROLL_ARROW_WIDTH)
                    .show(ui, |ui| {
                        if self.source_import.is_loading()
                            || self.advanced_download.is_loading()
                            || self.quick_download.is_loading()
                            || self.test_chapter_check_rx.is_some()
                            || self.stitch.is_loading()
                            || self.save.is_loading()
                            || self.waifu2x.is_loading()
                            || self.reline.is_loading()
                        {
                            let progress = self.current_progress(true);
                            ui.add(
                                ProgressBar::new(progress.fraction)
                                    .animate(true)
                                    .desired_width(fill_width(ui))
                                    .text(progress.label),
                            );
                            return;
                        }

                        if self.ribbon.pages().is_empty() {
                            ui.add_space(24.0);
                            ui.vertical_centered(|ui| {
                                ui.label(RichText::new(t!("launcher.new_project.ribbon_empty_hint")).size(18.0));
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new(
                                        t!("launcher.new_project.ribbon_empty_description"),
                                    )
                                    .small()
                                    .weak(),
                                );
                            });
                            ui.add_space(24.0);
                            return;
                        }

                        let pages_len = self.ribbon.pages().len();
                        let mut ribbon_action = None;
                        let mut add_manual_cut_at = None;
                        let mut manual_cut_context_guide = self.manual_cut_context_guide;
                        let mut selected_page = self.selected_ribbon_page;
                        for index in 0..pages_len {
                            let mut manual_overlay = None;
                            ui.vertical(|ui| {
                                {
                                    let page = &mut self.ribbon.pages_mut()[index];
                                    let width_scale = safe_dimension(ui.available_width(), 120.0)
                                        / page.original_size[0].max(1) as f32;
                                    ui.label(
                                        RichText::new(format!(
                                            "{}  {}x{}",
                                            page.name,
                                            page.original_size[0],
                                            page.original_size[1]
                                        ))
                                        .small()
                                        .weak(),
                                    );
                                    let image_size = egui::vec2(
                                        page.original_size[0] as f32 * width_scale,
                                        page.original_size[1] as f32 * width_scale,
                                    );
                                    let (image_rect, image_response) =
                                        ui.allocate_exact_size(image_size, egui::Sense::click());
                                    if image_response.clicked() {
                                        selected_page = Some(index);
                                    }
                                    if image_response.secondary_clicked()
                                        && let Some(pointer_pos) = image_response.interact_pointer_pos()
                                    {
                                        let page_height = page.original_size[1].max(1);
                                        let image_y =
                                            ((pointer_pos.y - image_rect.top()) / width_scale)
                                                .round()
                                                .clamp(
                                                    1.0,
                                                    page_height.saturating_sub(1) as f32,
                                                )
                                                as usize;
                                        manual_cut_context_guide = Some(ManualCutGuide {
                                            page_index: index,
                                            y: image_y,
                                        });
                                    }
                                    image_response.context_menu(|ui| {
                                        if ui.button(t!("launcher.new_project.add_cut_line_menu")).clicked() {
                                            add_manual_cut_at = manual_cut_context_guide;
                                            ui.close();
                                        }
                                        if ui
                                            .add_enabled(
                                                index > 0,
                                                Button::new(t!("launcher.new_project.stitch_with_prev_menu")),
                                            )
                                            .clicked()
                                        {
                                            ribbon_action =
                                                Some((index, RibbonImageControlAction::MergeWithPrevious));
                                            ui.close();
                                        }
                                        if ui
                                            .add_enabled(
                                                index + 1 < pages_len,
                                                Button::new(t!("launcher.new_project.stitch_with_next_menu")),
                                            )
                                            .clicked()
                                        {
                                            ribbon_action =
                                                Some((index, RibbonImageControlAction::MergeWithNext));
                                            ui.close();
                                        }
                                    });
                                    let viewport_rect = ui.clip_rect().expand(128.0);
                                    for (tile_index, tile) in page.tiles.iter_mut().enumerate() {
                                        if tile.texture.is_none() {
                                            let texture = ui.ctx().load_texture(
                                                format!("launcher-new-project-ribbon-{index}-{tile_index}"),
                                                tile.color_image.clone(),
                                                TextureOptions::LINEAR,
                                            );
                                            tile.texture = Some(texture);
                                        }
                                        if let Some(texture) = tile.texture.as_ref() {
                                            let tile_rect = egui::Rect::from_min_size(
                                                egui::pos2(
                                                    image_rect.left()
                                                        + tile.origin_px[0] as f32 * width_scale,
                                                    image_rect.top()
                                                        + tile.origin_px[1] as f32 * width_scale,
                                                ),
                                                egui::vec2(
                                                    tile.size[0] as f32 * width_scale,
                                                    tile.size[1] as f32 * width_scale,
                                                ),
                                            );
                                            if tile_rect.intersects(viewport_rect) {
                                                ui.painter().image(
                                                    texture.id(),
                                                    tile_rect,
                                                    egui::Rect::from_min_max(
                                                        egui::Pos2::ZERO,
                                                        egui::pos2(1.0, 1.0),
                                                    ),
                                                    egui::Color32::WHITE,
                                                );
                                            }
                                        }
                                    }
                                    if selected_page == Some(index) {
                                        ui.painter().rect_stroke(
                                            image_rect.expand(2.0),
                                            10.0,
                                            Stroke::new(
                                                2.0,
                                                egui::Color32::from_rgb(247, 196, 97),
                                            ),
                                            egui::StrokeKind::Outside,
                                        );
                                    }

                                    ribbon_action = show_ribbon_image_controls(
                                        ui,
                                        image_rect,
                                        index,
                                        pages_len,
                                    )
                                    .or(ribbon_action);

                                    if index + 1 < pages_len {
                                        page_boundary_screen_positions.push(image_rect.bottom());
                                    }
                                    manual_overlay = Some((image_rect, page.original_size, width_scale));
                                }
                                if let Some((image_rect, original_size, width_scale)) = manual_overlay
                                    && self.should_show_manual_cut_guides(index) {
                                        cut_marker_screen_positions.extend(self.draw_manual_cut_guides(
                                            ui,
                                            index,
                                            image_rect,
                                            original_size,
                                            width_scale,
                                        ));
                                    }
                            });
                            if index + 1 < pages_len {
                                ui.add_space(RIBBON_PREVIEW_SPACING);
                            }
                        }
                        self.selected_ribbon_page =
                            selected_page.map(|index| index.min(pages_len.saturating_sub(1)));
                        self.manual_cut_context_guide = manual_cut_context_guide;
                        self.apply_ribbon_action(ribbon_action);
                        if let Some(guide) = add_manual_cut_at {
                            self.add_manual_cut_guide(guide);
                        }
                    });
                self.draw_manual_cut_scroll_markers(
                    &scroll_output,
                    ui.painter(),
                    &cut_marker_screen_positions,
                    &page_boundary_screen_positions,
                );
            });
    }

    fn can_start_stitch(&self) -> bool {
        !self.source_import.is_loading()
            && !self.advanced_download.is_loading()
            && !self.quick_download.is_loading()
            && !self.stitch.is_loading()
            && !self.save.is_loading()
            && !self.waifu2x.is_loading()
            && !self.reline.is_loading()
            && !self.ribbon.pages().is_empty()
    }

    fn apply_source_import_result(&mut self, source_path: PathBuf, pages: Vec<RibbonPage>) {
        self.simple_stitch_done = false;
        self.simple_manual_cut_preview_active = false;
        self.advance_simple_import_step_after_success();
        if self.import_mode == ImportMode::ReplaceRibbon || self.ribbon.pages().is_empty() {
            self.ribbon.replace_source(source_path, pages);
            self.selected_ribbon_page = self.default_selected_page();
            return;
        }

        let insert_at = match self.import_mode {
            ImportMode::ReplaceRibbon => 0,
            ImportMode::AddToStart => 0,
            ImportMode::AddToEnd => self.ribbon.pages().len(),
            ImportMode::AddBeforeCurrent => self
                .selected_ribbon_page
                .unwrap_or(0)
                .min(self.ribbon.pages().len()),
            ImportMode::AddAfterCurrent => self
                .selected_ribbon_page
                .map_or(self.ribbon.pages().len(), |index| {
                    index.saturating_add(1).min(self.ribbon.pages().len())
                }),
        };
        let inserted_range = self.ribbon.insert_pages(source_path, insert_at, pages);
        self.selected_ribbon_page = Some(inserted_range.start);
    }

    fn default_selected_page(&self) -> Option<usize> {
        (!self.ribbon.pages().is_empty()).then_some(0)
    }

    fn selection_after_removal(&self, removed_index: usize) -> Option<usize> {
        let remaining_pages = self.ribbon.pages().len();
        if remaining_pages == 0 {
            None
        } else if removed_index >= remaining_pages {
            Some(remaining_pages - 1)
        } else {
            Some(removed_index)
        }
    }

    fn swap_manual_cut_guide_pages(&mut self, first_index: usize, second_index: usize) {
        for guide in &mut self.manual_cut_guides {
            if guide.page_index == first_index {
                guide.page_index = second_index;
            } else if guide.page_index == second_index {
                guide.page_index = first_index;
            }
        }
    }

    fn remove_manual_cut_guide_page(&mut self, removed_index: usize) {
        self.manual_cut_guides
            .retain(|guide| guide.page_index != removed_index);
        for guide in &mut self.manual_cut_guides {
            if guide.page_index > removed_index {
                guide.page_index -= 1;
            }
        }
    }

    fn merge_manual_cut_guide_pages(&mut self, first_index: usize, first_height: usize) {
        for guide in &mut self.manual_cut_guides {
            if guide.page_index == first_index + 1 {
                guide.page_index = first_index;
                guide.y = guide.y.saturating_add(first_height);
            } else if guide.page_index > first_index + 1 {
                guide.page_index -= 1;
            }
        }
    }

    fn merge_ribbon_pages(&mut self, first_index: usize) {
        let Some(first_height) = self
            .ribbon
            .pages()
            .get(first_index)
            .map(|page| page.original_size[1])
        else {
            return;
        };
        match self.ribbon.merge_with_next(first_index) {
            Ok(()) => {
                self.merge_manual_cut_guide_pages(first_index, first_height);
                self.clamp_manual_cut_guides_to_current_pages();
                self.selected_ribbon_page = Some(first_index);
                self.import_status = t!("launcher.new_project.pages_stitched_status").to_string();
                self.last_error = None;
            }
            Err(RibbonMergeError::WidthMismatch {
                first_name,
                first_width,
                second_name,
                second_width,
            }) => {
                self.last_error = Some(tf!("launcher.new_project.stitch_width_mismatch_error", first_name = first_name, first_width = first_width, second_name = second_name, second_width = second_width));
                self.import_status = t!("launcher.new_project.stitch_pages_cancelled").to_string();
            }
            Err(RibbonMergeError::MissingPage) => {
                self.last_error = Some(t!("launcher.new_project.no_adjacent_page_error").to_string());
                self.import_status = t!("launcher.new_project.stitch_pages_unavailable").to_string();
            }
        }
    }

    fn apply_ribbon_action(&mut self, action: Option<(usize, RibbonImageControlAction)>) {
        let Some((index, action)) = action else {
            return;
        };
        match action {
            RibbonImageControlAction::Crop => self.open_crop_editor(index),
            RibbonImageControlAction::MoveUp => {
                if self.ribbon.move_page_up(index) {
                    self.swap_manual_cut_guide_pages(index - 1, index);
                    self.selected_ribbon_page = Some(index - 1);
                    self.import_status = t!("launcher.new_project.image_moved_up").to_string();
                    self.last_error = None;
                }
            }
            RibbonImageControlAction::MoveDown => {
                if self.ribbon.move_page_down(index) {
                    self.swap_manual_cut_guide_pages(index, index + 1);
                    self.selected_ribbon_page = Some(index + 1);
                    self.import_status = t!("launcher.new_project.image_moved_down").to_string();
                    self.last_error = None;
                }
            }
            RibbonImageControlAction::Delete => {
                if let Some(removed_page) = self.ribbon.remove_page(index) {
                    self.selected_ribbon_page = self.selection_after_removal(index);
                    self.remove_manual_cut_guide_page(index);
                    self.clamp_manual_cut_guides_to_current_pages();
                    self.import_status =
                        tf!("launcher.new_project.image_removed_status", removed_page = removed_page.name);
                    self.last_error = None;
                }
            }
            RibbonImageControlAction::MergeWithPrevious => {
                if index > 0 {
                    self.merge_ribbon_pages(index - 1);
                }
            }
            RibbonImageControlAction::MergeWithNext => {
                self.merge_ribbon_pages(index);
            }
        }
    }

    fn open_crop_editor(&mut self, index: usize) {
        let Some(page) = self.ribbon.pages().get(index) else {
            return;
        };
        let source_image = page.source_image();
        let source_size = page.source_size();
        self.crop_editor = Some(CropEditorState {
            page_index: index,
            page_name: page.name.clone(),
            source_size,
            crop_rect: page.crop().unwrap_or(RibbonCrop {
                left: 0,
                top: 0,
                width: source_size[0].max(1),
                height: source_size[1].max(1),
            }),
            tiles: build_ribbon_tiles(source_image.as_ref()),
            drag_state: None,
            window_rect: None,
        });
    }

    fn show_crop_editor_window(&mut self, ctx: &egui::Context) {
        let Some(editor) = self.crop_editor.as_mut() else {
            return;
        };
        let viewport = ctx.content_rect().shrink(16.0);
        let max_window_size = egui::vec2(viewport.width(), viewport.height() * 0.8);
        let default_size = egui::vec2(
            viewport.width().clamp(CROP_WINDOW_MIN_SIZE.x, 920.0),
            max_window_size.y.clamp(CROP_WINDOW_MIN_SIZE.y, 760.0),
        );
        let window_size = editor
            .window_rect
            .map(|rect| rect.size())
            .unwrap_or(default_size)
            .min(max_window_size);
        let fallback_pos = egui::pos2(
            viewport.center().x - window_size.x * 0.5,
            viewport.center().y - window_size.y * 0.5,
        );
        let window_pos = clamp_window_pos_to_viewport(
            editor
                .window_rect
                .map(|rect| rect.min)
                .unwrap_or(fallback_pos),
            window_size,
            viewport,
        );

        let mut keep_open = true;
        let mut request_apply = false;
        let shown = Window::new(tf!("launcher.new_project.crop_editor_title", editor = editor.page_name))
            .id(egui::Id::new((
                "launcher_new_project_crop",
                editor.page_index,
            )))
            .default_size(default_size)
            .current_pos(window_pos)
            .min_size(CROP_WINDOW_MIN_SIZE.min(max_window_size))
            .max_size(max_window_size)
            .collapsible(false)
            .resizable(true)
            .constrain_to(viewport)
            .open(&mut keep_open)
            .show(ctx, |ui| {
                ui.set_min_size(CROP_WINDOW_MIN_SIZE.min(max_window_size));
                ui.label(
                    RichText::new(
                        t!("launcher.new_project.crop_editor_hint"),
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(8.0);
                ScrollArea::both()
                    .id_salt(("launcher_new_project_crop_scroll", editor.page_index))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        draw_crop_editor_canvas(ui, editor);
                    });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(t!("launcher.new_project.reset_button")).clicked() {
                        editor.crop_rect = RibbonCrop {
                            left: 0,
                            top: 0,
                            width: editor.source_size[0].max(1),
                            height: editor.source_size[1].max(1),
                        };
                        editor.drag_state = None;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_sized(egui::vec2(132.0, 32.0), Button::new(t!("launcher.new_project.apply_button")))
                            .clicked()
                        {
                            request_apply = true;
                        }
                    });
                });
            });
        if let Some(shown) = shown {
            editor.window_rect = Some(shown.response.rect);
        }

        if request_apply {
            let page_index = editor.page_index;
            let page_name = editor.page_name.clone();
            let crop_rect = editor.crop_rect;
            if self.ribbon.apply_crop(page_index, crop_rect) {
                self.clamp_manual_cut_guides_to_current_pages();
                self.import_status = tf!("launcher.new_project.image_cropped_status", page_name = page_name);
                self.last_error = None;
            } else {
                self.last_error = Some(t!("launcher.new_project.apply_crop_error").to_string());
            }
            self.crop_editor = None;
            return;
        }

        if !keep_open {
            self.crop_editor = None;
        }
    }

    fn has_manual_cut_preview(&self) -> bool {
        !self.manual_cut_guides.is_empty()
    }

    fn should_show_manual_cut_guides(&self, page_index: usize) -> bool {
        self.manual_cut_guides
            .iter()
            .any(|guide| guide.page_index == page_index)
    }

    fn add_manual_cut_guide(&mut self, guide: ManualCutGuide) {
        let Some(page) = self.ribbon.pages().get(guide.page_index) else {
            return;
        };
        if !manual_cut_y_is_valid(guide.y, page.original_size[1]) {
            self.last_error = Some(tf!("launcher.new_project.cut_line_min_distance_error", manual_cut_min_edge_distance_px = MANUAL_CUT_MIN_EDGE_DISTANCE_PX));
            self.import_status = t!("launcher.new_project.cut_line_not_added").to_string();
            return;
        };
        self.manual_cut_guides.push(guide);
        self.clamp_manual_cut_guides_to_current_pages();
        self.import_status = tf!("launcher.new_project.manual_cut_added_status", arg = self.manual_cut_guides.len());
        self.last_error = None;
    }

    fn start_stitch_split(&mut self, mode: StitchSplitMode) {
        let Some(options) = self.parse_stitch_options(mode) else {
            return;
        };
        let images = self.current_stitch_images();
        if images.is_empty() {
            self.last_error = Some(t!("launcher.new_project.open_or_download_chapter_error").to_string());
            self.import_status = t!("launcher.new_project.stitch_no_images_error").to_string();
            return;
        }
        self.manual_cut_guides.clear();
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "stitch",
            stage: stitch_mode_initial_stage(mode).to_string(),
            current: 0,
            total: 1,
        });
        self.import_status = stitch_mode_start_status(mode).to_string();
        self.stitch
            .begin(StitchRequest::StitchSplit { images, options });
    }

    fn restore_original_pages(&mut self) {
        if self.ribbon.restore_original() {
            self.selected_ribbon_page = self.default_selected_page();
            self.manual_cut_guides.clear();
            self.import_status = t!("launcher.new_project.source_images_restored").to_string();
            self.last_error = None;
        } else {
            self.last_error = Some(t!("launcher.new_project.source_images_not_loaded").to_string());
        }
    }

    fn apply_manual_cut_guides(&mut self) {
        if !self.has_manual_cut_preview() || self.stitch.is_loading() {
            return;
        }
        let images = self.current_stitch_images();
        if images.is_empty() {
            self.last_error = Some(t!("launcher.new_project.manual_cut_needs_page_error").to_string());
            return;
        }
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "stitch",
            stage: "split".to_string(),
            current: 0,
            total: self.manual_cut_guides.len().saturating_add(1).max(1),
        });
        self.import_status = t!("launcher.new_project.manual_cut_progress").to_string();
        self.stitch.begin(StitchRequest::ApplyManualCutsToPages {
            images,
            cut_guides: self.manual_cut_guides.clone(),
        });
    }

    fn start_cut_like_project_chapter(&mut self) {
        let Some(title) = self.current_cut_title_name().map(str::to_string) else {
            self.last_error = Some(t!("launcher.new_project.select_title_for_example_error").to_string());
            self.import_status = t!("launcher.new_project.cut_by_example_unavailable").to_string();
            return;
        };
        let Some(chapter) = self.current_cut_chapter_name().map(str::to_string) else {
            self.last_error = Some(t!("launcher.new_project.select_chapter_for_example_error").to_string());
            self.import_status = t!("launcher.new_project.cut_by_example_unavailable").to_string();
            return;
        };

        let src_dir = self
            .project_catalog
            .projects_root()
            .join(&title)
            .join(&chapter)
            .join(config::SRC_DIR);
        let reference_dir = if src_dir.exists() {
            src_dir
        } else {
            let legacy_dir = self
                .project_catalog
                .projects_root()
                .join(&title)
                .join(&chapter)
                .join("scr");
            if legacy_dir.exists() {
                legacy_dir
            } else {
                self.last_error = Some(t!("launcher.new_project.example_no_src_folder_error").to_string());
                self.import_status = t!("launcher.new_project.cut_by_example_unavailable").to_string();
                return;
            }
        };

        self.start_cut_like_reference(
            reference_dir,
            &tf!("launcher.new_project.cut_by_example_progress", title = title, chapter = chapter),
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_cut_like_folder(&mut self) {
        let Some(folder) = FileDialog::new().pick_folder() else {
            return;
        };
        self.start_cut_like_reference(
            folder.clone(),
            &tf!("launcher.new_project.cut_by_folder_progress", folder = folder.display()),
        );
    }

    /// Web stub: folder picking needs a native dialog (`rfd`) with no browser
    /// equivalent, so this reports the missing capability instead of opening one.
    #[cfg(target_arch = "wasm32")]
    fn start_cut_like_folder(&mut self) {
        self.last_error = Some(t!("launcher.new_project.choose_folder_web_unsupported").to_string());
        self.import_status = t!("launcher.new_project.cut_by_folder_web_unsupported").to_string();
    }

    fn start_cut_like_reference(&mut self, reference_dir: PathBuf, status_message: &str) {
        let images = self.current_stitch_images();
        if images.is_empty() {
            self.last_error = Some(t!("launcher.new_project.open_or_download_chapter_error").to_string());
            self.import_status = t!("launcher.new_project.cut_by_example_empty_ribbon_error").to_string();
            return;
        }

        self.manual_cut_guides.clear();
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "stitch",
            stage: "decode".to_string(),
            current: 0,
            total: 1,
        });
        self.import_status = status_message.to_string();
        crate::runtime_log::log_info(format!(
            "[new-project] starting reference-based cut using '{}'",
            reference_dir.display(),
        ));
        self.stitch.begin(StitchRequest::CutLikeReference {
            images,
            reference_dir,
        });
    }

    fn parse_stitch_options(&mut self, mode: StitchSplitMode) -> Option<StitchOptions> {
        let parts = if self.stitch_parts.trim().is_empty() {
            None
        } else {
            match self.stitch_parts.trim().parse::<usize>() {
                Ok(value) if value > 0 => Some(value),
                _ => {
                    self.last_error =
                        Some(t!("launcher.new_project.stitch_k_positive_error").to_string());
                    self.import_status = t!("launcher.new_project.stitch_invalid_params_error").to_string();
                    return None;
                }
            }
        };

        let parse_positive = |value: &str, name: &str| -> Result<usize, String> {
            match value.trim().parse::<usize>() {
                Ok(parsed) if parsed > 0 => Ok(parsed),
                _ => Err(tf!("launcher.new_project.stitch_field_positive_error", name = name)),
            }
        };

        let target_height = match parse_positive(&self.stitch_target_height, "Hmax") {
            Ok(value) => value,
            Err(message) => {
                self.last_error = Some(message);
                self.import_status = t!("launcher.new_project.stitch_invalid_params_error").to_string();
                return None;
            }
        };
        let band_rows = match parse_positive(&self.stitch_band_rows, "band_rows") {
            Ok(value) => value,
            Err(message) => {
                self.last_error = Some(message);
                self.import_status = t!("launcher.new_project.stitch_invalid_params_error").to_string();
                return None;
            }
        };
        let search_radius = match parse_positive(&self.stitch_search_radius, "search_radius") {
            Ok(value) => value,
            Err(message) => {
                self.last_error = Some(message);
                self.import_status = t!("launcher.new_project.stitch_invalid_params_error").to_string();
                return None;
            }
        };
        let tolerance = match self.stitch_tolerance.trim().parse::<u8>() {
            Ok(value) if value > 0 => value,
            _ => {
                self.last_error = Some(t!("launcher.new_project.stitch_tol_positive_error").to_string());
                self.import_status = t!("launcher.new_project.stitch_invalid_params_error").to_string();
                return None;
            }
        };

        Some(StitchOptions {
            parts,
            target_height,
            band_rows,
            tolerance,
            search_radius,
            prefer_up_first: self.stitch_prefer_up,
            mode,
        })
    }

    fn current_stitch_images(&self) -> Vec<StitchInputImage> {
        self.ribbon
            .pages()
            .iter()
            .map(|page| StitchInputImage {
                name: page.name.clone(),
                image: arc_image_clone(page.full_image()),
            })
            .collect()
    }

    fn current_waifu2x_images(&self) -> Vec<Waifu2xInputImage> {
        self.ribbon
            .pages()
            .iter()
            .map(|page| Waifu2xInputImage {
                name: page.name.clone(),
                image: arc_image_clone(page.full_image()),
            })
            .collect()
    }

    fn current_reline_images(&self) -> Vec<RelineInputImage> {
        self.ribbon
            .pages()
            .iter()
            .map(|page| RelineInputImage {
                name: page.name.clone(),
                image: arc_image_clone(page.full_image()),
            })
            .collect()
    }

    fn current_save_images(&self) -> Vec<ProjectSaveImage> {
        self.ribbon
            .pages()
            .iter()
            .map(|page| ProjectSaveImage {
                image: arc_image_clone(page.full_image()),
            })
            .collect()
    }

    fn refresh_project_catalog(&mut self) {
        self.project_catalog_error = None;
        self.project_catalog.refresh();
    }

    fn sync_save_title_from_input(&mut self) {
        if let Some(index) = self
            .project_catalog_snapshot
            .titles
            .iter()
            .position(|title| title == self.save_title_input.trim())
        {
            self.save_title = index;
        }
    }

    fn current_alt_title_name(&self) -> Option<&str> {
        self.project_catalog_snapshot
            .titles
            .get(self.alt_title)
            .map(String::as_str)
    }

    fn current_cut_title_name(&self) -> Option<&str> {
        self.project_catalog_snapshot
            .titles
            .get(self.cut_title)
            .map(String::as_str)
    }

    fn current_cut_chapter_options(&self) -> &[String] {
        self.current_cut_title_name()
            .map(|title| chapters_for_title(&self.project_catalog_snapshot, title))
            .unwrap_or(&[])
    }

    fn current_cut_chapter_name(&self) -> Option<&str> {
        self.current_cut_chapter_options()
            .get(self.cut_chapter)
            .map(String::as_str)
    }

    fn current_alt_chapter_options(&self) -> &[String] {
        self.current_alt_title_name()
            .map(|title| chapters_for_title(&self.project_catalog_snapshot, title))
            .unwrap_or(&[])
    }

    fn current_alt_chapter_name(&self) -> Option<&str> {
        self.current_alt_chapter_options()
            .get(self.alt_chapter)
            .map(String::as_str)
    }

    fn clamp_project_catalog_indexes(&mut self) {
        if self.save_title >= self.project_catalog_snapshot.titles.len() {
            self.save_title = 0;
        }
        if self.alt_title >= self.project_catalog_snapshot.titles.len() {
            self.alt_title = 0;
        }
        if self.cut_title >= self.project_catalog_snapshot.titles.len() {
            self.cut_title = 0;
        }
        let cut_chapters_len = self.current_cut_chapter_options().len();
        if self.cut_chapter >= cut_chapters_len {
            self.cut_chapter = 0;
        }
        let alt_chapters_len = self.current_alt_chapter_options().len();
        if self.alt_chapter >= alt_chapters_len {
            self.alt_chapter = 0;
        }
    }

    fn can_start_save(&self) -> bool {
        !self.source_import.is_loading()
            && !self.project_catalog.is_loading()
            && !self.advanced_download.is_loading()
            && !self.quick_download.is_loading()
            && !self.stitch.is_loading()
            && !self.save.is_loading()
            && !self.waifu2x.is_loading()
            && !self.reline.is_loading()
            && !self.ribbon.pages().is_empty()
    }

    fn start_save_to_project(&mut self, open_after_save: bool) -> bool {
        let title = self.save_title_input.trim().to_string();
        let chapter = self.save_chapter.trim().to_string();
        if title.is_empty() || chapter.is_empty() {
            self.last_error = Some(t!("launcher.new_project.specify_title_chapter_error").to_string());
            self.import_status = t!("launcher.new_project.save_to_project_unavailable").to_string();
            return false;
        }
        let target_dir = self
            .project_catalog
            .projects_root()
            .join(&title)
            .join(&chapter)
            .join(config::SRC_DIR);
        let should_continue = match confirm_overwrite_nonempty(&target_dir) {
            Ok(value) => value,
            Err(err) => {
                self.last_error =
                    Some(t!("launcher.new_project.check_project_folder_error").to_string());
                self.import_status = t!("launcher.new_project.save_to_project_failed").to_string();
                crate::runtime_log::log_error(format!(
                    "[new-project] failed to inspect project save dir '{}': {err}",
                    target_dir.display()
                ));
                return false;
            }
        };
        if !should_continue {
            return false;
        }
        self.begin_save(
            ProjectSaveRequest {
                target: ProjectSaveTarget::ProjectSource { title, chapter },
                images: self.current_save_images(),
            },
            open_after_save,
            t!("launcher.new_project.saving_to_project_progress"),
        );
        true
    }

    fn start_save_alt_version(&mut self) {
        let Some(title) = self.current_alt_title_name().map(str::to_string) else {
            self.last_error = Some(t!("launcher.new_project.select_title_for_alt_error").to_string());
            self.import_status = t!("launcher.new_project.save_alt_unavailable").to_string();
            return;
        };
        let Some(chapter) = self.current_alt_chapter_name().map(str::to_string) else {
            self.last_error = Some(t!("launcher.new_project.select_chapter_for_alt_error").to_string());
            self.import_status = t!("launcher.new_project.save_alt_unavailable").to_string();
            return;
        };
        let alt_name = self.alt_name.trim().to_string();
        if alt_name.is_empty() {
            self.last_error = Some(t!("launcher.new_project.specify_alt_name_error").to_string());
            self.import_status = t!("launcher.new_project.save_alt_unavailable").to_string();
            return;
        }
        let target_dir = self
            .project_catalog
            .projects_root()
            .join(&title)
            .join(&chapter)
            .join(config::ALT_VERS_DIR)
            .join(&alt_name);
        let should_continue = match confirm_overwrite_nonempty(&target_dir) {
            Ok(value) => value,
            Err(err) => {
                self.last_error =
                    Some(t!("launcher.new_project.check_alt_folder_error").to_string());
                self.import_status = t!("launcher.new_project.save_alt_failed").to_string();
                crate::runtime_log::log_error(format!(
                    "[new-project] failed to inspect alt save dir '{}': {err}",
                    target_dir.display()
                ));
                return;
            }
        };
        if !should_continue {
            return;
        }
        self.begin_save(
            ProjectSaveRequest {
                target: ProjectSaveTarget::AltVersion {
                    title,
                    chapter,
                    alt_name,
                },
                images: self.current_save_images(),
            },
            false,
            t!("launcher.new_project.saving_alt_progress"),
        );
    }

    /// Web stub: saving to an arbitrary folder requires a native folder dialog
    /// (`rfd`), unavailable in the browser. Reports the missing capability.
    #[cfg(target_arch = "wasm32")]
    fn start_save_to_folder(&mut self) {
        self.last_error = Some(t!("launcher.new_project.save_to_folder_web_unsupported").to_string());
        self.import_status = t!("launcher.new_project.save_to_folder_web_unsupported").to_string();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_save_to_folder(&mut self) {
        let mut dialog = FileDialog::new();
        // Reopen one level above the previous session pick: after saving a
        // chapter into `.../16`, the next dialog starts at the title folder
        // `.../` so sibling chapters are one click away.
        if let Some(parent) = self
            .last_independent_save_dir
            .as_deref()
            .and_then(std::path::Path::parent)
        {
            dialog = dialog.set_directory(parent);
        }
        let Some(folder) = dialog.pick_folder() else {
            return;
        };
        self.last_independent_save_dir = Some(folder.clone());
        let should_continue = match confirm_overwrite_nonempty(&folder) {
            Ok(value) => value,
            Err(err) => {
                self.last_error =
                    Some(t!("launcher.new_project.check_selected_folder_error").to_string());
                self.import_status = t!("launcher.new_project.save_to_folder_failed").to_string();
                crate::runtime_log::log_error(format!(
                    "[new-project] failed to inspect folder save dir '{}': {err}",
                    folder.display()
                ));
                return;
            }
        };
        if !should_continue {
            return;
        }
        self.begin_save(
            ProjectSaveRequest {
                target: ProjectSaveTarget::Folder {
                    folder: folder.clone(),
                },
                images: self.current_save_images(),
            },
            false,
            &tf!("launcher.new_project.saving_to_folder_progress", folder = folder.display()),
        );
    }

    fn begin_save(
        &mut self,
        request: ProjectSaveRequest,
        open_after_save: bool,
        status_message: &str,
    ) {
        if request.images.is_empty() {
            self.last_error = Some(t!("launcher.new_project.no_images_to_save_error").to_string());
            self.import_status = t!("launcher.new_project.save_empty_ribbon_error").to_string();
            return;
        }
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "save",
            stage: "prepare".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = status_message.to_string();
        self.pending_open_selection = None;
        self.open_after_save_requested = open_after_save;
        self.save.begin(request);
    }

    fn clamp_manual_cut_guides_to_current_pages(&mut self) {
        let pages = self.ribbon.pages();
        self.manual_cut_guides.retain(|guide| {
            pages
                .get(guide.page_index)
                .is_some_and(|page| manual_cut_y_is_valid(guide.y, page.original_size[1]))
        });
        self.manual_cut_guides
            .sort_unstable_by_key(|guide| (guide.page_index, guide.y));
        self.manual_cut_guides
            .dedup_by_key(|guide| (guide.page_index, guide.y));
    }

    fn manual_cut_drag_bounds(
        &self,
        guide_index: usize,
        page_index: usize,
        page_height: usize,
    ) -> (usize, usize) {
        let min_y = MANUAL_CUT_MIN_EDGE_DISTANCE_PX;
        let max_y = page_height.saturating_sub(MANUAL_CUT_MIN_EDGE_DISTANCE_PX);
        let previous_y = self
            .manual_cut_guides
            .iter()
            .enumerate()
            .filter(|(index, guide)| *index != guide_index && guide.page_index == page_index)
            .filter_map(|(_, guide)| {
                (guide.y < self.manual_cut_guides[guide_index].y).then_some(guide.y)
            })
            .max();
        let next_y = self
            .manual_cut_guides
            .iter()
            .enumerate()
            .filter(|(index, guide)| *index != guide_index && guide.page_index == page_index)
            .filter_map(|(_, guide)| {
                (guide.y > self.manual_cut_guides[guide_index].y).then_some(guide.y)
            })
            .min();
        let lower_bound = previous_y.map_or(min_y, |y| y.saturating_add(1).max(min_y));
        let upper_bound = next_y.map_or(max_y, |y| y.saturating_sub(1).min(max_y));
        (lower_bound, upper_bound.max(lower_bound))
    }

    fn draw_manual_cut_guides(
        &mut self,
        ui: &mut Ui,
        page_index: usize,
        image_rect: egui::Rect,
        original_size: [usize; 2],
        width_scale: f32,
    ) -> Vec<f32> {
        let painter = ui.painter();
        let image_height = original_size[1].max(1);
        let mut screen_positions = Vec::with_capacity(self.manual_cut_guides.len());
        let mut index = 0;
        while index < self.manual_cut_guides.len() {
            if self.manual_cut_guides[index].page_index != page_index {
                index += 1;
                continue;
            }
            let current = self.manual_cut_guides[index].y;
            let interaction_y = image_rect.top() + current as f32 * width_scale;
            let interaction_rect = egui::Rect::from_center_size(
                egui::pos2(image_rect.center().x, interaction_y),
                egui::vec2(MANUAL_CUT_HANDLE_WIDTH, MANUAL_CUT_HANDLE_HEIGHT),
            );
            let response = ui.interact(
                interaction_rect,
                ui.id().with(("manual_cut_handle", index)),
                egui::Sense::drag(),
            );
            if response.dragged() {
                let pointer_y = ui
                    .ctx()
                    .pointer_interact_pos()
                    .map(|position| position.y)
                    .unwrap_or(interaction_y);
                let image_y = ((pointer_y - image_rect.top()) / width_scale)
                    .round()
                    .clamp(
                        MANUAL_CUT_MIN_EDGE_DISTANCE_PX as f32,
                        image_height.saturating_sub(MANUAL_CUT_MIN_EDGE_DISTANCE_PX) as f32,
                    ) as usize;
                let (lower_bound, upper_bound) =
                    self.manual_cut_drag_bounds(index, page_index, image_height);
                self.manual_cut_guides[index].y = image_y.clamp(lower_bound, upper_bound);
            }

            let y = image_rect.top() + self.manual_cut_guides[index].y as f32 * width_scale;
            screen_positions.push(y);
            let line_start = egui::pos2(image_rect.left(), y);
            let line_end = egui::pos2(image_rect.right(), y);
            painter.line_segment(
                [line_start, line_end],
                Stroke::new(2.0, egui::Color32::from_rgb(255, 59, 48)),
            );

            let handle_rect = egui::Rect::from_center_size(
                egui::pos2(image_rect.center().x, y),
                egui::vec2(MANUAL_CUT_HANDLE_WIDTH, MANUAL_CUT_HANDLE_HEIGHT),
            );
            painter.rect_filled(handle_rect, 8.0, egui::Color32::from_rgb(190, 28, 28));
            painter.text(
                handle_rect.center(),
                egui::Align2::CENTER_CENTER,
                "^  v",
                egui::FontId::proportional(13.0),
                egui::Color32::WHITE,
            );

            let delete_center = egui::pos2(
                handle_rect.right() + MANUAL_CUT_DELETE_BUTTON_SIZE * 0.35,
                handle_rect.top() - MANUAL_CUT_DELETE_BUTTON_SIZE * 0.25,
            );
            let delete_rect = egui::Rect::from_center_size(
                delete_center,
                egui::vec2(MANUAL_CUT_DELETE_BUTTON_SIZE, MANUAL_CUT_DELETE_BUTTON_SIZE),
            );
            let delete_response = ui.interact(
                delete_rect,
                ui.id().with(("manual_cut_delete", index)),
                egui::Sense::click(),
            );
            painter.circle_filled(
                delete_rect.center(),
                MANUAL_CUT_DELETE_BUTTON_SIZE * 0.5,
                egui::Color32::from_rgb(220, 0, 0),
            );
            painter.circle_stroke(
                delete_rect.center(),
                MANUAL_CUT_DELETE_BUTTON_SIZE * 0.5,
                Stroke::new(1.0, egui::Color32::from_rgb(255, 120, 120)),
            );
            painter.text(
                delete_rect.center(),
                egui::Align2::CENTER_CENTER,
                "x",
                egui::FontId::proportional(15.0),
                egui::Color32::WHITE,
            );
            if delete_response.clicked() {
                self.manual_cut_guides.remove(index);
                screen_positions.pop();
                self.import_status = tf!("launcher.new_project.cut_line_removed_status", arg = self.manual_cut_guides.len());
                continue;
            }
            index += 1;
        }
        screen_positions
    }

    /// Draws cut and page-boundary arrows in the markable scrollbar gutter.
    ///
    /// `cut`/`page_boundary` positions are screen Y values collected while the
    /// ribbon content is rendered; they are converted back to content space and
    /// projected onto the bar via the widget gutter. Page boundaries (blue) are
    /// drawn under cut markers (red).
    fn draw_manual_cut_scroll_markers(
        &self,
        output: &MarkedScrollOutput<()>,
        painter: &egui::Painter,
        cut_screen_positions: &[f32],
        page_boundary_screen_positions: &[f32],
    ) {
        if output.content_size.y <= 1.0 {
            return;
        }
        // Screen Y collected during rendering -> content-space Y for the gutter.
        let to_content = |screen_y: f32| screen_y - output.inner_rect.top() + output.offset.y;

        let page_boundary_style = ArrowStyle {
            width: PAGE_BOUNDARY_SCROLL_ARROW_WIDTH,
            height: PAGE_BOUNDARY_SCROLL_ARROW_HEIGHT,
            fill: egui::Color32::from_rgb(40, 132, 255),
            stroke: Stroke::new(1.0, egui::Color32::from_rgb(130, 190, 255)),
            tail_length: 10.0 / 3.0,
            tail_width: 4.0 / 3.0,
        };
        let manual_cut_style = ArrowStyle {
            width: MANUAL_CUT_SCROLL_ARROW_WIDTH,
            height: MANUAL_CUT_SCROLL_ARROW_HEIGHT,
            fill: egui::Color32::from_rgb(255, 0, 0),
            stroke: Stroke::new(2.0, egui::Color32::from_rgb(255, 86, 86)),
            tail_length: 10.0,
            tail_width: 4.0,
        };

        let mut items: Vec<GutterItem> = Vec::new();
        items.extend(page_boundary_screen_positions.iter().map(|&screen_y| {
            arrow(
                ScrollSpan::pixel_at(to_content(screen_y)),
                page_boundary_style,
            )
            .layer(0)
        }));
        items.extend(cut_screen_positions.iter().map(|&screen_y| {
            arrow(ScrollSpan::pixel_at(to_content(screen_y)), manual_cut_style).layer(1)
        }));
        output.paint_gutter(painter, items);
    }

    fn open_folder_dialog(&mut self) {
        if self
            .source_import
            .begin_pick(OpenSourceKind::Folder, self.source_import_options())
        {
            self.manual_cut_guides.clear();
            self.last_error = None;
            self.active_progress = Some(OperationProgress {
                operation: "source",
                stage: "scan".to_string(),
                current: 0,
                total: 1,
            });
            self.import_status = t!("launcher.new_project.scanning_folder_status").to_string();
        }
    }

    fn open_file_dialog(&mut self) {
        if self
            .source_import
            .begin_pick(OpenSourceKind::File, self.source_import_options())
        {
            self.manual_cut_guides.clear();
            self.last_error = None;
            self.active_progress = Some(OperationProgress {
                operation: "source",
                stage: "scan".to_string(),
                current: 0,
                total: 1,
            });
            self.import_status = t!("launcher.new_project.opening_file_status").to_string();
        }
    }

    fn start_quick_download(&mut self) {
        let url = self.quick_link.trim();
        if url.is_empty() {
            self.last_error = Some(t!("launcher.new_project.paste_link_before_download_error").to_string());
            self.import_status = t!("launcher.new_project.quick_dl_waiting_link").to_string();
            return;
        }
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "quick_download",
            stage: "download".to_string(),
            current: 0,
            total: 1,
        });
        self.import_status = t!("launcher.new_project.quick_dl_preparing_status").to_string();
        self.quick_download.begin_download(url.to_string());
    }

    fn start_test_chapter_download(&mut self) {
        if self.test_chapter_check_rx.is_some() || self.quick_download.is_loading() {
            return;
        }
        let (tx, rx) = mpsc::channel::<TestChapterAvailabilityResult>();
        self.test_chapter_check_rx = Some(rx);
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "quick_download",
            stage: "connect".to_string(),
            current: 0,
            total: 1,
        });
        self.import_status = t!("launcher.new_project.checking_naver_status").to_string();
        let chapter_number = random_test_chapter_number();
        let chapter_url =
            format!("https://comic.naver.com/webtoon/detail?titleId=842647&no={chapter_number}");
        thread::spawn(move || {
            let result = check_test_chapter_site_availability(chapter_url);
            if tx.send(result).is_err() {
                crate::runtime_log::log_warn(
                    "[new-project] failed to send test chapter availability result to UI",
                );
            }
        });
    }

    fn start_waifu2x(&mut self) {
        let Some(options) = self.parse_waifu2x_options() else {
            return;
        };
        let images = self.current_waifu2x_images();
        if images.is_empty() {
            self.last_error = Some(t!("launcher.new_project.open_or_download_first_error").to_string());
            self.import_status = t!("launcher.new_project.waifu2x_empty_ribbon_error").to_string();
            return;
        }
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "waifu2x",
            stage: "prepare".to_string(),
            current: 0,
            total: images.len(),
        });
        self.import_status = t!("launcher.new_project.preparing_waifu2x_status").to_string();
        self.waifu2x.begin(images, options);
    }

    fn start_reline(&mut self) {
        let options = match self.reline_ui_mode {
            RelineUiMode::Simple => self.build_reline_simple_options(),
            RelineUiMode::Full => self.parse_reline_options(),
        };
        let Some(options) = options else {
            return;
        };
        let images = self.current_reline_images();
        if images.is_empty() {
            self.last_error = Some(t!("launcher.new_project.open_or_download_first_error").to_string());
            self.import_status = t!("launcher.new_project.reline_empty_ribbon_error").to_string();
            return;
        }
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "reline",
            stage: "prepare".to_string(),
            current: 0,
            total: images.len(),
        });
        self.import_status = t!("launcher.new_project.sending_to_reline_status").to_string();
        self.reline.begin(images, options);
    }

    fn start_image_processing(&mut self) {
        match self.image_processor {
            ImageProcessor::Waifu2x => self.start_waifu2x(),
            ImageProcessor::Reline => self.start_reline(),
        }
    }

    fn poll_folder_load(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.source_import.poll(ctx) {
            match event {
                SourceLoadEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "source",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                SourceLoadEvent::Loaded(result) => {
                    let page_count = result.pages.len();
                    self.apply_source_import_result(result.source_path.clone(), result.pages);
                    self.crop_editor = None;
                    self.manual_cut_guides.clear();
                    self.active_progress = None;
                    crate::runtime_log::log_info(format!(
                        "[new-project] imported {} ribbon images from '{}' (skipped={}, filtered_out={})",
                        page_count,
                        result.source_path.display(),
                        result.skipped_files,
                        result.filtered_out,
                    ));
                    self.import_status = if let Some((median, min_width, max_width)) =
                        result.filter_bounds
                    {
                        tf!("launcher.new_project.import_summary_with_median", result = result.imported_images, result_2 = result.source_path.display(), result_3 = result.skipped_files, result_4 = result.filtered_out, median = median, min_width = min_width, max_width = max_width)
                    } else {
                        tf!("launcher.new_project.import_summary", result = result.imported_images, result_2 = result.source_path.display(), result_3 = result.skipped_files, result_4 = result.filtered_out)
                    };
                    self.last_error = None;
                }
                SourceLoadEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    crate::runtime_log::log_error(format!(
                        "[new-project] source import failed: {}",
                        log_message
                    ));
                    self.ribbon.clear();
                    self.selected_ribbon_page = None;
                    self.crop_editor = None;
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.load_source_error").to_string();
                    self.last_error = Some(user_message);
                }
                SourceLoadEvent::WorkerDisconnected => {
                    crate::runtime_log::log_error(
                        "[new-project] source import worker disconnected unexpectedly",
                    );
                    self.ribbon.clear();
                    self.selected_ribbon_page = None;
                    self.crop_editor = None;
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.load_source_error").to_string();
                    self.last_error = Some(
                        t!("launcher.new_project.load_source_bg_error")
                            .to_string(),
                    );
                }
            }
        }
    }

    fn poll_project_catalog(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.project_catalog.poll(ctx) {
            match event {
                ProjectCatalogEvent::Loaded(snapshot) => {
                    self.project_catalog_error = None;
                    self.project_catalog_snapshot = snapshot;
                    self.clamp_project_catalog_indexes();
                    if self
                        .project_catalog_snapshot
                        .titles
                        .get(self.save_title)
                        .is_some_and(|_| {
                            let current = self.save_title_input.trim();
                            current.is_empty() || current == "Title A"
                        })
                    {
                        self.save_title_input =
                            self.project_catalog_snapshot.titles[self.save_title].clone();
                    }
                    self.sync_save_title_from_input();
                }
                ProjectCatalogEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    self.project_catalog_error = Some(user_message);
                    crate::runtime_log::log_error(format!(
                        "[new-project] project catalog refresh failed: {log_message}"
                    ));
                }
                ProjectCatalogEvent::WorkerDisconnected => {
                    self.project_catalog_error = Some(
                        t!("launcher.new_project.read_projects_bg_error").to_string(),
                    );
                    crate::runtime_log::log_error(
                        "[new-project] project catalog worker disconnected unexpectedly",
                    );
                }
            }
        }
    }

    fn poll_test_chapter_check(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.test_chapter_check_rx.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(TestChapterAvailabilityResult {
                available: false,
                chapter_url: String::new(),
                log_message: Some("test chapter availability worker disconnected".to_string()),
            }),
        };
        let Some(result) = result else {
            return;
        };
        self.test_chapter_check_rx = None;
        self.active_progress = None;

        if result.available {
            self.quick_link = result.chapter_url;
            self.start_quick_download();
            ctx.request_repaint();
            return;
        }

        if let Some(log_message) = result.log_message {
            crate::runtime_log::log_error(format!(
                "[new-project] comic.naver.com availability check failed: {log_message}"
            ));
        }
        self.import_status = t!("launcher.new_project.test_chapter_download_failed").to_string();
        self.last_error = Some(
            t!("launcher.new_project.naver_unavailable_vpn_hint")
                .to_string(),
        );
    }

    fn poll_quick_download(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.quick_download.poll(ctx) {
            match event {
                QuickDownloadEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "quick_download",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                QuickDownloadEvent::Loaded(result) => {
                    let page_count = result.pages.len();
                    self.ribbon
                        .replace_source(PathBuf::from(&result.source_url), result.pages);
                    self.selected_ribbon_page = self.default_selected_page();
                    self.crop_editor = None;
                    self.manual_cut_guides.clear();
                    self.simple_stitch_done = false;
                    self.simple_manual_cut_preview_active = false;
                    self.active_progress = None;
                    self.advance_simple_import_step_after_success();
                    crate::runtime_log::log_info(format!(
                        "[new-project] quick-downloaded {} ribbon images from '{}'",
                        page_count, result.source_url,
                    ));
                    self.import_status = tf!("launcher.new_project.quick_dl_done_status", result = result.downloaded_images, result_2 = result.source_url);
                    self.last_error = None;
                }
                QuickDownloadEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    crate::runtime_log::log_error(format!(
                        "[new-project] quick downloader failed: {log_message}",
                    ));
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.quick_dl_failed_error").to_string();
                    self.last_error = Some(user_message);
                }
                QuickDownloadEvent::WorkerDisconnected => {
                    crate::runtime_log::log_error(
                        "[new-project] quick download worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.quick_dl_failed_error").to_string();
                    self.last_error =
                        Some(t!("launcher.new_project.quick_dl_bg_error").to_string());
                }
            }
        }
    }

    fn poll_advanced_download(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.advanced_download.poll(ctx) {
            match event {
                AdvancedDownloadEvent::VersionMismatch {
                    studio_version,
                    downloader_version,
                } => {
                    self.open_advanced_downloader_version_warning(
                        &studio_version,
                        &downloader_version,
                    );
                }
                AdvancedDownloadEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "advanced_download",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                AdvancedDownloadEvent::BrowserOpened { current_url } => {
                    self.advanced_link_collect_active = false;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_intercept_active = false;
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status =
                        tf!("launcher.new_project.browser_opened_status", current_url = current_url);
                }
                AdvancedDownloadEvent::LinkCollectStarted { current_url } => {
                    self.advanced_link_collect_active = true;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_link_collect_last_poll_at =
                        Instant::now() - Duration::from_secs(1);
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = tf!("launcher.new_project.collect_started_status", current_url = current_url);
                }
                AdvancedDownloadEvent::LinkCollectCountUpdated { found_links } => {
                    self.advanced_link_collect_found_links = found_links;
                    self.advanced_link_collect_last_poll_at = Instant::now();
                }
                AdvancedDownloadEvent::InterceptStarted { current_url } => {
                    self.advanced_intercept_active = true;
                    self.advanced_intercept_counts = InterceptCounts::default();
                    self.advanced_intercept_last_poll_at = Instant::now() - Duration::from_secs(1);
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = if self.advanced_mode == AdvancedDownloadMode::DeepCapture
                    {
                        tf!("launcher.new_project.deep_intercept_started_status", current_url = current_url)
                    } else {
                        tf!("launcher.new_project.canvas_intercept_started_status", current_url = current_url)
                    };
                }
                AdvancedDownloadEvent::InterceptCountUpdated { counts } => {
                    self.advanced_intercept_counts = counts;
                    self.advanced_intercept_last_poll_at = Instant::now();
                }
                AdvancedDownloadEvent::Loaded(result) => {
                    self.advanced_link_collect_active = false;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_intercept_active = false;
                    self.advanced_intercept_counts = InterceptCounts::default();
                    let page_count = result.pages.len();
                    self.ribbon
                        .replace_source(PathBuf::from(&result.source_url), result.pages);
                    self.selected_ribbon_page = self.default_selected_page();
                    self.crop_editor = None;
                    self.manual_cut_guides.clear();
                    self.active_progress = None;
                    self.last_error = None;
                    self.simple_stitch_done = false;
                    self.simple_manual_cut_preview_active = false;
                    self.advance_simple_import_step_after_success();
                    self.import_status =
                        if self.advanced_mode == AdvancedDownloadMode::CanvasDownload {
                            tf!("launcher.new_project.canvas_mode_done_status", result = result.downloaded_images, result_2 = result.source_url)
                        } else {
                            tf!("launcher.new_project.advanced_dl_done_status", result = result.downloaded_images, result_2 = result.source_url)
                        };
                    crate::runtime_log::log_info(format!(
                        "[new-project] advanced downloader loaded {page_count} ribbon images from '{}'",
                        result.source_url,
                    ));
                }
                AdvancedDownloadEvent::AutoCandidatesReady(candidates) => {
                    self.advanced_link_collect_active = false;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_intercept_active = false;
                    self.advanced_intercept_counts = InterceptCounts::default();
                    let item_count = candidates.items.len();
                    let group_count = candidates.groups.len();
                    self.advanced_auto_review = Some(AdvancedAutoReviewState::new(candidates));
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = tf!("launcher.new_project.autofind_prepared_status", item_count = item_count, group_count = group_count);
                }
                AdvancedDownloadEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    self.advanced_link_collect_active = false;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_intercept_active = false;
                    self.advanced_intercept_counts = InterceptCounts::default();
                    crate::runtime_log::log_error(format!(
                        "[new-project] advanced downloader failed: {log_message}"
                    ));
                    self.active_progress = None;
                    self.import_status =
                        t!("launcher.new_project.advanced_dl_failed_error").to_string();
                    self.last_error = Some(user_message);
                }
                AdvancedDownloadEvent::WorkerDisconnected => {
                    self.advanced_link_collect_active = false;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_intercept_active = false;
                    self.advanced_intercept_counts = InterceptCounts::default();
                    crate::runtime_log::log_error(
                        "[new-project] advanced downloader worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status =
                        t!("launcher.new_project.advanced_dl_failed_error").to_string();
                    self.last_error = Some(
                        t!("launcher.new_project.advanced_dl_bg_error").to_string(),
                    );
                }
            }
        }

        self.poll_advanced_link_collect_status(ctx);
        self.poll_advanced_intercept_status(ctx);
    }

    fn poll_advanced_link_collect_status(&mut self, ctx: &egui::Context) {
        if !self.advanced_link_collect_active || self.advanced_download.has_pending_command() {
            return;
        }
        if self.advanced_link_collect_last_poll_at.elapsed() < Duration::from_millis(350) {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        let Some(browser) = self.selected_browser_name() else {
            return;
        };
        self.advanced_link_collect_last_poll_at = Instant::now();
        self.advanced_download
            .begin_query_link_collect_count(browser);
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn open_advanced_downloader_version_warning(
        &mut self,
        studio_version: &str,
        downloader_version: &str,
    ) {
        if self.advanced_downloader_version_warning_open
            || self.advanced_downloader_version_warning_dismissed
        {
            return;
        }
        self.advanced_downloader_version_warning_message =
            advanced_downloader_version_warning_message(studio_version, downloader_version);
        self.advanced_downloader_version_warning_open = true;
    }

    fn show_advanced_downloader_version_warning(&mut self, ctx: &egui::Context) {
        if !self.advanced_downloader_version_warning_open {
            return;
        }

        Window::new(t!("launcher.new_project.warning_dialog_title")).id(egui::Id::new("launcher.new_project.warning_dialog_title"))
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .show(ctx, |ui| {
                ui.label(&self.advanced_downloader_version_warning_message);
                ui.add_space(10.0);
                if ui.button("OK").clicked() {
                    self.advanced_downloader_version_warning_dismissed = true;
                    self.advanced_downloader_version_warning_open = false;
                }
            });
    }

    fn poll_advanced_intercept_status(&mut self, ctx: &egui::Context) {
        if !self.advanced_intercept_active || self.advanced_download.has_pending_command() {
            return;
        }
        if self.advanced_intercept_last_poll_at.elapsed() < Duration::from_millis(350) {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        let Some(browser) = self.selected_browser_name() else {
            return;
        };
        self.advanced_intercept_last_poll_at = Instant::now();
        if self.advanced_mode == AdvancedDownloadMode::DeepCapture {
            self.advanced_download
                .begin_query_deep_intercept_count(browser);
        } else {
            self.advanced_download
                .begin_query_canvas_intercept_count(browser);
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn poll_stitch(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.stitch.poll(ctx) {
            match event {
                StitchEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "stitch",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                StitchEvent::Completed(result) => {
                    let page_count = result.pages.len();
                    self.ribbon.replace_current(result.pages);
                    self.selected_ribbon_page = self.default_selected_page();
                    self.crop_editor = None;
                    self.manual_cut_guides = result.cut_guides;
                    self.clamp_manual_cut_guides_to_current_pages();
                    self.active_progress = None;
                    self.last_error = None;
                    self.simple_manual_cut_preview_active =
                        matches!(&result.kind, StitchSuccessKind::ManualPreview);
                    self.simple_stitch_done = matches!(
                        &result.kind,
                        StitchSuccessKind::AutoCut
                            | StitchSuccessKind::ReferenceCut
                            | StitchSuccessKind::ManualApply
                    );
                    self.import_status = match result.kind {
                        StitchSuccessKind::AutoCut => {
                            tf!("launcher.new_project.stitch_done_status", page_count = page_count)
                        }
                        StitchSuccessKind::StitchOnly => {
                            tf!("launcher.new_project.stitch_ribbon_done_status", page_count = page_count)
                        }
                        StitchSuccessKind::HeterogeneousBottoms => tf!("launcher.new_project.stitch_uneven_done_status", page_count = page_count),
                        StitchSuccessKind::ReferenceCut => tf!("launcher.new_project.cut_by_example_done_status", page_count = page_count),
                        StitchSuccessKind::ManualPreview => tf!("launcher.new_project.stitch_manual_lines_done_status", arg = self.manual_cut_guides.len()),
                        StitchSuccessKind::ManualApply => {
                            tf!("launcher.new_project.manual_cut_done_status", page_count = page_count)
                        }
                    };
                    crate::runtime_log::log_info(format!(
                        "[new-project] stitch worker completed: kind={}, pages={}, guides={}",
                        stitch_kind_name(&result.kind),
                        page_count,
                        self.manual_cut_guides.len(),
                    ));
                }
                StitchEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    crate::runtime_log::log_error(format!(
                        "[new-project] stitch worker failed: {log_message}"
                    ));
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.stitch_cut_failed_error").to_string();
                    self.last_error = Some(user_message);
                }
                StitchEvent::WorkerDisconnected => {
                    crate::runtime_log::log_error(
                        "[new-project] stitch worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.stitch_cut_failed_error").to_string();
                    self.last_error =
                        Some(t!("launcher.new_project.stitch_cut_bg_error").to_string());
                }
            }
        }
    }

    fn poll_save(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.save.poll(ctx) {
            match event {
                ProjectSaveEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "save",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                ProjectSaveEvent::Completed(result) => {
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = tf!("launcher.new_project.save_done_status", result = result.saved_images, result_2 = result.target_dir.display());
                    crate::runtime_log::log_info(format!(
                        "[new-project] save completed: files={}, dir='{}'",
                        result.saved_images,
                        result.target_dir.display()
                    ));
                    if self.open_after_save_requested {
                        if let Some(selection) = result.open_selection {
                            self.pending_open_selection = Some(selection);
                        } else {
                            self.pending_open_selection = None;
                        }
                    } else {
                        self.pending_open_selection = None;
                    }
                    if self.return_to_import_after_save_requested {
                        self.simple_mode_step = SimpleModeStep::ImportDownload;
                        self.simple_import_show_advanced = false;
                        self.simple_stitch_done = false;
                        self.simple_manual_cut_preview_active = false;
                    }
                    self.open_after_save_requested = false;
                    self.return_to_import_after_save_requested = false;
                    self.refresh_project_catalog();
                }
                ProjectSaveEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    self.open_after_save_requested = false;
                    self.return_to_import_after_save_requested = false;
                    self.pending_open_selection = None;
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.save_failed_error").to_string();
                    self.last_error = Some(user_message);
                    crate::runtime_log::log_error(format!(
                        "[new-project] save failed: {log_message}"
                    ));
                }
                ProjectSaveEvent::WorkerDisconnected => {
                    self.open_after_save_requested = false;
                    self.return_to_import_after_save_requested = false;
                    self.pending_open_selection = None;
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.save_failed_error").to_string();
                    self.last_error =
                        Some(t!("launcher.new_project.save_bg_error").to_string());
                    crate::runtime_log::log_error(
                        "[new-project] save worker disconnected unexpectedly",
                    );
                }
            }
        }
    }

    fn poll_waifu2x(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.waifu2x.poll(ctx) {
            match event {
                Waifu2xEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "waifu2x",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                Waifu2xEvent::Completed(result) => {
                    let page_count = result.pages.len();
                    self.ribbon.replace_current(result.pages);
                    self.selected_ribbon_page = self.default_selected_page();
                    self.crop_editor = None;
                    self.manual_cut_guides.clear();
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = tf!("launcher.new_project.waifu2x_done_status", result = result.processed_images, result_2 = result.backend_path.display());
                    crate::runtime_log::log_info(format!(
                        "[new-project] waifu2x completed: pages={}, backend='{}'",
                        page_count,
                        result.backend_path.display()
                    ));
                }
                Waifu2xEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    crate::runtime_log::log_error(format!(
                        "[new-project] waifu2x failed: {log_message}"
                    ));
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.waifu2x.failed_error").to_string();
                    self.last_error = Some(user_message);
                }
                Waifu2xEvent::WorkerDisconnected => {
                    crate::runtime_log::log_error(
                        "[new-project] waifu2x worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.waifu2x.failed_error").to_string();
                    self.last_error =
                        Some(t!("launcher.new_project.waifu2x_bg_error").to_string());
                }
            }
        }
    }

    fn poll_reline(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.reline.poll(ctx) {
            match event {
                RelineEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "reline",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                RelineEvent::Completed(result) => {
                    let page_count = result.pages.len();
                    self.ribbon.replace_current(result.pages);
                    self.selected_ribbon_page = self.default_selected_page();
                    self.crop_editor = None;
                    self.manual_cut_guides.clear();
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = tf!("launcher.new_project.reline_done_status", result = result.processed_images, result_2 = result.backend_endpoint);
                    crate::runtime_log::log_info(format!(
                        "[new-project] Reline completed: pages={}, endpoint='{}'",
                        page_count, result.backend_endpoint
                    ));
                }
                RelineEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    crate::runtime_log::log_error(format!(
                        "[new-project] Reline failed: {log_message}"
                    ));
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.reline_failed_error").to_string();
                    self.last_error = Some(user_message);
                }
                RelineEvent::WorkerDisconnected => {
                    crate::runtime_log::log_error(
                        "[new-project] Reline worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.reline_failed_error").to_string();
                    self.last_error =
                        Some(t!("launcher.new_project.reline_bg_error").to_string());
                }
            }
        }
    }

    fn poll_reline_model_catalog(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.reline_model_catalog.poll(ctx) {
            match event {
                RelineModelCatalogEvent::Loaded(models) => {
                    self.reline_model_catalog_error = None;
                    self.reline_model_catalog_entries = models;
                    if self.reline_model_name.trim().is_empty()
                        && let Some(first_model) = self.reline_model_catalog_entries.first()
                    {
                        self.reline_model_name = first_model.name.clone();
                    }
                    crate::runtime_log::log_info(format!(
                        "[new-project] loaded Reline model catalog: models={}",
                        self.reline_model_catalog_entries.len()
                    ));
                }
                RelineModelCatalogEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    self.reline_model_catalog_error = Some(user_message);
                    crate::runtime_log::log_error(format!(
                        "[new-project] Reline model catalog failed: {log_message}"
                    ));
                }
                RelineModelCatalogEvent::WorkerDisconnected => {
                    self.reline_model_catalog_error = Some(
                        t!("launcher.new_project.reline_models_bg_error").to_string(),
                    );
                    crate::runtime_log::log_error(
                        "[new-project] Reline model catalog worker disconnected unexpectedly",
                    );
                }
            }
        }
    }

    fn source_import_options(&self) -> SourceImportOptions {
        SourceImportOptions {
            filter_same_width: self.filter_same_width,
            extra_name_patterns: self.import_extra_names.clone(),
        }
    }

    fn parse_waifu2x_options(&mut self) -> Option<Waifu2xOptions> {
        let noise_levels = [-1, 0, 1, 2, 3];
        let scale_levels = [1, 2, 4, 8, 16, 32];
        let Some(&noise) = noise_levels.get(self.waifu_noise) else {
            self.last_error = Some(t!("launcher.new_project.waifu2x_invalid_noise_error").to_string());
            self.import_status = t!("launcher.new_project.waifu2x_invalid_params_error").to_string();
            return None;
        };
        let Some(&scale) = scale_levels.get(self.waifu_scale) else {
            self.last_error = Some(t!("launcher.new_project.waifu2x_invalid_scale_error").to_string());
            self.import_status = t!("launcher.new_project.waifu2x_invalid_params_error").to_string();
            return None;
        };
        let tile_size = match self.waifu_tile_size.trim().parse::<u32>() {
            Ok(value) if value == 0 || value >= 32 => value,
            _ => {
                self.last_error =
                    Some(t!("launcher.new_project.waifu2x_tile_size_error").to_string());
                self.import_status = t!("launcher.new_project.waifu2x_invalid_params_error").to_string();
                return None;
            }
        };
        Some(Waifu2xOptions {
            noise,
            scale,
            tile_size,
        })
    }

    fn parse_reline_options(&mut self) -> Option<RelineOptions> {
        const READER_MODES: [&str; 3] = ["rgb", "gray", "dynamic"];
        const TILERS: [&str; 3] = ["exact", "max", "no_tiling"];
        const DTYPES: [&str; 3] = ["F32", "F16", "BF16"];
        const CANNY_TYPES: [&str; 3] = ["invert", "normal", "unsharp"];
        const DOT_TYPES: [&str; 5] = ["line", "cross", "ellipse", "invline", "circle"];
        const HALFTONE_MODES: [&str; 4] = ["gray", "rgb", "hsv", "cmyk"];
        const HALFTONE_FILTERS: [&str; 22] = [
            "nearest",
            "box",
            "sbox4",
            "sbox8",
            "linear",
            "slinear4",
            "slinear8",
            "hamming",
            "shamming4",
            "shamming8",
            "catmullrom",
            "scatmullrom4",
            "scatmullrom8",
            "mitchell",
            "smitchell4",
            "smitchell8",
            "lanczos",
            "slanczos4",
            "slanczos8",
            "gauss",
            "sgauss4",
            "sgauss8",
        ];
        const RESIZE_FILTERS: [&str; 33] = [
            "nearest",
            "box",
            "sbox4",
            "sbox8",
            "ibox",
            "linear",
            "slinear4",
            "slinear8",
            "ilinear",
            "hamming",
            "shamming4",
            "shamming8",
            "ihamming",
            "catmullrom",
            "scatmullrom4",
            "scatmullrom8",
            "icatmullrom",
            "mitchell",
            "smitchell4",
            "smitchell8",
            "imitchell",
            "lanczos",
            "slanczos4",
            "slanczos8",
            "ilanczos",
            "gauss",
            "sgauss4",
            "sgauss8",
            "igauss",
            "dpid_0.25",
            "dpid_0.5",
            "dpid_0.75",
            "dpid_1",
        ];
        const CVT_TYPES: [&str; 4] = ["RGB2Gray2020", "RGB2Gray709", "RGB2Gray", "Gray2RGB"];

        let reader_mode = selected_label(&READER_MODES, self.reline_reader_mode, t!("launcher.new_project.reline_field_read_mode"))?;
        let tiler = selected_label(&TILERS, self.reline_tiler, t!("launcher.new_project.reline_field_tiling"))?;
        let dtype = selected_label(&DTYPES, self.reline_dtype, t!("launcher.new_project.reline_field_compute_type"))?;
        let canny_type = selected_label(&CANNY_TYPES, self.reline_sharp_canny_type, t!("launcher.new_project.reline_field_canny_mode"))?;
        let dot_type = selected_label(&DOT_TYPES, self.reline_halftone_dot_type, t!("launcher.new_project.reline_field_dot_type"))?;
        let halftone_mode = selected_label(
            &HALFTONE_MODES,
            self.reline_halftone_mode,
            t!("launcher.new_project.reline_field_halftone_color_mode"),
        )?;
        let halftone_filter = selected_label(
            &HALFTONE_FILTERS,
            self.reline_halftone_ssaa_filter,
            t!("launcher.new_project.reline_field_ssaa_filter"),
        )?;
        let resize_filter = selected_label(
            &RESIZE_FILTERS,
            self.reline_resize_filter,
            t!("launcher.new_project.reline_field_resize_filter"),
        )?;
        let cvt_type = selected_label(
            &CVT_TYPES,
            self.reline_cvt_color_type,
            t!("launcher.new_project.reline_field_colorconv_type"),
        )?;

        let exact_tiler_size = self.parse_required_u32_field(
            &self.reline_exact_tiler_size.clone(),
            t!("launcher.new_project.reline_field_exact_tile"),
        )?;
        let target_scale =
            self.parse_optional_u32_field(&self.reline_target_scale.clone(), t!("launcher.new_project.reline_field_target_scale"))?;
        let sharp_low_input = self.parse_required_i32_field(
            &self.reline_sharp_low_input.clone(),
            t!("launcher.new_project.reline_field_sharpen_input_low"),
        )?;
        let sharp_high_input = self.parse_required_i32_field(
            &self.reline_sharp_high_input.clone(),
            t!("launcher.new_project.reline_field_sharpen_input_high"),
        )?;
        let sharp_gamma =
            self.parse_required_f32_field(&self.reline_sharp_gamma.clone(), t!("launcher.new_project.reline_field_sharpen_gamma"))?;
        let sharp_diapason_white = self.parse_required_i32_field(
            &self.reline_sharp_diapason_white.clone(),
            t!("launcher.new_project.reline_field_sharpen_white"),
        )?;
        let sharp_diapason_black = self.parse_required_i32_field(
            &self.reline_sharp_diapason_black.clone(),
            t!("launcher.new_project.reline_field_sharpen_black"),
        )?;
        let halftone_dot_size = self.parse_required_i32_field(
            &self.reline_halftone_dot_size.clone(),
            t!("launcher.new_project.reline_field_dot_size"),
        )?;
        let halftone_angle =
            self.parse_required_i32_field(&self.reline_halftone_angle.clone(), t!("launcher.new_project.reline_field_angle"))?;
        let halftone_ssaa_scale = self
            .parse_optional_f32_field(&self.reline_halftone_ssaa_scale.clone(), t!("launcher.new_project.reline_field_ssaa_scale"))?;
        let resize_height =
            self.parse_optional_u32_field(&self.reline_resize_height.clone(), t!("launcher.new_project.reline_height"))?;
        let resize_width =
            self.parse_optional_u32_field(&self.reline_resize_width.clone(), t!("launcher.new_project.reline_width"))?;
        let resize_percent =
            self.parse_optional_f32_field(&self.reline_resize_percent.clone(), t!("launcher.new_project.reline_percent"))?;
        if self.reline_resize_enabled
            && resize_height.is_none()
            && resize_width.is_none()
            && resize_percent.is_none()
        {
            self.last_error =
                Some(t!("launcher.new_project.reline_resize_requires_error").to_string());
            self.import_status = t!("launcher.new_project.reline_invalid_params_error").to_string();
            return None;
        }
        let resize_spread_size = self
            .parse_required_u32_field(&self.reline_resize_spread_size.clone(), t!("launcher.new_project.reline_spread_size"))?;
        let level_low_input = self.parse_required_i32_field(
            &self.reline_level_low_input.clone(),
            t!("launcher.new_project.reline_input_low_level"),
        )?;
        let level_high_input = self.parse_required_i32_field(
            &self.reline_level_high_input.clone(),
            t!("launcher.new_project.reline_input_high_level"),
        )?;
        let level_low_output = self.parse_required_i32_field(
            &self.reline_level_low_output.clone(),
            t!("launcher.new_project.reline_output_low_level"),
        )?;
        let level_high_output = self.parse_required_i32_field(
            &self.reline_level_high_output.clone(),
            t!("launcher.new_project.reline_output_high_level"),
        )?;
        let level_gamma =
            self.parse_required_f32_field(&self.reline_level_gamma.clone(), t!("launcher.new_project.reline_gamma"))?;

        Some(RelineOptions {
            reader_mode: reader_mode.to_string(),
            upscale: RelineUpscaleOptions {
                enabled: self.reline_upscale_enabled,
                model_name: self.reline_model_name.trim().to_string(),
                model_path: self.reline_model_path.trim().to_string(),
                model_url: self.reline_model_url.trim().to_string(),
                tiler: tiler.to_string(),
                target_scale,
                dtype: dtype.to_string(),
                exact_tiler_size,
                allow_cpu_upscale: self.reline_allow_cpu_upscale,
            },
            sharp: RelineSharpOptions {
                enabled: self.reline_sharp_enabled,
                low_input: sharp_low_input,
                high_input: sharp_high_input,
                gamma: sharp_gamma,
                diapason_white: sharp_diapason_white,
                diapason_black: sharp_diapason_black,
                canny: self.reline_sharp_canny,
                canny_type: canny_type.to_string(),
            },
            halftone: RelineHalftoneOptions {
                enabled: self.reline_halftone_enabled,
                dot_size: halftone_dot_size,
                angle: halftone_angle,
                dot_type: dot_type.to_string(),
                halftone_mode: halftone_mode.to_string(),
                ssaa_scale: halftone_ssaa_scale,
                ssaa_filter: halftone_filter.to_string(),
                disable_auto_dot: self.reline_halftone_disable_auto_dot,
            },
            resize: RelineResizeOptions {
                enabled: self.reline_resize_enabled,
                height: resize_height,
                width: resize_width,
                percent: resize_percent,
                filter: resize_filter.to_string(),
                gamma_correction: self.reline_resize_gamma_correction,
                spread: self.reline_resize_spread,
                spread_size: resize_spread_size,
            },
            level: RelineLevelOptions {
                enabled: self.reline_level_enabled,
                low_input: level_low_input,
                high_input: level_high_input,
                low_output: level_low_output,
                high_output: level_high_output,
                gamma: level_gamma,
            },
            cvt_color: RelineCvtColorOptions {
                enabled: self.reline_cvt_color_enabled,
                cvt_type: cvt_type.to_string(),
            },
        })
    }

    /// Build `RelineOptions` from the simplified UI state (preset + high-level controls).
    ///
    /// Maps the guided controls onto safe pipeline defaults: upscale always runs with the
    /// selected model; the preset decides whether sharpening and halftone nodes are added; the
    /// sharpness control governs Canny-based edge enhancement; level/cvt_color stay disabled.
    /// Returns `None` (and sets `last_error`) only when the optional resize field is invalid.
    fn build_reline_simple_options(&mut self) -> Option<RelineOptions> {
        let preset = RelineSimplePreset::from_index(self.reline_simple_preset);

        // Target scale: 0 = model's native scale, 1 = ×2, 2 = ×4.
        let target_scale = match self.reline_simple_scale {
            1 => Some(2),
            2 => Some(4),
            _ => None,
        };

        // Sharpness strength: 0 = none, 1 = light (normal edges), 2 = strong (unsharp edges).
        let (sharp_node_enabled, canny, canny_type) = match self.reline_simple_sharp {
            1 => (true, true, "normal"),
            2 => (true, true, "unsharp"),
            _ => (false, false, "normal"),
        };

        // The preset decides which optional nodes participate; ModelOnly stays a clean pass.
        let (sharp_enabled, halftone_enabled) = match preset {
            RelineSimplePreset::ModelOnly => (false, false),
            RelineSimplePreset::RestoreLightSharp | RelineSimplePreset::Descreen => {
                (sharp_node_enabled, false)
            }
            RelineSimplePreset::AddHalftone => (sharp_node_enabled, true),
        };

        let resize_enabled = self.reline_simple_resize_enabled;
        let resize_height = if resize_enabled {
            Some(self.parse_required_u32_field(
                &self.reline_simple_resize_target.clone(),
                t!("launcher.new_project.reline_result_height"),
            )?)
        } else {
            None
        };

        Some(RelineOptions {
            reader_mode: "rgb".to_string(),
            upscale: RelineUpscaleOptions {
                enabled: true,
                model_name: self.reline_model_name.trim().to_string(),
                model_path: String::new(),
                model_url: String::new(),
                tiler: "exact".to_string(),
                target_scale,
                dtype: "F32".to_string(),
                exact_tiler_size: 800,
                allow_cpu_upscale: true,
            },
            sharp: RelineSharpOptions {
                enabled: sharp_enabled,
                low_input: 0,
                high_input: 255,
                gamma: 1.0,
                diapason_white: -1,
                diapason_black: -1,
                canny,
                canny_type: canny_type.to_string(),
            },
            halftone: RelineHalftoneOptions {
                enabled: halftone_enabled,
                dot_size: 7,
                angle: 0,
                dot_type: "circle".to_string(),
                halftone_mode: "gray".to_string(),
                ssaa_scale: None,
                ssaa_filter: "shamming4".to_string(),
                disable_auto_dot: false,
            },
            resize: RelineResizeOptions {
                enabled: resize_enabled,
                height: resize_height,
                width: None,
                percent: None,
                filter: "catmullrom".to_string(),
                gamma_correction: false,
                spread: false,
                spread_size: 2800,
            },
            level: RelineLevelOptions {
                enabled: false,
                low_input: 0,
                high_input: 255,
                low_output: 0,
                high_output: 255,
                gamma: 1.0,
            },
            cvt_color: RelineCvtColorOptions {
                enabled: false,
                cvt_type: "RGB2Gray2020".to_string(),
            },
        })
    }

    fn can_start_waifu2x(&self) -> bool {
        !self.source_import.is_loading()
            && !self.advanced_download.is_loading()
            && !self.quick_download.is_loading()
            && !self.stitch.is_loading()
            && !self.save.is_loading()
            && !self.waifu2x.is_loading()
            && !self.reline.is_loading()
            && !self.ribbon.pages().is_empty()
    }

    fn can_start_reline(&self) -> bool {
        // The simplified UI drives the model only through the gallery, so a model must be picked.
        let model_ready = match self.reline_ui_mode {
            RelineUiMode::Simple => !self.reline_model_name.trim().is_empty(),
            RelineUiMode::Full => true,
        };
        model_ready
            && !self.source_import.is_loading()
            && !self.advanced_download.is_loading()
            && !self.quick_download.is_loading()
            && !self.stitch.is_loading()
            && !self.save.is_loading()
            && !self.waifu2x.is_loading()
            && !self.reline.is_loading()
            && !self.ribbon.pages().is_empty()
    }

    fn can_start_image_processing(&self) -> bool {
        match self.image_processor {
            ImageProcessor::Waifu2x => self.can_start_waifu2x(),
            ImageProcessor::Reline => self.can_start_reline(),
        }
    }

    fn current_image_processing_operation(&self) -> &'static str {
        match self.image_processor {
            ImageProcessor::Waifu2x => "waifu2x",
            ImageProcessor::Reline => "reline",
        }
    }

    fn parse_required_u32_field(&mut self, raw: &str, field_name: &str) -> Option<u32> {
        match raw.trim().parse::<u32>() {
            Ok(value) if value > 0 => Some(value),
            _ => {
                self.last_error = Some(tf!("launcher.new_project.reline_field_positive_error", field_name = field_name));
                self.import_status = t!("launcher.new_project.reline_invalid_params_error").to_string();
                None
            }
        }
    }

    fn parse_optional_u32_field(&mut self, raw: &str, field_name: &str) -> Option<Option<u32>> {
        if raw.trim().is_empty() {
            return Some(None);
        }
        match raw.trim().parse::<u32>() {
            Ok(value) if value > 0 => Some(Some(value)),
            _ => {
                self.last_error = Some(tf!("launcher.new_project.reline_field_positive_error", field_name = field_name));
                self.import_status = t!("launcher.new_project.reline_invalid_params_error").to_string();
                None
            }
        }
    }

    fn parse_required_i32_field(&mut self, raw: &str, field_name: &str) -> Option<i32> {
        match raw.trim().parse::<i32>() {
            Ok(value) => Some(value),
            _ => {
                self.last_error = Some(tf!("launcher.new_project.reline_field_integer_error", field_name = field_name));
                self.import_status = t!("launcher.new_project.reline_invalid_params_error").to_string();
                None
            }
        }
    }

    fn parse_required_f32_field(&mut self, raw: &str, field_name: &str) -> Option<f32> {
        match raw.trim().parse::<f32>() {
            Ok(value) => Some(value),
            _ => {
                self.last_error = Some(tf!("launcher.new_project.reline_field_number_error", field_name = field_name));
                self.import_status = t!("launcher.new_project.reline_invalid_params_error").to_string();
                None
            }
        }
    }

    fn parse_optional_f32_field(&mut self, raw: &str, field_name: &str) -> Option<Option<f32>> {
        if raw.trim().is_empty() {
            return Some(None);
        }
        match raw.trim().parse::<f32>() {
            Ok(value) if value > 0.0 => Some(Some(value)),
            _ => {
                self.last_error = Some(tf!("launcher.new_project.reline_field_positive_error", field_name = field_name));
                self.import_status = t!("launcher.new_project.reline_invalid_params_error").to_string();
                None
            }
        }
    }

    fn waifu_backend_path_display(&self) -> String {
        self.waifu2x.backend_path().display().to_string()
    }

    fn handle_window_closed(&mut self) {
        self.waifu2x.shutdown();
        self.waifu2x = Waifu2xController::new();
        self.reline = RelineController::new();
        self.reline_model_catalog = RelineModelCatalogController::new();
    }

    fn clamp_advanced_indexes(&mut self) {
        if self.selected_browser >= self.browser_names.len() {
            self.selected_browser = 0;
        }
        if self.selected_site >= self.site_presets.len() {
            self.selected_site = 0;
        }
    }

    fn advanced_browser_available(&self) -> bool {
        match self.selected_advanced_backend {
            AdvancedBrowserBackend::Selenium => !self.browser_names.is_empty(),
            AdvancedBrowserBackend::Cloak => true,
        }
    }

    fn selected_browser_name(&self) -> Option<String> {
        match self.selected_advanced_backend {
            AdvancedBrowserBackend::Selenium => {
                self.browser_names.get(self.selected_browser).cloned()
            }
            AdvancedBrowserBackend::Cloak => Some(
                self.advanced_download
                    .browser_name_for_backend("CloakBrowser"),
            ),
        }
    }

    fn start_advanced_open(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        if self.advanced_page_url.trim().is_empty() {
            self.last_error = Some(t!("launcher.new_project.enter_page_link_error").to_string());
            self.import_status = t!("launcher.new_project.advanced_dl_waiting_link").to_string();
            return;
        }
        self.last_error = None;
        self.advanced_link_collect_found_links = 0;
        self.advanced_intercept_counts = InterceptCounts::default();
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "browser".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = t!("launcher.new_project.opening_page_selenium_status").to_string();
        self.advanced_download
            .begin_open(browser, self.advanced_page_url.clone());
    }

    fn start_advanced_fetch(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.advanced_link_collect_found_links = 0;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "collect".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = t!("launcher.new_project.collecting_links_status").to_string();
        if self.advanced_link_source_mode == AdvancedLinkSourceMode::AutoReview {
            self.advanced_download
                .begin_fetch_auto(browser, self.advanced_fetch_parallelism);
        } else {
            self.advanced_download.begin_fetch(
                browser,
                self.image_prefix.trim().to_string(),
                self.advanced_fetch_parallelism,
            );
        }
    }

    fn stop_advanced_auto_fetch(&mut self) {
        match self.advanced_download.request_cancel_current_auto_fetch() {
            Ok(()) => {
                self.import_status =
                    t!("launcher.new_project.stopping_autofind_status").to_string();
            }
            Err(err) => {
                self.last_error = Some(t!("launcher.new_project.stop_autofind_error").to_string());
                crate::runtime_log::log_warn(format!(
                    "[new-project] failed to request advanced auto fetch cancellation: {err}"
                ));
            }
        }
    }

    fn start_advanced_link_collect(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.advanced_link_collect_found_links = 0;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "browser".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = t!("launcher.new_project.starting_bg_collect_status").to_string();
        if self.advanced_link_source_mode == AdvancedLinkSourceMode::AutoReview {
            self.advanced_download
                .begin_start_auto_link_collect(browser, self.advanced_fetch_parallelism);
        } else {
            self.advanced_download.begin_start_link_collect(
                browser,
                self.image_prefix.trim().to_string(),
                self.advanced_fetch_parallelism,
            );
        }
    }

    fn finish_advanced_link_collect(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.advanced_intercept_counts = InterceptCounts::default();
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "download".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status =
            t!("launcher.new_project.stopping_collect_status").to_string();
        if self.advanced_link_source_mode == AdvancedLinkSourceMode::AutoReview {
            self.advanced_download.begin_stop_auto_link_collect(browser);
        } else {
            self.advanced_download.begin_stop_link_collect(browser);
        }
    }

    fn start_advanced_canvas_fetch(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.advanced_intercept_counts = InterceptCounts::default();
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "collect_canvas".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = t!("launcher.new_project.collecting_canvas_status").to_string();
        self.advanced_download.begin_fetch_canvas(browser);
    }

    fn start_advanced_canvas_intercept(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "browser".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = t!("launcher.new_project.starting_canvas_intercept_status").to_string();
        self.advanced_download.begin_start_canvas_intercept(browser);
    }

    fn finish_advanced_canvas_intercept(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "save_canvas".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status =
            t!("launcher.new_project.stopping_canvas_intercept_status").to_string();
        self.advanced_download.begin_stop_canvas_intercept(browser);
    }

    /// Forces the deep-capture preconditions for the simple-mode
    /// "Автоматический перехват картинок" section, whose buttons have no
    /// backend/mode selectors: Cloak backend on both the UI state and the
    /// controller, and `DeepCapture` mode so the count poller queries the
    /// deep-intercept counters. `set_backend` is a no-op when already Cloak.
    fn prepare_simple_deep_capture(&mut self) {
        self.selected_advanced_backend = AdvancedBrowserBackend::Cloak;
        self.advanced_mode = AdvancedDownloadMode::DeepCapture;
        self.advanced_download
            .set_backend(AdvancedBrowserBackend::Cloak);
    }

    fn start_advanced_deep_intercept(&mut self) {
        if self.selected_advanced_backend != AdvancedBrowserBackend::Cloak {
            self.last_error =
                Some(t!("launcher.new_project.deep_intercept_cloak_only_error").to_string());
            return;
        }
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.cloak_unavailable_error").to_string());
            self.import_status = t!("launcher.new_project.deep_intercept_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "browser".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status =
            t!("launcher.new_project.starting_deep_intercept_status").to_string();
        self.advanced_download.begin_start_deep_intercept(browser);
    }

    fn finish_advanced_deep_intercept(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.cloak_unavailable_error").to_string());
            self.import_status = t!("launcher.new_project.deep_intercept_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "download".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status =
            t!("launcher.new_project.stopping_deep_intercept_status").to_string();
        self.advanced_download.begin_stop_deep_intercept(browser);
    }

    fn save_advanced_prefix(&mut self) {
        let prefix = self.image_prefix.trim().to_string();
        let new_name = self.site_name.trim().to_string();
        let selected_name = self
            .site_presets
            .get(self.selected_site)
            .map(|(name, _)| name.clone())
            .unwrap_or_default();
        if prefix.is_empty() {
            self.last_error = Some(t!("launcher.new_project.enter_url_prefix_error").to_string());
            self.import_status = t!("launcher.new_project.prefix_not_specified_error").to_string();
            return;
        }
        let target_name = if new_name.is_empty() {
            selected_name
        } else {
            new_name
        };
        if target_name.trim().is_empty() {
            self.last_error =
                Some(t!("launcher.new_project.enter_site_name_error").to_string());
            self.import_status = t!("launcher.new_project.save_prefix_failed").to_string();
            return;
        }

        match save_image_url_preset(&target_name, &prefix) {
            Ok(()) => {
                self.site_presets = load_image_url_presets();
                self.selected_site = self
                    .site_presets
                    .iter()
                    .position(|(name, _)| *name == target_name)
                    .unwrap_or(0);
                self.site_name.clear();
                self.last_error = None;
                self.import_status =
                    tf!("launcher.new_project.prefix_saved_status", target_name = target_name.trim());
            }
            Err(err) => {
                crate::runtime_log::log_error(format!(
                    "[new-project] failed to save image url preset '{target_name}': {err}"
                ));
                self.last_error =
                    Some(t!("launcher.new_project.save_prefix_config_error").to_string());
                self.import_status = t!("launcher.new_project.save_prefix_failed").to_string();
            }
        }
    }

    fn current_progress(&self, any_loading: bool) -> ProgressDisplay {
        if let Some(progress) = self.active_progress.as_ref() {
            let stage_name = progress_stage_title(&progress.stage);
            let label = if progress.total == 0 {
                format!(
                    "{}: {stage_name}",
                    progress_operation_title(progress.operation)
                )
            } else {
                format!(
                    "{}: {stage_name} ({}/{})",
                    progress_operation_title(progress.operation),
                    progress.current.min(progress.total),
                    progress.total,
                )
            };
            return ProgressDisplay {
                fraction: progress_fraction(progress.current, progress.total),
                label,
            };
        }

        ProgressDisplay {
            fraction: progress_value(any_loading),
            label: progress_label(any_loading).to_string(),
        }
    }

    fn show_operation_progress(&self, ui: &mut Ui, operation: &'static str) {
        let Some(progress) = self.active_progress.as_ref() else {
            return;
        };
        if progress.operation != operation {
            return;
        }
        ui.add_space(8.0);
        ui.label(
            RichText::new(tf!("launcher.new_project.stage_status", progress_stage_title = progress_stage_title(&progress.stage)))
                .small()
                .weak(),
        );
        ui.add(
            ProgressBar::new(progress_fraction(progress.current, progress.total))
                .animate(true)
                .desired_width(fill_width(ui))
                .text(progress_status_label(
                    &progress.stage,
                    progress.current,
                    progress.total,
                )),
        );
    }

    fn show_batch_processing_window(&mut self, ctx: &egui::Context) {
        if !self.batch_processing_window_open {
            return;
        }
        let viewport_id = egui::ViewportId::from_hash_of("launcher_batch_processing_window");
        let builder = egui::ViewportBuilder::default()
            .with_title(t!("launcher.batch.window_title"))
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([900.0, 600.0])
            .with_resizable(true);
        ctx.show_viewport_immediate(viewport_id, builder, |ui, class| {
            let ctx = ui.ctx().clone();
            let keep_open = self.batch_processing.show(ui, class);
            if !keep_open {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                self.batch_processing_window_open = false;
            }
        });
    }
}

struct ProgressDisplay {
    fraction: f32,
    label: String,
}

fn progress_status_label(stage: &str, current: usize, total: usize) -> String {
    if stage == "collect_canvas" && total == 0 && current > 0 {
        return tf!("launcher.new_project.canvas_collect_status", current = current);
    }
    let prefix = progress_stage_title(stage);
    if total == 0 {
        format!("{prefix}...")
    } else {
        format!("{prefix}: {}/{}", current.min(total), total)
    }
}

fn section_group(ui: &mut Ui, title: &str, add_body: impl FnOnce(&mut Ui)) {
    Frame::group(ui.style())
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.label(RichText::new(title).size(18.0).strong());
            ui.add_space(10.0);
            add_body(ui);
        });
}

fn sub_group(ui: &mut Ui, title: &str, add_body: impl FnOnce(&mut Ui)) {
    Frame::group(ui.style())
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.label(RichText::new(title).size(15.0).strong());
            ui.add_space(8.0);
            add_body(ui);
        });
}

fn field_label(ui: &mut Ui, label: &str) {
    ui.label(RichText::new(label).small());
}

fn field_label_hover(ui: &mut Ui, label: &str, hover_text: &str) {
    ui.label(RichText::new(label).small())
        .on_hover_text(hover_text);
}

fn numeric_text_field(ui: &mut Ui, label: &str, value: &mut String) {
    field_label(ui, label);
    ui.add(TextEdit::singleline(value).desired_width(160.0));
}

fn selected_label<'a>(values: &'a [&str], selected: usize, field_name: &str) -> Option<&'a str> {
    match values.get(selected).copied() {
        Some(value) => Some(value),
        None => {
            crate::runtime_log::log_error(format!(
                "[new-project] invalid combo index for {field_name}: {selected}"
            ));
            None
        }
    }
}

fn combo_index(ui: &mut Ui, id: &str, values: &[&str], selected: &mut usize) {
    ComboBox::from_id_salt(id)
        .width(fill_width(ui))
        .selected_text(values.get(*selected).copied().unwrap_or("—"))
        .show_ui(ui, |ui| {
            for (index, value) in values.iter().enumerate() {
                ui.selectable_value(selected, index, *value);
            }
        });
}

fn combo_index_owned(ui: &mut Ui, id: &str, values: &[String], selected: &mut usize) {
    ComboBox::from_id_salt(id)
        .width(fill_width(ui))
        .selected_text(values.get(*selected).map(String::as_str).unwrap_or("—"))
        .show_ui(ui, |ui| {
            for (index, value) in values.iter().enumerate() {
                ui.selectable_value(selected, index, value);
            }
        });
}

fn combo_index_pairs(ui: &mut Ui, id: &str, values: &[(String, String)], selected: &mut usize) {
    ComboBox::from_id_salt(id)
        .width(fill_width(ui))
        .selected_text(
            values
                .get(*selected)
                .map(|(name, _)| name.as_str())
                .unwrap_or("—"),
        )
        .show_ui(ui, |ui| {
            for (index, (name, _)) in values.iter().enumerate() {
                ui.selectable_value(selected, index, name);
            }
        });
}

fn fill_width(ui: &Ui) -> f32 {
    safe_dimension(ui.available_width(), 120.0)
}

fn safe_dimension(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 1.0 {
        value
    } else {
        fallback
    }
}

fn manual_cut_y_is_valid(y: usize, page_height: usize) -> bool {
    y >= MANUAL_CUT_MIN_EDGE_DISTANCE_PX
        && y.saturating_add(MANUAL_CUT_MIN_EDGE_DISTANCE_PX) <= page_height
}

fn standard_dark_style() -> egui::Style {
    egui::Style {
        visuals: egui::Visuals::dark(),
        ..Default::default()
    }
}

fn advanced_group_color(group_id: usize) -> egui::Color32 {
    const COLORS: [egui::Color32; 10] = [
        egui::Color32::from_rgb(244, 67, 54),
        egui::Color32::from_rgb(33, 150, 243),
        egui::Color32::from_rgb(76, 175, 80),
        egui::Color32::from_rgb(255, 193, 7),
        egui::Color32::from_rgb(156, 39, 176),
        egui::Color32::from_rgb(0, 188, 212),
        egui::Color32::from_rgb(255, 112, 67),
        egui::Color32::from_rgb(139, 195, 74),
        egui::Color32::from_rgb(121, 85, 72),
        egui::Color32::from_rgb(96, 125, 139),
    ];
    COLORS[group_id % COLORS.len()]
}

fn auto_review_card_layout(ui: &Ui) -> (usize, f32) {
    let available_width = ui.available_width().min(ui.clip_rect().width()).max(1.0);
    let mut columns = 1usize;
    loop {
        let next_columns = columns.saturating_add(1);
        let gap_width = auto_review_gap_width(next_columns);
        let next_width = ((available_width - gap_width) / auto_review_columns_as_f32(next_columns))
            .floor()
            .min(AUTO_REVIEW_CARD_SIDE);
        if next_width < AUTO_REVIEW_CARD_MIN_SIDE {
            break;
        }
        columns = next_columns;
    }
    let gap_width = auto_review_gap_width(columns);
    let card_side = if columns == 1 {
        available_width.floor().clamp(1.0, AUTO_REVIEW_CARD_SIDE)
    } else {
        ((available_width - gap_width) / auto_review_columns_as_f32(columns))
            .floor()
            .clamp(AUTO_REVIEW_CARD_MIN_SIDE, AUTO_REVIEW_CARD_SIDE)
    };
    (columns, card_side)
}

fn auto_review_gap_width(columns: usize) -> f32 {
    (1..columns).fold(0.0, |width, _| width + AUTO_REVIEW_CARD_GAP)
}

fn auto_review_columns_as_f32(columns: usize) -> f32 {
    (0..columns).fold(0.0_f32, |count, _| count + 1.0).max(1.0)
}

fn auto_review_index_as_f32(index: usize) -> f32 {
    (0..index).fold(0.0_f32, |count, _| count + 1.0)
}

fn auto_review_default_removed_groups(candidates: &AdvancedAutoCandidateSet) -> HashSet<usize> {
    candidates
        .groups
        .iter()
        .filter(|group| group.item_ids.len() <= 1)
        .map(|group| group.id)
        .collect()
}

/// Candidate ids the review opens with unchecked: helper-flagged probable junk
/// (size-outlier icons, sprites, UI chrome), so the user keeps the real pages.
fn auto_review_default_removed_items(candidates: &AdvancedAutoCandidateSet) -> HashSet<usize> {
    candidates
        .items
        .iter()
        .filter(|item| item.probable_junk)
        .map(|item| item.id)
        .collect()
}

fn shorten_url(url: &str, limit: usize) -> String {
    if url.chars().count() <= limit {
        return url.to_string();
    }
    let mut value = url
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    value.push('…');
    value
}

fn dynamic_image_preview(
    image: &DynamicImage,
    max_width: u32,
    max_height: u32,
) -> egui::ColorImage {
    let preview = image.thumbnail(max_width, max_height).to_rgba8();
    let width = usize::try_from(preview.width()).unwrap_or(1).max(1);
    let height = usize::try_from(preview.height()).unwrap_or(1).max(1);
    egui::ColorImage::from_rgba_unmultiplied([width, height], preview.as_raw())
}

fn button_sized(ui: &mut Ui, label: &str, size: egui::Vec2, enabled: bool) -> egui::Response {
    ui.add_enabled(enabled, Button::new(label).min_size(size))
}

fn ribbon_control_button(label: &str, fill: egui::Color32, enabled: bool) -> Button<'static> {
    let (fill, stroke, text_color) = if enabled {
        (
            fill,
            egui::Color32::from_rgba_premultiplied(255, 255, 255, 92),
            egui::Color32::WHITE,
        )
    } else {
        (
            egui::Color32::from_rgba_premultiplied(72, 76, 84, 220),
            egui::Color32::from_rgba_premultiplied(255, 255, 255, 28),
            egui::Color32::from_rgba_premultiplied(255, 255, 255, 80),
        )
    };
    Button::new(RichText::new(label).size(13.0).strong().color(text_color))
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(999.0)
}

fn show_ribbon_image_controls(
    ui: &mut Ui,
    image_rect: egui::Rect,
    index: usize,
    pages_len: usize,
) -> Option<(usize, RibbonImageControlAction)> {
    let can_move_up = index > 0;
    let can_move_down = index + 1 < pages_len;
    let button_span = RIBBON_DELETE_BUTTON_SIZE * 3.0 + RIBBON_IMAGE_CONTROL_GAP * 2.0;
    let group_width =
        RIBBON_CROP_BUTTON_WIDTH + RIBBON_IMAGE_CONTROL_GAP + RIBBON_DELETE_BUTTON_SIZE;
    let sticky_top = (ui.clip_rect().top() + 8.0).clamp(
        image_rect.top() + 8.0,
        (image_rect.bottom() - button_span - 8.0).max(image_rect.top() + 8.0),
    );
    let sticky_left = (image_rect.right() - group_width - 8.0).max(image_rect.left() + 8.0);
    let controls_rect = egui::Rect::from_min_size(
        egui::pos2(sticky_left, sticky_top),
        egui::vec2(group_width, button_span),
    );
    let crop_rect = egui::Rect::from_min_size(
        controls_rect.min,
        egui::vec2(RIBBON_CROP_BUTTON_WIDTH, RIBBON_DELETE_BUTTON_SIZE),
    );
    let up_rect = egui::Rect::from_min_size(
        egui::pos2(
            controls_rect.right() - RIBBON_DELETE_BUTTON_SIZE,
            controls_rect.top(),
        ),
        egui::vec2(RIBBON_DELETE_BUTTON_SIZE, RIBBON_DELETE_BUTTON_SIZE),
    );
    let down_rect = egui::Rect::from_min_size(
        egui::pos2(
            controls_rect.right() - RIBBON_DELETE_BUTTON_SIZE,
            up_rect.bottom() + RIBBON_IMAGE_CONTROL_GAP,
        ),
        egui::vec2(RIBBON_DELETE_BUTTON_SIZE, RIBBON_DELETE_BUTTON_SIZE),
    );
    let delete_rect = egui::Rect::from_min_size(
        egui::pos2(
            controls_rect.right() - RIBBON_DELETE_BUTTON_SIZE,
            down_rect.bottom() + RIBBON_IMAGE_CONTROL_GAP,
        ),
        egui::vec2(RIBBON_DELETE_BUTTON_SIZE, RIBBON_DELETE_BUTTON_SIZE),
    );

    if ui.put(crop_rect, ribbon_crop_button()).clicked() {
        return Some((index, RibbonImageControlAction::Crop));
    }
    if ui
        .put(
            up_rect,
            ribbon_control_button("/\\", egui::Color32::from_rgb(52, 104, 173), can_move_up),
        )
        .clicked()
        && can_move_up
    {
        return Some((index, RibbonImageControlAction::MoveUp));
    }
    if ui
        .put(
            down_rect,
            ribbon_control_button("\\/", egui::Color32::from_rgb(52, 104, 173), can_move_down),
        )
        .clicked()
        && can_move_down
    {
        return Some((index, RibbonImageControlAction::MoveDown));
    }
    if ui
        .put(
            delete_rect,
            ribbon_control_button("×", egui::Color32::from_rgb(190, 48, 48), true),
        )
        .clicked()
    {
        return Some((index, RibbonImageControlAction::Delete));
    }
    None
}

fn ribbon_crop_button() -> Button<'static> {
    Button::new(
        RichText::new(t!("launcher.new_project.crop_menu"))
            .size(13.0)
            .strong()
            .color(egui::Color32::WHITE),
    )
    .fill(egui::Color32::from_rgb(174, 103, 24))
    .stroke(Stroke::new(
        1.0,
        egui::Color32::from_rgba_premultiplied(255, 220, 170, 150),
    ))
    .corner_radius(10.0)
}

fn clamp_window_pos_to_viewport(
    desired_pos: egui::Pos2,
    window_size: egui::Vec2,
    viewport: egui::Rect,
) -> egui::Pos2 {
    egui::pos2(
        desired_pos.x.clamp(
            viewport.left(),
            (viewport.right() - window_size.x).max(viewport.left()),
        ),
        desired_pos.y.clamp(
            viewport.top(),
            (viewport.bottom() - window_size.y).max(viewport.top()),
        ),
    )
}

fn draw_crop_editor_canvas(ui: &mut Ui, editor: &mut CropEditorState) {
    let image_size = egui::vec2(editor.source_size[0] as f32, editor.source_size[1] as f32);
    let (image_rect, _) = ui.allocate_exact_size(image_size, egui::Sense::hover());
    paint_tiled_image(
        ui,
        image_rect,
        1.0,
        editor.tiles.as_mut_slice(),
        &format!("launcher-new-project-crop-{}", editor.page_index),
    );

    let crop_rect_screen = crop_rect_to_screen(editor.crop_rect, image_rect);
    paint_crop_overlay(ui, image_rect, crop_rect_screen);
    paint_crop_handles(ui, crop_rect_screen);
    handle_crop_drag(ui, image_rect, editor, crop_rect_screen);
}

fn paint_tiled_image(
    ui: &mut Ui,
    image_rect: egui::Rect,
    width_scale: f32,
    tiles: &mut [RibbonTile],
    texture_prefix: &str,
) {
    let viewport_rect = ui.clip_rect().expand(128.0);
    for (tile_index, tile) in tiles.iter_mut().enumerate() {
        if tile.texture.is_none() {
            let texture = ui.ctx().load_texture(
                format!("{texture_prefix}-{tile_index}"),
                tile.color_image.clone(),
                TextureOptions::LINEAR,
            );
            tile.texture = Some(texture);
        }
        if let Some(texture) = tile.texture.as_ref() {
            let tile_rect = egui::Rect::from_min_size(
                egui::pos2(
                    image_rect.left() + tile.origin_px[0] as f32 * width_scale,
                    image_rect.top() + tile.origin_px[1] as f32 * width_scale,
                ),
                egui::vec2(
                    tile.size[0] as f32 * width_scale,
                    tile.size[1] as f32 * width_scale,
                ),
            );
            if tile_rect.intersects(viewport_rect) {
                ui.painter().image(
                    texture.id(),
                    tile_rect,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
        }
    }
}

fn crop_rect_to_screen(crop_rect: RibbonCrop, image_rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(
            image_rect.left() + crop_rect.left as f32,
            image_rect.top() + crop_rect.top as f32,
        ),
        egui::pos2(
            image_rect.left() + (crop_rect.left + crop_rect.width) as f32,
            image_rect.top() + (crop_rect.top + crop_rect.height) as f32,
        ),
    )
}

fn screen_capture_rect_to_screen(selection: RibbonCrop, desktop_rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(
            desktop_rect.left() + selection.left as f32,
            desktop_rect.top() + selection.top as f32,
        ),
        egui::pos2(
            desktop_rect.left() + (selection.left + selection.width) as f32,
            desktop_rect.top() + (selection.top + selection.height) as f32,
        ),
    )
}

fn paint_crop_overlay(ui: &Ui, image_rect: egui::Rect, crop_rect_screen: egui::Rect) {
    let painter = ui.painter();
    let shade = egui::Color32::from_rgba_premultiplied(0, 0, 0, 110);
    let top = egui::Rect::from_min_max(
        image_rect.min,
        egui::pos2(image_rect.right(), crop_rect_screen.top()),
    );
    let bottom = egui::Rect::from_min_max(
        egui::pos2(image_rect.left(), crop_rect_screen.bottom()),
        image_rect.max,
    );
    let left = egui::Rect::from_min_max(
        egui::pos2(image_rect.left(), crop_rect_screen.top()),
        egui::pos2(crop_rect_screen.left(), crop_rect_screen.bottom()),
    );
    let right = egui::Rect::from_min_max(
        egui::pos2(crop_rect_screen.right(), crop_rect_screen.top()),
        egui::pos2(image_rect.right(), crop_rect_screen.bottom()),
    );
    for rect in [top, bottom, left, right] {
        if rect.is_positive() {
            painter.rect_filled(rect, 0.0, shade);
        }
    }
    painter.rect_stroke(
        crop_rect_screen,
        8.0,
        Stroke::new(2.0, egui::Color32::from_rgb(255, 168, 61)),
        egui::StrokeKind::Inside,
    );
}

fn paint_screen_capture_overlay(ui: &Ui, desktop_rect: egui::Rect, selection_rect: egui::Rect) {
    let painter = ui.painter();
    let shade = egui::Color32::from_rgba_premultiplied(4, 8, 14, 156);
    let top = egui::Rect::from_min_max(
        desktop_rect.min,
        egui::pos2(desktop_rect.right(), selection_rect.top()),
    );
    let bottom = egui::Rect::from_min_max(
        egui::pos2(desktop_rect.left(), selection_rect.bottom()),
        desktop_rect.max,
    );
    let left = egui::Rect::from_min_max(
        egui::pos2(desktop_rect.left(), selection_rect.top()),
        egui::pos2(selection_rect.left(), selection_rect.bottom()),
    );
    let right = egui::Rect::from_min_max(
        egui::pos2(selection_rect.right(), selection_rect.top()),
        egui::pos2(desktop_rect.right(), selection_rect.bottom()),
    );
    for rect in [top, bottom, left, right] {
        if rect.is_positive() {
            painter.rect_filled(rect, 0.0, shade);
        }
    }
    painter.rect_stroke(
        selection_rect,
        10.0,
        Stroke::new(2.0, egui::Color32::from_rgb(255, 196, 74)),
        egui::StrokeKind::Inside,
    );
    painter.rect_stroke(
        selection_rect.expand(1.0),
        10.0,
        Stroke::new(1.0, egui::Color32::from_rgba_premultiplied(18, 18, 18, 220)),
        egui::StrokeKind::Outside,
    );
}

fn paint_crop_handles(ui: &Ui, crop_rect_screen: egui::Rect) {
    let painter = ui.painter();
    for (_, handle_rect) in crop_handle_rects(crop_rect_screen) {
        painter.rect_filled(handle_rect, 4.0, egui::Color32::from_rgb(255, 168, 61));
        painter.rect_stroke(
            handle_rect,
            4.0,
            Stroke::new(1.0, egui::Color32::from_rgb(70, 37, 6)),
            egui::StrokeKind::Inside,
        );
    }
}

fn paint_screen_capture_handles(ui: &Ui, selection_rect: egui::Rect) {
    let painter = ui.painter();
    for (_, handle_rect) in crop_handle_rects(selection_rect) {
        painter.rect_filled(handle_rect, 4.0, egui::Color32::from_rgb(255, 196, 74));
        painter.rect_stroke(
            handle_rect,
            4.0,
            Stroke::new(1.0, egui::Color32::from_rgb(74, 43, 6)),
            egui::StrokeKind::Inside,
        );
    }
}

fn render_screen_capture_overlay(ui: &mut Ui, overlay: &mut ScreenCaptureOverlayState) {
    let available = ui.max_rect();
    let desktop_rect = egui::Rect::from_min_size(available.min, available.size());
    let selection_rect = screen_capture_rect_to_screen(overlay.selection, desktop_rect);
    paint_screen_capture_overlay(ui, desktop_rect, selection_rect);
    paint_screen_capture_handles(ui, selection_rect);
    handle_screen_capture_drag(ui, desktop_rect, overlay, selection_rect);
}

fn show_screen_capture_overlay_controls(ui: &mut Ui, selection: RibbonCrop) -> bool {
    let rect = screen_capture_rect_to_screen(selection, ui.max_rect());
    let button_size = egui::vec2(132.0, 32.0);
    let controls_width = 312.0;
    let x =
        (rect.right() - controls_width).clamp(16.0, ui.max_rect().right() - controls_width - 16.0);
    let y = (rect.top() - button_size.y - 12.0).max(16.0);
    let controls_rect =
        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(controls_width, button_size.y));

    let frame = Frame::new()
        .fill(egui::Color32::from_rgba_premultiplied(14, 18, 24, 228))
        .corner_radius(10.0)
        .stroke(Stroke::new(1.0, egui::Color32::from_rgb(255, 196, 74)))
        .inner_margin(egui::Margin::symmetric(8, 6));

    let mut capture_clicked = false;
    ui.scope_builder(egui::UiBuilder::new().max_rect(controls_rect), |ui| {
        frame.show(ui, |ui| {
            ui.horizontal(|ui| {
                if button_sized(ui, t!("launcher.new_project.capture_snap_button"), button_size, true).clicked() {
                    capture_clicked = true;
                }
                ui.label(
                    RichText::new(format!("{} × {}", selection.width, selection.height))
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);
                ui.label(RichText::new(t!("launcher.new_project.capture_esc_hint")).small().weak());
            });
        });
    });
    capture_clicked
}

fn handle_screen_capture_drag(
    ui: &mut Ui,
    desktop_rect: egui::Rect,
    overlay: &mut ScreenCaptureOverlayState,
    selection_rect: egui::Rect,
) {
    let handle_rects = crop_handle_rects(selection_rect);
    for (handle, rect) in handle_rects {
        process_screen_capture_drag_response(ui, desktop_rect, overlay, rect, handle);
    }
    let move_rect = selection_rect.shrink(CROP_HANDLE_SIZE * 0.75);
    if move_rect.is_positive() {
        process_screen_capture_drag_response(
            ui,
            desktop_rect,
            overlay,
            move_rect,
            CropHandle::Move,
        );
    }
}

fn process_screen_capture_drag_response(
    ui: &mut Ui,
    desktop_rect: egui::Rect,
    overlay: &mut ScreenCaptureOverlayState,
    drag_rect: egui::Rect,
    handle: CropHandle,
) {
    let response = ui.interact(
        drag_rect,
        ui.id()
            .with(("launcher_new_project_screen_capture_drag", handle)),
        egui::Sense::drag(),
    );
    if response.drag_started()
        && let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
    {
        overlay.drag_state = Some(CropDragState {
            handle,
            start_rect: overlay.selection,
            start_pointer_px: egui::pos2(
                pointer_pos.x - desktop_rect.left(),
                pointer_pos.y - desktop_rect.top(),
            ),
        });
    }
    if response.dragged()
        && let Some(drag_state) = overlay.drag_state
        && drag_state.handle == handle
        && let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
    {
        let current_pointer_px = egui::pos2(
            pointer_pos.x - desktop_rect.left(),
            pointer_pos.y - desktop_rect.top(),
        );
        overlay.selection = apply_crop_drag(
            drag_state.start_rect,
            drag_state.handle,
            drag_state.start_pointer_px,
            current_pointer_px,
            [
                usize::try_from(overlay.desktop_bounds.width).unwrap_or(usize::MAX),
                usize::try_from(overlay.desktop_bounds.height).unwrap_or(usize::MAX),
            ],
            SCREEN_CAPTURE_MIN_SIDE,
        );
    }
    if response.drag_stopped()
        && overlay
            .drag_state
            .is_some_and(|state| state.handle == handle)
    {
        overlay.drag_state = None;
    }
}

fn handle_crop_drag(
    ui: &mut Ui,
    image_rect: egui::Rect,
    editor: &mut CropEditorState,
    crop_rect_screen: egui::Rect,
) {
    let handle_rects = crop_handle_rects(crop_rect_screen);
    for (handle, rect) in handle_rects {
        process_crop_drag_response(ui, image_rect, editor, rect, handle);
    }
    let move_rect = crop_rect_screen.shrink(CROP_HANDLE_SIZE * 0.75);
    if move_rect.is_positive() {
        process_crop_drag_response(ui, image_rect, editor, move_rect, CropHandle::Move);
    }
}

fn crop_handle_rects(crop_rect_screen: egui::Rect) -> [(CropHandle, egui::Rect); 8] {
    let center = crop_rect_screen.center();
    [
        (
            CropHandle::TopLeft,
            egui::Rect::from_center_size(
                crop_rect_screen.left_top(),
                egui::vec2(CROP_HANDLE_SIZE, CROP_HANDLE_SIZE),
            ),
        ),
        (
            CropHandle::Top,
            egui::Rect::from_center_size(
                egui::pos2(center.x, crop_rect_screen.top()),
                egui::vec2(CROP_HANDLE_SIZE * 1.4, CROP_HANDLE_SIZE),
            ),
        ),
        (
            CropHandle::TopRight,
            egui::Rect::from_center_size(
                crop_rect_screen.right_top(),
                egui::vec2(CROP_HANDLE_SIZE, CROP_HANDLE_SIZE),
            ),
        ),
        (
            CropHandle::Right,
            egui::Rect::from_center_size(
                egui::pos2(crop_rect_screen.right(), center.y),
                egui::vec2(CROP_HANDLE_SIZE, CROP_HANDLE_SIZE * 1.4),
            ),
        ),
        (
            CropHandle::BottomRight,
            egui::Rect::from_center_size(
                crop_rect_screen.right_bottom(),
                egui::vec2(CROP_HANDLE_SIZE, CROP_HANDLE_SIZE),
            ),
        ),
        (
            CropHandle::Bottom,
            egui::Rect::from_center_size(
                egui::pos2(center.x, crop_rect_screen.bottom()),
                egui::vec2(CROP_HANDLE_SIZE * 1.4, CROP_HANDLE_SIZE),
            ),
        ),
        (
            CropHandle::BottomLeft,
            egui::Rect::from_center_size(
                crop_rect_screen.left_bottom(),
                egui::vec2(CROP_HANDLE_SIZE, CROP_HANDLE_SIZE),
            ),
        ),
        (
            CropHandle::Left,
            egui::Rect::from_center_size(
                egui::pos2(crop_rect_screen.left(), center.y),
                egui::vec2(CROP_HANDLE_SIZE, CROP_HANDLE_SIZE * 1.4),
            ),
        ),
    ]
}

fn process_crop_drag_response(
    ui: &mut Ui,
    image_rect: egui::Rect,
    editor: &mut CropEditorState,
    drag_rect: egui::Rect,
    handle: CropHandle,
) {
    let response = ui.interact(
        drag_rect,
        ui.id()
            .with(("launcher_new_project_crop_drag", editor.page_index, handle)),
        egui::Sense::drag(),
    );
    if response.drag_started()
        && let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
    {
        editor.drag_state = Some(CropDragState {
            handle,
            start_rect: editor.crop_rect,
            start_pointer_px: egui::pos2(
                pointer_pos.x - image_rect.left(),
                pointer_pos.y - image_rect.top(),
            ),
        });
    }
    if response.dragged()
        && let Some(drag_state) = editor.drag_state
        && drag_state.handle == handle
        && let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
    {
        let current_pointer_px = egui::pos2(
            pointer_pos.x - image_rect.left(),
            pointer_pos.y - image_rect.top(),
        );
        editor.crop_rect = apply_crop_drag(
            drag_state.start_rect,
            drag_state.handle,
            drag_state.start_pointer_px,
            current_pointer_px,
            editor.source_size,
            CROP_MIN_SIDE,
        );
    }
    if response.drag_stopped()
        && editor
            .drag_state
            .is_some_and(|state| state.handle == handle)
    {
        editor.drag_state = None;
    }
}

fn apply_crop_drag(
    start_rect: RibbonCrop,
    handle: CropHandle,
    start_pointer_px: egui::Pos2,
    current_pointer_px: egui::Pos2,
    source_size: [usize; 2],
    min_side_px: usize,
) -> RibbonCrop {
    let delta_x = (current_pointer_px.x - start_pointer_px.x).round() as i64;
    let delta_y = (current_pointer_px.y - start_pointer_px.y).round() as i64;
    let min_side = i64::try_from(
        min_side_px
            .min(source_size[0].max(1))
            .min(source_size[1].max(1)),
    )
    .unwrap_or(1);
    let mut left = i64::try_from(start_rect.left).unwrap_or(0);
    let mut top = i64::try_from(start_rect.top).unwrap_or(0);
    let mut right = i64::try_from(start_rect.left + start_rect.width).unwrap_or(i64::MAX);
    let mut bottom = i64::try_from(start_rect.top + start_rect.height).unwrap_or(i64::MAX);
    let max_width = i64::try_from(source_size[0].max(1)).unwrap_or(i64::MAX);
    let max_height = i64::try_from(source_size[1].max(1)).unwrap_or(i64::MAX);

    if handle == CropHandle::Move {
        let width = right - left;
        let height = bottom - top;
        left = (left + delta_x).clamp(0, max_width - width);
        top = (top + delta_y).clamp(0, max_height - height);
        right = left + width;
        bottom = top + height;
    } else {
        if matches!(
            handle,
            CropHandle::Left | CropHandle::TopLeft | CropHandle::BottomLeft
        ) {
            left = (left + delta_x).clamp(0, right - min_side);
        }
        if matches!(
            handle,
            CropHandle::Right | CropHandle::TopRight | CropHandle::BottomRight
        ) {
            right = (right + delta_x).clamp(left + min_side, max_width);
        }
        if matches!(
            handle,
            CropHandle::Top | CropHandle::TopLeft | CropHandle::TopRight
        ) {
            top = (top + delta_y).clamp(0, bottom - min_side);
        }
        if matches!(
            handle,
            CropHandle::Bottom | CropHandle::BottomLeft | CropHandle::BottomRight
        ) {
            bottom = (bottom + delta_y).clamp(top + min_side, max_height);
        }
    }

    RibbonCrop {
        left: usize::try_from(left).unwrap_or(0),
        top: usize::try_from(top).unwrap_or(0),
        width: usize::try_from((right - left).max(1)).unwrap_or(source_size[0].max(1)),
        height: usize::try_from((bottom - top).max(1)).unwrap_or(source_size[1].max(1)),
    }
}

fn progress_value(is_loading: bool) -> f32 {
    if is_loading { 0.0 } else { 1.0 }
}

/// Resolves the virtual-desktop bounds used to place the capture overlay.
///
/// The whole capture-overlay chain (the `ViewportBuilder` position/size, the
/// default selection, `screen_capture_selection_to_global_rect`, and
/// `screencapture -R`) works in logical points. On macOS the pixel-based
/// `query_virtual_desktop_bounds` reads a full-screen PNG header and therefore
/// reports *device pixels*, which on a Retina (2x) display are twice the point
/// magnitude — that would make the overlay 2x oversized and the captured region
/// 2x wrong. To keep every stage consistent in points, macOS instead sources the
/// bounds from egui's monitor size (already in logical points).
///
/// Limitation: egui reports the monitor the launcher window currently sits on,
/// so on a multi-display desktop only that monitor is covered. This is correct
/// for the common single-display case and strictly better than the previously
/// 2x-broken pixel-based bounds. If egui has no monitor size yet (`None`), macOS
/// falls back to the pixel-based query rather than failing.
///
/// Non-macOS platforms use `query_virtual_desktop_bounds` exactly as before.
///
/// # Errors
/// Propagates the platform-specific error from `query_virtual_desktop_bounds`
/// when the desktop bounds cannot be determined.
fn resolve_capture_desktop_bounds(ctx: &egui::Context) -> Result<ScreenRect, String> {
    #[cfg(target_os = "macos")]
    {
        // Prefer egui's point-based monitor size so overlay sizing, the default
        // selection, and `screencapture -R` all share the point coordinate space.
        if let Some(bounds) = ctx
            .input(|input| input.viewport().monitor_size)
            .and_then(macos_monitor_size_to_bounds)
        {
            return Ok(bounds);
        }
        // Monitor size unavailable (e.g. not yet populated): degrade to the
        // existing pixel-based behavior instead of crashing.
        screen_capture::query_virtual_desktop_bounds()
    }
    #[cfg(not(target_os = "macos"))]
    {
        // `ctx` is only needed for the macOS monitor-size path; other platforms
        // keep using the pixel-based desktop query unchanged.
        let _ = ctx;
        screen_capture::query_virtual_desktop_bounds()
    }
}

/// Converts an egui monitor size (logical points) into capture-overlay bounds.
///
/// The overlay is anchored at the top-left of the current monitor, so the result
/// uses origin `(0, 0)` and the rounded point dimensions. Returns `None` for a
/// non-finite or sub-pixel size so the caller can fall back to the pixel-based
/// desktop query.
#[cfg(target_os = "macos")]
fn macos_monitor_size_to_bounds(monitor_size: egui::Vec2) -> Option<ScreenRect> {
    let width = monitor_size.x;
    let height = monitor_size.y;
    // Reject unusable sizes; a valid monitor is at least one point on each side.
    if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 {
        return None;
    }
    // Round to whole points and clamp to a sane monitor range. Bounding to
    // `u16::MAX` makes the following f32 -> u32 conversion provably exact
    // (integral value within `1..=65535`), which is the safety justification the
    // project's no-blind-`as` rule requires.
    let width = width.round().clamp(1.0, f32::from(u16::MAX));
    let height = height.round().clamp(1.0, f32::from(u16::MAX));
    Some(ScreenRect {
        x: 0,
        y: 0,
        width: width as u32,
        height: height as u32,
    })
}

fn default_screen_capture_selection(desktop_bounds: ScreenRect) -> RibbonCrop {
    let desktop_width = usize::try_from(desktop_bounds.width)
        .unwrap_or(usize::MAX)
        .max(1);
    let desktop_height = usize::try_from(desktop_bounds.height)
        .unwrap_or(usize::MAX)
        .max(1);
    let width = (desktop_width.saturating_mul(45) / 100)
        .max(SCREEN_CAPTURE_MIN_SIDE)
        .min(desktop_width);
    let height = (desktop_height.saturating_mul(32) / 100)
        .max(SCREEN_CAPTURE_MIN_SIDE)
        .min(desktop_height);
    RibbonCrop {
        left: desktop_width.saturating_sub(width) / 2,
        top: desktop_height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn screen_capture_selection_to_global_rect(
    desktop_bounds: ScreenRect,
    selection: RibbonCrop,
) -> ScreenRect {
    ScreenRect {
        x: desktop_bounds
            .x
            .saturating_add(i32::try_from(selection.left).unwrap_or(i32::MAX)),
        y: desktop_bounds
            .y
            .saturating_add(i32::try_from(selection.top).unwrap_or(i32::MAX)),
        width: u32::try_from(selection.width).unwrap_or(u32::MAX),
        height: u32::try_from(selection.height).unwrap_or(u32::MAX),
    }
}

fn progress_label(is_loading: bool) -> &'static str {
    if is_loading {
        t!("launcher.common.loading")
    } else {
        t!("launcher.common.done")
    }
}

fn random_test_chapter_number() -> usize {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.subsec_nanos());
    usize::try_from(nanos % 100).unwrap_or(0).saturating_add(1)
}

/// Web stub: the availability probe uses a native HTTP client (`ureq`) that is not
/// compiled for wasm. Returns an unavailable result with a diagnostic log message
/// rather than a fake "reachable" answer.
#[cfg(target_arch = "wasm32")]
fn check_test_chapter_site_availability(chapter_url: String) -> TestChapterAvailabilityResult {
    TestChapterAvailabilityResult {
        available: false,
        chapter_url,
        log_message: Some(
            t!("launcher.new_project.site_check_web_unsupported").to_string(),
        ),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn check_test_chapter_site_availability(chapter_url: String) -> TestChapterAvailabilityResult {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(TEST_CHAPTER_SITE_CHECK_TIMEOUT)
        .timeout_read(TEST_CHAPTER_SITE_CHECK_TIMEOUT)
        .timeout_write(TEST_CHAPTER_SITE_CHECK_TIMEOUT)
        .build();
    let request = agent.get("https://comic.naver.com/").set(
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/115.0.0.0 Safari/537.36",
    );
    match request.call() {
        Ok(_) => TestChapterAvailabilityResult {
            available: true,
            chapter_url,
            log_message: None,
        },
        Err(ureq::Error::Status(status, response)) => TestChapterAvailabilityResult {
            available: false,
            chapter_url,
            log_message: Some(format!(
                "comic.naver.com returned status {status}; body={}",
                response.into_string().unwrap_or_default()
            )),
        },
        Err(ureq::Error::Transport(transport)) => TestChapterAvailabilityResult {
            available: false,
            chapter_url,
            log_message: Some(format!("comic.naver.com transport error: {transport}")),
        },
    }
}

fn progress_fraction(current: usize, total: usize) -> f32 {
    if total == 0 {
        0.0
    } else {
        (current.min(total) as f32 / total as f32).clamp(0.0, 1.0)
    }
}

fn progress_operation_title(operation: &str) -> &'static str {
    match operation {
        "source" => t!("launcher.new_project.progress_download_images"),
        "save" => t!("launcher.new_project.progress_save"),
        "advanced_download" => t!("launcher.new_project.progress_advanced_dl"),
        "quick_download" => t!("launcher.new_project.progress_quick_dl"),
        "stitch" => t!("launcher.new_project.progress_cut"),
        "waifu2x" => "waifu2x",
        "reline" => "Reline",
        _ => t!("launcher.new_project.progress_processing"),
    }
}

fn supported_quick_download_sites() -> Vec<&'static str> {
    supported_sites_tooltip()
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

fn progress_stage_title(stage: &str) -> &'static str {
    match stage {
        "prepare" => t!("launcher.new_project.progress_prepare_files"),
        "clean" => t!("launcher.new_project.progress_clean_folder"),
        "write" => t!("launcher.new_project.progress_write_png"),
        "browser" => t!("launcher.new_project.progress_start_browser"),
        "collect" => t!("launcher.new_project.progress_search_links"),
        "collect_canvas" => t!("launcher.new_project.progress_collect_canvas"),
        "connect" => t!("launcher.new_project.progress_check_availability"),
        "scan" => t!("launcher.new_project.progress_search_source"),
        "parse_html" => t!("launcher.new_project.progress_parse_html"),
        "archive" => t!("launcher.new_project.progress_extract_archive"),
        "download_waifu2x" => t!("launcher.new_project.progress_download_waifu2x"),
        "extract_waifu2x" => t!("launcher.new_project.progress_extract_waifu2x"),
        "download" => t!("launcher.new_project.progress_download_pages"),
        "save_canvas" => t!("launcher.new_project.progress_save_canvas"),
        "decode" => t!("launcher.new_project.progress_read_images"),
        "filter" => t!("launcher.new_project.progress_filter_width"),
        "preview" => t!("launcher.new_project.progress_prepare_preview"),
        "normalize" => t!("launcher.new_project.progress_align_width"),
        "stitch" => t!("launcher.new_project.progress_stitch_ribbon"),
        "waifu2x" => t!("launcher.new_project.progress_waifu2x"),
        "reline" => t!("launcher.new_project.progress_reline"),
        "cuts" => t!("launcher.new_project.progress_find_cut_lines"),
        "compose" => t!("launcher.new_project.progress_assemble_ribbon"),
        "split" => t!("launcher.new_project.progress_cut_pages"),
        _ => t!("launcher.new_project.progress_processing"),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn confirm_overwrite_nonempty(path: &std::path::Path) -> Result<bool, std::io::Error> {
    if !path.exists() || !dir_has_entries(path)? {
        return Ok(true);
    }
    Ok(MessageDialog::new()
        .set_title("ManhwaStudio")
        .set_description(t!("launcher.new_project.folder_not_empty_prompt"))
        .set_buttons(MessageButtons::YesNo)
        .set_level(MessageLevel::Warning)
        .show()
        == MessageDialogResult::Yes)
}

/// Web stub: the native confirmation modal (`rfd::MessageDialog`) has no browser
/// equivalent. Empty/absent targets proceed as on native; for a non-empty target we
/// cannot prompt, so we log the skipped confirmation and proceed instead of blocking.
#[cfg(target_arch = "wasm32")]
fn confirm_overwrite_nonempty(path: &std::path::Path) -> Result<bool, std::io::Error> {
    if !path.exists() || !dir_has_entries(path)? {
        return Ok(true);
    }
    crate::runtime_log::log_warn(
        "overwrite confirmation dialog unavailable on web build; proceeding without prompt",
    );
    Ok(true)
}

fn load_image_url_presets() -> Vec<(String, String)> {
    let config_result = config::load_user_config();
    let cfg = match config_result {
        Ok(cfg) => cfg,
        Err(err) => {
            crate::runtime_log::log_warn(format!(
                "[new-project] failed to load user config for image url presets: {err}"
            ));
            return Vec::new();
        }
    };
    let mut presets = cfg
        .get_path(&["NewProjectWindow", "ImageUrlPrefs"])
        .and_then(serde_json::Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(name, value)| {
                    value
                        .as_str()
                        .map(|prefix| (name.clone(), prefix.to_string()))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    presets.sort_by(|left, right| left.0.cmp(&right.0));
    presets
}

fn save_image_url_preset(site_name: &str, prefix: &str) -> anyhow::Result<()> {
    let mut cfg = config::load_user_config()?;
    let mut map = cfg
        .get_path(&["NewProjectWindow", "ImageUrlPrefs"])
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    map.insert(
        site_name.trim().to_string(),
        serde_json::Value::String(prefix.trim().to_string()),
    );
    cfg.set_path(
        &["NewProjectWindow", "ImageUrlPrefs"],
        serde_json::Value::Object(map),
    )?;
    Ok(())
}

/// Load the persisted Reline UI mode from user config.
///
/// Returns `RelineUiMode::Simple` when the key is missing or the config cannot be read, so the
/// guided UI is the default on first run.
fn load_reline_ui_mode() -> RelineUiMode {
    let cfg = match config::load_user_config() {
        Ok(cfg) => cfg,
        Err(err) => {
            crate::runtime_log::log_warn(format!(
                "[new-project] failed to load user config for Reline UI mode: {err}"
            ));
            return RelineUiMode::Simple;
        }
    };
    cfg.get_path(&["NewProjectWindow", "RelineUiMode"])
        .and_then(serde_json::Value::as_str)
        .map(RelineUiMode::from_config_str)
        .unwrap_or(RelineUiMode::Simple)
}

/// Persist the Reline UI mode to user config. Called only on toggle change, not per frame.
fn save_reline_ui_mode(mode: RelineUiMode) {
    let result = (|| -> anyhow::Result<()> {
        let mut cfg = config::load_user_config()?;
        cfg.set_path(
            &["NewProjectWindow", "RelineUiMode"],
            serde_json::Value::String(mode.as_config_str().to_string()),
        )?;
        Ok(())
    })();
    if let Err(err) = result {
        crate::runtime_log::log_warn(format!(
            "[new-project] failed to save Reline UI mode: {err}"
        ));
    }
}

fn arc_image_clone(image: Arc<RgbaImage>) -> RgbaImage {
    (*image).clone()
}

fn stitch_kind_name(kind: &StitchSuccessKind) -> &'static str {
    match kind {
        StitchSuccessKind::AutoCut => "auto_cut",
        StitchSuccessKind::ReferenceCut => "reference_cut",
        StitchSuccessKind::ManualPreview => "manual_preview",
        StitchSuccessKind::ManualApply => "manual_apply",
        StitchSuccessKind::StitchOnly => "stitch_only",
        StitchSuccessKind::HeterogeneousBottoms => "heterogeneous_bottoms",
    }
}

fn stitch_mode_initial_stage(mode: StitchSplitMode) -> &'static str {
    match mode {
        StitchSplitMode::StitchOnly
        | StitchSplitMode::ManualCutPreview
        | StitchSplitMode::HeterogeneousBottoms => "stitch",
        StitchSplitMode::AutoCut => "cut",
    }
}

fn stitch_mode_start_status(mode: StitchSplitMode) -> &'static str {
    match mode {
        StitchSplitMode::StitchOnly => t!("launcher.new_project.stitching_progress"),
        StitchSplitMode::ManualCutPreview => t!("launcher.new_project.stitching_manual_progress"),
        StitchSplitMode::AutoCut => t!("launcher.new_project.stitching_auto_progress"),
        StitchSplitMode::HeterogeneousBottoms => t!("launcher.new_project.stitching_uneven_progress"),
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_capture_bounds_tests {
    use super::{ScreenRect, macos_monitor_size_to_bounds};

    #[test]
    fn retina_point_size_maps_to_point_bounds_at_origin() {
        // egui reports the monitor in logical points, so a 2x Retina 2880x1800
        // panel arrives here already as 1440x900 points.
        let bounds = macos_monitor_size_to_bounds(egui::vec2(1440.0, 900.0));
        assert_eq!(
            bounds,
            Some(ScreenRect {
                x: 0,
                y: 0,
                width: 1440,
                height: 900,
            })
        );
    }

    #[test]
    fn fractional_points_round_to_nearest_whole_point() {
        let bounds = macos_monitor_size_to_bounds(egui::vec2(1512.4, 981.6));
        assert_eq!(
            bounds,
            Some(ScreenRect {
                x: 0,
                y: 0,
                width: 1512,
                height: 982,
            })
        );
    }

    #[test]
    fn unusable_sizes_return_none_for_fallback() {
        assert_eq!(macos_monitor_size_to_bounds(egui::vec2(0.0, 900.0)), None);
        assert_eq!(macos_monitor_size_to_bounds(egui::vec2(1440.0, 0.5)), None);
        assert_eq!(
            macos_monitor_size_to_bounds(egui::vec2(f32::NAN, 900.0)),
            None
        );
        assert_eq!(
            macos_monitor_size_to_bounds(egui::vec2(f32::INFINITY, 900.0)),
            None
        );
    }
}
