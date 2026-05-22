/*
FILE HEADER (tabs/cleaning/tab.rs)
- Назначение: состояние вкладки Cleaning и координация `CanvasView` + активного cleaning-инструмента.
- Ключевые поля `CleaningTabState`:
  - `canvas`: холст с overlay-слоями клина.
  - `tools` / `active_tool_idx`: набор инструментов и выбранный инструмент.
  - `stroke_active` / `last_stroke_point`: состояние текущего штриха.
  - `panel_rects`: прямоугольники плавающих панелей (`остров` + `панель инструмента`) для фильтрации ввода.
  - `text_mask_model`: shared-модель маски текста для mask-layer overlay в cleaning-canvas.
  - `quick_text_mask_panel_open`: состояние плавающей панели "Быстрый клин найденного текста".
  - `text_mask_textures`: tile-кэш текстовой маски для оверлея в cleaning-canvas with LRU metadata
    for memory-pressure eviction.
  - `text_mask_load_*`: асинхронная подзагрузка масок из `text_detection`, если в shared-модели ещё нет данных.
  - `save_job_*`: фоновое сохранение clean_layers без блокировки GUI.
- `quick_clean_*`: состояние быстрого клина по маске текста (UI-параметры, фоновые job-события, прогресс).
- `overlays_model`: shared clean-overlay model; committed edits land there and use its diff-based undo/redo history.
- Ключевые методы:
  - `draw`: кадр вкладки (гейты input, рендер canvas, UI панелей, overlay UI инструмента).
  - `draw_tool_panel`: отдельное плавающее окно инструмента (выбор инструмента + его UI) со сворачиванием.
  - `draw_quick_text_mask_panel`: плавающая сворачиваемая панель быстрого клина (параметры + запуск + прогресс).
  - `active_cursor_occluder`: вычисляет scene-область активного курсора кисти для скрытия on_top/aside пузырей.
  - `start_text_mask_load_job_if_needed/poll_text_mask_load_job`: фоновые загрузка и применение масок.
  - `start_quick_text_clean_job/poll_quick_text_clean_job`: многопоточная обработка страниц по маске текста
    с прогрессом и применением patch-ов в `CleanOverlaysModel`.
  - `handle_history_hotkeys`: Ctrl+Z / Ctrl+Shift+Z для committed overlay-дельт из shared history.
  - `handle_active_tool_input/hotkeys/wheel`: маршрутизация ввода в активный инструмент.
  - `canvas_pointer_occluded`: общий гейт ввода, когда pointer занят floating UI/popup/dialog поверх canvas.
  - `zoom_by_shortcut/reset_zoom_shortcut`: прокси zoom-hotkeys CanvasView с учётом блокировок от инструмента.
  - `viewport_snapshot/apply_viewport_snapshot`: bridge для общего viewport sync в `MangaApp`.
- Важно: если активный инструмент возвращает `block_canvas_zoom() = true` (например, открыт region editor),
  zoom CanvasView блокируется, чтобы Ctrl/Z-комбинации обрабатывались только инструментом.
  Для инструментов, которым нужен `Ctrl+ЛКМ` (например, `Замазка` для прямоугольника),
  zoom также блокируется адресно на эту комбинацию.
*/
use super::tools::{
    AotInpaintTool, CleaningCursorOccluder, CleaningTool, GradientFillTool, LamaInpaintTool,
    LamaMpeInpaintTool, StampTool, StrokeModifiers, StrokePoint, TextureSynthesisInpaintTool,
    ZamazkaTool,
};
use crate::app::{PageImageInfo, PageTexture};
use crate::canvas::{
    CanvasDrawParams, CanvasHooks, CanvasUiStatus, CanvasView, CanvasViewportSnapshot,
    SourceTextureUploadBudget,
};
use crate::memory_manager::{
    CacheEvictionReport, CacheEvictionRequest, CacheReloadCost, CacheResourceInfo,
    CacheResourceKind, select_eviction_candidates,
};
use crate::models::bubbles_model::BubblesModel;
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::models::text_mask_model::TextMaskModel;
use crate::project::ProjectData;
use crate::tabs::translation::backend_health::AiBackendHealthSnapshot;
use crate::widgets::{WheelComboBox, WheelSlider};
use eframe::egui;
use egui::{Align, Color32, Layout, Pos2, Rect};
use std::collections::VecDeque;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;

const STROKE_OVERLAY_UPLOAD_MIN_INTERVAL_S: f64 = 1.0 / 30.0;
const PIXEL_INSPECTION_ZOOM_THRESHOLD: f32 = 5.0;
const TEXT_MASK_TILE_SIDE: usize = 1024;
const TEXT_MASK_TEXTURE_OPTIONS: egui::TextureOptions = egui::TextureOptions::NEAREST;
const TEXT_MASK_VISUAL_ALPHA_MAX: u8 = 96;
const QUICK_TEXT_CLEAN_CONTOUR_TOLERANCE: u8 = 6;
const SAVE_HINT_TEXT: &str = "Сохранение...";
const PYTORCH_UNAVAILABLE_HINT: &str = "PyTorch не установлен";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum UnevenBackgroundTool {
    NoProcessing,
}

impl UnevenBackgroundTool {
    fn title(self) -> &'static str {
        match self {
            Self::NoProcessing => "Не обрабатывать",
        }
    }
}

#[derive(Clone)]
struct TextMaskTextureTile {
    texture: egui::TextureHandle,
    origin_px: [usize; 2],
    size_px: [usize; 2],
}

#[derive(Clone)]
struct TextMaskTexturePage {
    size: [usize; 2],
    tiles: Vec<TextMaskTextureTile>,
    last_used_frame: u64,
}

#[derive(Debug, Clone)]
struct TextMaskLoadPage {
    page_idx: usize,
    mask_size: [u32; 2],
    mask_alpha: Vec<u8>,
}

#[derive(Debug)]
struct TextMaskLoadResult {
    pages: Vec<TextMaskLoadPage>,
    loaded: usize,
    missing: usize,
    failed: usize,
}

#[derive(Debug, Clone)]
struct QuickTextCleanTask {
    page_idx: usize,
    page_path: PathBuf,
    mask_path: PathBuf,
    mask_from_model: Option<TextMaskLoadPage>,
}

#[derive(Debug)]
struct QuickTextCleanPageResult {
    page_idx: usize,
    patch: Option<egui::ColorImage>,
    regions_total: usize,
    regions_filled: usize,
    regions_skipped: usize,
    error: Option<String>,
    missing_mask: bool,
}

#[derive(Debug)]
enum QuickTextCleanJobEvent {
    Started { total_pages: usize },
    PageProcessed(QuickTextCleanPageResult),
    Finished,
}

#[derive(Debug, Default, Clone)]
struct QuickTextCleanProgress {
    total_pages: usize,
    done_pages: usize,
    regions_total: usize,
    regions_filled: usize,
    regions_skipped: usize,
    failed_pages: usize,
    missing_masks: usize,
}

pub struct CleaningTabState {
    canvas: CanvasView,
    tools: Vec<Box<dyn CleaningTool>>,
    tool_labels: Vec<String>,
    active_tool_idx: usize,
    stroke_active: bool,
    last_stroke_point: Option<StrokePoint>,
    active_stroke_page_idx: Option<usize>,
    panel_rects: Vec<egui::Rect>,
    text_mask_model: Option<Arc<Mutex<TextMaskModel>>>,
    quick_text_mask_panel_open: bool,
    text_mask_textures: HashMap<usize, TextMaskTexturePage>,
    text_mask_synced_revision: u64,
    text_mask_load_in_progress: bool,
    text_mask_load_rx: Option<Receiver<Result<TextMaskLoadResult, String>>>,
    text_mask_load_status: Option<String>,
    overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    save_job_in_progress: bool,
    save_job_rx: Option<Receiver<Result<(), String>>>,
    save_status_text: Option<String>,
    quick_clean_auto_expand_px: i32,
    quick_clean_uneven_background_tool: UnevenBackgroundTool,
    quick_clean_job_in_progress: bool,
    quick_clean_job_rx: Option<Receiver<QuickTextCleanJobEvent>>,
    quick_clean_progress: QuickTextCleanProgress,
    quick_clean_status_text: Option<String>,
    ai_backend_health: Option<Arc<Mutex<AiBackendHealthSnapshot>>>,
}

