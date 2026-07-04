/*
File: src/launcher/psd_import_window.rs

Purpose:
Detached egui window for importing simple PSD layers into launcher projects.

Main responsibilities:
- render a dark-themed PSD import UI with layer mapping and preview;
- load PSD/ZIP/RAR sources on background threads using the in-tree `ag-psd` crate;
- warn when unsupported complexity is detected (for example groups or PSB files);
- save selected raster layers into project `src/` and `clean_layers/` without blocking the GUI.

Notes:
This implementation targets simple flat PSD files. `ag-psd` decodes 8/16/32-bit documents
(down-converting to 8-bit RGBA), so 16-bit "клин" exports load fine; it exposes the nested
group hierarchy via `Layer::children`, which we flatten to leaf raster layers and flag with a
warning when groups are present.
*/

use crate::launcher::new_project::project_io::{
    ProjectCatalogController, ProjectCatalogEvent, ProjectCatalogSnapshot, chapters_for_title,
};
use crate::launcher::state::OpenProjectSelection;
use crate::runtime_log;
use crate::widgets::EditableComboBox;
use egui::{
    self, Align, Button, CentralPanel, Color32, ColorImage, ComboBox, Context, Frame, Grid, Id,
    Layout, RichText, ScrollArea, Sense, SidePanel, Stroke, TextureHandle, TextureOptions, Ui,
    ViewportClass,
};
use ag_psd::psd::{Layer, PixelData, ReadOptions};
use ag_psd::read_psd;
use image::RgbaImage;
#[cfg(not(target_arch = "wasm32"))]
use rfd::FileDialog;
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::process::Command;
use std::sync::{
    Arc,
    mpsc::{self, Receiver},
};
use ms_thread as thread;
use web_time::Duration;
use zip::ZipArchive;

const WINDOW_TITLE: &str = "Импорт из PSD";
const TOP_BUTTON_SIZE: egui::Vec2 = egui::vec2(196.0, 34.0);
const ACTION_BUTTON_SIZE: egui::Vec2 = egui::vec2(176.0, 36.0);
const SIDE_PANEL_MIN_WIDTH: f32 = 340.0;
const SIDE_PANEL_MAX_WIDTH: f32 = 470.0;
const PREVIEW_MIN_WIDTH: f32 = 420.0;
const PREVIEW_BACKGROUND: Color32 = Color32::from_rgb(18, 18, 20);
const ERROR_COLOR: Color32 = Color32::from_rgb(214, 104, 104);
const SUCCESS_COLOR: Color32 = Color32::from_rgb(72, 170, 102);
const WARNING_COLOR: Color32 = Color32::from_rgb(214, 170, 92);
const TYPE_SKIP: &str = "Не импортировать";
const TYPE_SOURCE: &str = "Исходник";
const TYPE_CLEAN: &str = "Клин";
const ROW_TITLE_MIN_WIDTH: f32 = 120.0;
const ROW_SIZE_WIDTH: f32 = 72.0;
const ROW_PAGE_WIDTH: f32 = 104.0;
const ROW_TYPE_WIDTH: f32 = 128.0;
const PREVIEW_CONTAINER_MIN_WIDTH: f32 = 260.0;
const PREVIEW_CONTAINER_MIN_HEIGHT: f32 = 140.0;
const PREVIEW_RESIZE_HANDLE_WIDTH: f32 = 10.0;
const PREVIEW_RESIZE_GRAB_WIDTH: f32 = 18.0;
const PREVIEW_CHROME_HEIGHT: f32 = 72.0;
const PREVIEW_FRAME_VERTICAL_PADDING: f32 = 28.0;
const PREVIEW_TILE_MAX_HEIGHT: usize = 4096;

pub struct PsdImportWindowState {
    projects_root: PathBuf,
    source_summary: String,
    warnings: Vec<String>,
    rows: Vec<PsdLayerRow>,
    available_pages: Vec<u32>,
    page_inputs: HashMap<String, String>,
    page_combos: HashMap<String, EditableComboBox>,
    selected_row: Option<usize>,
    preview_cache: HashMap<String, PreviewTexture>,
    preview_status: PreviewStatus,
    preview_scroll_y: f32,
    preview_scroll_by_page: HashMap<u32, f32>,
    preview_container_width: Option<f32>,
    title_input: String,
    chapter_input: String,
    title_combo: EditableComboBox,
    catalog: ProjectCatalogController,
    catalog_snapshot: ProjectCatalogSnapshot,
    catalog_error: Option<String>,
    status: ImportStatus,
    loaded_documents: Option<Arc<Vec<LoadedPsdDocument>>>,
    pending_scan: Option<Receiver<ScanWorkerResult>>,
    pending_preview: Option<Receiver<PreviewWorkerResult>>,
    pending_import: Option<Receiver<ImportWorkerResult>>,
    open_after_save_requested: bool,
    queued_open: Option<OpenProjectSelection>,
}

#[derive(Clone)]
struct PsdLayerRow {
    row_key: String,
    file_name: String,
    page: u32,
    layer_title: String,
    size: (u32, u32),
    import_type: LayerImportType,
    document_index: usize,
    source: LayerSource,
}

/// Where a row's pixel data comes from inside its PSD document.
///
/// Most rows map to a specific layer. Flattened PSDs (no layer section, only the
/// merged composite) instead produce a single `Composite` fallback row that reads
/// the whole-document image via `Psd::rgba()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayerSource {
    Layer(usize),
    Composite,
}

/// A raster image already decoded to 8-bit RGBA, shared cheaply (via `Arc`) between
/// the layer rows, the preview render and the final import.
#[derive(Clone)]
struct DecodedImage {
    width: u32,
    height: u32,
    /// RGBA8 pixels, exactly `width * height * 4` bytes long.
    data: Arc<Vec<u8>>,
}

#[derive(Clone)]
struct DecodedLayer {
    name: String,
    image: DecodedImage,
}

#[derive(Clone)]
struct LoadedPsdDocument {
    file_name: String,
    page: u32,
    /// Flattened leaf raster layers (groups expanded), ordered top-to-bottom.
    layers: Vec<DecodedLayer>,
    /// Merged composite image, only kept for flattened PSDs that have no usable
    /// layers (see `LayerSource::Composite`). `None` whenever `layers` is non-empty.
    composite: Option<DecodedImage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum LayerImportType {
    Skip,
    Source,
    Clean,
}

#[derive(Clone)]
enum PreviewStatus {
    Idle,
    Loading(String),
    Ready,
    Error(String),
}

enum ImportStatus {
    Idle,
    LoadingCatalog,
    Scanning,
    Importing,
    Success(String),
    Error(String),
}

struct ScanWorkerResult {
    result: Result<ScanResponse, WorkerError>,
}

struct PreviewWorkerResult {
    row_key: String,
    result: Result<Arc<RgbaImage>, WorkerError>,
}

struct PreviewTexture {
    image: Arc<RgbaImage>,
    tiles: Vec<PreviewTextureTile>,
}

struct PreviewTextureTile {
    texture: TextureHandle,
    origin_px: egui::Vec2,
    size: egui::Vec2,
}

struct ImportWorkerResult {
    result: Result<ImportResponse, WorkerError>,
}

struct ScanResponse {
    source_summary: String,
    warnings: Vec<String>,
    available_pages: Vec<u32>,
    rows: Vec<PsdLayerRow>,
    documents: Arc<Vec<LoadedPsdDocument>>,
}

struct ImportResponse {
    project_dir: PathBuf,
    saved_pages: usize,
    title: String,
    chapter: String,
}

#[derive(Debug)]
struct WorkerError {
    user_message: String,
    log_message: String,
}

enum ScanRequest {
    Files { paths: Vec<PathBuf> },
    Folder { path: PathBuf },
}

#[derive(Clone)]
struct ImportAssignment {
    page: u32,
    import_type: LayerImportType,
    document_index: usize,
    source: LayerSource,
}

impl PsdImportWindowState {
    pub fn new(projects_root: PathBuf) -> Self {
        let mut state = Self {
            projects_root: projects_root.clone(),
            source_summary: "PSD источник не выбран".to_string(),
            warnings: Vec::new(),
            rows: Vec::new(),
            available_pages: Vec::new(),
            page_inputs: HashMap::new(),
            page_combos: HashMap::new(),
            selected_row: None,
            preview_cache: HashMap::new(),
            preview_status: PreviewStatus::Idle,
            preview_scroll_y: 0.0,
            preview_scroll_by_page: HashMap::new(),
            preview_container_width: None,
            title_input: String::new(),
            chapter_input: String::new(),
            title_combo: EditableComboBox::new("launcher_psd_import_title")
                .with_hint_text("Выберите тайтл или введите новый")
                .with_desired_text_width(360.0),
            catalog: ProjectCatalogController::new(projects_root),
            catalog_snapshot: ProjectCatalogSnapshot {
                titles: Vec::new(),
                chapters_by_title: HashMap::new(),
            },
            catalog_error: None,
            status: ImportStatus::LoadingCatalog,
            loaded_documents: None,
            pending_scan: None,
            pending_preview: None,
            pending_import: None,
            open_after_save_requested: false,
            queued_open: None,
        };
        state.catalog.refresh();
        state
    }

    pub fn show(&mut self, ctx: &Context, viewport_class: ViewportClass) -> bool {
        self.poll_catalog(ctx);
        self.poll_scan(ctx);
        self.poll_preview(ctx);
        self.poll_import(ctx);

        if self.is_busy() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }

