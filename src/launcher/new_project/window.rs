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
    Slider, Stroke, TextEdit, TextureOptions, Ui, ViewportClass, Window, WindowLevel,
};
use image::{DynamicImage, RgbaImage};
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config;
use crate::launcher::new_project::advanced_download::{
    AdvancedDownloadController, AdvancedDownloadEvent, advanced_downloader_version_warning_message,
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
    QuickDownloadController, QuickDownloadEvent, SUPPORTED_SITES_TOOLTIP,
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
use crate::launcher::state::OpenProjectSelection;
use crate::paste_image;
use crate::screen_capture::{self, ScreenRect};
use crate::widgets::EditableComboBox;

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
const TEST_CHAPTER_SITE_CHECK_TIMEOUT: Duration = Duration::from_secs(12);

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
            Self::ImportDownload => "Импорт/выкачка",
            Self::StitchCut => "Сшивание и нарезка",
            Self::Denoise => "Удаление шума",
            Self::Save => "Сохранение",
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum AdvancedDownloadMode {
    PatternLinkSearch,
    CanvasDownload,
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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Clone, Copy)]
struct ScrollArrowMarkerStyle {
    arrow_width: f32,
    arrow_height: f32,
    fill: egui::Color32,
    stroke: Stroke,
    tail_length: f32,
    tail_width: f32,
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
    selected_browser: usize,
    selected_site: usize,
    browser_names: Vec<String>,
    site_presets: Vec<(String, String)>,
    advanced_mode: AdvancedDownloadMode,
    advanced_link_collect_active: bool,
    advanced_link_collect_found_links: usize,
    advanced_link_collect_last_poll_at: Instant,
    advanced_intercept_active: bool,
    advanced_intercept_found_pages: usize,
    advanced_intercept_last_poll_at: Instant,
    advanced_downloader_version_warning_open: bool,
    advanced_downloader_version_warning_dismissed: bool,
    advanced_downloader_version_warning_message: String,
    site_name: String,
    image_prefix: String,
    advanced_fetch_parallelism: usize,
    stitch_parts: String,
    stitch_target_height: String,
    stitch_band_rows: String,
    stitch_tolerance: String,
    stitch_search_radius: String,
    stitch_prefer_up: bool,
    cut_as_chapter_enabled: bool,
    cut_title: usize,
    cut_chapter: usize,
    waifu_noise: usize,
    waifu_scale: usize,
    waifu_tile_size: String,
    save_title: usize,
    save_title_input: String,
    save_title_combo: EditableComboBox,
    save_chapter: String,
    save_mode: SaveMode,
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
}