impl Default for CleaningTabState {
    fn default() -> Self {
        let mut canvas = CanvasView::default();
        canvas.editable = false;

        let tools: Vec<Box<dyn CleaningTool>> = vec![
            Box::<ZamazkaTool>::default(),
            Box::<StampTool>::default(),
            Box::<GradientFillTool>::default(),
            Box::<TextureSynthesisInpaintTool>::default(),
            Box::<LamaInpaintTool>::default(),
            Box::<LamaMpeInpaintTool>::default(),
            Box::<AotInpaintTool>::default(),
        ];
        let tool_labels = tools.iter().map(|tool| tool.title().to_string()).collect();

        let mut state = Self {
            canvas,
            tools,
            tool_labels,
            active_tool_idx: 0,
            stroke_active: false,
            last_stroke_point: None,
            active_stroke_page_idx: None,
            panel_rects: Vec::with_capacity(2),
            text_mask_model: None,
            quick_text_mask_panel_open: false,
            text_mask_textures: HashMap::new(),
            text_mask_synced_revision: 0,
            text_mask_load_in_progress: false,
            text_mask_load_rx: None,
            text_mask_load_status: None,
            overlays_model: None,
            save_job_in_progress: false,
            save_job_rx: None,
            save_status_text: None,
            quick_clean_auto_expand_px: 8,
            quick_clean_uneven_background_tool: UnevenBackgroundTool::NoProcessing,
            quick_clean_job_in_progress: false,
            quick_clean_job_rx: None,
            quick_clean_progress: QuickTextCleanProgress::default(),
            quick_clean_status_text: None,
            ai_backend_health: None,
        };
        state.activate_tool(0);
        state
    }
}

impl CleaningTabState {
    pub fn set_bubbles_model(&mut self, model: Arc<Mutex<BubblesModel>>) {
        self.canvas.set_bubbles_model(model);
    }

    pub fn set_overlays_model(&mut self, model: Arc<Mutex<CleanOverlaysModel>>) {
        self.canvas.set_overlays_model(Arc::clone(&model));
        self.overlays_model = Some(model);
    }

    pub fn set_text_mask_model(&mut self, model: Arc<Mutex<TextMaskModel>>) {
        self.text_mask_model = Some(model);
        self.text_mask_synced_revision = 0;
        self.text_mask_textures.clear();
        self.text_mask_load_status = None;
    }

    pub fn set_ai_backend_health(&mut self, snapshot: Arc<Mutex<AiBackendHealthSnapshot>>) {
        self.ai_backend_health = Some(snapshot);
    }

    pub fn set_canvas_scroll_area_id_salt(&mut self, id_salt: &'static str) {
        self.canvas.set_scroll_area_id_salt(id_salt);
    }

    pub fn viewport_snapshot(&self) -> CanvasViewportSnapshot {
        self.canvas.viewport_snapshot()
    }

    pub fn apply_viewport_snapshot(&mut self, snapshot: CanvasViewportSnapshot) {
        self.canvas.apply_viewport_snapshot(snapshot);
    }

    pub fn cleaning_mask_gpu_memory_snapshot(
        &self,
        pinned_pages: &BTreeSet<usize>,
    ) -> Vec<CacheResourceInfo> {
        self.text_mask_textures
            .iter()
            .map(|(page_idx, page_tex)| CacheResourceInfo {
                id: format!("cleaning-mask-gpu:{page_idx}"),
                kind: CacheResourceKind::CleaningMaskGpu,
                page_idx: Some(*page_idx),
                estimated_bytes: text_mask_texture_page_estimated_bytes(page_tex),
                last_used_frame: page_tex.last_used_frame,
                reload_cost: CacheReloadCost::RebuildFromModel,
                dirty: false,
                visible: pinned_pages.contains(page_idx),
                reconstructable: true,
            })
            .collect()
    }

    pub fn evict_cleaning_mask_gpu_cache(
        &mut self,
        request: &CacheEvictionRequest,
    ) -> CacheEvictionReport {
        let snapshot = self.cleaning_mask_gpu_memory_snapshot(&request.pinned_pages);
        let candidates = select_eviction_candidates(&snapshot, request);
        let mut evicted = Vec::new();
        let mut freed = 0_u64;
        for resource in candidates.resources {
            let Some(page_idx) = resource.page_idx else {
                continue;
            };
            if self.text_mask_textures.remove(&page_idx).is_some() {
                freed = freed.saturating_add(resource.estimated_bytes);
                evicted.push(resource);
            }
        }
        CacheEvictionReport {
            resources: evicted,
            estimated_freed_bytes: freed,
        }
    }

    pub fn clean_overlay_gpu_memory_snapshot(
        &self,
        pinned_pages: &BTreeSet<usize>,
    ) -> Vec<CacheResourceInfo> {
        self.canvas.clean_overlay_gpu_memory_snapshot(pinned_pages)
    }

    pub fn evict_clean_overlay_gpu_cache(
        &mut self,
        request: &CacheEvictionRequest,
    ) -> CacheEvictionReport {
        self.canvas.evict_clean_overlay_gpu_cache(request)
    }

    pub fn active_source_page_window(&self, neighbor_radius: usize) -> HashSet<usize> {
        self.canvas.active_source_page_window(neighbor_radius)
    }

    pub fn source_pixel_inspection_active(&self) -> bool {
        self.canvas.source_pixel_inspection_active()
    }

