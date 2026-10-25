/*
File: crates/ms-launcher/src/new_project/window/mod.rs

Purpose:
Content of the launcher's "New Project" window (mirrors the legacy Python layout). The launcher
hosts it in a `ms_widgets::EmbeddedWindow`, which decides separate OS window vs embedded.

Main responsibilities:
- poll every worker on each pass the window is open (`poll`), draw the content and its own
  dialogs only while the window is visible (`show_content`);
- render the import/download/stitch/save control column from the Python window;
- own batch processing as a child `EmbeddedWindow` (`show_children`; closed with this window,
  `on_window_closed`);
- react to ribbon/source events produced by sibling logic modules.

Key items here: every type of the window (`NewProjectWindowState` and its private state types),
the layout constants, and the entry points `new`, `poll`, `show_content`, `show_children`,
`on_window_closed`, the tutorial hooks and `show_contents`. The section bodies, controller
wiring and helpers are `impl NewProjectWindowState` blocks in the child files (map in this
directory's `MODULE_README.md`).

Notes:
The folder/file import flow, quick downloader, and stitch/split workflow are implemented in this
module. Long-running image processing is delegated to background controllers to keep the egui window
responsive while the ribbon updates. Ribbon and crop-editor page images are painted only through
`ribbon::paint_ribbon_tiles` (tiled, culled, budgeted uploads); this module owns their layout, not
their tiling.
File dialogs are non-blocking `ms_platform::file_dialog` requests polled every frame (their
trigger buttons are disabled while one is open), and overwriting a non-empty save folder is
confirmed through the in-app `ConfirmModal` (`overwrite_confirm`).
*/

use egui::{
    Align, Button, CentralPanel, Frame, Layout, ProgressBar, RichText, ScrollArea,
    Stroke, TextEdit, TextureHandle, TextureOptions, Ui, Window,
    WindowLevel,
};
use image::{DynamicImage, RgbaImage};
use ms_platform::file_dialog::{self, DialogKind, FileDialogError, FileDialogRequest, PendingDialog};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use ms_thread as thread;
use web_time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ms_config as config;
use crate::confirm_modal::{ConfirmAnswer, ConfirmModal};
use crate::new_project::advanced_download::{
    AdvancedAutoCandidateSet, AdvancedBrowserBackend, AdvancedDownloadController,
    AdvancedDownloadEvent, InterceptCounts, build_pages_from_auto_candidates,
};
use crate::new_project::open_source::{
    OpenSourceKind, SourceImportController, SourceImportOptions, SourceLoadEvent,
};
use crate::new_project::project_io::{
    ProjectCatalogController, ProjectCatalogEvent, ProjectCatalogSnapshot, ProjectSaveController,
    ProjectSaveEvent, ProjectSaveImage, ProjectSaveRequest, ProjectSaveTarget, chapters_for_title,
    dir_has_entries,
};
use crate::new_project::quick_download::{
    QuickDownloadController, QuickDownloadEvent, supported_sites_tooltip,
};
use crate::new_project::reline::{
    RelineController, RelineCvtColorOptions, RelineEvent, RelineHalftoneOptions, RelineInputImage,
    RelineLevelOptions, RelineModelCatalogController, RelineModelCatalogEntry,
    RelineModelCatalogEvent, RelineOptions, RelineResizeOptions, RelineSharpOptions,
    RelineUpscaleOptions,
};
use crate::new_project::ribbon::{
    ImportedImage, RibbonCrop, RibbonMergeError, RibbonPage, RibbonState, RibbonTiles,
    build_ribbon_pages, build_ribbon_tiles, paint_ribbon_tiles, ribbon_upload_budget,
};
use crate::new_project::stitching::{
    ManualCutGuide, StitchController, StitchEvent, StitchInputImage, StitchOptions, StitchRequest,
    StitchSplitMode, StitchSuccessKind,
};
use crate::new_project::waifu2x::{
    Waifu2xController, Waifu2xEvent, Waifu2xInputImage, Waifu2xOptions,
};
#[cfg(feature = "tutorial")]
use crate::new_project::tutorial::{self, NpTutorialCommand, NpTutorialCtx};
use crate::state::OpenProjectSelection;
use ms_sysprobe::paste_image;
#[cfg(feature = "tutorial")]
use ms_settings_ui::tutorial::{TutorialController, TutorialId, TutorialProgressHandle, TutorialStep};
use ms_sysprobe::screen_capture::{self, ScreenRect};
use ms_widgets::{
    ArrowStyle, EditableComboBox, EmbeddedWindow, EmbeddedWindowState, GutterItem, MarkedScrollArea,
    MarkedScrollOutput, ScrollSpan, WheelComboBox, WheelSlider, arrow, cycle_wrapped_index,
};
use crate::new_project::batch_processing::BatchProcessingWindowState;