        match viewport_class {
            ViewportClass::Embedded => self.show_embedded(ctx),
            _ => {
                // A native window is its own viewport but shares the launcher's single egui
                // Context, so its style is global. Switch to this window's dark style while it
                // renders and restore the previous (launcher) style afterwards, so it never
                // leaks back and leaves the launcher's combo boxes / text fields unstyled.
                let previous_style = ctx.style();
                ctx.set_style(standard_dark_style());
                let keep_open = self.show_native(ctx);
                ctx.set_style(previous_style);
                keep_open
            }
        }
    }

    pub fn take_open_project_selection(&mut self) -> Option<OpenProjectSelection> {
        self.queued_open.take()
    }

    pub fn set_projects_root(&mut self, projects_root: PathBuf) {
        self.projects_root = projects_root.clone();
        self.catalog = ProjectCatalogController::new(projects_root);
        self.catalog_snapshot = ProjectCatalogSnapshot {
            titles: Vec::new(),
            chapters_by_title: HashMap::new(),
        };
        self.catalog_error = None;
        self.catalog.refresh();
    }

    fn show_native(&mut self, ctx: &Context) -> bool {
        if ctx.input(|input| input.viewport().close_requested()) {
            return false;
        }
        CentralPanel::default()
            .frame(Frame::new().fill(Color32::from_rgb(24, 24, 27)))
            .show(ctx, |ui| self.show_contents(ui));
        true
    }

    fn show_embedded(&mut self, ctx: &Context) -> bool {
        let mut keep_open = true;
        egui::Window::new(WINDOW_TITLE)
            .open(&mut keep_open)
            .default_size(egui::vec2(1360.0, 820.0))
            .min_width(1120.0)
            .min_height(720.0)
            .show(ctx, |ui| {
                ui.set_style(standard_dark_style());
                self.show_contents(ui);
            });
        keep_open
    }

    fn show_contents(&mut self, ui: &mut Ui) {
        ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
        ui.visuals_mut().widgets.noninteractive.bg_fill = Color32::from_rgb(34, 34, 38);

        ui.vertical(|ui| {
            self.show_top_bar(ui);
            if !self.warnings.is_empty() {
                ui.add_space(4.0);
                Frame::group(ui.style())
                    .stroke(Stroke::new(1.0, WARNING_COLOR))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("Предупреждения")
                                .strong()
                                .color(WARNING_COLOR),
                        );
                        for warning in &self.warnings {
                            ui.colored_label(WARNING_COLOR, warning);
                        }
                    });
            }
            ui.separator();
            ui.add_space(4.0);