    pub fn zoom_by_shortcut(&mut self, factor: f32) -> bool {
        if self
            .tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.block_canvas_zoom())
        {
            return false;
        }
        self.canvas.zoom_by_shortcut(factor)
    }

    pub fn reset_zoom_shortcut(&mut self) -> bool {
        if self
            .tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.block_canvas_zoom())
        {
            return false;
        }
        self.canvas.reset_zoom_shortcut()
    }

    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        project: &ProjectData,
        page_infos: &HashMap<usize, PageImageInfo>,
        texture_cache: &mut HashMap<usize, PageTexture>,
        status: CanvasUiStatus,
    ) {
        if ctx.input(|i| i.pointer.primary_released()) {
            self.finish_stroke();
        }
        let canvas_rect = ui.max_rect();
        let history_hotkeys_handled = self.handle_history_hotkeys(ctx);
        let hotkeys_handled = self.handle_active_tool_hotkeys(ctx, canvas_rect);
        let tool_blocks_canvas_zoom = self
            .tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.block_canvas_zoom());
        let (primary_down, secondary_down, space_down, modifiers, z_down) = ctx.input(|i| {
            (
                i.pointer.primary_down(),
                i.pointer.secondary_down(),
                i.key_down(egui::Key::Space),
                i.modifiers,
                i.key_down(egui::Key::Z),
            )
        });
        let zoom_modifier_down = z_down || modifiers.ctrl || modifiers.command;
        let tool_blocks_ctrl_primary_zoom = primary_down
            && zoom_modifier_down
            && self
                .tools
                .get(self.active_tool_idx)
                .is_some_and(|tool| tool.block_canvas_zoom_on_ctrl_primary());
        let wheel_blocked = self.handle_active_tool_wheel(ctx, canvas_rect) || self.stroke_active;
        self.canvas.set_wheel_scroll_blocked(wheel_blocked);
        self.canvas.set_zoom_blocked(
            self.stroke_active || tool_blocks_canvas_zoom || tool_blocks_ctrl_primary_zoom,
        );
        let suppress_overlay_render = self
            .tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.suppress_base_overlay_render());
        self.canvas
            .set_overlay_render_suppressed(suppress_overlay_render);

        let space_pan_active = space_down;
        if let Some(active_tool) = self.tools.get_mut(self.active_tool_idx) {
            active_tool.set_space_pan_active(space_pan_active);
        }
        let block_drag_scroll = self.tools.get(self.active_tool_idx).is_some_and(|tool| {
            (primary_down && tool.block_canvas_drag_scroll_on_primary())
                || (secondary_down && tool.block_canvas_drag_scroll_on_secondary())
        });
        self.canvas.set_drag_scroll_blocked(block_drag_scroll);
        self.canvas
            .set_overlay_upload_min_interval_s(if self.stroke_active {
                STROKE_OVERLAY_UPLOAD_MIN_INTERVAL_S
            } else {
                0.0
            });
        let pixel_inspection_enabled = self.canvas.zoom() >= PIXEL_INSPECTION_ZOOM_THRESHOLD;
        self.canvas
            .set_pixel_sampling_nearest(pixel_inspection_enabled);
        self.canvas.set_pixel_grid_visible(pixel_inspection_enabled);

        self.poll_text_mask_load_job();
        self.poll_quick_text_clean_job();
        let cursor_occluder = self.active_cursor_occluder(ctx, canvas_rect);
        let mut hooks = CleaningHooks {
            quick_text_mask_panel_open: self.quick_text_mask_panel_open,
            text_mask_model: self.text_mask_model.as_ref().cloned(),
            text_mask_textures: &mut self.text_mask_textures,
            text_mask_synced_revision: &mut self.text_mask_synced_revision,
            cursor_occluder,
        };
        let mut source_upload_budget = SourceTextureUploadBudget::source_page_reupload_default();
        self.canvas.draw(CanvasDrawParams {
            ctx,
            ui,
            project,
            page_infos,
            texture_cache,
            status,
            source_upload_budget: &mut source_upload_budget,
            hooks: &mut hooks,
        });
        self.poll_save_job();
        self.panel_rects.clear();
        self.draw_top_island_panel(ctx, canvas_rect, project);
        self.draw_tool_panel(ctx, canvas_rect);
        self.draw_quick_text_mask_panel(ctx, canvas_rect, project);
        self.handle_active_tool_input(ctx, canvas_rect, project);
        let ai_backend_available = self.ai_backend_available();
        let ai_backend_torch_available = self.ai_backend_torch_available();
        if let Some(active_tool) = self.tools.get_mut(self.active_tool_idx) {
            active_tool.set_ai_backend_available(ai_backend_available);
            active_tool.set_ai_backend_torch_available(ai_backend_torch_available);
            active_tool.draw_overlay_ui(ctx, &mut self.canvas, project);
        }
        self.draw_active_tool_cursor(ctx, ui, canvas_rect);
        self.canvas.draw_pixel_grid_overlay(ui);
        if self.save_job_in_progress || hotkeys_handled || history_hotkeys_handled {
            ctx.request_repaint();
        }
        if self.quick_text_mask_panel_open {
            ctx.request_repaint();
        }
        if self.text_mask_load_in_progress {
            ctx.request_repaint();
        }
        if self.quick_clean_job_in_progress {
            ctx.request_repaint();
        }
    }

    fn ai_backend_available(&self) -> bool {
        let Some(snapshot) = self.ai_backend_health.as_ref() else {
            return false;
        };
        match snapshot.lock() {
            Ok(guard) => guard.connected,
            Err(poisoned) => poisoned.into_inner().connected,
        }
    }

    fn ai_backend_torch_available(&self) -> bool {
        let Some(snapshot) = self.ai_backend_health.as_ref() else {
            return false;
        };
        match snapshot.lock() {
            Ok(guard) => guard.is_torch_available.unwrap_or(true),
            Err(poisoned) => poisoned.into_inner().is_torch_available.unwrap_or(true),
        }
    }

    fn tool_available(&self, idx: usize) -> bool {
        self.tools
            .get(idx)
            .is_some_and(|tool| !tool.pytorch_required() || self.ai_backend_torch_available())
    }

    fn first_available_tool_idx(&self) -> Option<usize> {
        self.tools
            .iter()
            .enumerate()
            .find_map(|(idx, _)| self.tool_available(idx).then_some(idx))
    }

    fn ensure_active_tool_available(&mut self) {
        if self.tool_available(self.active_tool_idx) {
            return;
        }
        if let Some(idx) = self.first_available_tool_idx() {
            self.activate_tool(idx);
        }
    }

    fn activate_tool(&mut self, idx: usize) {
        if idx >= self.tools.len() {
            return;
        }

        self.finish_stroke();

        if let Some(current) = self.tools.get_mut(self.active_tool_idx) {
            current.deactivate(&mut self.canvas);
        }

        self.active_tool_idx = idx;

        if let Some(active) = self.tools.get_mut(self.active_tool_idx) {
            active.activate(&mut self.canvas);
        }
    }

    fn finish_stroke(&mut self) {
        if !self.stroke_active {
            self.last_stroke_point = None;
            self.active_stroke_page_idx = None;
            return;
        }
        self.stroke_active = false;
        self.last_stroke_point = None;
        self.active_stroke_page_idx = None;
        if let Some(active) = self.tools.get_mut(self.active_tool_idx) {
            active.stroke_end(&mut self.canvas);
            active.set_temporary_erase(false);
        }
    }

    fn draw_top_island_panel(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: egui::Rect,
        project: &ProjectData,
    ) {
        let mut overlays_visible = self.canvas.clean_overlays_visible();
        let mut clear_page = false;
        let mut request_save = false;
        let mut toggle_quick_clean_panel = false;

        let panel = egui::Area::new("cleaning_top_island_panel".into())
            .fixed_pos(canvas_rect.left_top() + egui::vec2(360.0, 12.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut overlays_visible, "Показать слой");
                            if ui.button("Очистить текущий слой").clicked() {
                                clear_page = true;
                            }
                            if ui
                                .add_enabled(
                                    !self.save_job_in_progress,
                                    egui::Button::new("Сохранить клин"),
                                )
                                .clicked()
                            {
                                request_save = true;
                            }
                        });
                        ui.horizontal(|ui| {
                            let quick_button = ui.button("Быстрый клин найденного текста");
                            if quick_button.clicked() {
                                toggle_quick_clean_panel = true;
                            }
                            let status_height = ui.spacing().interact_size.y;
                            let status_width = ui.available_width().max(0.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(status_width, status_height),
                                Layout::left_to_right(Align::Center),
                                |ui| {
                                    if self.save_job_in_progress {
                                        ui.spinner();
                                        ui.label(SAVE_HINT_TEXT);
                                    }
                                },
                            );
                        });

                        ui.small("ЛКМ: рисование, Shift+ЛКМ: стирание");
                        ui.small("Space+drag: прокрутка холста, -=/: размер кисти");
                        if !self.save_job_in_progress
                            && let Some(status) = self.save_status_text.as_ref()
                        {
                            ui.small(status);
                        }
                    });
                })
            });

        self.panel_rects.push(panel.response.rect);

        if overlays_visible != self.canvas.clean_overlays_visible() {
            self.canvas.set_clean_overlays_visible(overlays_visible);
        }

        if clear_page {
            self.canvas
                .clear_overlay_index(self.canvas.current_page_idx());
        }

        if request_save {
            self.start_save_job(project);
        }

        if toggle_quick_clean_panel {
            let next_open = !self.quick_text_mask_panel_open;
            self.quick_text_mask_panel_open = next_open;
            if next_open {
                self.start_text_mask_load_job_if_needed(project);
            }
        }
    }

    fn draw_tool_panel(&mut self, ctx: &egui::Context, canvas_rect: egui::Rect) {
        self.ensure_active_tool_available();
        let mut activate_tool_idx = self.active_tool_idx;
        let tool_panel_default_pos = self
            .panel_rects
            .first()
            .map(|top_island_rect| {
                egui::pos2(top_island_rect.right() + 12.0, top_island_rect.top())
            })
            .unwrap_or_else(|| canvas_rect.left_top() + egui::vec2(720.0, 12.0));
        let window = egui::Window::new("Инструменты клина")
            .id(egui::Id::new("cleaning_tool_floating_panel"))
            .default_pos(tool_panel_default_pos)
            .collapsible(true)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Инструмент");
                    let selected_title = self
                        .tool_labels
                        .get(self.active_tool_idx)
                        .cloned()
                        .unwrap_or_else(|| "-".to_string());
                    WheelComboBox::from_id_salt("cleaning_tool_picker")
                        .selected_text(selected_title)
                        .show_ui(ui, |ui| {
                            for (idx, label) in self.tool_labels.iter().enumerate() {
                                let is_available = self.tool_available(idx);
                                let response = ui.add_enabled(
                                    is_available,
                                    egui::Button::new(label).selected(activate_tool_idx == idx),
                                );
                                let response = if is_available {
                                    response
                                } else {
                                    response.on_disabled_hover_text(
                                        egui::RichText::new(PYTORCH_UNAVAILABLE_HINT)
                                            .color(egui::Color32::from_rgb(240, 102, 102)),
                                    )
                                };
                                if response.clicked() {
                                    activate_tool_idx = idx;
                                }
                            }
                        });
                });
                ui.separator();
                if let Some(tool) = self.tools.get_mut(self.active_tool_idx) {
                    tool.draw_ui(ui);
                }
            });

        if let Some(window) = window {
            self.panel_rects.push(window.response.rect);
        }

        if activate_tool_idx != self.active_tool_idx && self.tool_available(activate_tool_idx) {
            self.activate_tool(activate_tool_idx);
        }
    }

    fn draw_quick_text_mask_panel(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: egui::Rect,
        project: &ProjectData,
    ) {
        if !self.quick_text_mask_panel_open {
            return;
        }
        let mut panel_open = self.quick_text_mask_panel_open;
        let mut run_current_page = false;
        let mut run_all_pages = false;
        let window = egui::Window::new("Быстрый клин найденного текста")
            .id(egui::Id::new("cleaning_quick_text_mask_panel"))
            .default_pos(canvas_rect.left_top() + egui::vec2(1080.0, 12.0))
            .collapsible(true)
            .resizable(true)
            .open(&mut panel_open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Авторасширение маски");
                    ui.add(
                        WheelSlider::new(&mut self.quick_clean_auto_expand_px, 0..=64)
                            .suffix(" пикс"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Инструмент обработки неравномерного фона");
                    WheelComboBox::from_id_salt("quick-clean-uneven-bg-tool")
                        .selected_text(self.quick_clean_uneven_background_tool.title())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.quick_clean_uneven_background_tool,
                                UnevenBackgroundTool::NoProcessing,
                                UnevenBackgroundTool::NoProcessing.title(),
                            );
                        });
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !self.quick_clean_job_in_progress,
                            egui::Button::new("Заклинить текущую страницу"),
                        )
                        .clicked()
                    {
                        run_current_page = true;
                    }
                    if ui
                        .add_enabled(
                            !self.quick_clean_job_in_progress,
                            egui::Button::new("Заклинить все страницы"),
                        )
                        .clicked()
                    {
                        run_all_pages = true;
                    }
                });
                if self.text_mask_load_in_progress {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.small("Загрузка маски из text_detection...");
                    });
                } else if let Some(status) = self.text_mask_load_status.as_ref() {
                    ui.small(status);
                }
                if self.quick_clean_job_in_progress {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.small("Быстрый клин выполняется...");
                    });
                }
                if self.quick_clean_progress.total_pages > 0 {
                    let progress = (self.quick_clean_progress.done_pages as f32
                        / self.quick_clean_progress.total_pages as f32)
                        .clamp(0.0, 1.0);
                    ui.add(egui::ProgressBar::new(progress).text(format!(
                        "Страницы: {}/{}",
                        self.quick_clean_progress.done_pages, self.quick_clean_progress.total_pages
                    )));
                    ui.small(format!(
                        "Области: заполнено {}, пропущено {}, ошибок страниц {}, без маски {}",
                        self.quick_clean_progress.regions_filled,
                        self.quick_clean_progress.regions_skipped,
                        self.quick_clean_progress.failed_pages,
                        self.quick_clean_progress.missing_masks
                    ));
                }
                if let Some(status) = self.quick_clean_status_text.as_ref() {
                    ui.small(status);
                }
            });
        self.quick_text_mask_panel_open = panel_open;
        if let Some(window) = window {
            self.panel_rects.push(window.response.rect);
        }
        if run_current_page {
            self.start_text_mask_load_job_if_needed(project);
            self.start_quick_text_clean_job(project, vec![self.canvas.current_page_idx()]);
        }
        if run_all_pages {
            self.start_text_mask_load_job_if_needed(project);
            let page_indices: Vec<usize> = project.pages.iter().map(|page| page.idx).collect();
            self.start_quick_text_clean_job(project, page_indices);
        }
    }

    fn start_text_mask_load_job_if_needed(&mut self, project: &ProjectData) {
        if self.text_mask_load_in_progress {
            return;
        }
        let Some(model) = self.text_mask_model.as_ref().cloned() else {
            return;
        };
        let mut missing_indices = Vec::<usize>::new();
        if let Ok(model) = model.lock() {
            for page in &project.pages {
                if model.page(page.idx).is_none() {
                    missing_indices.push(page.idx);
                }
            }
        } else {
            return;
        }
        if missing_indices.is_empty() {
            self.text_mask_load_status = Some("Маска уже загружена.".to_string());
            return;
        }

        let storage_dir = project.paths.text_detection_dir.clone();
        let (tx, rx) = mpsc::channel::<Result<TextMaskLoadResult, String>>();
        self.text_mask_load_rx = Some(rx);
        self.text_mask_load_in_progress = true;
        self.text_mask_load_status =
            Some("Пробую загрузить маску из text_detection...".to_string());
        thread::spawn(move || {
            let _ = tx.send(load_text_masks_from_storage(&storage_dir, &missing_indices));
        });
    }

    fn poll_text_mask_load_job(&mut self) {
        let Some(rx) = self.text_mask_load_rx.as_ref() else {
            return;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.text_mask_load_in_progress = false;
                self.text_mask_load_rx = None;
                self.text_mask_load_status =
                    Some("Загрузка маски прервана: канал закрыт.".to_string());
                return;
            }
        };
        self.text_mask_load_in_progress = false;
        self.text_mask_load_rx = None;

        match event {
            Ok(result) => {
                let mut applied = 0usize;
                if let Some(model) = self.text_mask_model.as_ref()
                    && let Ok(mut model) = model.lock()
                {
                    for page in result.pages {
                        model.set_page(
                            page.page_idx,
                            page.mask_size,
                            page.mask_size,
                            page.mask_alpha,
                        );
                        applied = applied.saturating_add(1);
                    }
                }
                self.text_mask_load_status = Some(format!(
                    "Загрузка маски: загружено {}/{} (в хранилище: {}, пропущено: {}, ошибок: {}).",
                    applied,
                    result
                        .loaded
                        .saturating_add(result.missing)
                        .saturating_add(result.failed),
                    result.loaded,
                    result.missing,
                    result.failed
                ));
            }
            Err(error) => {
                self.text_mask_load_status = Some(format!("Ошибка загрузки маски: {error}"));
            }
        }
    }

    fn start_save_job(&mut self, project: &ProjectData) {
        if self.save_job_in_progress {
            return;
        }
        let Some(model) = self.overlays_model.as_ref().cloned() else {
            self.save_status_text =
                Some("Сохранение недоступно: модель оверлеев не подключена.".to_string());
            return;
        };
        let save_dir = project.paths.clean_layers_dir.clone();
        let overlay_snapshots = match model.lock() {
            Ok(locked) => locked.save_snapshots(),
            Err(_) => {
                self.save_job_in_progress = false;
                self.save_job_rx = None;
                self.save_status_text =
                    Some("Не удалось получить lock модели оверлеев.".to_string());
                return;
            }
        };
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        self.save_job_rx = Some(rx);
        self.save_job_in_progress = true;
        self.save_status_text = Some("Сохранение клина...".to_string());

        thread::spawn(move || {
            let result = save_clean_overlay_snapshots(&save_dir, &overlay_snapshots);
            let _ = tx.send(result);
        });
    }

    fn poll_save_job(&mut self) {
        let Some(rx) = self.save_job_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.save_job_in_progress = false;
                self.save_job_rx = None;
                self.save_status_text = Some("Клин сохранён в папку clean_layers.".to_string());
            }
            Ok(Err(err)) => {
                self.save_job_in_progress = false;
                self.save_job_rx = None;
                self.save_status_text = Some(format!("Ошибка сохранения клина: {err}"));
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.save_job_in_progress = false;
                self.save_job_rx = None;
                self.save_status_text = Some("Сохранение прервано: канал закрыт.".to_string());
            }
        }
    }

    fn start_quick_text_clean_job(&mut self, project: &ProjectData, page_indices: Vec<usize>) {
        if self.quick_clean_job_in_progress {
            return;
        }
        if page_indices.is_empty() {
            self.quick_clean_status_text = Some("Нет страниц для обработки.".to_string());
            return;
        }
        if self.overlays_model.is_none() {
            self.quick_clean_status_text =
                Some("Быстрый клин недоступен: модель оверлеев не подключена.".to_string());
            return;
        }
        let text_mask_model = self.text_mask_model.as_ref().cloned();
        let mut tasks = Vec::new();
        for page_idx in page_indices {
            let Some(page) = project.pages.iter().find(|page| page.idx == page_idx) else {
                continue;
            };
            let mask_from_model = text_mask_model
                .as_ref()
                .and_then(|model| model.lock().ok())
                .and_then(|model| model.page(page_idx).cloned())
                .map(|page| TextMaskLoadPage {
                    page_idx,
                    mask_size: page.mask_size,
                    mask_alpha: page.mask_alpha,
                });
            tasks.push(QuickTextCleanTask {
                page_idx,
                page_path: page.path.clone(),
                mask_path: text_detection_mask_file_path(
                    &project.paths.text_detection_dir,
                    page_idx,
                ),
                mask_from_model,
            });
        }
        if tasks.is_empty() {
            self.quick_clean_status_text = Some("Нет доступных страниц для обработки.".to_string());
            return;
        }

        let auto_expand_px = self.quick_clean_auto_expand_px.clamp(0, 128) as usize;
        let uneven_tool = self.quick_clean_uneven_background_tool;
        let (tx, rx) = mpsc::channel::<QuickTextCleanJobEvent>();
        self.quick_clean_job_rx = Some(rx);
        self.quick_clean_job_in_progress = true;
        self.quick_clean_progress = QuickTextCleanProgress::default();
        self.quick_clean_status_text = Some("Запущен быстрый клин...".to_string());

        thread::spawn(move || {
            let _ = tx.send(QuickTextCleanJobEvent::Started {
                total_pages: tasks.len(),
            });
            let worker_count = thread::available_parallelism()
                .map(|count| count.get().saturating_sub(1).max(1))
                .unwrap_or(1)
                .min(tasks.len().max(1));

            let (task_tx, task_rx) = mpsc::channel::<QuickTextCleanTask>();
            let task_rx = Arc::new(Mutex::new(task_rx));
            let (result_tx, result_rx) = mpsc::channel::<QuickTextCleanPageResult>();
            let mut workers = Vec::with_capacity(worker_count);
            for _ in 0..worker_count {
                let worker_rx = Arc::clone(&task_rx);
                let worker_tx = result_tx.clone();
                workers.push(thread::spawn(move || {
                    loop {
                        let task = {
                            let Ok(rx) = worker_rx.lock() else {
                                break;
                            };
                            match rx.recv() {
                                Ok(task) => task,
                                Err(_) => break,
                            }
                        };
                        let result = run_quick_text_clean_on_page(
                            task,
                            auto_expand_px,
                            uneven_tool,
                            QUICK_TEXT_CLEAN_CONTOUR_TOLERANCE,
                        );
                        if worker_tx.send(result).is_err() {
                            break;
                        }
                    }
                }));
            }
            drop(result_tx);

            for task in tasks {
                if task_tx.send(task).is_err() {
                    break;
                }
            }
            drop(task_tx);

            while let Ok(result) = result_rx.recv() {
                let _ = tx.send(QuickTextCleanJobEvent::PageProcessed(result));
            }
            for worker in workers {
                let _ = worker.join();
            }
            let _ = tx.send(QuickTextCleanJobEvent::Finished);
        });
    }

    fn poll_quick_text_clean_job(&mut self) {
        loop {
            let event = {
                let Some(rx) = self.quick_clean_job_rx.as_ref() else {
                    return;
                };
                rx.try_recv()
            };
            match event {
                Ok(QuickTextCleanJobEvent::Started { total_pages }) => {
                    self.quick_clean_progress = QuickTextCleanProgress {
                        total_pages,
                        ..QuickTextCleanProgress::default()
                    };
                    self.quick_clean_status_text =
                        Some("Быстрый клин: чтение страниц и анализ маски...".to_string());
                }
                Ok(QuickTextCleanJobEvent::PageProcessed(result)) => {
                    self.quick_clean_progress.done_pages =
                        self.quick_clean_progress.done_pages.saturating_add(1);
                    self.quick_clean_progress.regions_total = self
                        .quick_clean_progress
                        .regions_total
                        .saturating_add(result.regions_total);
                    self.quick_clean_progress.regions_filled = self
                        .quick_clean_progress
                        .regions_filled
                        .saturating_add(result.regions_filled);
                    self.quick_clean_progress.regions_skipped = self
                        .quick_clean_progress
                        .regions_skipped
                        .saturating_add(result.regions_skipped);
                    if result.missing_mask {
                        self.quick_clean_progress.missing_masks =
                            self.quick_clean_progress.missing_masks.saturating_add(1);
                    }
                    if result.error.is_some() {
                        self.quick_clean_progress.failed_pages =
                            self.quick_clean_progress.failed_pages.saturating_add(1);
                    }
                    if let Some(patch) = result.patch {
                        self.apply_quick_text_patch_to_overlay(result.page_idx, patch);
                    }
                    self.quick_clean_status_text = Some(format!(
                        "Быстрый клин: страница {} обработана (областей: {}, заполнено: {}, пропущено: {}).",
                        result.page_idx,
                        result.regions_total,
                        result.regions_filled,
                        result.regions_skipped
                    ));
                }
                Ok(QuickTextCleanJobEvent::Finished) => {
                    self.quick_clean_job_in_progress = false;
                    self.quick_clean_job_rx = None;
                    self.quick_clean_status_text = Some(format!(
                        "Быстрый клин завершён: страниц {}/{}, заполнено областей {}, пропущено {}, ошибок {}, без маски {}.",
                        self.quick_clean_progress.done_pages,
                        self.quick_clean_progress.total_pages,
                        self.quick_clean_progress.regions_filled,
                        self.quick_clean_progress.regions_skipped,
                        self.quick_clean_progress.failed_pages,
                        self.quick_clean_progress.missing_masks
                    ));
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.quick_clean_job_in_progress = false;
                    self.quick_clean_job_rx = None;
                    self.quick_clean_status_text =
                        Some("Быстрый клин прерван: канал job закрыт.".to_string());
                    break;
                }
            }
        }
    }

    fn apply_quick_text_patch_to_overlay(&mut self, page_idx: usize, patch: egui::ColorImage) {
        if patch.size[0] == 0 || patch.size[1] == 0 {
            return;
        }
        let Some(model) = self.overlays_model.as_ref() else {
            return;
        };
        let Ok(mut model) = model.lock() else {
            return;
        };
        let mut base = model
            .get(page_idx)
            .cloned()
            .unwrap_or_else(|| egui::ColorImage::filled(patch.size, egui::Color32::TRANSPARENT));
        if base.size != patch.size {
            base = resize_color_image_nearest(&base, patch.size[0], patch.size[1]);
        }
        let mut applied = false;
        for (dst, src) in base.pixels.iter_mut().zip(patch.pixels.iter()) {
            if src.a() == 0 {
                continue;
            }
            *dst = *src;
            applied = true;
        }
        if applied {
            model.replace(page_idx, &base);
        }
    }

    fn active_tool_captures_pointer(&self, pointer_pos: egui::Pos2) -> bool {
        self.tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.captures_canvas_pointer(pointer_pos))
    }

    fn pointer_in_any_panel(&self, pointer_pos: egui::Pos2) -> bool {
        self.panel_rects
            .iter()
            .any(|panel_rect| panel_rect.contains(pointer_pos))
    }

    fn canvas_pointer_occluded(&self, ctx: &egui::Context, pointer_pos: egui::Pos2) -> bool {
        ctx.is_popup_open()
            || self.pointer_in_any_panel(pointer_pos)
            || self.active_tool_captures_pointer(pointer_pos)
            || ctx.layer_id_at(pointer_pos).is_some_and(|layer| {
                matches!(
                    layer.order,
                    egui::Order::Middle
                        | egui::Order::Foreground
                        | egui::Order::Tooltip
                        | egui::Order::Debug
                )
            })
    }

    fn handle_active_tool_input(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: egui::Rect,
        project: &ProjectData,
    ) {
        let (
            pointer_pos,
            primary_pressed,
            primary_down,
            primary_released,
            secondary_pressed,
            modifiers,
            z_down,
        ) = ctx.input(|i| {
            (
                i.pointer.interact_pos(),
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.primary_released(),
                i.pointer.secondary_pressed(),
                i.modifiers,
                i.key_down(egui::Key::Z),
            )
        });

        if primary_released {
            self.finish_stroke();
            return;
        }

        let Some(pointer_pos) = pointer_pos else {
            return;
        };

        let zoom_modifier_down = z_down || modifiers.ctrl || modifiers.command;
        if zoom_modifier_down && primary_down {
            let tool_consumes_ctrl_primary = self
                .tools
                .get(self.active_tool_idx)
                .is_some_and(|tool| tool.block_canvas_zoom_on_ctrl_primary());
            if !tool_consumes_ctrl_primary {
                self.finish_stroke();
                return;
            }
        }

        if self
            .tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.space_pan_active())
        {
            self.finish_stroke();
            return;
        }

        if !canvas_rect.contains(pointer_pos) {
            self.finish_stroke();
            return;
        }

        if self.canvas_pointer_occluded(ctx, pointer_pos) {
            self.finish_stroke();
            return;
        }

        let page_idx = if let Some(idx) = self.active_stroke_page_idx {
            if self.canvas.page_contains_scene_pos(idx, pointer_pos) {
                Some(idx)
            } else {
                self.canvas.page_index_at_scene_pos(pointer_pos)
            }
        } else {
            self.canvas.page_index_at_scene_pos(pointer_pos)
        };
        let Some(page_idx) = page_idx else {
            return;
        };

        let point = StrokePoint {
            page_idx,
            scene_pos: pointer_pos,
            modifiers: StrokeModifiers {
                shift: modifiers.shift,
                ctrl: modifiers.ctrl || modifiers.command,
            },
        };

        if secondary_pressed
            && let Some(active_tool) = self.tools.get_mut(self.active_tool_idx)
            && active_tool.secondary_click(&mut self.canvas, project, point)
        {
            ctx.request_repaint();
            return;
        }

        if !primary_down {
            return;
        }

        if let Some(active_tool) = self.tools.get_mut(self.active_tool_idx) {
            active_tool.set_temporary_erase(point.modifiers.shift);

            if !self.stroke_active || primary_pressed {
                if !active_tool.wants_primary_stroke(point) {
                    return;
                }
                self.stroke_active = true;
                self.last_stroke_point = Some(point);
                self.active_stroke_page_idx = Some(page_idx);
                active_tool.stroke_begin(&mut self.canvas, point);
                ctx.request_repaint();
                return;
            }

            if let Some(prev) = self.last_stroke_point {
                if prev.scene_pos == point.scene_pos {
                    return;
                }
                if prev.page_idx == point.page_idx {
                    active_tool.stroke_update(&mut self.canvas, prev, point);
                    self.last_stroke_point = Some(point);
                    self.active_stroke_page_idx = Some(point.page_idx);
                    ctx.request_repaint();
                } else {
                    active_tool.stroke_end(&mut self.canvas);
                    active_tool.stroke_begin(&mut self.canvas, point);
                    self.last_stroke_point = Some(point);
                    self.active_stroke_page_idx = Some(point.page_idx);
                    ctx.request_repaint();
                }
            }
        }
    }

    fn handle_active_tool_hotkeys(&mut self, ctx: &egui::Context, canvas_rect: egui::Rect) -> bool {
        let (pointer_pos, modifiers, z_down) =
            ctx.input(|i| (i.pointer.hover_pos(), i.modifiers, i.key_down(egui::Key::Z)));
        let wants_keyboard_input = ctx.wants_keyboard_input();
        if wants_keyboard_input {
            return false;
        }
        if modifiers.ctrl || modifiers.command || z_down {
            return false;
        }
        let Some(pointer_pos) = pointer_pos else {
            return false;
        };
        if !canvas_rect.contains(pointer_pos) {
            return false;
        }
        if self.canvas_pointer_occluded(ctx, pointer_pos) {
            return false;
        }
        let Some(active_tool) = self.tools.get_mut(self.active_tool_idx) else {
            return false;
        };
        active_tool.on_key_event(ctx)
    }

    fn handle_history_hotkeys(&mut self, ctx: &egui::Context) -> bool {
        if ctx.wants_keyboard_input() || self.stroke_active {
            return false;
        }
        if self
            .tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.block_canvas_zoom())
        {
            return false;
        }
        let command_shift_mods = egui::Modifiers {
            shift: true,
            command: true,
            ..egui::Modifiers::NONE
        };
        let (redo, undo) = ctx.input_mut(|input| {
            (
                input.consume_key(command_shift_mods, egui::Key::Z),
                input.consume_key(egui::Modifiers::COMMAND, egui::Key::Z),
            )
        });
        let Some(model) = self.overlays_model.as_ref() else {
            return false;
        };
        let Ok(mut model) = model.lock() else {
            return false;
        };
        if redo && model.redo_overlay_history() {
            return true;
        }
        if undo && model.undo_overlay_history() {
            return true;
        }
        false
    }

    fn handle_active_tool_wheel(&mut self, ctx: &egui::Context, canvas_rect: egui::Rect) -> bool {
        let (pointer_pos, modifiers, scroll_delta) = ctx.input(|i| {
            (
                i.pointer.hover_pos(),
                i.modifiers,
                if i.smooth_scroll_delta.length_sq() > f32::EPSILON {
                    i.smooth_scroll_delta
                } else {
                    i.raw_scroll_delta
                },
            )
        });
        let Some(pointer_pos) = pointer_pos else {
            return false;
        };
        if !canvas_rect.contains(pointer_pos) {
            return false;
        }
        if !modifiers.shift {
            return false;
        }
        // With Shift some platforms remap wheel into horizontal scrolling,
        // so fallback to X when Y is near zero.
        let mut wheel_delta = scroll_delta.y;
        if wheel_delta.abs() <= f32::EPSILON {
            wheel_delta = scroll_delta.x;
        }
        if wheel_delta.abs() <= f32::EPSILON {
            return false;
        }
        if self.canvas_pointer_occluded(ctx, pointer_pos) {
            return false;
        }
        let Some(active_tool) = self.tools.get_mut(self.active_tool_idx) else {
            return false;
        };
        let handled = active_tool.on_wheel_event(wheel_delta, modifiers);
        if handled {
            ctx.request_repaint();
        }
        handled
    }

    fn draw_active_tool_cursor(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        canvas_rect: egui::Rect,
    ) {
        let pointer_pos = ctx.input(|i| i.pointer.interact_pos().or(i.pointer.hover_pos()));
        let pointer_pos = pointer_pos.or_else(|| self.last_stroke_point.map(|p| p.scene_pos));
        let Some(pointer_pos) = pointer_pos else {
            return;
        };
        if !canvas_rect.contains(pointer_pos) {
            return;
        }
        if self.canvas_pointer_occluded(ctx, pointer_pos) {
            return;
        }
        let page_idx = self.canvas.page_index_at_scene_pos(pointer_pos);
        let Some(active_tool) = self.tools.get_mut(self.active_tool_idx) else {
            return;
        };
        if let Some(page_idx) = page_idx {
            let modifiers = ctx.input(|i| i.modifiers);
            active_tool.ensure_hover_overlay(
                &mut self.canvas,
                StrokePoint {
                    page_idx,
                    scene_pos: pointer_pos,
                    modifiers: StrokeModifiers {
                        shift: modifiers.shift,
                        ctrl: modifiers.ctrl || modifiers.command,
                    },
                },
            );
        }
        active_tool.draw_cursor(ui, &self.canvas, Some(pointer_pos));
    }

    fn active_cursor_occluder(
        &self,
        ctx: &egui::Context,
        canvas_rect: egui::Rect,
    ) -> Option<CleaningCursorOccluder> {
        let pointer_pos = ctx.input(|i| i.pointer.interact_pos().or(i.pointer.hover_pos()));
        let pointer_pos = pointer_pos.or_else(|| self.last_stroke_point.map(|p| p.scene_pos));
        let pointer_pos = pointer_pos?;
        if !canvas_rect.contains(pointer_pos) {
            return None;
        }
        if self.canvas_pointer_occluded(ctx, pointer_pos) {
            return None;
        }
        self.tools
            .get(self.active_tool_idx)
            .and_then(|tool| tool.bubble_occluder(&self.canvas, Some(pointer_pos)))
    }
}