mod advanced_ui;
mod capture;
mod crop_editor;
mod jobs;
mod left_panel;
mod options;
mod prefs;
mod processing_ui;
mod ribbon_view;
mod save;
#[cfg(test)]
mod tests;
mod widgets;

// Free helpers that siblings call, re-imported here so every sibling (`use super::*`) and the
// tests resolve them by their bare names. Helpers used only inside their own file stay private
// to that file's scope (`capture.rs`, `options.rs`, `save.rs`).
use self::advanced_ui::*;
use self::crop_editor::*;
use self::jobs::*;
use self::prefs::*;
use self::ribbon_view::*;
use self::widgets::*;

/// `EmbeddedWindow` identity of the batch-processing child window: never shown.
const BATCH_PROCESSING_WINDOW_ID: &str = "launcher_batch_processing_window";
/// Background of the whole window content (the window's own plain dark look).
const WINDOW_FILL: egui::Color32 = egui::Color32::from_rgb(24, 24, 27);
const LEFT_PANEL_WIDTH: f32 = 444.0;
const SECTION_SPACING: f32 = 14.0;
const ACTION_BUTTON_SIZE: egui::Vec2 = egui::vec2(170.0, 34.0);
const SMALL_BUTTON_SIZE: egui::Vec2 = egui::vec2(92.0, 32.0);
const VIEWER_MIN_HEIGHT: f32 = 560.0;
const RIBBON_PREVIEW_SPACING: f32 = 10.0;
const RIBBON_DELETE_BUTTON_SIZE: f32 = 26.0;
const RIBBON_CROP_BUTTON_WIDTH: f32 = 116.0;
const RIBBON_IMAGE_CONTROL_GAP: f32 = 6.0;
/// Smallest share of the available viewer width the ribbon container may be dragged down to.
const RIBBON_MIN_WIDTH_FRACTION: f32 = 0.2;
/// Width (points) of the invisible drag strip centered on each ribbon container edge. Half of it
/// reaches inside the container, which stays within the frame's 12pt inner margin and therefore
/// never overlaps the page rects or the marked-scrollbar gutter.
const RIBBON_RESIZE_GRAB_WIDTH: f32 = 18.0;
/// Width (points) of the painted resize handle bar drawn on each ribbon container edge.
const RIBBON_RESIZE_HANDLE_WIDTH: f32 = 10.0;
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

/// A save waiting for the user to confirm overwriting a non-empty destination folder.
struct PendingOverwrite {
    modal: ConfirmModal,
    launch: SaveLaunch,
}