            let left_panel_width = self.left_panel_width(ui.available_width());
            SidePanel::left("psd_import_left_panel")
                .resizable(false)
                .exact_width(left_panel_width)
                .show_inside(ui, |ui| {
                    ScrollArea::vertical()
                        .id_salt("psd_import_left_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.show_rows_table(ui);
                            ui.separator();
                            self.show_save_block(ui);
                        });
                });

            CentralPanel::default().show_inside(ui, |ui| {
                self.show_preview_panel(ui);
            });
        });
    }

    fn left_panel_width(&self, available_width: f32) -> f32 {
        let max_from_window = (available_width - PREVIEW_MIN_WIDTH).max(SIDE_PANEL_MIN_WIDTH);
        max_from_window.clamp(SIDE_PANEL_MIN_WIDTH, SIDE_PANEL_MAX_WIDTH)
    }

    fn show_top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            if button_sized(ui, "Открыть PSD/ZIP...", TOP_BUTTON_SIZE, !self.is_busy()).clicked()
            {
                self.pick_files();
            }
            if button_sized(ui, "Открыть папку с PSD", TOP_BUTTON_SIZE, !self.is_busy()).clicked()
            {
                self.pick_folder();
            }
            if button_sized(ui, "Поменять фон и клин", TOP_BUTTON_SIZE, !self.is_busy()).clicked()
            {
                self.swap_source_and_clean();
            }
            ui.add_space(8.0);
            ui.label(RichText::new(&self.source_summary).color(Color32::from_gray(210)));
        });
    }

    fn show_rows_table(&mut self, ui: &mut Ui) {
        ui.heading("Слои");
        ui.label(RichText::new("Назначьте для каждой страницы исходник и клин.").small());
        if has_fully_skipped_document(&self.rows) {
            ui.colored_label(
                WARNING_COLOR,
                "Один или несколько загруженных psd файлов не были обработаны автоматически. Пожалуйста, назначьте исходник и клин вручную.",
            );
        }
        ui.add_space(6.0);

        Frame::group(ui.style())
            .stroke(Stroke::new(1.0, Color32::from_rgb(56, 56, 62)))
            .show(ui, |ui| {
                if self.rows.is_empty() {
                    ui.label("Слои ещё не загружены.");
                    return;
                }

                let title_width = (ui.available_width()
                    - ROW_SIZE_WIDTH
                    - ROW_PAGE_WIDTH
                    - ROW_TYPE_WIDTH
                    - ui.spacing().item_spacing.x * 3.0)
                    .max(ROW_TITLE_MIN_WIDTH);
                Grid::new("psd_import_rows_grid")
                    .num_columns(4)
                    .striped(true)
                    .min_col_width(0.0)
                    .show(ui, |ui| {
                        ui.add_sized(
                            [title_width, 22.0],
                            egui::Label::new(RichText::new("Картинка").strong()),
                        );
                        ui.add_sized(
                            [ROW_SIZE_WIDTH, 22.0],
                            egui::Label::new(RichText::new("Размер").strong()),
                        );
                        ui.add_sized(
                            [ROW_PAGE_WIDTH, 22.0],
                            egui::Label::new(RichText::new("Страница").strong()),
                        );
                        ui.add_sized(
                            [ROW_TYPE_WIDTH, 22.0],
                            egui::Label::new(RichText::new("Тип").strong()),
                        );
                        ui.end_row();

                        for row_index in 0..self.rows.len() {
                            self.show_row(ui, row_index, title_width);
                            ui.end_row();
                        }
                    });
            });
    }

    fn show_row(&mut self, ui: &mut Ui, row_index: usize, title_width: f32) {
        let selected = self.selected_row == Some(row_index);
        let row_title = self.format_row_title(row_index);
        let page_before = self.rows[row_index].page;
        let import_before = self.rows[row_index].import_type;

        let response = ui.add_sized(
            [title_width, 30.0],
            Button::new(row_title).selected(selected).truncate(),
        );
        if response.clicked() {
            self.select_row(row_index);
        }

        ui.add_sized(
            [ROW_SIZE_WIDTH, 30.0],
            egui::Label::new(format!(
                "{}x{}",
                self.rows[row_index].size.0, self.rows[row_index].size.1
            )),
        );

        ui.scope(|ui| {
            ui.set_width(ROW_PAGE_WIDTH);
            self.show_page_combo(ui, row_index);
        });

        ui.scope(|ui| {
            ui.set_width(ROW_TYPE_WIDTH);
            ComboBox::from_id_salt(("psd_import_type", row_index))
                .width(ROW_TYPE_WIDTH - 8.0)
                .selected_text(self.rows[row_index].import_type.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.rows[row_index].import_type,
                        LayerImportType::Skip,
                        TYPE_SKIP,
                    );
                    ui.selectable_value(
                        &mut self.rows[row_index].import_type,
                        LayerImportType::Source,
                        TYPE_SOURCE,
                    );
                    ui.selectable_value(
                        &mut self.rows[row_index].import_type,
                        LayerImportType::Clean,
                        TYPE_CLEAN,
                    );
                });
        });

        if self.rows[row_index].page != page_before
            && let Err(message) = self.validate_row_change(row_index)
        {
            self.rows[row_index].page = page_before;
            self.rows[row_index].import_type = import_before;
            self.sync_page_input(row_index);
            self.status = ImportStatus::Error(message);
        } else if self.rows[row_index].import_type != import_before
            && let Err(message) = self.validate_row_change(row_index)
        {
            self.rows[row_index].page = page_before;
            self.rows[row_index].import_type = import_before;
            self.sync_page_input(row_index);
            self.status = ImportStatus::Error(message);
        }
    }

    fn show_page_combo(&mut self, ui: &mut Ui, row_index: usize) {
        let Some(row) = self.rows.get(row_index) else {
            return;
        };
        let row_key = row.row_key.clone();
        let page = row.page;
        let options = self
            .available_pages
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>();
        let value = self
            .page_inputs
            .entry(row_key.clone())
            .or_insert_with(|| page.to_string());
        let combo = self.page_combos.entry(row_key.clone()).or_insert_with(|| {
            EditableComboBox::new(("psd_import_page", row_key))
                .with_hint_text("№")
                .with_desired_text_width(ROW_PAGE_WIDTH - 32.0)
                .with_popup_max_height(180.0)
        });

        let response = combo.draw(ui, value, &options);
        if response.changed
            && let Err(message) = self.commit_page_input(row_index)
        {
            self.status = ImportStatus::Error(message);
        }
    }

    fn show_save_block(&mut self, ui: &mut Ui) {
        ui.heading("Сохранить главу");
        ui.label("Тайтл");
        let title_changed = ui
            .scope(|ui| {
                self.title_combo
                    .draw(ui, &mut self.title_input, &self.catalog_snapshot.titles)
            })
            .inner
            .changed;
        if title_changed {
            clear_success_status(&mut self.status);
        }

        ui.label("Глава");
        let chapter_response = ui.add_sized(
            [360.0, ui.spacing().interact_size.y.max(32.0)],
            egui::TextEdit::singleline(&mut self.chapter_input),
        );
        if chapter_response.changed() {
            clear_success_status(&mut self.status);
        }

        if let Some(error) = &self.catalog_error {
            ui.colored_label(ERROR_COLOR, error);
        } else {
            let chapters = chapters_for_title(&self.catalog_snapshot, &self.title_input);
            let joined = if chapters.is_empty() {
                "Существующие главы не найдены".to_string()
            } else {
                format!("Существующие главы: {}", chapters.join(", "))
            };
            ui.label(RichText::new(joined).small().color(Color32::from_gray(180)));
        }

        ui.add_space(8.0);
        self.show_status(ui);
        ui.add_space(8.0);

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let can_import = self.can_import();
            if button_sized(ui, "Сохранить и открыть", ACTION_BUTTON_SIZE, can_import).clicked()
            {
                self.start_import(true);
            }
            if button_sized(ui, "Сохранить", ACTION_BUTTON_SIZE, can_import).clicked() {
                self.start_import(false);
            }
            if button_sized(
                ui,
                "Обновить",
                egui::vec2(112.0, 36.0),
                !self.catalog.is_loading(),
            )
            .clicked()
            {
                self.catalog.refresh();
                self.status = ImportStatus::LoadingCatalog;
            }
        });
    }

    fn show_status(&self, ui: &mut Ui) {
        match &self.status {
            ImportStatus::Idle => {
                ui.label(RichText::new("Готово к импорту PSD.").small());
            }
            ImportStatus::LoadingCatalog => {
                ui.label(RichText::new("Считываем список тайтлов...").small());
            }
            ImportStatus::Scanning => {
                ui.label(RichText::new("Читаем PSD и строим список слоёв...").small());
            }
            ImportStatus::Importing => {
                ui.label(RichText::new("Импортируем выбранные слои...").small());
            }
            ImportStatus::Success(message) => {
                ui.colored_label(SUCCESS_COLOR, message);
            }
            ImportStatus::Error(message) => {
                ui.colored_label(ERROR_COLOR, message);
            }
        }
    }

    fn show_preview_panel(&mut self, ui: &mut Ui) {
        ui.heading("Предпросмотр");
        ui.add_space(8.0);
        let outer_rect = ui.available_rect_before_wrap();
        ui.allocate_rect(outer_rect, Sense::hover());

        let available_width = outer_rect.width();
        let container_width = self.apply_preview_resize(ui, outer_rect, available_width);
        let container_height = self.preview_container_height(container_width, outer_rect.height());
        let container_rect = egui::Rect::from_min_size(
            egui::pos2(
                outer_rect.center().x - container_width * 0.5,
                outer_rect.min.y,
            ),
            egui::vec2(container_width, container_height),
        );

        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(container_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
            |ui| {
                Frame::group(ui.style())
                    .fill(PREVIEW_BACKGROUND)
                    .stroke(Stroke::new(1.0, Color32::from_rgb(56, 56, 62)))
                    .show(ui, |ui| {
                        ui.set_min_width(container_width);
                        match self.selected_row.and_then(|index| self.rows.get(index)) {
                            Some(row) => {
                                ui.label(RichText::new(Self::format_row_title_for(row)).strong());
                                ui.label(
                                    RichText::new(format!(
                                        "{}x{}  |  страница {}",
                                        row.size.0, row.size.1, row.page
                                    ))
                                    .small()
                                    .color(Color32::from_gray(180)),
                                );
                                ui.add_space(10.0);

                                if let Some(preview) = self.preview_cache.get(&row.row_key) {
                                    let scroll_output = ScrollArea::vertical()
                                        .id_salt("psd_import_preview_scroll")
                                        .vertical_scroll_offset(self.preview_scroll_y)
                                        .show(ui, |ui| {
                                            let available = ui.available_size_before_wrap();
                                            let preview_size = egui::vec2(
                                                preview.image.width() as f32,
                                                preview.image.height() as f32,
                                            );
                                            let scale =
                                                preview_scale_to_width(preview_size, available.x);
                                            let top_left = ui.min_rect().min;
                                            for tile in &preview.tiles {
                                                let tile_rect = egui::Rect::from_min_size(
                                                    top_left
                                                        + egui::vec2(
                                                            tile.origin_px.x * scale,
                                                            tile.origin_px.y * scale,
                                                        ),
                                                    tile.size * scale,
                                                );
                                                ui.put(
                                                    tile_rect,
                                                    egui::Image::new((
                                                        tile.texture.id(),
                                                        tile.size * scale,
                                                    )),
                                                );
                                            }
                                            ui.allocate_space(preview_size * scale);
                                        });
                                    self.preview_scroll_y = scroll_output.state.offset.y;
                                    self.preview_scroll_by_page
                                        .insert(row.page, self.preview_scroll_y);
                                } else {
                                    match &self.preview_status {
                                        PreviewStatus::Idle => {
                                            ui.label("Выберите слой для предпросмотра.");
                                        }
                                        PreviewStatus::Loading(row_key)
                                            if row_key == &row.row_key =>
                                        {
                                            ui.spinner();
                                            ui.label("Рендерим выбранный слой...");
                                        }
                                        PreviewStatus::Error(message) => {
                                            ui.colored_label(ERROR_COLOR, message);
                                        }
                                        _ => {
                                            ui.label("Предпросмотр ещё не загружен.");
                                        }
                                    }
                                }
                            }
                            None => {
                                ui.label("Откройте PSD/ZIP и выберите слой.");
                            }
                        }
                    });
            },
        );
    }

    fn apply_preview_resize(
        &mut self,
        ui: &mut Ui,
        outer_rect: egui::Rect,
        available_width: f32,
    ) -> f32 {
        let min_width = PREVIEW_CONTAINER_MIN_WIDTH.min(available_width);
        let max_width = available_width.max(min_width);
        let mut width = self
            .preview_container_width
            .unwrap_or(max_width)
            .clamp(min_width, max_width);

        let left_x = outer_rect.center().x - width * 0.5;
        let right_x = outer_rect.center().x + width * 0.5;
        let left_handle = egui::Rect::from_min_max(
            egui::pos2(left_x - PREVIEW_RESIZE_GRAB_WIDTH * 0.5, outer_rect.min.y),
            egui::pos2(left_x + PREVIEW_RESIZE_GRAB_WIDTH * 0.5, outer_rect.max.y),
        );
        let right_handle = egui::Rect::from_min_max(
            egui::pos2(right_x - PREVIEW_RESIZE_GRAB_WIDTH * 0.5, outer_rect.min.y),
            egui::pos2(right_x + PREVIEW_RESIZE_GRAB_WIDTH * 0.5, outer_rect.max.y),
        );

        let left_response = ui.interact(
            left_handle,
            Id::new("psd_import_preview_resize_left"),
            Sense::drag(),
        );
        let right_response = ui.interact(
            right_handle,
            Id::new("psd_import_preview_resize_right"),
            Sense::drag(),
        );

        if left_response.hovered()
            || left_response.dragged()
            || right_response.hovered()
            || right_response.dragged()
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }

        if left_response.dragged() {
            width = (width - left_response.drag_delta().x).clamp(min_width, max_width);
        }
        if right_response.dragged() {
            width = (width + right_response.drag_delta().x).clamp(min_width, max_width);
        }

        self.preview_container_width = Some(width);
        let painter = ui.painter();
        for x in [left_x, right_x] {
            let handle_rect = egui::Rect::from_center_size(
                egui::pos2(x, outer_rect.center().y),
                egui::vec2(
                    PREVIEW_RESIZE_HANDLE_WIDTH,
                    (outer_rect.height() - 12.0).max(48.0),
                ),
            );
            painter.rect_filled(handle_rect, 4.0, Color32::from_rgb(64, 64, 70));
        }

        width
    }

    fn preview_container_height(&self, container_width: f32, max_height: f32) -> f32 {
        let content_height = match self.selected_row.and_then(|index| self.rows.get(index)) {
            Some(row) => match self.preview_cache.get(&row.row_key) {
                Some(preview) => {
                    let preview_size =
                        egui::vec2(preview.image.width() as f32, preview.image.height() as f32);
                    let scale = preview_scale_to_width(preview_size, container_width - 20.0);
                    PREVIEW_CHROME_HEIGHT + preview_size.y * scale + PREVIEW_FRAME_VERTICAL_PADDING
                }
                None => PREVIEW_CONTAINER_MIN_HEIGHT,
            },
            None => PREVIEW_CONTAINER_MIN_HEIGHT,
        };

        content_height.clamp(
            PREVIEW_CONTAINER_MIN_HEIGHT,
            max_height.max(PREVIEW_CONTAINER_MIN_HEIGHT),
        )
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn pick_files(&mut self) {
        let Some(paths) = FileDialog::new()
            .set_directory(self.projects_root.clone())
            .add_filter("Photoshop / ZIP / RAR", &["psd", "zip", "psb", "rar"])
            .pick_files()
        else {
            return;
        };
        self.start_scan(ScanRequest::Files { paths });
    }

    /// Web stub: native file/folder pickers (`rfd`) have no browser equivalent.
    /// Reports the missing capability instead of opening a dialog.
    #[cfg(target_arch = "wasm32")]
    fn pick_files(&mut self) {
        self.status = ImportStatus::Error("Выбор файлов недоступен в веб-версии.".to_string());
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn pick_folder(&mut self) {
        let Some(path) = FileDialog::new()
            .set_directory(self.projects_root.clone())
            .pick_folder()
        else {
            return;
        };
        self.start_scan(ScanRequest::Folder { path });
    }

    /// Web stub twin of `pick_files` for the folder picker.
    #[cfg(target_arch = "wasm32")]
    fn pick_folder(&mut self) {
        self.status = ImportStatus::Error("Выбор папки недоступен в веб-версии.".to_string());
    }

    fn start_scan(&mut self, request: ScanRequest) {
        self.pending_preview = None;
        self.pending_import = None;
        self.open_after_save_requested = false;
        self.queued_open = None;
        self.loaded_documents = None;
        self.rows.clear();
        self.available_pages.clear();
        self.page_inputs.clear();
        self.page_combos.clear();
        self.warnings.clear();
        self.selected_row = None;
        self.preview_cache.clear();
        self.preview_status = PreviewStatus::Idle;
        self.preview_scroll_y = 0.0;
        self.preview_scroll_by_page.clear();
        self.preview_container_width = None;
        self.source_summary = "Читаем PSD источник...".to_string();
        self.status = ImportStatus::Scanning;

        let (tx, rx) = mpsc::channel();
        self.pending_scan = Some(rx);
        let spawn_result = thread::Builder::new()
            .name("launcher-psd-scan".to_string())
            .spawn(move || {
                let result = run_scan_worker(request);
                if tx.send(ScanWorkerResult { result }).is_err() {
                    runtime_log::log_warn("[launcher-psd] failed to deliver scan result");
                }
            });

        if let Err(err) = spawn_result {
            runtime_log::log_error(format!(
                "[launcher-psd] failed to spawn scan worker thread: {err}"
            ));
            self.status =
                ImportStatus::Error("Не удалось запустить импорт PSD в фоне.".to_string());
            self.pending_scan = None;
        }
    }

    fn poll_scan(&mut self, ctx: &Context) {
        let Some(pending) = self.pending_scan.take() else {
            return;
        };
        match pending.try_recv() {
            Ok(result) => {
                ctx.request_repaint();
                match result.result {
                    Ok(response) => {
                        self.source_summary = response.source_summary;
                        self.warnings = response.warnings;
                        self.available_pages = response.available_pages;
                        self.rows = response.rows;
                        self.rebuild_page_editors();
                        self.loaded_documents = Some(response.documents);
                        self.selected_row = (!self.rows.is_empty()).then_some(0);
                        self.preview_status = PreviewStatus::Idle;
                        self.status = ImportStatus::Idle;
                        self.request_selected_preview();
                    }
                    Err(error) => {
                        runtime_log::log_error(format!(
                            "[launcher-psd] scan failed: {}",
                            error.log_message
                        ));
                        self.status = ImportStatus::Error(error.user_message);
                        self.source_summary = "PSD источник не выбран".to_string();
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.pending_scan = Some(pending);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.status =
                    ImportStatus::Error("Фоновая загрузка PSD неожиданно завершилась.".to_string());
            }
        }
    }

    fn request_selected_preview(&mut self) {
        let Some(row_index) = self.selected_row else {
            return;
        };
        let Some(row) = self.rows.get(row_index).cloned() else {
            return;
        };
        if self.preview_cache.contains_key(&row.row_key) {
            self.preview_status = PreviewStatus::Ready;
            return;
        }
        let Some(documents) = self.loaded_documents.clone() else {
            return;
        };

        let row_key = row.row_key.clone();
        let (tx, rx) = mpsc::channel();
        self.pending_preview = Some(rx);
        self.preview_status = PreviewStatus::Loading(row_key.clone());
        let spawn_result = thread::Builder::new()
            .name("launcher-psd-preview".to_string())
            .spawn(move || {
                let result = render_preview_image(&documents, row.document_index, row.source);
                if tx.send(PreviewWorkerResult { row_key, result }).is_err() {
                    runtime_log::log_warn("[launcher-psd] failed to deliver preview result");
                }
            });

        if let Err(err) = spawn_result {
            runtime_log::log_error(format!(
                "[launcher-psd] failed to spawn preview worker thread: {err}"
            ));
            self.preview_status =
                PreviewStatus::Error("Не удалось запустить рендер предпросмотра.".to_string());
            self.pending_preview = None;
        }
    }

    fn poll_preview(&mut self, ctx: &Context) {
        let Some(pending) = self.pending_preview.take() else {
            return;
        };
        match pending.try_recv() {
            Ok(result) => {
                ctx.request_repaint();
                match result.result {
                    Ok(image) => {
                        let tiles = match build_preview_texture_tiles(ctx, &result.row_key, &image)
                        {
                            Ok(tiles) => tiles,
                            Err(error) => {
                                runtime_log::log_error(format!(
                                    "[launcher-psd] preview tiling failed: {}",
                                    error.log_message
                                ));
                                self.preview_status = PreviewStatus::Error(error.user_message);
                                return;
                            }
                        };
                        self.preview_cache
                            .insert(result.row_key.clone(), PreviewTexture { image, tiles });
                        self.preview_status = PreviewStatus::Ready;
                    }
                    Err(error) => {
                        runtime_log::log_error(format!(
                            "[launcher-psd] preview failed: {}",
                            error.log_message
                        ));
                        self.preview_status = PreviewStatus::Error(error.user_message);
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.pending_preview = Some(pending);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.preview_status = PreviewStatus::Error(
                    "Фоновый рендер предпросмотра неожиданно завершился.".to_string(),
                );
            }
        }
    }

    fn start_import(&mut self, open_after_save: bool) {
        let title = self.title_input.trim().to_string();
        let chapter = self.chapter_input.trim().to_string();
        if title.is_empty() || chapter.is_empty() {
            self.status = ImportStatus::Error("Укажите тайтл и главу.".to_string());
            return;
        }
        if self.rows.is_empty() {
            self.status = ImportStatus::Error("Сначала откройте PSD источник.".to_string());
            return;
        }
        for row_index in 0..self.rows.len() {
            if let Err(message) = self.commit_page_input(row_index) {
                self.status = ImportStatus::Error(message);
                return;
            }
        }
        if let Err(message) = self.validate_all_rows() {
            self.status = ImportStatus::Error(message);
            return;
        }
        let Some(documents) = self.loaded_documents.clone() else {
            self.status = ImportStatus::Error("PSD документы ещё не загружены.".to_string());
            return;
        };

        let assignments = self
            .rows
            .iter()
            .filter(|row| row.import_type != LayerImportType::Skip)
            .map(|row| ImportAssignment {
                page: row.page,
                import_type: row.import_type,
                document_index: row.document_index,
                source: row.source,
            })
            .collect::<Vec<_>>();

        let projects_root = self.projects_root.clone();
        let title_owned = title;
        let chapter_owned = chapter;
        let (tx, rx) = mpsc::channel();
        self.pending_import = Some(rx);
        self.status = ImportStatus::Importing;
        self.open_after_save_requested = open_after_save;
        self.queued_open = None;

        let spawn_result = thread::Builder::new()
            .name("launcher-psd-import".to_string())
            .spawn(move || {
                let result = run_import_worker(
                    &projects_root,
                    &title_owned,
                    &chapter_owned,
                    documents,
                    assignments,
                );
                if tx.send(ImportWorkerResult { result }).is_err() {
                    runtime_log::log_warn("[launcher-psd] failed to deliver import result");
                }
            });

        if let Err(err) = spawn_result {
            runtime_log::log_error(format!(
                "[launcher-psd] failed to spawn import worker thread: {err}"
            ));
            self.pending_import = None;
            self.open_after_save_requested = false;
            self.status = ImportStatus::Error("Не удалось запустить сохранение PSD.".to_string());
        }
    }

    fn poll_import(&mut self, ctx: &Context) {
        let Some(pending) = self.pending_import.take() else {
            return;
        };
        match pending.try_recv() {
            Ok(result) => {
                ctx.request_repaint();
                match result.result {
                    Ok(response) => {
                        self.status = ImportStatus::Success(format!(
                            "Сохранено страниц: {}",
                            response.saved_pages
                        ));
                        if self.open_after_save_requested {
                            self.queued_open = Some(OpenProjectSelection {
                                project_dir: response.project_dir.clone(),
                                title: response.title.clone(),
                                chapter: response.chapter.clone(),
                                resume_unsaved: false,
                            });
                        }
                        self.catalog.refresh();
                    }
                    Err(error) => {
                        runtime_log::log_error(format!(
                            "[launcher-psd] import failed: {}",
                            error.log_message
                        ));
                        self.queued_open = None;
                        self.status = ImportStatus::Error(error.user_message);
                    }
                }
                self.open_after_save_requested = false;
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.pending_import = Some(pending);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.open_after_save_requested = false;
                self.queued_open = None;
                self.status =
                    ImportStatus::Error("Фоновый импорт PSD неожиданно завершился.".to_string());
            }
        }
    }

    fn poll_catalog(&mut self, ctx: &Context) {
        match self.catalog.poll(ctx) {
            Some(ProjectCatalogEvent::Loaded(snapshot)) => {
                self.catalog_snapshot = snapshot;
                self.catalog_error = None;
                if matches!(self.status, ImportStatus::LoadingCatalog) {
                    self.status = ImportStatus::Idle;
                }
            }
            Some(ProjectCatalogEvent::Failed {
                user_message,
                log_message,
            }) => {
                runtime_log::log_error(format!(
                    "[launcher-psd] project catalog load failed: {log_message}"
                ));
                self.catalog_error = Some(user_message.clone());
                self.status = ImportStatus::Error(user_message);
            }
            Some(ProjectCatalogEvent::WorkerDisconnected) => {
                self.catalog_error =
                    Some("Фоновая загрузка списка проектов неожиданно завершилась.".to_string());
                self.status =
                    ImportStatus::Error("Не удалось обновить список проектов.".to_string());
            }
            None => {}
        }
    }

    fn swap_source_and_clean(&mut self) {
        let mut source_by_page = HashMap::new();
        let mut clean_by_page = HashMap::new();
        for (index, row) in self.rows.iter().enumerate() {
            match row.import_type {
                LayerImportType::Source => {
                    source_by_page.insert(row.page, index);
                }
                LayerImportType::Clean => {
                    clean_by_page.insert(row.page, index);
                }
                LayerImportType::Skip => {}
            }
        }

        for page in source_by_page
            .keys()
            .filter(|page| clean_by_page.contains_key(page))
            .copied()
            .collect::<Vec<_>>()
        {
            if let (Some(source_index), Some(clean_index)) =
                (source_by_page.get(&page), clean_by_page.get(&page))
            {
                self.rows[*source_index].import_type = LayerImportType::Clean;
                self.rows[*clean_index].import_type = LayerImportType::Source;
            }
        }
        clear_success_status(&mut self.status);
    }

    fn validate_row_change(&self, row_index: usize) -> Result<(), String> {
        let row = self
            .rows
            .get(row_index)
            .ok_or_else(|| "Строка PSD больше не существует.".to_string())?;
        if row.import_type == LayerImportType::Skip {
            return Ok(());
        }

        for (other_index, other) in self.rows.iter().enumerate() {
            if other_index == row_index
                || other.import_type == LayerImportType::Skip
                || other.page != row.page
            {
                continue;
            }
            if other.size != row.size {
                return Err(format!(
                    "Нельзя назначить страницу {}: размер {}x{} не совпадает с уже назначенными на этой странице ({}x{}).",
                    row.page, row.size.0, row.size.1, other.size.0, other.size.1
                ));
            }
            if other.import_type == row.import_type {
                return Err(format!(
                    "На странице {} уже есть '{}'.",
                    row.page,
                    row.import_type.label()
                ));
            }
        }
        Ok(())
    }

    fn commit_page_input(&mut self, row_index: usize) -> Result<(), String> {
        let row = self
            .rows
            .get(row_index)
            .ok_or_else(|| "Строка PSD больше не существует.".to_string())?;
        let Some(input) = self.page_inputs.get(&row.row_key) else {
            return Ok(());
        };
        let trimmed = input.trim();
        let page = trimmed
            .parse::<u32>()
            .map_err(|_| "Страница должна быть положительным числом.".to_string())?;
        if page == 0 {
            return Err("Страница должна быть не меньше 1.".to_string());
        }
        if let Some(row) = self.rows.get_mut(row_index) {
            row.page = page;
        }
        Ok(())
    }

    fn sync_page_input(&mut self, row_index: usize) {
        if let Some(row) = self.rows.get(row_index) {
            self.page_inputs
                .insert(row.row_key.clone(), row.page.to_string());
        }
    }

    fn rebuild_page_editors(&mut self) {
        self.page_inputs.clear();
        self.page_combos.clear();
        for row in &self.rows {
            self.page_inputs
                .insert(row.row_key.clone(), row.page.to_string());
            self.page_combos.insert(
                row.row_key.clone(),
                EditableComboBox::new(("psd_import_page", row.row_key.clone()))
                    .with_hint_text("№")
                    .with_desired_text_width(ROW_PAGE_WIDTH - 32.0)
                    .with_popup_max_height(180.0),
            );
        }
    }

    fn validate_all_rows(&self) -> Result<(), String> {
        let mut imported_rows = 0usize;
        let mut pages_with_source = BTreeSet::new();
        let mut pages_with_clean = BTreeSet::new();
        let mut page_sizes = HashMap::<u32, (u32, u32)>::new();

        for row in &self.rows {
            if row.import_type == LayerImportType::Skip {
                continue;
            }
            if row.page == 0 {
                return Err("Страница должна быть не меньше 1.".to_string());
            }
            imported_rows += 1;
            if let Some(existing) = page_sizes.get(&row.page) {
                if *existing != row.size {
                    return Err(format!(
                        "На странице {} есть импортируемые слои разных размеров.",
                        row.page
                    ));
                }
            } else {
                page_sizes.insert(row.page, row.size);
            }
            match row.import_type {
                LayerImportType::Source => {
                    if !pages_with_source.insert(row.page) {
                        return Err(format!(
                            "На странице {} назначено несколько '{}'.",
                            row.page, TYPE_SOURCE
                        ));
                    }
                }
                LayerImportType::Clean => {
                    if !pages_with_clean.insert(row.page) {
                        return Err(format!(
                            "На странице {} назначено несколько '{}'.",
                            row.page, TYPE_CLEAN
                        ));
                    }
                }
                LayerImportType::Skip => {}
            }
        }

        if imported_rows == 0 {
            return Err("Нечего импортировать: все строки отключены.".to_string());
        }

        Ok(())
    }

    fn can_import(&self) -> bool {
        !self.rows.is_empty()
            && !self.is_busy()
            && !self.title_input.trim().is_empty()
            && !self.chapter_input.trim().is_empty()
    }

    fn is_busy(&self) -> bool {
        self.pending_scan.is_some()
            || self.pending_preview.is_some()
            || self.pending_import.is_some()
    }

    fn format_row_title(&self, row_index: usize) -> String {
        self.rows
            .get(row_index)
            .map(Self::format_row_title_for)
            .unwrap_or_default()
    }

    fn format_row_title_for(row: &PsdLayerRow) -> String {
        format!("{}: {}", row.file_name, row.layer_title)
    }

    fn select_row(&mut self, row_index: usize) {
        let previous_page = self
            .selected_row
            .and_then(|index| self.rows.get(index))
            .map(|row| row.page);
        let new_page = self.rows.get(row_index).map(|row| row.page);
        if let Some(page) = previous_page {
            self.preview_scroll_by_page
                .insert(page, self.preview_scroll_y);
        }
        self.preview_scroll_y = new_page
            .and_then(|page| self.preview_scroll_by_page.get(&page).copied())
            .unwrap_or(0.0);
        self.selected_row = Some(row_index);
        self.request_selected_preview();
    }
}

impl LayerImportType {
    fn label(self) -> &'static str {
        match self {
            Self::Skip => TYPE_SKIP,
            Self::Source => TYPE_SOURCE,
            Self::Clean => TYPE_CLEAN,
        }
    }
}

/// Build the import rows for a single PSD document.
///
/// Normal documents yield one row per leaf raster layer. Flattened documents that
/// produce no usable layer rows (no layers at all) fall back to a single `Composite`
/// row representing the whole-document merged image, typed as the source page.
fn build_document_rows(
    document: &LoadedPsdDocument,
    document_index: usize,
    warnings: &mut Vec<String>,
) -> Vec<PsdLayerRow> {
    // `document.layers` is already flattened (groups expanded) and ordered
    // top-to-bottom, so no `reverse()` is needed before `auto_assign_types`.
    let mut document_rows = Vec::new();
    for (layer_index, layer) in document.layers.iter().enumerate() {
        document_rows.push(PsdLayerRow {
            row_key: format!("{document_index}:{layer_index}"),
            file_name: document.file_name.clone(),
            page: document.page,
            layer_title: layer.name.clone(),
            size: (layer.image.width, layer.image.height),
            import_type: LayerImportType::Skip,
            document_index,
            source: LayerSource::Layer(layer_index),
        });
    }

    if document_rows.is_empty() {
        // Flattened PSD: no usable layers. Fall back to the merged composite image
        // so the document still imports as a single source page.
        let Some(composite) = document.composite.as_ref() else {
            warnings.push(format!(
                "{}: плоский PSD без слоёв и без растровых данных, пропущен.",
                document.file_name
            ));
            return Vec::new();
        };
        return vec![PsdLayerRow {
            row_key: format!("{document_index}:composite"),
            file_name: document.file_name.clone(),
            page: document.page,
            layer_title: TYPE_SOURCE.to_string(),
            size: (composite.width, composite.height),
            // Explicitly typed as source; this is the only row for the document so
            // `auto_assign_types` (which only acts on exactly one source/clean pair)
            // would never touch it anyway.
            import_type: LayerImportType::Source,
            document_index,
            source: LayerSource::Composite,
        }];
    }

    auto_assign_types(&mut document_rows);
    document_rows
}

fn run_scan_worker(request: ScanRequest) -> Result<ScanResponse, WorkerError> {
    let mut warnings = Vec::new();
    let documents = match request {
        ScanRequest::Files { paths } => scan_selected_files(paths, &mut warnings)?,
        ScanRequest::Folder { path } => scan_folder(path, &mut warnings)?,
    };

    if documents.is_empty() {
        return Err(WorkerError {
            user_message: "PSD файлы не найдены.".to_string(),
            log_message: "scan produced zero PSD documents".to_string(),
        });
    }

    let available_pages = documents
        .iter()
        .map(|document| document.page)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let source_summary = if documents.len() == 1 {
        documents[0].file_name.clone()
    } else {
        format!("Загружено PSD: {}", documents.len())
    };

    let shared_documents = Arc::new(documents);
    let mut rows = Vec::new();
    for (document_index, document) in shared_documents.iter().enumerate() {
        rows.extend(build_document_rows(document, document_index, &mut warnings));
    }
    sort_rows_for_table(&mut rows);

    Ok(ScanResponse {
        source_summary,
        warnings,
        available_pages,
        rows,
        documents: shared_documents,
    })
}

fn scan_selected_files(
    paths: Vec<PathBuf>,
    warnings: &mut Vec<String>,
) -> Result<Vec<LoadedPsdDocument>, WorkerError> {
    if paths.is_empty() {
        return Err(WorkerError {
            user_message: "Не выбраны PSD файлы.".to_string(),
            log_message: "file picker returned no paths".to_string(),
        });
    }

    let mut psd_files = Vec::new();
    let mut archive_files = Vec::new();
    let mut unsupported = Vec::new();
    for path in paths {
        match lowercase_ext(&path).as_deref() {
            Some("psd") => psd_files.push(path),
            Some("zip" | "rar") => archive_files.push(path),
            Some("psb") => warnings.push(format!(
                "{}: формат PSB не поддерживается Rust importer и пропущен.",
                path.display()
            )),
            _ => unsupported.push(path),
        }
    }

    if !unsupported.is_empty() {
        return Err(WorkerError {
            user_message: "Выбраны неподдерживаемые файлы.".to_string(),
            log_message: format!(
                "unsupported files: {}",
                unsupported
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }

    if !archive_files.is_empty() && !psd_files.is_empty() {
        return Err(WorkerError {
            user_message: "Можно выбрать либо PSD файлы, либо один архив.".to_string(),
            log_message: "mixed PSD files and archives in one selection".to_string(),
        });
    }

    if archive_files.len() > 1 {
        return Err(WorkerError {
            user_message: "Можно выбрать только один архив.".to_string(),
            log_message: "multiple archives selected".to_string(),
        });
    }

    if let Some(archive_path) = archive_files.into_iter().next() {
        return scan_archive(&archive_path, warnings);
    }

    let mut documents = Vec::new();
    let mut sorted = psd_files;
    sorted.sort_by_key(|path| path.as_os_str().to_owned());
    for path in sorted {
        let bytes = fs::read(&path).map_err(|err| WorkerError {
            user_message: "Не удалось открыть PSD файл.".to_string(),
            log_message: format!("failed to read '{}': {err}", path.display()),
        })?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown.psd")
            .to_string();
        documents.push(load_document_from_bytes(
            &file_name,
            extract_page_from_name(&file_name).unwrap_or(1),
            bytes,
            warnings,
        )?);
    }
    Ok(documents)
}

fn scan_folder(
    path: PathBuf,
    warnings: &mut Vec<String>,
) -> Result<Vec<LoadedPsdDocument>, WorkerError> {
    let mut found = Vec::new();
    for entry in walk_dir(&path)? {
        match lowercase_ext(&entry).as_deref() {
            Some("psd") => found.push(entry),
            Some("psb") => warnings.push(format!(
                "{}: формат PSB не поддерживается Rust importer и пропущен.",
                entry.display()
            )),
            _ => {}
        }
    }

    if found.is_empty() {
        return Err(WorkerError {
            user_message: "В выбранной папке PSD файлы не найдены.".to_string(),
            log_message: format!("no psd files in '{}'", path.display()),
        });
    }

    found.sort_by_key(|entry| entry.as_os_str().to_owned());
    let mut documents = Vec::new();
    for entry in found {
        let bytes = fs::read(&entry).map_err(|err| WorkerError {
            user_message: "Не удалось открыть PSD файл.".to_string(),
            log_message: format!("failed to read '{}': {err}", entry.display()),
        })?;
        let file_name = entry
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown.psd")
            .to_string();
        documents.push(load_document_from_bytes(
            &file_name,
            extract_page_from_name(&file_name).unwrap_or(1),
            bytes,
            warnings,
        )?);
    }
    Ok(documents)
}

fn scan_zip_archive(
    zip_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<LoadedPsdDocument>, WorkerError> {
    let file = File::open(zip_path).map_err(|err| WorkerError {
        user_message: "Не удалось открыть ZIP архив.".to_string(),
        log_message: format!("failed to open zip '{}': {err}", zip_path.display()),
    })?;
    let mut archive = ZipArchive::new(file).map_err(|err| WorkerError {
        user_message: "Не удалось прочитать ZIP архив.".to_string(),
        log_message: format!("failed to parse zip '{}': {err}", zip_path.display()),
    })?;

    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|err| WorkerError {
            user_message: "Не удалось прочитать ZIP архив.".to_string(),
            log_message: format!("failed to read zip entry {index}: {err}"),
        })?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().replace('\\', "/");
        if name.to_ascii_lowercase().ends_with(".psb") {
            warnings.push(format!(
                "{}: файл '{}' внутри ZIP имеет формат PSB и пропущен.",
                zip_path.display(),
                name
            ));
            continue;
        }
        if !name.to_ascii_lowercase().ends_with(".psd") {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|err| WorkerError {
            user_message: "Не удалось распаковать PSD из ZIP.".to_string(),
            log_message: format!("failed to read zip member '{name}': {err}"),
        })?;
        entries.push((name, bytes));
    }

    if entries.is_empty() {
        return Err(WorkerError {
            user_message: "В ZIP архиве PSD файлы не найдены.".to_string(),
            log_message: format!("no psd entries found in '{}'", zip_path.display()),
        });
    }

    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut documents = Vec::new();
    for (name, bytes) in entries {
        let file_name = Path::new(&name)
            .file_name()
            .and_then(|item| item.to_str())
            .unwrap_or("unknown.psd")
            .to_string();
        documents.push(load_document_from_bytes(
            &file_name,
            extract_page_from_name(&name).unwrap_or(1),
            bytes,
            warnings,
        )?);
    }
    Ok(documents)
}

fn scan_rar_archive(
    rar_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<LoadedPsdDocument>, WorkerError> {
    let extract_dir = create_temp_extract_dir()?;
    let result = scan_rar_archive_from_temp_dir(rar_path, &extract_dir, warnings);
    if let Err(err) = fs::remove_dir_all(&extract_dir) {
        runtime_log::log_warn(format!(
            "[launcher-psd] failed to remove temporary rar dir '{}': {err}",
            extract_dir.display()
        ));
    }
    result
}

fn scan_rar_archive_from_temp_dir(
    rar_path: &Path,
    extract_dir: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<LoadedPsdDocument>, WorkerError> {
    extract_archive_with_commands(
        rar_path,
        extract_dir,
        &["rar", "unrar", "unar", "7z", "7za"],
    )?;

    let mut found = Vec::new();
    for entry in walk_dir(extract_dir)? {
        match lowercase_ext(&entry).as_deref() {
            Some("psd") => found.push(entry),
            Some("psb") => warnings.push(format!(
                "{}: файл '{}' внутри RAR имеет формат PSB и пропущен.",
                rar_path.display(),
                entry.display()
            )),
            _ => {}
        }
    }

    if found.is_empty() {
        return Err(WorkerError {
            user_message: "В RAR архиве PSD файлы не найдены.".to_string(),
            log_message: format!("no psd entries found in rar '{}'", rar_path.display()),
        });
    }

    found.sort_by_key(|entry| entry.as_os_str().to_owned());
    let mut documents = Vec::new();
    for entry in found {
        let bytes = fs::read(&entry).map_err(|err| WorkerError {
            user_message: "Не удалось распаковать PSD из RAR.".to_string(),
            log_message: format!(
                "failed to read extracted rar member '{}': {err}",
                entry.display()
            ),
        })?;
        let relative_name = entry
            .strip_prefix(extract_dir)
            .ok()
            .unwrap_or(entry.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        let file_name = Path::new(&relative_name)
            .file_name()
            .and_then(|item| item.to_str())
            .unwrap_or("unknown.psd")
            .to_string();
        documents.push(load_document_from_bytes(
            &file_name,
            extract_page_from_name(&relative_name).unwrap_or(1),
            bytes,
            warnings,
        )?);
    }
    Ok(documents)
}

fn scan_archive(
    archive_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<LoadedPsdDocument>, WorkerError> {
    match lowercase_ext(archive_path).as_deref() {
        Some("zip") => scan_zip_archive(archive_path, warnings),
        Some("rar") => scan_rar_archive(archive_path, warnings),
        _ => Err(WorkerError {
            user_message: "Поддерживаются только ZIP и RAR архивы.".to_string(),
            log_message: format!("unsupported archive '{}'", archive_path.display()),
        }),
    }
}

fn create_temp_extract_dir() -> Result<PathBuf, WorkerError> {
    let timestamp = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!(
        "mangafucker_psd_import_{}_{}",
        std::process::id(),
        timestamp
    ));
    fs::create_dir_all(&dir).map_err(|err| WorkerError {
        user_message: "Не удалось подготовить временную папку для архива.".to_string(),
        log_message: format!("failed to create temp dir '{}': {err}", dir.display()),
    })?;
    Ok(dir)
}

/// Web stub: archive extraction shells out to external tools (`rar`/`7z`/…) via
/// `std::process::Command`, which cannot run in the browser. Returns a clear error.
#[cfg(target_arch = "wasm32")]
fn extract_archive_with_commands(
    path: &Path,
    _output_dir: &Path,
    _commands: &[&str],
) -> Result<(), WorkerError> {
    Err(WorkerError {
        user_message: "Распаковка архивов недоступна в веб-версии.".to_string(),
        log_message: format!(
            "archive extraction via external tools is unavailable on the web build for '{}'",
            path.display()
        ),
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn extract_archive_with_commands(
    path: &Path,
    output_dir: &Path,
    commands: &[&str],
) -> Result<(), WorkerError> {
    for command in commands {
        let status = if *command == "rar" || *command == "unrar" {
            Command::new(command)
                .arg("x")
                .arg("-o+")
                .arg(path)
                .arg(output_dir)
                .status()
        } else if *command == "unar" {
            Command::new(command)
                .arg("-q")
                .arg("-f")
                .arg("-no-recursion")
                .arg("-output-directory")
                .arg(output_dir)
                .arg(path)
                .status()
        } else {
            Command::new(command)
                .arg("x")
                .arg("-y")
                .arg(format!("-o{}", output_dir.display()))
                .arg(path)
                .status()
        };
        match status {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => {
                runtime_log::log_warn(format!(
                    "[launcher-psd] extractor '{}' exited with status {} for '{}'",
                    command,
                    status,
                    path.display()
                ));
            }
            Err(err) => {
                runtime_log::log_warn(format!(
                    "[launcher-psd] failed to run extractor '{}' for '{}': {err}",
                    command,
                    path.display()
                ));
            }
        }
    }

    Err(WorkerError {
        user_message:
            "Не удалось распаковать архив. Нужен совместимый `rar`, `unrar`, `unar`, `7z` или `7za`."
                .to_string(),
        log_message: format!("no extractor succeeded for '{}'", path.display()),
    })
}

fn load_document_from_bytes(
    file_name: &str,
    page: u32,
    bytes: Vec<u8>,
    warnings: &mut Vec<String>,
) -> Result<LoadedPsdDocument, WorkerError> {
    // `use_image_data` keeps decoded pixels as raw 8-bit RGBA byte buffers (16/32-bit
    // samples are down-converted to their top byte). `skip_composite_image_data`
    // together with ag-psd's gating means the merged composite is only decoded for
    // flattened PSDs (documents without a layer section), so layered documents never
    // pay for a composite we would immediately drop.
    let options = ReadOptions {
        use_image_data: Some(true),
        skip_composite_image_data: Some(true),
        skip_thumbnail: Some(true),
        skip_linked_files_data: Some(true),
        ..Default::default()
    };
    let psd = read_psd(&bytes, &options).map_err(|err| WorkerError {
        user_message: format!("Не удалось прочитать PSD '{}'.", file_name),
        log_message: format!("failed to parse psd '{file_name}': {err}"),
    })?;

    let mut layers = Vec::new();
    let mut has_groups = false;
    if let Some(children) = psd.children.as_ref() {
        collect_leaf_layers(children, &mut layers, &mut has_groups);
    }
    if has_groups {
        warnings.push(format!(
            "{}: обнаружены группы. Импорт работает только с простыми плоскими слоями; порядок и иерархия групп могут быть неточными.",
            file_name
        ));
    }

    // Keep the composite only as a fallback for flattened PSDs; when real layers
    // exist it is never used and ag-psd will not even have decoded it.
    let composite = if layers.is_empty() {
        psd.image_data
            .as_ref()
            .and_then(decoded_image_from_pixel_data)
    } else {
        None
    };

    Ok(LoadedPsdDocument {
        file_name: file_name.to_string(),
        page,
        layers,
        composite,
    })
}

/// Recursively walk ag-psd's layer tree, collecting leaf raster layers (those with
/// decoded pixel data) into `out` while flagging whether any groups were present.
///
/// `psd.children` is ordered top-to-bottom and groups are expanded in place, so the
/// resulting list preserves visual top-to-bottom order.
fn collect_leaf_layers(layers: &[Layer], out: &mut Vec<DecodedLayer>, has_groups: &mut bool) {
    for layer in layers {
        if let Some(children) = layer.children.as_ref() {
            *has_groups = true;
            collect_leaf_layers(children, out, has_groups);
            continue;
        }
        // Empty/zero-size layers (text, adjustment, hidden empties) carry no usable
        // pixels and are silently skipped.
        let Some(image) = layer
            .image_data
            .as_ref()
            .and_then(decoded_image_from_pixel_data)
        else {
            continue;
        };
        let name = layer
            .additional_info
            .name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Без имени".to_string());
        out.push(DecodedLayer { name, image });
    }
}

/// Convert an ag-psd `PixelData` (8-bit RGBA after `use_image_data`) into a
/// `DecodedImage`, rejecting empty buffers and trimming any trailing padding so the
/// data is exactly `width * height * 4` bytes.
fn decoded_image_from_pixel_data(pixel_data: &PixelData) -> Option<DecodedImage> {
    if pixel_data.width == 0 || pixel_data.height == 0 {
        return None;
    }
    let expected = (pixel_data.width as usize)
        .checked_mul(pixel_data.height as usize)?
        .checked_mul(4)?;
    if pixel_data.data.len() < expected {
        return None;
    }
    Some(DecodedImage {
        width: pixel_data.width,
        height: pixel_data.height,
        data: Arc::new(pixel_data.data[..expected].to_vec()),
    })
}

fn auto_assign_types(rows: &mut [PsdLayerRow]) {
    let mut by_page_and_size: HashMap<(u32, (u32, u32)), Vec<usize>> = HashMap::new();
    for (index, row) in rows.iter().enumerate() {
        by_page_and_size
            .entry((row.page, row.size))
            .or_default()
            .push(index);
    }

    let pairs = by_page_and_size
        .values()
        .filter(|indices| indices.len() == 2)
        .cloned()
        .collect::<Vec<_>>();

    if pairs.len() != 1 {
        return;
    }

    let mut indices = pairs[0].clone();
    indices.sort_unstable();
    if let Some(upper) = rows.get_mut(indices[0]) {
        upper.import_type = LayerImportType::Source;
    }
    if let Some(lower) = rows.get_mut(indices[1]) {
        lower.import_type = LayerImportType::Clean;
    }
}

fn sort_rows_for_table(rows: &mut [PsdLayerRow]) {
    rows.sort_by(|left, right| {
        natural_name_cmp(&left.file_name, &right.file_name)
            .then_with(|| left.layer_title.cmp(&right.layer_title))
    });
}

fn has_fully_skipped_document(rows: &[PsdLayerRow]) -> bool {
    let mut has_imported_row_by_document = HashMap::<usize, bool>::new();
    for row in rows {
        let has_imported_row = row.import_type != LayerImportType::Skip;
        has_imported_row_by_document
            .entry(row.document_index)
            .and_modify(|existing| *existing |= has_imported_row)
            .or_insert(has_imported_row);
    }
    has_imported_row_by_document
        .values()
        .any(|has_imported_row| !has_imported_row)
}

fn natural_name_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left_bytes = left.as_bytes();
    let right_bytes = right.as_bytes();
    let mut left_index = 0usize;
    let mut right_index = 0usize;

    while left_index < left_bytes.len() && right_index < right_bytes.len() {
        let left_byte = left_bytes[left_index];
        let right_byte = right_bytes[right_index];
        if left_byte.is_ascii_digit() && right_byte.is_ascii_digit() {
            let ordering =
                digit_run_cmp(left_bytes, &mut left_index, right_bytes, &mut right_index);
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        } else {
            let ordering = left_byte.cmp(&right_byte);
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
            left_index += 1;
            right_index += 1;
        }
    }

    left_bytes.len().cmp(&right_bytes.len())
}

fn digit_run_cmp(
    left: &[u8],
    left_index: &mut usize,
    right: &[u8],
    right_index: &mut usize,
) -> std::cmp::Ordering {
    let left_start = *left_index;
    let right_start = *right_index;
    while *left_index < left.len() && left[*left_index].is_ascii_digit() {
        *left_index += 1;
    }
    while *right_index < right.len() && right[*right_index].is_ascii_digit() {
        *right_index += 1;
    }

    let left_significant = first_significant_digit(left, left_start, *left_index);
    let right_significant = first_significant_digit(right, right_start, *right_index);
    let left_significant_len = *left_index - left_significant;
    let right_significant_len = *right_index - right_significant;

    left_significant_len
        .cmp(&right_significant_len)
        .then_with(|| {
            left[left_significant..*left_index].cmp(&right[right_significant..*right_index])
        })
        .then_with(|| (*left_index - left_start).cmp(&(*right_index - right_start)))
}

fn first_significant_digit(bytes: &[u8], start: usize, end: usize) -> usize {
    let mut index = start;
    while index < end && bytes[index] == b'0' {
        index += 1;
    }
    index
}

fn render_preview_image(
    documents: &Arc<Vec<LoadedPsdDocument>>,
    document_index: usize,
    source: LayerSource,
) -> Result<Arc<RgbaImage>, WorkerError> {
    let rgba = render_layer_rgba(documents, document_index, source)?;
    Ok(Arc::new(rgba))
}

fn run_import_worker(
    projects_root: &Path,
    title: &str,
    chapter: &str,
    documents: Arc<Vec<LoadedPsdDocument>>,
    assignments: Vec<ImportAssignment>,
) -> Result<ImportResponse, WorkerError> {
    let chapter_dir = projects_root.join(title).join(chapter);
    let src_dir = chapter_dir.join("src");
    let clean_dir = chapter_dir.join("clean_layers");
    fs::create_dir_all(&src_dir).map_err(|err| WorkerError {
        user_message: "Не удалось создать папку src.".to_string(),
        log_message: format!("failed to create '{}': {err}", src_dir.display()),
    })?;
    fs::create_dir_all(&clean_dir).map_err(|err| WorkerError {
        user_message: "Не удалось создать папку clean_layers.".to_string(),
        log_message: format!("failed to create '{}': {err}", clean_dir.display()),
    })?;

    let mut by_page: HashMap<u32, HashMap<LayerImportType, ImportAssignment>> = HashMap::new();
    for assignment in assignments {
        by_page
            .entry(assignment.page)
            .or_default()
            .insert(assignment.import_type, assignment);
    }

    let mut pages = by_page.keys().copied().collect::<Vec<_>>();
    pages.sort_unstable();

    let mut saved_pages = 0usize;
    for page in pages {
        let Some(entries) = by_page.get(&page) else {
            continue;
        };
        let filename = import_filename_for_page(page)?;
        if let Some(source) = entries.get(&LayerImportType::Source) {
            let image = render_layer_rgba(&documents, source.document_index, source.source)?;
            image
                .save(src_dir.join(&filename))
                .map_err(|err| WorkerError {
                    user_message: "Не удалось сохранить исходник страницы.".to_string(),
                    log_message: format!(
                        "failed to save source page {page} as '{filename}': {err}"
                    ),
                })?;
        }
        if let Some(clean) = entries.get(&LayerImportType::Clean) {
            let image = render_layer_rgba(&documents, clean.document_index, clean.source)?;
            image
                .save(clean_dir.join(&filename))
                .map_err(|err| WorkerError {
                    user_message: "Не удалось сохранить клин страницы.".to_string(),
                    log_message: format!("failed to save clean page {page} as '{filename}': {err}"),
                })?;
        }
        saved_pages += 1;
    }

    Ok(ImportResponse {
        project_dir: chapter_dir,
        saved_pages,
        title: title.to_string(),
        chapter: chapter.to_string(),
    })
}

fn import_filename_for_page(page: u32) -> Result<String, WorkerError> {
    let Some(file_index) = page.checked_sub(1) else {
        return Err(WorkerError {
            user_message: "Номер страницы должен быть не меньше 1.".to_string(),
            log_message: format!("invalid one-based page number: {page}"),
        });
    };
    Ok(format!("{file_index:03}.png"))
}

fn render_layer_rgba(
    documents: &Arc<Vec<LoadedPsdDocument>>,
    document_index: usize,
    source: LayerSource,
) -> Result<RgbaImage, WorkerError> {
    let document = documents.get(document_index).ok_or_else(|| WorkerError {
        user_message: "PSD документ больше не доступен.".to_string(),
        log_message: format!("missing document index {document_index}"),
    })?;

    let (image, label) = match source {
        LayerSource::Layer(layer_index) => {
            let layer = document.layers.get(layer_index).ok_or_else(|| WorkerError {
                user_message: "PSD слой больше не доступен.".to_string(),
                log_message: format!(
                    "missing layer index {layer_index} in document '{}'",
                    document.file_name
                ),
            })?;
            (&layer.image, format!("layer '{}'", layer.name))
        }
        LayerSource::Composite => {
            let composite = document.composite.as_ref().ok_or_else(|| WorkerError {
                user_message: "Плоский PSD не содержит растровых данных.".to_string(),
                log_message: format!(
                    "empty composite rgba buffer for document '{}'",
                    document.file_name
                ),
            })?;
            (composite, "composite".to_string())
        }
    };

    let (width, height) = (image.width, image.height);
    // `Arc<Vec<u8>>` is shared with the row table / preview cache, so clone the bytes
    // for `RgbaImage`, which needs to own its buffer.
    let pixels = image.data.as_ref().clone();
    RgbaImage::from_raw(width, height, pixels).ok_or_else(|| WorkerError {
        user_message: "Не удалось собрать растровый слой PSD.".to_string(),
        log_message: format!(
            "invalid rgba buffer for document '{}' {label}",
            document.file_name
        ),
    })
}

fn build_preview_texture_tiles(
    ctx: &Context,
    row_key: &str,
    image: &Arc<RgbaImage>,
) -> Result<Vec<PreviewTextureTile>, WorkerError> {
    let width = image.width() as usize;
    let height = image.height() as usize;
    if width == 0 || height == 0 {
        return Err(WorkerError {
            user_message: "PSD слой пустой и не может быть показан.".to_string(),
            log_message: "preview image has zero width or height".to_string(),
        });
    }

    let mut tiles = Vec::new();
    let mut tile_index = 0usize;
    let mut start_y = 0u32;

    // The full raster stays in RAM; tiling exists only to satisfy GPU texture limits.
    while start_y < image.height() {
        let tile_height = (image.height() - start_y).min(PREVIEW_TILE_MAX_HEIGHT as u32);
        let raw = copy_rgba_tile_rows(image, 0, start_y, image.width(), tile_height);
        let color_image = ColorImage::from_rgba_unmultiplied([width, tile_height as usize], &raw);
        let texture = ctx.load_texture(
            format!("launcher-psd-preview-{row_key}-{tile_index}"),
            color_image,
            TextureOptions::LINEAR,
        );
        tiles.push(PreviewTextureTile {
            texture,
            origin_px: egui::vec2(0.0, start_y as f32),
            size: egui::vec2(width as f32, tile_height as f32),
        });
        tile_index += 1;
        start_y += tile_height;
    }

    Ok(tiles)
}

fn copy_rgba_tile_rows(
    rgba: &RgbaImage,
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

fn walk_dir(root: &Path) -> Result<Vec<PathBuf>, WorkerError> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|err| WorkerError {
            user_message: "Не удалось прочитать папку с PSD.".to_string(),
            log_message: format!("failed to read dir '{}': {err}", dir.display()),
        })?;
        for entry_result in entries {
            let entry = entry_result.map_err(|err| WorkerError {
                user_message: "Не удалось прочитать папку с PSD.".to_string(),
                log_message: format!("failed to read dir entry in '{}': {err}", dir.display()),
            })?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    Ok(files)
}

fn lowercase_ext(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|item| item.to_str())
        .map(|item| item.to_ascii_lowercase())
}

fn extract_page_from_name(name: &str) -> Option<u32> {
    let path = name.replace('\\', "/");
    let stem = Path::new(&path).file_stem()?.to_str()?;
    stem.parse::<u32>().ok()
}

fn preview_scale_to_width(source_size: egui::Vec2, available_width: f32) -> f32 {
    if source_size.x <= 0.0 || available_width <= 0.0 {
        return 1.0;
    }
    (available_width / source_size.x).max(0.05)
}

fn clear_success_status(status: &mut ImportStatus) {
    if matches!(status, ImportStatus::Success(_)) {
        *status = ImportStatus::Idle;
    }
}

fn button_sized(ui: &mut Ui, label: &str, size: egui::Vec2, enabled: bool) -> egui::Response {
    ui.add_enabled(enabled, Button::new(label).min_size(size))
}

fn standard_dark_style() -> egui::Style {
    egui::Style {
        visuals: egui::Visuals::dark(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{LayerImportType, LayerSource, PsdLayerRow, import_filename_for_page};

    #[test]
    fn import_filename_preserves_page_gaps() {
        let page_five = match import_filename_for_page(5) {
            Ok(filename) => filename,
            Err(error) => panic!("{}", error.log_message),
        };
        let page_seven = match import_filename_for_page(7) {
            Ok(filename) => filename,
            Err(error) => panic!("{}", error.log_message),
        };

        assert_eq!(page_five, "004.png");
        assert_eq!(page_seven, "006.png");
    }

    #[test]
    fn import_filename_rejects_zero_page() {
        assert!(import_filename_for_page(0).is_err());
    }

    #[test]
    fn table_rows_sort_by_file_then_layer_name() {
        let mut rows = vec![
            test_row("10.psd", "clean"),
            test_row("2.psd", "clean"),
            test_row("1.psd", "clean"),
            test_row("b.psd", "clean"),
            test_row("a.psd", "source"),
            test_row("a.psd", "clean"),
        ];

        super::sort_rows_for_table(&mut rows);

        let names = rows
            .iter()
            .map(|row| (row.file_name.as_str(), row.layer_title.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                ("1.psd", "clean"),
                ("2.psd", "clean"),
                ("10.psd", "clean"),
                ("a.psd", "clean"),
                ("a.psd", "source"),
                ("b.psd", "clean"),
            ]
        );
    }

    #[test]
    fn fully_skipped_document_warning_detects_unassigned_psd() {
        let rows = vec![
            test_row_with_document("assigned.psd", "source", 0, LayerImportType::Source),
            test_row_with_document("assigned.psd", "clean", 0, LayerImportType::Clean),
            test_row_with_document("unassigned.psd", "layer", 1, LayerImportType::Skip),
        ];

        assert!(super::has_fully_skipped_document(&rows));
    }

    #[test]
    fn fully_skipped_document_warning_ignores_partially_assigned_psd() {
        let rows = vec![
            test_row_with_document("assigned.psd", "source", 0, LayerImportType::Source),
            test_row_with_document("assigned.psd", "extra", 0, LayerImportType::Skip),
        ];

        assert!(!super::has_fully_skipped_document(&rows));
    }

    fn test_row(file_name: &str, layer_title: &str) -> PsdLayerRow {
        test_row_with_document(file_name, layer_title, 0, LayerImportType::Skip)
    }

    fn test_row_with_document(
        file_name: &str,
        layer_title: &str,
        document_index: usize,
        import_type: LayerImportType,
    ) -> PsdLayerRow {
        PsdLayerRow {
            row_key: format!("{file_name}:{layer_title}"),
            file_name: file_name.to_string(),
            page: 1,
            layer_title: layer_title.to_string(),
            size: (1, 1),
            import_type,
            document_index,
            source: LayerSource::Layer(0),
        }
    }

    #[test]
    fn flattened_psd_yields_single_source_composite_row() {
        use ag_psd::psd::{ColorMode, PixelData, Psd as AgPsd, WriteOptions};
        use ag_psd::write_psd;

        let width = 3u32;
        let height = 2u32;
        // Opaque red composite, RGBA8.
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            data.extend_from_slice(&[255, 0, 0, 255]);
        }

        let psd = AgPsd {
            width: width as f64,
            height: height as f64,
            color_mode: Some(ColorMode::Rgb),
            bits_per_channel: Some(8.0),
            // Flattened: no layer section, only the merged composite.
            children: None,
            image_data: Some(PixelData {
                width,
                height,
                data,
            }),
            ..Default::default()
        };
        let bytes = write_psd(&psd, &WriteOptions::default());

        let mut warnings = Vec::new();
        let document = super::load_document_from_bytes("001.psd", 1, bytes, &mut warnings)
            .expect("flattened psd loads");
        assert!(
            document.layers.is_empty(),
            "fixture is flattened (no layers)"
        );
        assert!(
            document.composite.is_some(),
            "flattened psd keeps its composite"
        );

        let rows = super::build_document_rows(&document, 0, &mut warnings);

        assert_eq!(rows.len(), 1, "flattened psd yields exactly one row");
        let row = &rows[0];
        assert_eq!(row.import_type, LayerImportType::Source);
        assert_eq!(row.source, super::LayerSource::Composite);
        assert_eq!(row.size, (width, height));
        assert!(warnings.is_empty(), "no warnings for a valid flattened psd");

        // The composite pixels must flow through the shared render path.
        let documents = std::sync::Arc::new(vec![document]);
        let image = super::render_layer_rgba(&documents, 0, super::LayerSource::Composite)
            .expect("composite renders");
        assert_eq!(image.dimensions(), (width, height));
    }
}