struct CleaningHooks<'a> {
    quick_text_mask_panel_open: bool,
    text_mask_model: Option<Arc<Mutex<TextMaskModel>>>,
    text_mask_textures: &'a mut HashMap<usize, TextMaskTexturePage>,
    text_mask_synced_revision: &'a mut u64,
    cursor_occluder: Option<CleaningCursorOccluder>,
}

impl CleaningHooks<'_> {
    fn draw_text_mask_overlay_on_page_if_enabled(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        page_rect: Rect,
    ) {
        if !self.quick_text_mask_panel_open {
            return;
        }
        let Some(model) = self.text_mask_model.as_ref() else {
            return;
        };
        let clip_rect = ui.clip_rect().intersect(page_rect);
        if !clip_rect.is_positive() {
            return;
        }
        let painter = ui.painter().with_clip_rect(clip_rect);
        let guard = match model.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };

        let revision = guard.revision();
        if revision != *self.text_mask_synced_revision {
            *self.text_mask_synced_revision = revision;
            self.text_mask_textures.clear();
        }

        let Some(mask_page) = guard.page(page_idx) else {
            return;
        };
        if mask_page.mask_alpha.is_empty() {
            return;
        }
        draw_text_mask_overlay_on_page(TextMaskOverlayDrawParams {
            textures: self.text_mask_textures,
            ctx,
            painter: &painter,
            page_idx,
            page_rect,
            mask_size: mask_page.mask_size,
            mask_alpha: &mask_page.mask_alpha,
            current_frame: ctx.cumulative_frame_nr(),
        });
    }
}