/// One save to start once its destination is settled: where it goes, what happens after it
/// finishes, and the status line shown while it runs.
struct SaveLaunch {
    target: ProjectSaveTarget,
    open_after_save: bool,
    /// Switch back to the import panel after a successful save ("save and download more").
    return_to_import: bool,
    status_message: String,
}

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
    tiles: RibbonTiles,
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
    /// When `true` (the default) every ribbon page is scaled to fill the ribbon width. When
    /// `false` all pages share one scale derived from the widest page, so their on-screen widths
    /// keep the ratio of their source widths. Persisted under
    /// `NewProjectWindow/RibbonUniformWidth`.
    ribbon_uniform_width: bool,
    /// Share of the available viewer width used by the ribbon container, in
    /// `RIBBON_MIN_WIDTH_FRACTION..=1.0`. `1.0` (the default) means the ribbon spans the whole
    /// viewer column. Persisted under `NewProjectWindow/RibbonWidthFraction`.
    ribbon_width_fraction: f32,
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
    /// Open "cut like a reference folder" picker, polled every frame.
    cut_like_folder_dialog: Option<PendingDialog>,
    /// Open "save into a folder" picker, polled every frame.
    save_folder_dialog: Option<PendingDialog>,
    /// Save waiting for the overwrite confirmation modal.
    overwrite_confirm: Option<PendingOverwrite>,
    pending_open_wiki_guide: bool,
    /// The batch-processing window, a child of this one (`show_children`).
    batch_window: EmbeddedWindowState,
    batch_processing: BatchProcessingWindowState,
    /// Onboarding overlay for THIS window (its own controller, rendered by `show_content`, so
    /// the overlay shows only while the new-project window is visible). Drives the
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
            ribbon_uniform_width: load_ribbon_uniform_width(),
            ribbon_width_fraction: load_ribbon_width_fraction(),
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
            cut_like_folder_dialog: None,
            save_folder_dialog: None,
            overwrite_confirm: None,
            pending_open_wiki_guide: false,
            batch_window: EmbeddedWindowState::new(),
            batch_processing: BatchProcessingWindowState::new(),
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
    /// `show_content` after `sync` drains the command queue, so `self.tutorial` is no
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

    /// Polls every worker of the window. The owner calls it on every pass the window is open,
    /// also while it is covered (by batch processing or the guide) or collapsed, so imports,
    /// downloads and saves keep progressing.
    pub fn poll(&mut self, ctx: &egui::Context) {
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
        self.poll_cut_like_folder_dialog();
        self.poll_save_folder_dialog();
        self.poll_waifu2x(ctx);
        self.poll_reline(ctx);
        self.poll_reline_model_catalog(ctx);
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
    }

    /// Draws the window content into the `Ui` the owner's `EmbeddedWindow` hands out, plus
    /// the window's own dialogs (crop editor, auto-review, screen-capture overlay, overwrite
    /// confirmation) and its tutorial overlay. The owner runs it under the plain base style
    /// (`content_style`), which also covers those `ctx`-level dialogs.
    pub fn show_content(&mut self, ui: &mut egui::Ui) {
        let ctx_owned = ui.ctx().clone();
        let ctx = &ctx_owned;

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

        ui.painter().rect_filled(ui.max_rect(), 0.0, WINDOW_FILL);
        Frame::new().inner_margin(egui::Margin::same(18)).show(ui, |ui| self.show_contents(ui));
        self.show_crop_editor_window(ctx);
        self.show_advanced_auto_review_window(ctx);
        self.show_screen_capture_overlay(ctx);
        self.show_overwrite_confirm(ctx);
        // Overlay last so it draws on top within THIS window.
        #[cfg(feature = "tutorial")]
        self.tutorial.render(ctx);
    }

    /// Shows the windows opened FROM this one (batch processing) for this pass; the owner runs
    /// it as the `children` of the New Project `EmbeddedWindow`, so it runs also while New
    /// Project is covered or collapsed. `test_mode` (`--test-launcher`) picks the app id the
    /// separate batch window carries.
    pub fn show_children(&mut self, ui: &mut egui::Ui, test_mode: bool) {
        let builder = crate::app::with_window_identity(
            egui::ViewportBuilder::default().with_inner_size([1200.0, 800.0]).with_min_inner_size([900.0, 600.0]).with_resizable(true),
            test_mode,
        );
        let batch = &mut self.batch_processing;
        let response = EmbeddedWindow::new(BATCH_PROCESSING_WINDOW_ID, t!("launcher.batch.window_title"), &mut self.batch_window)
            .viewport(builder)
            .collapsible(true)
            .content_style(Arc::new(ms_theme::base_style()))
            .show(ui, |ui| batch.show_content(ui), |_| {});
        let close_clicked = response.inner.is_some_and(|outcome| outcome.close_clicked);
        if close_clicked || response.close_requested {
            close_batch_window(&mut self.batch_window, &mut self.batch_processing);
        }
    }

    /// Releases what belongs to one opening of the window. The owner calls it when the user
    /// closed the window; it also closes batch processing, a child of this window.
    pub fn on_window_closed(&mut self) {
        self.handle_window_closed();
        close_batch_window(&mut self.batch_window, &mut self.batch_processing);
        // Re-arm the open edge so re-opening the window can autoplay again.
        #[cfg(feature = "tutorial")]
        {
            self.tutorial_first_frame = true;
        }
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

    fn show_contents(&mut self, ui: &mut Ui) {
        ui.columns(2, |columns| {
            columns[0].set_width(LEFT_PANEL_WIDTH);
            self.show_left_panel(&mut columns[0]);
            self.show_viewer_panel(&mut columns[1]);
        });
    }
}

struct ProgressDisplay {
    fraction: f32,
    label: String,
}