impl NewProjectWindowState {
    pub fn new(projects_root: PathBuf) -> Self {
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
            selected_browser: 0,
            selected_site,
            browser_names,
            site_presets,
            advanced_mode: AdvancedDownloadMode::PatternLinkSearch,
            advanced_link_collect_active: false,
            advanced_link_collect_found_links: 0,
            advanced_link_collect_last_poll_at: Instant::now(),
            advanced_intercept_active: false,
            advanced_intercept_found_pages: 0,
            advanced_intercept_last_poll_at: Instant::now(),
            advanced_downloader_version_warning_open: false,
            advanced_downloader_version_warning_dismissed: false,
            advanced_downloader_version_warning_message: String::new(),
            site_name: String::new(),
            image_prefix: default_prefix,
            advanced_fetch_parallelism: 4,
            stitch_parts: String::new(),
            stitch_target_height: "19000".to_string(),
            stitch_band_rows: "4".to_string(),
            stitch_tolerance: "15".to_string(),
            stitch_search_radius: "5500".to_string(),
            stitch_prefer_up: true,
            cut_as_chapter_enabled: false,
            cut_title: 0,
            cut_chapter: 0,
            waifu_noise: 4,
            waifu_scale: 0,
            waifu_tile_size: "384".to_string(),
            save_title: 0,
            save_title_input: "Title A".to_string(),
            save_title_combo: EditableComboBox::new("launcher_new_project_save_title")
                .with_hint_text("Выберите тайтл или введите свой"),
            save_chapter: String::new(),
            save_mode: SaveMode::ProjectBase,
            alt_title: 0,
            alt_chapter: 0,
            alt_name: String::new(),
            project_catalog_error: None,
            import_status: "Изображения ещё не загружены".to_string(),
            last_error: None,
            source_import: SourceImportController::new(),
            project_catalog: ProjectCatalogController::new(projects_root.clone()),
            advanced_download,
            quick_download: QuickDownloadController::new(),
            stitch: StitchController::new(),
            save: ProjectSaveController::new(projects_root),
            waifu2x: Waifu2xController::new(),
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
        };
        state.project_catalog.refresh();
        state
    }

    pub fn show(&mut self, ctx: &egui::Context, viewport_class: ViewportClass) -> bool {
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
        let keep_open = match viewport_class {
            ViewportClass::Embedded => self.show_embedded(ctx),
            _ => self.show_native(ctx),
        };
        self.show_crop_editor_window(ctx);
        self.show_advanced_downloader_version_warning(ctx);
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
            || self.clipboard_paste_in_flight
            || self.screen_capture_overlay.is_some()
            || self.screen_capture_pending.is_some()
            || self.screen_capture_in_flight
            || self.crop_editor.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        if !keep_open {
            self.handle_window_closed();
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

    fn show_native(&mut self, ctx: &egui::Context) -> bool {
        if ctx.input(|input| input.viewport().close_requested()) {
            return false;
        }
        ctx.set_style(standard_dark_style());

        CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(24, 24, 27))
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| {
                self.show_contents(ui, false);
            });
        true
    }

    fn show_embedded(&mut self, ctx: &egui::Context) -> bool {
        let mut keep_open = true;
        Window::new("Новый проект")
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
            || self.clipboard_paste_in_flight
            || self.screen_capture_in_flight;
        ui.set_width(LEFT_PANEL_WIDTH);
        ui.vertical(|ui| {
            ui.label(RichText::new("Общий прогресс").small());
            ui.label(RichText::new(&self.import_status).small().weak());
            let global_progress = self.current_progress(any_loading);
            ui.horizontal(|ui| {
                ui.add(
                    ProgressBar::new(global_progress.fraction)
                        .animate(any_loading)
                        .desired_width((LEFT_PANEL_WIDTH - 86.0).max(180.0))
                        .text(global_progress.label),
                );
                if button_sized(ui, "Гайд", egui::vec2(64.0, 22.0), true).clicked() {
                    self.pending_open_wiki_guide = true;
                }
            });
            if button_sized(
                ui,
                "Массовая обработка",
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
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.left_panel_mode,
                LeftPanelMode::Simple,
                "Простой режим",
            );
            ui.selectable_value(
                &mut self.left_panel_mode,
                LeftPanelMode::Full,
                "Полная панель",
            );
        });
    }

    fn show_full_panel(&mut self, ui: &mut Ui) {
        ScrollArea::vertical()
            .id_salt("launcher_new_project_left_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                section_group(ui, "Импорт", |ui| self.show_import_section(ui));
                ui.add_space(SECTION_SPACING);
                section_group(ui, "Быстрый выкачиватель", |ui| {
                    self.show_quick_downloader(ui)
                });
                ui.add_space(SECTION_SPACING);
                section_group(ui, "Продвинутый выкачиватель", |ui| {
                    self.show_advanced_downloader(ui)
                });
                ui.add_space(SECTION_SPACING);
                section_group(ui, "Сшивание / Нарезка", |ui| {
                    self.show_stitch_section(ui)
                });
                ui.add_space(SECTION_SPACING);
                section_group(ui, "waifu2x", |ui| self.show_waifu_section(ui));
                ui.add_space(SECTION_SPACING);
                section_group(ui, "Сохранение", |ui| self.show_save_section(ui));
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
            &format!(
                "Шаг {}. {}",
                self.simple_mode_step.number(),
                self.simple_mode_step.title()
            ),
            |ui| {
                ui.label(
                    RichText::new(
                        "Перед тем, как открыть основную студию, нужно скачать предварительно обработать главу. Выполните эти этапы.",
                    )
                    .color(egui::Color32::WHITE)
                    .strong(),
                );
                ui.add_space(SECTION_SPACING);

                if self.simple_import_show_advanced {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(
                                "Продвинутый выкачиватель открыт внутри простого режима.",
                            )
                            .small()
                            .weak(),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if button_sized(
                                ui,
                                "Вернуться к вариантам",
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

                sub_group(ui, "Откуда вы берёте главу?", |ui| {
                    ui.label(
                        RichText::new("Уже скачанный архив, папка, или одна картинка:").strong(),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if button_sized(
                            ui,
                            "Открыть архив/картинку",
                            egui::vec2(198.0, 34.0),
                            file_button_enabled,
                        )
                        .clicked()
                        {
                            self.open_file_dialog();
                        }
                        if button_sized(
                            ui,
                            "Открыть папку",
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
                        "Вставить из буфера",
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
                    "Скачанная веб-страница (Ctrl+S в браузере)",
                    |ui| {
                        ui.horizontal(|ui| {
                            if button_sized(
                                ui,
                                "Открыть папку страницы",
                                egui::vec2(198.0, 34.0),
                                folder_button_enabled,
                            )
                            .clicked()
                            {
                                self.open_folder_dialog();
                            }
                            if button_sized(
                                ui,
                                "Открыть HTML страницы",
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
                    "Глава бесплатная и на одном из следующих сайтов:",
                    |ui| {
                        egui::CollapsingHeader::new("Поддерживаемые сайты")
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
                                .hint_text("Вставьте ссылку на главу"),
                        );
                        ui.add_space(8.0);
                        if button_sized(
                            ui,
                            "Скачать",
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
                    "Не планировал переводить и просто хочешь поигратся с программой?",
                    |ui| {
                        ui.label(
                            RichText::new(
                                "Можно скачать тестовую главу. Убедитесь, что у вас работает сайт comic.naver.com.",
                            )
                            .small()
                            .weak(),
                        );
                        ui.add_space(8.0);
                        if button_sized(
                            ui,
                            "Скачать тестовую главу",
                            egui::vec2(220.0, 34.0),
                            quick_download_enabled,
                        )
                        .clicked()
                        {
                            self.start_test_chapter_download();
                        }
                    },
                );

                ui.add_space(SECTION_SPACING);
                sub_group(
                    ui,
                    "Все предыдущие методы не сработали",
                    |ui| {
                        ui.label(
                            RichText::new("Будьте готовы порыться в HTML коде сайта.")
                                .small()
                                .weak(),
                        );
                        ui.add_space(8.0);
                        if button_sized(
                            ui,
                            "Открыть продвинутый выкачиватель",
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
            &format!(
                "Шаг {}. {}",
                self.simple_mode_step.number(),
                self.simple_mode_step.title()
            ),
            |ui| {
                ui.label(
                    "У вас вертикальный комикс (манхва/вебтун), или страничный (манга, классический западный)? Если страничный, то сшивание не нужно, переходите к следующему этапу.",
                );
                ui.add_space(8.0);
                if button_sized(ui, "Пропустить сшивание", egui::vec2(220.0, 34.0), true).clicked()
                {
                    self.simple_stitch_done = true;
                    self.simple_manual_cut_preview_active = false;
                    self.simple_mode_step = SimpleModeStep::Denoise;
                }

                ui.add_space(SECTION_SPACING * 1.6);
                ui.label(RichText::new("Если это вебтун, то нарежьте его:").strong());
                ui.add_space(8.0);
                if button_sized(
                    ui,
                    "Сшить и нарезать автоматически",
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
                    "Сшить и посмотреть места резки",
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
                            "Автоматические разрезы расставлены. Посмотрите каждое место, отмеченное красной стрелочкой, и убедитесь, что разрезы будут в удобных местах. Можете добавить дополнительные разрезы в меню ПКМ. В конце нажмите большую красную кнопку Нарезать вверху ленты",
                        )
                        .color(egui::Color32::WHITE)
                        .strong(),
                    );
                }

                if self.simple_stitch_done {
                    ui.add_space(SECTION_SPACING);
                    ui.label(
                        RichText::new(
                            "Лишние разрезы можно сшить назад, выбрав в меню ПКМ опцию сшивания текущей страницы со следующей и предыдущей",
                        )
                        .color(egui::Color32::WHITE)
                        .strong(),
                    );
                    ui.add_space(10.0);
                    if button_sized(ui, "Далее", egui::vec2(124.0, 34.0), true).clicked() {
                        self.simple_mode_step = SimpleModeStep::Denoise;
                    }
                }
            },
        );
    }

    fn show_simple_denoise_step(&mut self, ui: &mut Ui) {
        section_group(
            ui,
            &format!(
                "Шаг {}. {}",
                self.simple_mode_step.number(),
                self.simple_mode_step.title()
            ),
            |ui| {
                ui.label(
                    RichText::new(
                        "Это не обязательно. Может помочь, если глава с пиратского сайта и в плохом качестве, но может немного подпортить мелкие текстуры на чистой главе",
                    )
                    .color(egui::Color32::WHITE)
                    .strong(),
                );
                self.show_operation_progress(ui, "waifu2x");

                ui.add_space(SECTION_SPACING * 1.6);
                if button_sized(
                    ui,
                    "Удалить шум через waifu2x",
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    self.can_start_waifu2x(),
                )
                .clicked()
                {
                    self.start_waifu2x();
                    self.simple_mode_step = SimpleModeStep::Save;
                }
                if button_sized(ui, "пропустить", egui::vec2(140.0, 34.0), true).clicked()
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
            &format!(
                "Шаг {}. {}",
                self.simple_mode_step.number(),
                self.simple_mode_step.title()
            ),
            |ui| {
                let mut save_to_project = self.save_mode != SaveMode::Independent;
                if ui
                    .checkbox(&mut save_to_project, "Сохранить в проекты ManhwaStudio")
                    .changed()
                    && save_to_project
                {
                    self.save_mode = SaveMode::ProjectBase;
                }

                let mut save_to_folder = self.save_mode == SaveMode::Independent;
                if ui
                    .checkbox(&mut save_to_folder, "Сохранить в любую другую папку")
                    .changed()
                    && save_to_folder
                {
                    self.save_mode = SaveMode::Independent;
                }
                ui.label(
                    RichText::new("Если просто решили использовать эту программу для выкачки")
                        .small()
                        .weak(),
                );

                ui.add_space(SECTION_SPACING);
                if self.project_catalog.is_loading() {
                    ui.label(
                        RichText::new("Обновляем список тайтлов и глав...")
                            .small()
                            .weak(),
                    );
                } else if let Some(error) = &self.project_catalog_error {
                    ui.colored_label(egui::Color32::from_rgb(255, 120, 120), error);
                }

                if self.save_mode == SaveMode::Independent {
                    if button_sized(
                        ui,
                        "Выбрать папку для сохранения",
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

                ui.label("Введите название своего тайтла или выберите из существующих:");
                let save_title_response =
                    self.save_title_combo
                        .draw(ui, &mut self.save_title_input, titles.as_slice());
                if save_title_response.changed {
                    self.sync_save_title_from_input();
                }
                if button_sized(ui, "Обновить", SMALL_BUTTON_SIZE, can_refresh_catalog).clicked()
                {
                    self.refresh_project_catalog();
                }

                ui.add_space(8.0);
                ui.label("Введите номер главы или её название:");
                ui.add(TextEdit::singleline(&mut self.save_chapter).desired_width(fill_width(ui)));

                ui.add_space(SECTION_SPACING);
                if button_sized(
                    ui,
                    "Сохранить главу и открыть в основной студии",
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    can_save,
                )
                .clicked()
                {
                    self.start_save_to_project(true);
                }
                if button_sized(
                    ui,
                    "Сохранить главу и скачать ещё одну",
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
                        "Главы хранятся в ваших документах в папке manhwastudio_projects, там папки с тайтлами, в тайтлах главы. Исходники найдете в папке главы в src",
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
            "Далее"
        } else {
            "Вперед"
        };
        ui.horizontal(|ui| {
            if button_sized(
                ui,
                "Назад",
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
            if button_sized(
                ui,
                "Открыть папку...",
                ACTION_BUTTON_SIZE,
                !self.source_import.is_loading(),
            )
            .clicked()
            {
                self.open_folder_dialog();
            }
            if button_sized(
                ui,
                "Открыть файл...",
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
            "Вставить из буфера",
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
            "Выйти из режима захвата"
        } else {
            "Режим захвата"
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
                self.start_screen_capture_mode();
            }
        }
        if !SCREEN_CAPTURE_UI_ENABLED {
            ui.label(
                RichText::new("Режим захвата временно отключён.")
                    .small()
                    .weak(),
            );
        }
        field_label(ui, "Режим импорта:");
        let mut import_mode_index = self.import_mode.as_index();
        combo_index(
            ui,
            "launcher_new_project_import_mode",
            &[
                "Заменить ленту",
                "Добавить в начало",
                "Добавить в конец",
                "Добавить перед текущей страницей",
                "Добавить после текущей страницы",
            ],
            &mut import_mode_index,
        );
        self.import_mode = ImportMode::from_index(import_mode_index);
        ui.checkbox(
            &mut self.filter_same_width,
            "Фильтровать по одинаковой ширине (±50%)",
        );
        field_label(ui, "Доп. имена файлов (маски * и ? поддерживаются)");
        ui.add(
            TextEdit::singleline(&mut self.import_extra_names)
                .desired_width(fill_width(ui))
                .hint_text("resource, scan*.*, page????"),
        );
        self.show_operation_progress(ui, "source");
    }

    fn show_quick_downloader(&mut self, ui: &mut Ui) {
        field_label(ui, "Ссылка на главу");
        ui.add(
            TextEdit::singleline(&mut self.quick_link)
                .desired_width(fill_width(ui))
                .hint_text("Вставьте ссылку на главу, если сайт поддерживается"),
        );
        ui.add_space(6.0);
        let can_start_download =
            !self.source_import.is_loading() && !self.quick_download.is_loading();
        let response = button_sized(
            ui,
            "Загрузить главы из ссылки",
            egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
            can_start_download,
        );
        let response = response.on_hover_text(SUPPORTED_SITES_TOOLTIP);
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
        self.import_status = "Чтение изображения из буфера обмена...".to_string();
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
                                "Буфер обмена вернул изображение в неподдерживаемом размере."
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
                error: Some("Поток чтения буфера обмена был прерван.".to_string()),
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
            self.import_status = "Изображение вставлено из буфера обмена.".to_string();
            self.last_error = None;
            self.advance_simple_import_step_after_success();
            crate::runtime_log::log_info("[new-project] ribbon image pasted from clipboard");
        } else if let Some(error) = result.error {
            self.import_status = "Не удалось вставить изображение из буфера обмена.".to_string();
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

    fn start_screen_capture_mode(&mut self) {
        match screen_capture::query_virtual_desktop_bounds() {
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
                    "Режим захвата активен. Переместите рамку и нажмите S.".to_string();
                self.last_error = None;
                crate::runtime_log::log_info(format!(
                    "[new-project] screen capture mode enabled for desktop {}x{} at {},{}",
                    desktop_bounds.width, desktop_bounds.height, desktop_bounds.x, desktop_bounds.y
                ));
            }
            Err(error) => {
                self.import_status = "Не удалось включить режим захвата.".to_string();
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
        self.import_status = "Режим захвата выключен.".to_string();
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
            "Подготовка снимка: временно скрываю рамку и снимаю экран...".to_string();
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
        self.import_status = "Снимаю выделенную область экрана...".to_string();

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
                error: Some("Поток захвата экрана был прерван.".to_string()),
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
            self.import_status = "Снимок области экрана добавлен в ленту.".to_string();
            self.last_error = None;
            self.advance_simple_import_step_after_success();
            crate::runtime_log::log_info("[new-project] screen capture inserted into ribbon");
        } else if let Some(error) = result.error {
            self.import_status = "Не удалось снять выделенную область экрана.".to_string();
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
                .with_title("Режим захвата")
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

        ctx.show_viewport_immediate(viewport_id, builder, |ctx, _class| {
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
                .show(ctx, |ui| {
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
        field_label(ui, "Ссылка на страницу");
        ui.add(
            TextEdit::singleline(&mut self.advanced_page_url)
                .desired_width(fill_width(ui))
                .hint_text("Откройте страницу главы в выбранном браузере"),
        );

        field_label(ui, "Браузер");
        self.clamp_advanced_indexes();
        if self.browser_names.is_empty() {
            ui.label(
                RichText::new("Поддерживаемые браузеры не найдены на этой системе.")
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
            && !self.browser_names.is_empty()
            && !self.advanced_page_url.trim().is_empty();
        if button_sized(
            ui,
            "Открыть в браузере",
            egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
            can_open_browser,
        )
        .clicked()
        {
            self.start_advanced_open();
        }
        ui.label(
            RichText::new("Убедитесь, что все картинки на сайте прогружены.")
                .small()
                .weak(),
        );

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        field_label(ui, "Режим");
        ui.add_enabled_ui(
            !self.advanced_link_collect_active && !self.advanced_intercept_active,
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(
                        &mut self.advanced_mode,
                        AdvancedDownloadMode::PatternLinkSearch,
                        "Поиск ссылок по паттерну",
                    );
                    ui.selectable_value(
                        &mut self.advanced_mode,
                        AdvancedDownloadMode::CanvasDownload,
                        "Скачивание Canvas со страницы",
                    );
                });
            },
        );
        ui.add_space(8.0);

        if self.advanced_mode == AdvancedDownloadMode::PatternLinkSearch {
            field_label(
                ui,
                "Сайт (пресет) / префиксы ссылок (* — любая последовательность, ? — символ)",
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

            field_label(ui, "Префикс");
            ui.add(TextEdit::singleline(&mut self.image_prefix).desired_width(fill_width(ui)));

            field_label(ui, "Потоков выкачки");
            ui.add_enabled(
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active,
                Slider::new(&mut self.advanced_fetch_parallelism, 1..=8).text("потоков"),
            );

            field_label(ui, "Название нового сайта");
            ui.add(
                TextEdit::singleline(&mut self.site_name)
                    .desired_width(fill_width(ui))
                    .hint_text("название для сохранения"),
            );

            if button_sized(
                ui,
                "Сохранить префикс",
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active,
            )
            .clicked()
            {
                self.save_advanced_prefix();
            }
            if self.advanced_link_collect_active {
                ui.label(
                    RichText::new(format!(
                        "Собрано ссылок: {}",
                        self.advanced_link_collect_found_links
                    ))
                    .color(egui::Color32::from_rgb(76, 175, 80))
                    .strong(),
                );
                ui.add_space(8.0);
            }
            if button_sized(
                ui,
                "Скачать сразу",
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active
                    && !self.browser_names.is_empty(),
            )
            .clicked()
            {
                self.start_advanced_fetch();
            }
            if button_sized(
                ui,
                "Начать сбор ссылок",
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active
                    && !self.browser_names.is_empty(),
            )
            .clicked()
            {
                self.start_advanced_link_collect();
            }
            if button_sized(
                ui,
                "Остановить сбор ссылок",
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && self.advanced_link_collect_active
                    && !self.browser_names.is_empty(),
            )
            .clicked()
            {
                self.finish_advanced_link_collect();
            }
        } else {
            if self.advanced_intercept_active {
                ui.label(
                    RichText::new(format!(
                        "Найдено страниц: {}",
                        self.advanced_intercept_found_pages
                    ))
                    .color(egui::Color32::from_rgb(76, 175, 80))
                    .strong(),
                );
                ui.add_space(8.0);
            }
            if button_sized(
                ui,
                "Скачать сразу",
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active
                    && !self.browser_names.is_empty(),
            )
            .clicked()
            {
                self.start_advanced_canvas_fetch();
            }
            if button_sized(
                ui,
                "Начать перехват",
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active
                    && !self.browser_names.is_empty(),
            )
            .clicked()
            {
                self.start_advanced_canvas_intercept();
            }
            if button_sized(
                ui,
                "Завершить перехват",
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && self.advanced_intercept_active
                    && !self.browser_names.is_empty(),
            )
            .clicked()
            {
                self.finish_advanced_canvas_intercept();
            }
        }
        self.show_operation_progress(ui, "advanced_download");
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

        field_label(ui, "K (кол-во частей, пусто = авто)");
        ui.add(
            TextEdit::singleline(&mut self.stitch_parts)
                .desired_width(160.0)
                .hint_text("пусто = авто"),
        );

        field_label(ui, "Hmax (лимит высоты, px)");
        ui.add(TextEdit::singleline(&mut self.stitch_target_height).desired_width(160.0));

        field_label(ui, "Белая полоса: band_rows");
        ui.add(TextEdit::singleline(&mut self.stitch_band_rows).desired_width(160.0));

        field_label(ui, "tol (допуск одноцветности)");
        ui.add(TextEdit::singleline(&mut self.stitch_tolerance).desired_width(160.0));

        field_label(ui, "search_radius (px)");
        ui.add(TextEdit::singleline(&mut self.stitch_search_radius).desired_width(160.0));

        ui.checkbox(&mut self.stitch_prefer_up, "Сначала вверх при refine");

        let wide_button = egui::vec2(LEFT_PANEL_WIDTH - 52.0, 32.0);
        if button_sized(ui, "Сшить ленту", wide_button, can_start).clicked() {
            self.start_stitch_split(StitchSplitMode::StitchOnly);
        }
        if button_sized(ui, "Сшить и проставить линии резки", wide_button, can_start).clicked()
        {
            self.start_stitch_split(StitchSplitMode::ManualCutPreview);
        }
        if button_sized(ui, "Сшить и нарезать автоматически", wide_button, can_start).clicked()
        {
            self.start_stitch_split(StitchSplitMode::AutoCut);
        }
        if button_sized(
            ui,
            "Сшить только в неоднородных местах",
            wide_button,
            can_start,
        )
        .clicked()
        {
            self.start_stitch_split(StitchSplitMode::HeterogeneousBottoms);
        }
        if button_sized(ui, "Вернуть исходное", ACTION_BUTTON_SIZE, can_restore).clicked()
        {
            self.restore_original_pages();
        }
        if can_apply_manual
            && button_sized(ui, "Применить ручную нарезку", wide_button, true).clicked()
        {
            self.apply_manual_cut_guides();
        }

        ui.add_space(8.0);
        self.show_operation_progress(ui, "stitch");
        sub_group(ui, "Нарезать как главу", |ui| {
            ui.checkbox(
                &mut self.cut_as_chapter_enabled,
                "Включить нарезку по существующей главе",
            );
            ui.add_enabled_ui(self.cut_as_chapter_enabled, |ui| {
                field_label(ui, "Тайтл");
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
                field_label(ui, "Глава");
                combo_index_owned(
                    ui,
                    "launcher_new_project_cut_chapter",
                    cut_chapters.as_slice(),
                    &mut self.cut_chapter,
                );
                ui.horizontal(|ui| {
                    if button_sized(ui, "Обновить", SMALL_BUTTON_SIZE, can_refresh_catalog)
                        .clicked()
                    {
                        self.refresh_project_catalog();
                    }
                    if button_sized(ui, "Взять эту главу", ACTION_BUTTON_SIZE, can_take_chapter)
                        .clicked()
                    {
                        self.start_cut_like_project_chapter();
                    }
                });
                if button_sized(
                    ui,
                    "Выбрать папку",
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

    fn show_waifu_section(&mut self, ui: &mut Ui) {
        let noise_levels = ["-1", "0", "1", "2", "3"];
        let scale_levels = ["1", "2", "4", "8", "16", "32"];

        field_label(ui, "Бэкенд / путь");
        let mut backend = self.waifu_backend_path_display();
        ui.add_enabled(
            false,
            TextEdit::singleline(&mut backend).desired_width(fill_width(ui)),
        );
        if let Some(reason) = self.waifu2x.unavailable_reason() {
            ui.colored_label(egui::Color32::from_rgb(236, 112, 99), reason);
        }

        field_label(ui, "Шумоподавление -n");
        combo_index(
            ui,
            "launcher_new_project_w2x_noise",
            &noise_levels,
            &mut self.waifu_noise,
        );

        field_label(ui, "Масштаб -s");
        combo_index(
            ui,
            "launcher_new_project_w2x_scale",
            &scale_levels,
            &mut self.waifu_scale,
        );

        field_label(ui, "Tile size -t (>=32, 0=auto)");
        ui.add(TextEdit::singleline(&mut self.waifu_tile_size).desired_width(160.0));

        if button_sized(
            ui,
            "Прогнать через waifu2x",
            egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
            self.can_start_waifu2x(),
        )
        .clicked()
        {
            self.start_waifu2x();
        }
        self.show_operation_progress(ui, "waifu2x");
    }

    fn show_save_section(&mut self, ui: &mut Ui) {
        let titles = self.project_catalog_snapshot.titles.clone();
        let alt_chapters = self.current_alt_chapter_options().to_vec();
        let can_refresh_catalog = !self.project_catalog.is_loading() && !self.save.is_loading();
        let can_save = self.can_start_save();

        ui.vertical(|ui| {
            let mut project_base = self.save_mode == SaveMode::ProjectBase;
            if ui
                .checkbox(&mut project_base, "Сохранить как основу проекта")
                .changed()
                && project_base
            {
                self.save_mode = SaveMode::ProjectBase;
            }

            let mut alt_version = self.save_mode == SaveMode::AltVersion;
            if ui
                .checkbox(&mut alt_version, "Сохранить как альтернативную версию")
                .changed()
                && alt_version
            {
                self.save_mode = SaveMode::AltVersion;
            }

            let mut independent = self.save_mode == SaveMode::Independent;
            if ui
                .checkbox(&mut independent, "Независимое сохранение")
                .changed()
                && independent
            {
                self.save_mode = SaveMode::Independent;
            }
        });
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!(
                "Папка проектов: {}",
                self.project_catalog.projects_root().display()
            ))
            .small()
            .weak(),
        );
        if self.project_catalog.is_loading() {
            ui.label(
                RichText::new("Обновляем список тайтлов и глав...")
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
                "Сохранить как основу проекта",
                |ui| {
                    field_label(ui, "Тайтл");
                    let save_title_response = self.save_title_combo.draw(
                        ui,
                        &mut self.save_title_input,
                        titles.as_slice(),
                    );
                    if save_title_response.changed {
                        self.sync_save_title_from_input();
                    }
                    if button_sized(ui, "Обновить", SMALL_BUTTON_SIZE, can_refresh_catalog)
                        .clicked()
                    {
                        self.refresh_project_catalog();
                    }
                    field_label(ui, "Название главы");
                    ui.add(
                        TextEdit::singleline(&mut self.save_chapter).desired_width(fill_width(ui)),
                    );
                    if button_sized(
                        ui,
                        "Сохранить и открыть",
                        egui::vec2(LEFT_PANEL_WIDTH - 84.0, 32.0),
                        can_save,
                    )
                    .clicked()
                    {
                        self.start_save_to_project(true);
                    }
                    if button_sized(
                        ui,
                        "Сохранить в проект",
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
                "Сохранить как альтернативную версию",
                |ui| {
                    field_label(ui, "Тайтл");
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
                    if button_sized(ui, "Обновить", SMALL_BUTTON_SIZE, can_refresh_catalog)
                        .clicked()
                    {
                        self.refresh_project_catalog();
                    }

                    field_label(ui, "Глава");
                    combo_index_owned(
                        ui,
                        "launcher_new_project_alt_chapter",
                        alt_chapters.as_slice(),
                        &mut self.alt_chapter,
                    );
                    if button_sized(ui, "Обновить", SMALL_BUTTON_SIZE, can_refresh_catalog)
                        .clicked()
                    {
                        self.refresh_project_catalog();
                    }

                    field_label(ui, "Название альтер-версии");
                    ui.add(TextEdit::singleline(&mut self.alt_name).desired_width(fill_width(ui)));
                    if button_sized(
                        ui,
                        "Сохранить как альтер-версию",
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
            sub_group(ui, "Независимое сохранение", |ui| {
                if button_sized(
                    ui,
                    "Сохранить в папку",
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
                        ui.label(RichText::new("Превью страниц").size(20.0).strong());
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new(format!(
                                "Страниц на ленте: {}",
                                self.ribbon.pages().len()
                            ))
                            .small()
                            .weak(),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if self.has_manual_cut_preview() {
                                let button = Button::new(
                                    RichText::new("Нарезать")
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
                                ui.label(RichText::new("Папка не выбрана").small().weak());
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
                let scroll_output = ScrollArea::vertical()
                    .id_salt("launcher_new_project_ribbon_scroll")
                    .auto_shrink([false, false])
                    .max_height((available_height - 8.0).max(220.0))
                    .show(ui, |ui| {
                        if self.source_import.is_loading()
                            || self.advanced_download.is_loading()
                            || self.quick_download.is_loading()
                            || self.test_chapter_check_rx.is_some()
                            || self.stitch.is_loading()
                            || self.save.is_loading()
                            || self.waifu2x.is_loading()
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
                                ui.label(RichText::new("Лента пока пуста").size(18.0));
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new(
                                        "Откройте папку, и окно соберёт простую вертикальную ленту из найденных изображений.",
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
                                        if ui.button("Добавить линию резки").clicked() {
                                            add_manual_cut_at = manual_cut_context_guide;
                                            ui.close();
                                        }
                                        if ui
                                            .add_enabled(
                                                index > 0,
                                                Button::new("Склеить с предыдущей страницей"),
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
                                                Button::new("Склеить со следующей страницей"),
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
                    ui,
                    scroll_output.inner_rect,
                    scroll_output.content_size.y,
                    scroll_output.state.offset.y,
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
                self.import_status = "Страницы склеены.".to_string();
                self.last_error = None;
            }
            Err(RibbonMergeError::WidthMismatch {
                first_name,
                first_width,
                second_name,
                second_width,
            }) => {
                self.last_error = Some(format!(
                    "Нельзя склеить страницы разной ширины: '{first_name}' — {first_width}px, '{second_name}' — {second_width}px."
                ));
                self.import_status = "Склейка страниц отменена.".to_string();
            }
            Err(RibbonMergeError::MissingPage) => {
                self.last_error = Some("Нет соседней страницы для склейки.".to_string());
                self.import_status = "Склейка страниц недоступна.".to_string();
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
                    self.import_status = "Изображение перемещено вверх.".to_string();
                    self.last_error = None;
                }
            }
            RibbonImageControlAction::MoveDown => {
                if self.ribbon.move_page_down(index) {
                    self.swap_manual_cut_guide_pages(index, index + 1);
                    self.selected_ribbon_page = Some(index + 1);
                    self.import_status = "Изображение перемещено вниз.".to_string();
                    self.last_error = None;
                }
            }
            RibbonImageControlAction::Delete => {
                if let Some(removed_page) = self.ribbon.remove_page(index) {
                    self.selected_ribbon_page = self.selection_after_removal(index);
                    self.remove_manual_cut_guide_page(index);
                    self.clamp_manual_cut_guides_to_current_pages();
                    self.import_status =
                        format!("Изображение '{}' удалено из ленты.", removed_page.name);
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
        let shown = Window::new(format!("Обрезка: {}", editor.page_name))
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
                        "Оригинал сохраняется. Рамка задаёт область, которая попадёт в ленту.",
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
                    if ui.button("Сбросить").clicked() {
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
                            .add_sized(egui::vec2(132.0, 32.0), Button::new("Применить"))
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
                self.import_status = format!("Изображение '{page_name}' обрезано.");
                self.last_error = None;
            } else {
                self.last_error = Some("Не удалось применить обрезку.".to_string());
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
            self.last_error = Some(format!(
                "Линия резки должна быть не ближе {MANUAL_CUT_MIN_EDGE_DISTANCE_PX}px к началу или концу картинки."
            ));
            self.import_status = "Линия резки не добавлена.".to_string();
            return;
        };
        self.manual_cut_guides.push(guide);
        self.clamp_manual_cut_guides_to_current_pages();
        self.import_status = format!(
            "Добавлена ручная линия резки. Всего линий: {}.",
            self.manual_cut_guides.len()
        );
        self.last_error = None;
    }

    fn start_stitch_split(&mut self, mode: StitchSplitMode) {
        let Some(options) = self.parse_stitch_options(mode) else {
            return;
        };
        let images = self.current_stitch_images();
        if images.is_empty() {
            self.last_error = Some("Сначала откройте папку или скачайте главу.".to_string());
            self.import_status = "Сшивание недоступно: нет загруженных изображений.".to_string();
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
            self.import_status = "Исходные изображения восстановлены.".to_string();
            self.last_error = None;
        } else {
            self.last_error = Some("Исходные изображения ещё не были загружены.".to_string());
        }
    }

    fn apply_manual_cut_guides(&mut self) {
        if !self.has_manual_cut_preview() || self.stitch.is_loading() {
            return;
        }
        let images = self.current_stitch_images();
        if images.is_empty() {
            self.last_error = Some("Для ручной нарезки нужна хотя бы одна страница.".to_string());
            return;
        }
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "stitch",
            stage: "split".to_string(),
            current: 0,
            total: self.manual_cut_guides.len().saturating_add(1).max(1),
        });
        self.import_status = "Ручная нарезка ленты...".to_string();
        self.stitch.begin(StitchRequest::ApplyManualCutsToPages {
            images,
            cut_guides: self.manual_cut_guides.clone(),
        });
    }

    fn start_cut_like_project_chapter(&mut self) {
        let Some(title) = self.current_cut_title_name().map(str::to_string) else {
            self.last_error = Some("Выберите тайтл для примера главы.".to_string());
            self.import_status = "Нарезка по примеру главы недоступна.".to_string();
            return;
        };
        let Some(chapter) = self.current_cut_chapter_name().map(str::to_string) else {
            self.last_error = Some("Выберите главу для примера нарезки.".to_string());
            self.import_status = "Нарезка по примеру главы недоступна.".to_string();
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
                self.last_error = Some("У выбранной главы не найдена папка src/scr.".to_string());
                self.import_status = "Нарезка по примеру главы недоступна.".to_string();
                return;
            }
        };

        self.start_cut_like_reference(
            reference_dir,
            &format!("Нарезаем по примеру {title}/{chapter}..."),
        );
    }

    fn start_cut_like_folder(&mut self) {
        let Some(folder) = FileDialog::new().pick_folder() else {
            return;
        };
        self.start_cut_like_reference(
            folder.clone(),
            &format!("Нарезаем по примеру папки '{}'...", folder.display()),
        );
    }

    fn start_cut_like_reference(&mut self, reference_dir: PathBuf, status_message: &str) {
        let images = self.current_stitch_images();
        if images.is_empty() {
            self.last_error = Some("Сначала откройте папку или скачайте главу.".to_string());
            self.import_status = "Нарезка по примеру главы недоступна: лента пуста.".to_string();
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
                        Some("K должно быть положительным целым числом или пустым.".to_string());
                    self.import_status = "Некорректные параметры склейки.".to_string();
                    return None;
                }
            }
        };

        let parse_positive = |value: &str, name: &str| -> Result<usize, String> {
            match value.trim().parse::<usize>() {
                Ok(parsed) if parsed > 0 => Ok(parsed),
                _ => Err(format!("{name} должно быть больше нуля.")),
            }
        };

        let target_height = match parse_positive(&self.stitch_target_height, "Hmax") {
            Ok(value) => value,
            Err(message) => {
                self.last_error = Some(message);
                self.import_status = "Некорректные параметры склейки.".to_string();
                return None;
            }
        };
        let band_rows = match parse_positive(&self.stitch_band_rows, "band_rows") {
            Ok(value) => value,
            Err(message) => {
                self.last_error = Some(message);
                self.import_status = "Некорректные параметры склейки.".to_string();
                return None;
            }
        };
        let search_radius = match parse_positive(&self.stitch_search_radius, "search_radius") {
            Ok(value) => value,
            Err(message) => {
                self.last_error = Some(message);
                self.import_status = "Некорректные параметры склейки.".to_string();
                return None;
            }
        };
        let tolerance = match self.stitch_tolerance.trim().parse::<u8>() {
            Ok(value) if value > 0 => value,
            _ => {
                self.last_error = Some("tol должен быть целым числом больше нуля.".to_string());
                self.import_status = "Некорректные параметры склейки.".to_string();
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
            && !self.ribbon.pages().is_empty()
    }

    fn start_save_to_project(&mut self, open_after_save: bool) -> bool {
        let title = self.save_title_input.trim().to_string();
        let chapter = self.save_chapter.trim().to_string();
        if title.is_empty() || chapter.is_empty() {
            self.last_error = Some("Укажите тайтл и название главы.".to_string());
            self.import_status = "Сохранение в проект недоступно.".to_string();
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
                    Some("Не удалось проверить папку проекта перед сохранением.".to_string());
                self.import_status = "Сохранение в проект завершилось с ошибкой.".to_string();
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
            "Сохраняем главу в проект...",
        );
        true
    }

    fn start_save_alt_version(&mut self) {
        let Some(title) = self.current_alt_title_name().map(str::to_string) else {
            self.last_error = Some("Выберите тайтл для альтер-версии.".to_string());
            self.import_status = "Сохранение альтер-версии недоступно.".to_string();
            return;
        };
        let Some(chapter) = self.current_alt_chapter_name().map(str::to_string) else {
            self.last_error = Some("Выберите главу для альтер-версии.".to_string());
            self.import_status = "Сохранение альтер-версии недоступно.".to_string();
            return;
        };
        let alt_name = self.alt_name.trim().to_string();
        if alt_name.is_empty() {
            self.last_error = Some("Укажите название альтер-версии.".to_string());
            self.import_status = "Сохранение альтер-версии недоступно.".to_string();
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
                    Some("Не удалось проверить папку альтер-версии перед сохранением.".to_string());
                self.import_status = "Сохранение альтер-версии завершилось с ошибкой.".to_string();
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
            "Сохраняем альтер-версию...",
        );
    }

    fn start_save_to_folder(&mut self) {
        let Some(folder) = FileDialog::new().pick_folder() else {
            return;
        };
        let should_continue = match confirm_overwrite_nonempty(&folder) {
            Ok(value) => value,
            Err(err) => {
                self.last_error =
                    Some("Не удалось проверить выбранную папку перед сохранением.".to_string());
                self.import_status = "Сохранение в папку завершилось с ошибкой.".to_string();
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
            &format!("Сохраняем изображения в '{}'...", folder.display()),
        );
    }

    fn begin_save(
        &mut self,
        request: ProjectSaveRequest,
        open_after_save: bool,
        status_message: &str,
    ) {
        if request.images.is_empty() {
            self.last_error = Some("На холсте нет изображений для сохранения.".to_string());
            self.import_status = "Сохранение недоступно: лента пуста.".to_string();
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
                self.import_status = format!(
                    "Линия резки удалена. Осталось линий: {}.",
                    self.manual_cut_guides.len()
                );
                continue;
            }
            index += 1;
        }
        screen_positions
    }

    fn draw_manual_cut_scroll_markers(
        &self,
        ui: &Ui,
        scroll_inner_rect: egui::Rect,
        content_height: f32,
        scroll_offset_y: f32,
        cut_screen_positions: &[f32],
        page_boundary_screen_positions: &[f32],
    ) {
        if content_height <= 1.0 {
            return;
        }
        draw_scroll_arrow_markers(
            ui,
            scroll_inner_rect,
            content_height,
            scroll_offset_y,
            page_boundary_screen_positions,
            ScrollArrowMarkerStyle {
                arrow_width: PAGE_BOUNDARY_SCROLL_ARROW_WIDTH,
                arrow_height: PAGE_BOUNDARY_SCROLL_ARROW_HEIGHT,
                fill: egui::Color32::from_rgb(40, 132, 255),
                stroke: Stroke::new(1.0, egui::Color32::from_rgb(130, 190, 255)),
                tail_length: 10.0 / 3.0,
                tail_width: 4.0 / 3.0,
            },
        );
        draw_scroll_arrow_markers(
            ui,
            scroll_inner_rect,
            content_height,
            scroll_offset_y,
            cut_screen_positions,
            ScrollArrowMarkerStyle {
                arrow_width: MANUAL_CUT_SCROLL_ARROW_WIDTH,
                arrow_height: MANUAL_CUT_SCROLL_ARROW_HEIGHT,
                fill: egui::Color32::from_rgb(255, 0, 0),
                stroke: Stroke::new(2.0, egui::Color32::from_rgb(255, 86, 86)),
                tail_length: 10.0,
                tail_width: 4.0,
            },
        );
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
            self.import_status = "Сканирование папки...".to_string();
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
            self.import_status = "Открытие файла...".to_string();
        }
    }

    fn start_quick_download(&mut self) {
        let url = self.quick_link.trim();
        if url.is_empty() {
            self.last_error = Some("Вставьте ссылку на главу перед загрузкой.".to_string());
            self.import_status = "Быстрый выкачиватель ждёт ссылку.".to_string();
            return;
        }
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "quick_download",
            stage: "download".to_string(),
            current: 0,
            total: 1,
        });
        self.import_status = "Подготовка быстрого выкачивания...".to_string();
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
        self.import_status = "Проверяем доступность comic.naver.com...".to_string();
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
            self.last_error = Some("Сначала откройте или скачайте изображения.".to_string());
            self.import_status = "waifu2x недоступен: лента пуста.".to_string();
            return;
        }
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "waifu2x",
            stage: "prepare".to_string(),
            current: 0,
            total: images.len(),
        });
        self.import_status = "Подготавливаем waifu2x runtime...".to_string();
        self.waifu2x.begin(images, options);
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
                        format!(
                            "Загружено {} изображений из {}. Пропущено: {}, отфильтровано: {}. Медиана ширины: {} px, диапазон: [{}; {}].",
                            result.imported_images,
                            result.source_path.display(),
                            result.skipped_files,
                            result.filtered_out,
                            median,
                            min_width,
                            max_width,
                        )
                    } else {
                        format!(
                            "Загружено {} изображений из {}. Пропущено: {}, отфильтровано: {}.",
                            result.imported_images,
                            result.source_path.display(),
                            result.skipped_files,
                            result.filtered_out,
                        )
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
                    self.import_status = "Не удалось загрузить источник".to_string();
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
                    self.import_status = "Не удалось загрузить источник".to_string();
                    self.last_error = Some(
                        "Не удалось загрузить источник. Фоновая задача завершилась с ошибкой."
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
                        "Фоновый поток чтения списка проектов неожиданно завершился.".to_string(),
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
        self.import_status = "Тестовую главу скачать не удалось.".to_string();
        self.last_error = Some(
            "Сайт comic.naver.com недоступен. Попробуйте включить VPN если вы из России"
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
                    self.import_status = format!(
                        "Быстрый выкачиватель загрузил {} изображений из {}.",
                        result.downloaded_images, result.source_url,
                    );
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
                    self.import_status = "Быстрый выкачиватель завершился с ошибкой.".to_string();
                    self.last_error = Some(user_message);
                }
                QuickDownloadEvent::WorkerDisconnected => {
                    crate::runtime_log::log_error(
                        "[new-project] quick download worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status = "Быстрый выкачиватель завершился с ошибкой.".to_string();
                    self.last_error =
                        Some("Фоновый поток загрузки неожиданно завершился.".to_string());
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
                        format!("Страница открыта в браузере Selenium: {current_url}");
                }
                AdvancedDownloadEvent::LinkCollectStarted { current_url } => {
                    self.advanced_link_collect_active = true;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_link_collect_last_poll_at =
                        Instant::now() - Duration::from_secs(1);
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = format!(
                        "Сбор ссылок запущен в Selenium-браузере: {current_url}. Прокручивайте страницу или открывайте новые блоки, затем нажмите «Остановить сбор ссылок»."
                    );
                }
                AdvancedDownloadEvent::LinkCollectCountUpdated { found_links } => {
                    self.advanced_link_collect_found_links = found_links;
                    self.advanced_link_collect_last_poll_at = Instant::now();
                }
                AdvancedDownloadEvent::InterceptStarted { current_url } => {
                    self.advanced_intercept_active = true;
                    self.advanced_intercept_found_pages = 0;
                    self.advanced_intercept_last_poll_at = Instant::now() - Duration::from_secs(1);
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = format!(
                        "Перехват Canvas запущен в Selenium-браузере: {current_url}. Выполните нужные действия на странице и затем нажмите «Завершить перехват»."
                    );
                }
                AdvancedDownloadEvent::InterceptCountUpdated { found_pages } => {
                    self.advanced_intercept_found_pages = found_pages;
                    self.advanced_intercept_last_poll_at = Instant::now();
                }
                AdvancedDownloadEvent::Loaded(result) => {
                    self.advanced_link_collect_active = false;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_intercept_active = false;
                    self.advanced_intercept_found_pages = 0;
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
                            format!(
                                "Canvas-режим загрузил {} изображений из {}.",
                                result.downloaded_images, result.source_url,
                            )
                        } else {
                            format!(
                                "Продвинутый выкачиватель загрузил {} изображений из {}.",
                                result.downloaded_images, result.source_url,
                            )
                        };
                    crate::runtime_log::log_info(format!(
                        "[new-project] advanced downloader loaded {page_count} ribbon images from '{}'",
                        result.source_url,
                    ));
                }
                AdvancedDownloadEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    self.advanced_link_collect_active = false;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_intercept_active = false;
                    self.advanced_intercept_found_pages = 0;
                    crate::runtime_log::log_error(format!(
                        "[new-project] advanced downloader failed: {log_message}"
                    ));
                    self.active_progress = None;
                    self.import_status =
                        "Продвинутый выкачиватель завершился с ошибкой.".to_string();
                    self.last_error = Some(user_message);
                }
                AdvancedDownloadEvent::WorkerDisconnected => {
                    self.advanced_link_collect_active = false;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_intercept_active = false;
                    self.advanced_intercept_found_pages = 0;
                    crate::runtime_log::log_error(
                        "[new-project] advanced downloader worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status =
                        "Продвинутый выкачиватель завершился с ошибкой.".to_string();
                    self.last_error = Some(
                        "Фоновый поток Selenium-выкачивателя неожиданно завершился.".to_string(),
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

        Window::new("Предупреждение")
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
        self.advanced_download
            .begin_query_canvas_intercept_count(browser);
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
                            format!("Склейка завершена, получено {page_count} частей.")
                        }
                        StitchSuccessKind::StitchOnly => {
                            format!("Сшивание ленты завершено, получено {page_count} частей.")
                        }
                        StitchSuccessKind::HeterogeneousBottoms => format!(
                            "Сшивание неоднородных мест завершено, получено {page_count} частей."
                        ),
                        StitchSuccessKind::ReferenceCut => format!(
                            "Нарезка по примеру главы завершена, получено {page_count} частей."
                        ),
                        StitchSuccessKind::ManualPreview => format!(
                            "Склейка завершена: выставлено {} ручных линий.",
                            self.manual_cut_guides.len()
                        ),
                        StitchSuccessKind::ManualApply => {
                            format!("Ручная нарезка завершена, получено {page_count} частей.")
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
                    self.import_status = "Сшивание / нарезка завершились с ошибкой.".to_string();
                    self.last_error = Some(user_message);
                }
                StitchEvent::WorkerDisconnected => {
                    crate::runtime_log::log_error(
                        "[new-project] stitch worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status = "Сшивание / нарезка завершились с ошибкой.".to_string();
                    self.last_error =
                        Some("Фоновый поток склейки неожиданно завершился.".to_string());
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
                    self.import_status = format!(
                        "Сохранено {} файлов в '{}'.",
                        result.saved_images,
                        result.target_dir.display()
                    );
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
                    self.import_status = "Сохранение завершилось с ошибкой.".to_string();
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
                    self.import_status = "Сохранение завершилось с ошибкой.".to_string();
                    self.last_error =
                        Some("Фоновый поток сохранения неожиданно завершился.".to_string());
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
                    self.import_status = format!(
                        "waifu2x обработал {} изображений через {}.",
                        result.processed_images,
                        result.backend_path.display()
                    );
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
                    self.import_status = "waifu2x завершился с ошибкой.".to_string();
                    self.last_error = Some(user_message);
                }
                Waifu2xEvent::WorkerDisconnected => {
                    crate::runtime_log::log_error(
                        "[new-project] waifu2x worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status = "waifu2x завершился с ошибкой.".to_string();
                    self.last_error =
                        Some("Фоновый поток waifu2x неожиданно завершился.".to_string());
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
            self.last_error = Some("Некорректный уровень шумоподавления для waifu2x.".to_string());
            self.import_status = "Некорректные параметры waifu2x.".to_string();
            return None;
        };
        let Some(&scale) = scale_levels.get(self.waifu_scale) else {
            self.last_error = Some("Некорректный масштаб для waifu2x.".to_string());
            self.import_status = "Некорректные параметры waifu2x.".to_string();
            return None;
        };
        let tile_size = match self.waifu_tile_size.trim().parse::<u32>() {
            Ok(value) if value == 0 || value >= 32 => value,
            _ => {
                self.last_error =
                    Some("Tile size должен быть равен 0 или не меньше 32.".to_string());
                self.import_status = "Некорректные параметры waifu2x.".to_string();
                return None;
            }
        };
        Some(Waifu2xOptions {
            noise,
            scale,
            tile_size,
        })
    }

    fn can_start_waifu2x(&self) -> bool {
        !self.source_import.is_loading()
            && !self.advanced_download.is_loading()
            && !self.quick_download.is_loading()
            && !self.stitch.is_loading()
            && !self.save.is_loading()
            && !self.waifu2x.is_loading()
            && !self.ribbon.pages().is_empty()
    }

    fn waifu_backend_path_display(&self) -> String {
        self.waifu2x.backend_path().display().to_string()
    }

    fn handle_window_closed(&mut self) {
        self.waifu2x.shutdown();
        self.waifu2x = Waifu2xController::new();
    }

    fn clamp_advanced_indexes(&mut self) {
        if self.selected_browser >= self.browser_names.len() {
            self.selected_browser = 0;
        }
        if self.selected_site >= self.site_presets.len() {
            self.selected_site = 0;
        }
    }

    fn selected_browser_name(&self) -> Option<String> {
        self.browser_names.get(self.selected_browser).cloned()
    }

    fn start_advanced_open(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some("Не найден ни один поддерживаемый браузер.".to_string());
            self.import_status = "Браузер для Selenium недоступен.".to_string();
            return;
        };
        if self.advanced_page_url.trim().is_empty() {
            self.last_error = Some("Введите ссылку на страницу.".to_string());
            self.import_status = "Продвинутый выкачиватель ждёт ссылку.".to_string();
            return;
        }
        self.last_error = None;
        self.advanced_link_collect_found_links = 0;
        self.advanced_intercept_found_pages = 0;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "browser".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = "Открываем страницу в Selenium-браузере...".to_string();
        self.advanced_download
            .begin_open(browser, self.advanced_page_url.clone());
    }

    fn start_advanced_fetch(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some("Не найден ни один поддерживаемый браузер.".to_string());
            self.import_status = "Браузер для Selenium недоступен.".to_string();
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
        self.import_status = "Собираем ссылки из активной вкладки браузера...".to_string();
        self.advanced_download.begin_fetch(
            browser,
            self.image_prefix.trim().to_string(),
            self.advanced_fetch_parallelism,
        );
    }

    fn start_advanced_link_collect(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some("Не найден ни один поддерживаемый браузер.".to_string());
            self.import_status = "Браузер для Selenium недоступен.".to_string();
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
        self.import_status = "Запускаем фоновый сбор ссылок в Selenium-браузере...".to_string();
        self.advanced_download.begin_start_link_collect(
            browser,
            self.image_prefix.trim().to_string(),
            self.advanced_fetch_parallelism,
        );
    }

    fn finish_advanced_link_collect(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some("Не найден ни один поддерживаемый браузер.".to_string());
            self.import_status = "Браузер для Selenium недоступен.".to_string();
            return;
        };
        self.last_error = None;
        self.advanced_intercept_found_pages = 0;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "download".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status =
            "Останавливаем сбор ссылок и скачиваем найденные страницы...".to_string();
        self.advanced_download.begin_stop_link_collect(browser);
    }

    fn start_advanced_canvas_fetch(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some("Не найден ни один поддерживаемый браузер.".to_string());
            self.import_status = "Браузер для Selenium недоступен.".to_string();
            return;
        };
        self.last_error = None;
        self.advanced_intercept_found_pages = 0;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "collect_canvas".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = "Собираем Canvas с текущей страницы Selenium...".to_string();
        self.advanced_download.begin_fetch_canvas(browser);
    }

    fn start_advanced_canvas_intercept(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some("Не найден ни один поддерживаемый браузер.".to_string());
            self.import_status = "Браузер для Selenium недоступен.".to_string();
            return;
        };
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "browser".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = "Запускаем фоновый перехват Canvas в Selenium-браузере...".to_string();
        self.advanced_download.begin_start_canvas_intercept(browser);
    }

    fn finish_advanced_canvas_intercept(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some("Не найден ни один поддерживаемый браузер.".to_string());
            self.import_status = "Браузер для Selenium недоступен.".to_string();
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
            "Останавливаем перехват Canvas и сохраняем найденные холсты...".to_string();
        self.advanced_download.begin_stop_canvas_intercept(browser);
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
            self.last_error = Some("Введите префикс URL.".to_string());
            self.import_status = "Префикс для сайта не указан.".to_string();
            return;
        }
        let target_name = if new_name.is_empty() {
            selected_name
        } else {
            new_name
        };
        if target_name.trim().is_empty() {
            self.last_error =
                Some("Введите название сайта или выберите существующий пресет.".to_string());
            self.import_status = "Не удалось сохранить префикс.".to_string();
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
                    format!("Префикс для сайта «{}» сохранён.", target_name.trim());
            }
            Err(err) => {
                crate::runtime_log::log_error(format!(
                    "[new-project] failed to save image url preset '{target_name}': {err}"
                ));
                self.last_error =
                    Some("Не удалось сохранить префикс сайта в user_config.json.".to_string());
                self.import_status = "Не удалось сохранить префикс.".to_string();
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
            RichText::new(format!("Этап: {}", progress_stage_title(&progress.stage)))
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
            .with_title("Массовая обработка")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([900.0, 600.0])
            .with_resizable(true);
        ctx.show_viewport_immediate(viewport_id, builder, |ctx, class| {
            let keep_open = self.batch_processing.show(ctx, class);
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
        return format!("Сбор Canvas: найдено {current} стр.");
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

fn draw_scroll_arrow_markers(
    ui: &Ui,
    scroll_inner_rect: egui::Rect,
    content_height: f32,
    scroll_offset_y: f32,
    screen_positions: &[f32],
    style: ScrollArrowMarkerStyle,
) {
    if screen_positions.is_empty() {
        return;
    }
    let marker_rect = egui::Rect::from_min_max(
        egui::pos2(
            scroll_inner_rect.right() - style.arrow_width,
            scroll_inner_rect.top(),
        ),
        egui::pos2(scroll_inner_rect.right(), scroll_inner_rect.bottom()),
    )
    .shrink2(egui::vec2(2.0, 4.0));
    if marker_rect.height() <= style.arrow_height {
        return;
    }

    let painter = ui.painter();
    for screen_y in screen_positions {
        let content_y =
            (*screen_y - scroll_inner_rect.top() + scroll_offset_y).clamp(0.0, content_height);
        let ratio = (content_y / content_height).clamp(0.0, 1.0);
        let marker_y = egui::lerp(marker_rect.top()..=marker_rect.bottom(), ratio);
        let arrow_half_height = style.arrow_height * 0.5;
        let arrow_tip = egui::pos2(marker_rect.right(), marker_y);
        let arrow_tail = marker_rect.left();
        painter.add(egui::Shape::convex_polygon(
            vec![
                arrow_tip,
                egui::pos2(arrow_tail, marker_y - arrow_half_height),
                egui::pos2(arrow_tail, marker_y + arrow_half_height),
            ],
            style.fill,
            style.stroke,
        ));
        painter.line_segment(
            [
                egui::pos2(arrow_tail - style.tail_length, marker_y),
                egui::pos2(arrow_tail + style.tail_length * 0.2, marker_y),
            ],
            Stroke::new(style.tail_width, style.fill),
        );
    }
}

fn standard_dark_style() -> egui::Style {
    egui::Style {
        visuals: egui::Visuals::dark(),
        ..Default::default()
    }
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
        RichText::new("Обрезать")
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
                if button_sized(ui, "Снять (S)", button_size, true).clicked() {
                    capture_clicked = true;
                }
                ui.label(
                    RichText::new(format!("{} × {}", selection.width, selection.height))
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);
                ui.label(RichText::new("Esc: выйти").small().weak());
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
        "Загрузка..."
    } else {
        "Готово"
    }
}

fn random_test_chapter_number() -> usize {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.subsec_nanos());
    usize::try_from(nanos % 100).unwrap_or(0).saturating_add(1)
}

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
        "source" => "Загрузка изображений",
        "save" => "Сохранение",
        "advanced_download" => "Продвинутый выкачиватель",
        "quick_download" => "Быстрый выкачиватель",
        "stitch" => "Нарезка",
        "waifu2x" => "waifu2x",
        _ => "Обработка",
    }
}

fn supported_quick_download_sites() -> Vec<&'static str> {
    SUPPORTED_SITES_TOOLTIP
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

fn progress_stage_title(stage: &str) -> &'static str {
    match stage {
        "prepare" => "Подготовка файлов",
        "clean" => "Очистка папки",
        "write" => "Запись PNG",
        "browser" => "Запуск браузера",
        "collect" => "Поиск ссылок",
        "collect_canvas" => "Сбор Canvas",
        "connect" => "Проверка доступности",
        "scan" => "Поиск источника",
        "parse_html" => "Разбор HTML",
        "archive" => "Распаковка архива",
        "download_waifu2x" => "Загрузка waifu2x",
        "extract_waifu2x" => "Распаковка waifu2x",
        "download" => "Загрузка страниц",
        "save_canvas" => "Сохранение Canvas",
        "decode" => "Чтение изображений",
        "filter" => "Фильтрация ширины",
        "preview" => "Подготовка превью",
        "normalize" => "Выравнивание ширины",
        "stitch" => "Склейка ленты",
        "waifu2x" => "Обработка waifu2x",
        "cuts" => "Поиск линий нарезки",
        "compose" => "Сборка ленты",
        "split" => "Нарезка страниц",
        _ => "Обработка",
    }
}

fn confirm_overwrite_nonempty(path: &std::path::Path) -> Result<bool, std::io::Error> {
    if !path.exists() || !dir_has_entries(path)? {
        return Ok(true);
    }
    Ok(MessageDialog::new()
        .set_title("ManhwaStudio")
        .set_description("Папка не пустая. Перезаписать файлы?")
        .set_buttons(MessageButtons::YesNo)
        .set_level(MessageLevel::Warning)
        .show()
        == MessageDialogResult::Yes)
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
        StitchSplitMode::StitchOnly => "Сшивание ленты...",
        StitchSplitMode::ManualCutPreview => "Сшивание и подготовка ручных линий...",
        StitchSplitMode::AutoCut => "Сшивание и автоматическая нарезка...",
        StitchSplitMode::HeterogeneousBottoms => "Сшивание только в неоднородных местах...",
    }
}