impl CanvasHooks for CleaningHooks<'_> {
    fn draw_canvas_mask_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        _zoom: f32,
    ) {
        self.draw_text_mask_overlay_on_page_if_enabled(ui, ctx, page_idx, image_rect);
    }

    fn should_hide_on_top_bubble(
        &mut self,
        page_idx: usize,
        _bubble: &crate::project::Bubble,
        bubble_rect: Rect,
    ) -> bool {
        self.cursor_occluder.is_some_and(|occluder| {
            occluder.page_idx == page_idx
                && circle_intersects_rect(
                    occluder.center_scene_pos,
                    occluder.radius_scene,
                    bubble_rect,
                )
        })
    }

    fn should_hide_aside_bubble_line(
        &mut self,
        page_idx: usize,
        _bubble: &crate::project::Bubble,
        line_start: Pos2,
        line_end: Pos2,
    ) -> bool {
        self.cursor_occluder.is_some_and(|occluder| {
            occluder.page_idx == page_idx
                && circle_intersects_segment(
                    occluder.center_scene_pos,
                    occluder.radius_scene,
                    line_start,
                    line_end,
                )
        })
    }
}

fn circle_intersects_rect(center: Pos2, radius: f32, rect: Rect) -> bool {
    let closest = Pos2::new(
        center.x.clamp(rect.left(), rect.right()),
        center.y.clamp(rect.top(), rect.bottom()),
    );
    center.distance_sq(closest) <= radius * radius
}

fn circle_intersects_segment(center: Pos2, radius: f32, start: Pos2, end: Pos2) -> bool {
    distance_sq_to_segment(center, start, end) <= radius * radius
}

fn distance_sq_to_segment(point: Pos2, start: Pos2, end: Pos2) -> f32 {
    let segment = end - start;
    let segment_len_sq = segment.length_sq();
    if segment_len_sq <= f32::EPSILON {
        return point.distance_sq(start);
    }
    let t = ((point - start).dot(segment) / segment_len_sq).clamp(0.0, 1.0);
    let projection = start + segment * t;
    point.distance_sq(projection)
}

fn run_quick_text_clean_on_page(
    task: QuickTextCleanTask,
    auto_expand_px: usize,
    uneven_tool: UnevenBackgroundTool,
    contour_tolerance: u8,
) -> QuickTextCleanPageResult {
    let page_idx = task.page_idx;
    match run_quick_text_clean_on_page_impl(task, auto_expand_px, uneven_tool, contour_tolerance) {
        Ok(result) => result,
        Err(error) => QuickTextCleanPageResult {
            page_idx,
            patch: None,
            regions_total: 0,
            regions_filled: 0,
            regions_skipped: 0,
            error: Some(error),
            missing_mask: false,
        },
    }
}

fn run_quick_text_clean_on_page_impl(
    task: QuickTextCleanTask,
    auto_expand_px: usize,
    uneven_tool: UnevenBackgroundTool,
    contour_tolerance: u8,
) -> Result<QuickTextCleanPageResult, String> {
    let page_idx = task.page_idx;
    let base_rgba = image::open(&task.page_path)
        .map_err(|err| {
            format!(
                "Не удалось открыть страницу {}: {err}",
                task.page_path.display()
            )
        })?
        .to_rgba8();
    let width = base_rgba.width() as usize;
    let height = base_rgba.height() as usize;
    let Some(mask_page) = resolve_quick_clean_mask_page(&task) else {
        return Ok(QuickTextCleanPageResult {
            page_idx,
            patch: None,
            regions_total: 0,
            regions_filled: 0,
            regions_skipped: 0,
            error: None,
            missing_mask: true,
        });
    };
    if mask_page.mask_alpha.is_empty() {
        return Ok(QuickTextCleanPageResult {
            page_idx,
            patch: None,
            regions_total: 0,
            regions_filled: 0,
            regions_skipped: 0,
            error: None,
            missing_mask: true,
        });
    }

    let mut binary_mask = mask_page.mask_alpha;
    if mask_page.mask_size != [width as u32, height as u32] {
        binary_mask = resize_binary_mask_nearest(
            &binary_mask,
            mask_page.mask_size[0] as usize,
            mask_page.mask_size[1] as usize,
            width,
            height,
        );
    }
    for value in &mut binary_mask {
        *value = if *value > 0 { 255 } else { 0 };
    }

    let components = extract_connected_components(&binary_mask, width, height);
    if components.pixels.is_empty() {
        return Ok(QuickTextCleanPageResult {
            page_idx,
            patch: None,
            regions_total: 0,
            regions_filled: 0,
            regions_skipped: 0,
            error: None,
            missing_mask: false,
        });
    }

    let expanded_components = if auto_expand_px > 0 {
        let dilated = dilate_binary_mask(&binary_mask, width, height, auto_expand_px);
        Some(extract_connected_components(&dilated, width, height))
    } else {
        None
    };
    let mut patch = egui::ColorImage::filled([width, height], egui::Color32::TRANSPARENT);
    let mut regions_filled = 0usize;
    let mut regions_skipped = 0usize;
    for comp_pixels in &components.pixels {
        let label = comp_pixels
            .first()
            .and_then(|idx| components.labels.get(*idx))
            .copied()
            .unwrap_or(-1);
        if label < 0 {
            regions_skipped = regions_skipped.saturating_add(1);
            continue;
        }

        if let Some(fill_color) = sample_uniform_contour_color(
            &base_rgba,
            width,
            height,
            &components.labels,
            label,
            comp_pixels,
            contour_tolerance,
        ) {
            fill_region_with_color(&mut patch, comp_pixels, fill_color);
            regions_filled = regions_filled.saturating_add(1);
            continue;
        }

        let mut expanded_filled = false;
        if auto_expand_px > 0
            && let Some(expanded) = expanded_components.as_ref()
            && let Some(seed) = comp_pixels.first().copied()
        {
            let exp_label = expanded.labels.get(seed).copied().unwrap_or(-1);
            if exp_label >= 0 {
                let exp_pixels = expanded
                    .pixels
                    .get(exp_label as usize)
                    .cloned()
                    .unwrap_or_default();
                if !exp_pixels.is_empty()
                    && let Some(fill_color) = sample_uniform_contour_color(
                        &base_rgba,
                        width,
                        height,
                        &expanded.labels,
                        exp_label,
                        &exp_pixels,
                        contour_tolerance,
                    )
                {
                    fill_region_with_color(&mut patch, &exp_pixels, fill_color);
                    regions_filled = regions_filled.saturating_add(1);
                    expanded_filled = true;
                }
            }
        }

        if !expanded_filled {
            match uneven_tool {
                UnevenBackgroundTool::NoProcessing => {
                    regions_skipped = regions_skipped.saturating_add(1);
                }
            }
        }
    }

    let has_patch = patch.pixels.iter().any(|px| px.a() > 0);
    Ok(QuickTextCleanPageResult {
        page_idx,
        patch: has_patch.then_some(patch),
        regions_total: components.pixels.len(),
        regions_filled,
        regions_skipped,
        error: None,
        missing_mask: false,
    })
}

#[derive(Debug)]
struct ConnectedComponents {
    labels: Vec<i32>,
    pixels: Vec<Vec<usize>>,
}

fn resolve_quick_clean_mask_page(task: &QuickTextCleanTask) -> Option<TextMaskLoadPage> {
    if let Some(mask) = task.mask_from_model.as_ref() {
        return Some(mask.clone());
    }
    if !task.mask_path.exists() {
        return None;
    }
    let mask_img = image::open(&task.mask_path).ok()?.to_luma8();
    let w = mask_img.width() as usize;
    let h = mask_img.height() as usize;
    if w == 0 || h == 0 {
        return None;
    }
    let mut alpha = Vec::with_capacity(w.saturating_mul(h));
    for px in mask_img.into_raw() {
        alpha.push(if px > 0 { 255 } else { 0 });
    }
    Some(TextMaskLoadPage {
        page_idx: task.page_idx,
        mask_size: [w as u32, h as u32],
        mask_alpha: alpha,
    })
}

fn extract_connected_components(mask: &[u8], width: usize, height: usize) -> ConnectedComponents {
    let mut labels = vec![-1i32; width.saturating_mul(height)];
    let mut pixels = Vec::<Vec<usize>>::new();
    if mask.is_empty() || width == 0 || height == 0 {
        return ConnectedComponents { labels, pixels };
    }

    let mut queue = VecDeque::<usize>::new();
    let mut label = 0i32;
    for seed in 0..mask.len() {
        if mask[seed] == 0 || labels[seed] >= 0 {
            continue;
        }
        labels[seed] = label;
        queue.clear();
        queue.push_back(seed);
        let mut component_pixels = Vec::<usize>::new();
        while let Some(idx) = queue.pop_front() {
            component_pixels.push(idx);
            let x = idx % width;
            let y = idx / width;
            for ny in y.saturating_sub(1)..=(y + 1).min(height - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                    let nidx = ny.saturating_mul(width).saturating_add(nx);
                    if mask[nidx] == 0 || labels[nidx] >= 0 {
                        continue;
                    }
                    labels[nidx] = label;
                    queue.push_back(nidx);
                }
            }
        }
        pixels.push(component_pixels);
        label = label.saturating_add(1);
    }
    ConnectedComponents { labels, pixels }
}

fn sample_uniform_contour_color(
    base_rgba: &image::RgbaImage,
    width: usize,
    height: usize,
    labels: &[i32],
    target_label: i32,
    component_pixels: &[usize],
    tolerance: u8,
) -> Option<egui::Color32> {
    let raw = base_rgba.as_raw();
    let mut sampled = 0usize;
    let mut ref_color = [0u8; 3];
    let mut sum_r = 0u64;
    let mut sum_g = 0u64;
    let mut sum_b = 0u64;

    for idx in component_pixels {
        let x = idx % width;
        let y = idx / width;
        for ny in y.saturating_sub(1)..=(y + 1).min(height - 1) {
            for nx in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                let nidx = ny.saturating_mul(width).saturating_add(nx);
                if labels.get(nidx).copied().unwrap_or(-1) == target_label {
                    continue;
                }
                let src = nidx.saturating_mul(4);
                if src + 2 >= raw.len() {
                    continue;
                }
                let rgb = [raw[src], raw[src + 1], raw[src + 2]];
                if sampled == 0 {
                    ref_color = rgb;
                } else if !color_within_tolerance(ref_color, rgb, tolerance) {
                    return None;
                }
                sum_r = sum_r.saturating_add(rgb[0] as u64);
                sum_g = sum_g.saturating_add(rgb[1] as u64);
                sum_b = sum_b.saturating_add(rgb[2] as u64);
                sampled = sampled.saturating_add(1);
            }
        }
    }

    if sampled < 4 {
        return None;
    }
    Some(egui::Color32::from_rgb(
        (sum_r / sampled as u64) as u8,
        (sum_g / sampled as u64) as u8,
        (sum_b / sampled as u64) as u8,
    ))
}

fn color_within_tolerance(a: [u8; 3], b: [u8; 3], tolerance: u8) -> bool {
    let td = tolerance as i16;
    (a[0] as i16 - b[0] as i16).abs() <= td
        && (a[1] as i16 - b[1] as i16).abs() <= td
        && (a[2] as i16 - b[2] as i16).abs() <= td
}

fn fill_region_with_color(patch: &mut egui::ColorImage, pixels: &[usize], color: egui::Color32) {
    for idx in pixels {
        if let Some(dst) = patch.pixels.get_mut(*idx) {
            *dst = color;
        }
    }
}

fn dilate_binary_mask(mask: &[u8], width: usize, height: usize, radius: usize) -> Vec<u8> {
    if mask.is_empty() || width == 0 || height == 0 {
        return Vec::new();
    }
    if radius == 0 {
        return mask.to_vec();
    }
    let mut out = vec![0u8; mask.len()];
    for y in 0..height {
        let y0 = y.saturating_sub(radius);
        let y1 = (y + radius).min(height - 1);
        for x in 0..width {
            let x0 = x.saturating_sub(radius);
            let x1 = (x + radius).min(width - 1);
            let mut any = false;
            'scan: for yy in y0..=y1 {
                let row = yy.saturating_mul(width);
                for xx in x0..=x1 {
                    if mask[row + xx] != 0 {
                        any = true;
                        break 'scan;
                    }
                }
            }
            out[y.saturating_mul(width).saturating_add(x)] = if any { 255 } else { 0 };
        }
    }
    out
}

fn resize_binary_mask_nearest(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<u8> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 || src.is_empty() {
        return vec![0u8; dst_w.saturating_mul(dst_h)];
    }
    let mut out = vec![0u8; dst_w.saturating_mul(dst_h)];
    for y in 0..dst_h {
        let sy = y.saturating_mul(src_h) / dst_h;
        for x in 0..dst_w {
            let sx = x.saturating_mul(src_w) / dst_w;
            let sidx = sy.saturating_mul(src_w).saturating_add(sx);
            let didx = y.saturating_mul(dst_w).saturating_add(x);
            out[didx] = src.get(sidx).copied().unwrap_or(0);
        }
    }
    out
}

fn resize_color_image_nearest(
    src: &egui::ColorImage,
    dst_w: usize,
    dst_h: usize,
) -> egui::ColorImage {
    if src.size[0] == 0 || src.size[1] == 0 || dst_w == 0 || dst_h == 0 {
        return egui::ColorImage::filled([dst_w.max(1), dst_h.max(1)], egui::Color32::TRANSPARENT);
    }
    let src_w = src.size[0];
    let src_h = src.size[1];
    let mut out = egui::ColorImage::filled([dst_w, dst_h], egui::Color32::TRANSPARENT);
    for y in 0..dst_h {
        let sy = y.saturating_mul(src_h) / dst_h;
        for x in 0..dst_w {
            let sx = x.saturating_mul(src_w) / dst_w;
            let sidx = sy.saturating_mul(src_w).saturating_add(sx);
            let didx = y.saturating_mul(dst_w).saturating_add(x);
            if let (Some(src_px), Some(dst_px)) = (src.pixels.get(sidx), out.pixels.get_mut(didx)) {
                *dst_px = *src_px;
            }
        }
    }
    out
}

fn load_text_masks_from_storage(
    storage_dir: &Path,
    page_indices: &[usize],
) -> Result<TextMaskLoadResult, String> {
    if !storage_dir.exists() {
        return Ok(TextMaskLoadResult {
            pages: Vec::new(),
            loaded: 0,
            missing: page_indices.len(),
            failed: 0,
        });
    }

    let mut pages = Vec::<TextMaskLoadPage>::new();
    let mut loaded = 0usize;
    let mut missing = 0usize;
    let mut failed = 0usize;

    for page_idx in page_indices {
        let path = text_detection_mask_file_path(storage_dir, *page_idx);
        if !path.exists() {
            missing = missing.saturating_add(1);
            continue;
        }
        match image::open(&path) {
            Ok(img) => {
                let luma = img.to_luma8();
                let w = luma.width();
                let h = luma.height();
                if w == 0 || h == 0 {
                    failed = failed.saturating_add(1);
                    continue;
                }
                let mut alpha = Vec::with_capacity((w as usize).saturating_mul(h as usize));
                for px in luma.into_raw() {
                    alpha.push(if px > 0 { 255 } else { 0 });
                }
                pages.push(TextMaskLoadPage {
                    page_idx: *page_idx,
                    mask_size: [w, h],
                    mask_alpha: alpha,
                });
                loaded = loaded.saturating_add(1);
            }
            Err(_) => {
                failed = failed.saturating_add(1);
            }
        }
    }

    Ok(TextMaskLoadResult {
        pages,
        loaded,
        missing,
        failed,
    })
}

fn save_clean_overlay_snapshots(
    save_dir: &std::path::Path,
    snapshots: &[(String, Arc<image::RgbaImage>)],
) -> Result<(), String> {
    std::fs::create_dir_all(save_dir)
        .map_err(|err| format!("Не удалось создать папку {}: {err}", save_dir.display()))?;
    for (stem, image) in snapshots {
        let dst = save_dir.join(format!("{stem}.png"));
        image
            .save(&dst)
            .map_err(|err| format!("Не удалось сохранить клин {}: {err}", dst.display()))?;
    }
    Ok(())
}

fn text_detection_mask_file_path(dir: &Path, page_idx: usize) -> PathBuf {
    dir.join(format!("{page_idx:05}_mask.png"))
}

struct TextMaskOverlayDrawParams<'a> {
    textures: &'a mut HashMap<usize, TextMaskTexturePage>,
    ctx: &'a egui::Context,
    painter: &'a egui::Painter,
    page_idx: usize,
    page_rect: Rect,
    mask_size: [u32; 2],
    mask_alpha: &'a [u8],
    current_frame: u64,
}

fn draw_text_mask_overlay_on_page(params: TextMaskOverlayDrawParams<'_>) {
    let TextMaskOverlayDrawParams {
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
        let page_tex = build_text_mask_texture_page(ctx, page_idx, [mask_w, mask_h], mask_alpha);
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

fn build_text_mask_texture_page(
    ctx: &egui::Context,
    page_idx: usize,
    size: [usize; 2],
    alpha: &[u8],
) -> TextMaskTexturePage {
    let w = size[0];
    let h = size[1];
    if w == 0 || h == 0 {
        return TextMaskTexturePage {
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
            let tw = (w - x).min(TEXT_MASK_TILE_SIDE);
            let th = (h - y).min(TEXT_MASK_TILE_SIDE);
            let tile_img = build_text_mask_tile_image(size, alpha, x, y, tw, th);
            let texture = ctx.load_texture(
                format!("cleaning-text-mask-{page_idx}-{x}-{y}"),
                tile_img,
                TEXT_MASK_TEXTURE_OPTIONS,
            );
            tiles.push(TextMaskTextureTile {
                texture,
                origin_px: [x, y],
                size_px: [tw, th],
            });
            x += TEXT_MASK_TILE_SIDE;
        }
        y += TEXT_MASK_TILE_SIDE;
    }
    TextMaskTexturePage {
        size,
        tiles,
        last_used_frame: 0,
    }
}

fn text_mask_texture_page_estimated_bytes(page_tex: &TextMaskTexturePage) -> u64 {
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

fn build_text_mask_tile_image(
    size: [usize; 2],
    alpha: &[u8],
    origin_x: usize,
    origin_y: usize,
    tile_w: usize,
    tile_h: usize,
) -> egui::ColorImage {
    let full_w = size[0];
    let mut raw = vec![0u8; tile_w.saturating_mul(tile_h).saturating_mul(4)];
    for ty in 0..tile_h {
        let sy = origin_y + ty;
        let row_off = sy.saturating_mul(full_w);
        for tx in 0..tile_w {
            let sx = origin_x + tx;
            let src_idx = row_off.saturating_add(sx);
            let dst_idx = ty
                .saturating_mul(tile_w)
                .saturating_add(tx)
                .saturating_mul(4);
            let src_alpha = alpha.get(src_idx).copied().unwrap_or(0);
            let a = ((src_alpha as u16 * TEXT_MASK_VISUAL_ALPHA_MAX as u16) / 255) as u8;
            raw[dst_idx] = a;
            raw[dst_idx + 1] = 0;
            raw[dst_idx + 2] = 0;
            raw[dst_idx + 3] = a;
        }
    }
    egui::ColorImage::from_rgba_premultiplied([tile_w, tile_h], &raw)
}
