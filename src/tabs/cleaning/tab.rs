/*
FILE HEADER (tabs/cleaning/tab.rs)
- Назначение: состояние вкладки Cleaning и координация `CanvasView` + активного cleaning-инструмента.
- Ключевые поля `CleaningTabState`:
  - `canvas`: холст с overlay-слоями клина. The Cleaning tab has NO bottom-hint (the canvas
    `bottom_hint` stays `None`, so the overlay is not drawn) and does not persist a collapsed flag.
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
    с прогрессом и применением patch-ов в `CleanOverlaysModel`; pixel-level autoclean algorithm lives in
    `autoclean.rs`.
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
use super::autoclean::{autoclean_page, UnevenBackgroundTool};
use super::tools::{
    AotInpaintTool, CleaningCursorOccluder, CleaningTool, FluxFillInpaintTool, GradientFillTool,
    LamaInpaintTool, LamaMpeInpaintTool, SdxlInpaintTool, StampTool, StrokeModifiers, StrokePoint,
    TextureSynthesisInpaintTool, ZamazkaTool,
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
use crate::widgets::{AiButton, AiCaps, AiRequirement, WheelComboBox, WheelSlider};
use eframe::egui;
use egui::{Align, Color32, Layout, Pos2, Rect};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use ms_thread as thread;

const STROKE_OVERLAY_UPLOAD_MIN_INTERVAL_S: f64 = 1.0 / 30.0;
const TEXT_MASK_TILE_SIDE: usize = 1024;
const TEXT_MASK_VISUAL_ALPHA_MAX: u8 = 96;
/// Runtime (not `const`) because `t!` is not const; resolves the active catalog value.
#[must_use]
fn save_hint_text() -> &'static str {
    t!("cleaning.tab.saving_status")
}
const FLOATING_PANEL_MARGIN: f32 = 12.0;
/// Дополнительный отступ панели инструментов от правого края вьюпорта, чтобы
/// плавающее окно не перекрывало вертикальный скроллбар холста.
const CLEANING_TOOL_PANEL_SCROLLBAR_MARGIN: f32 = 15.0;
const CLEANING_TOOL_PANEL_DEFAULT_WIDTH: f32 = 352.0;
const CLEANING_TOOL_BUTTONS_PER_ROW: usize = 3;
const BRUSH_TOOL_INDICES: [usize; 2] = [0, 1];
const MASK_REMOVAL_TOOL_INDICES: [usize; 5] = [2, 3, 4, 5, 6];
// Инструменты редактирования области (SDXL, FLUX.1 Fill) — отдельной строкой.
const AREA_EDIT_TOOL_INDICES: [usize; 2] = [7, 8];


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
    // Sampling mode the tiles were uploaded with. When the active pixel
    // inspection mode flips, the page is rebuilt so the mask matches the
    // source/overlay sampling instead of staying fixed at one filter.
    texture_options: egui::TextureOptions,
}

#[derive(Debug, Clone)]
struct TextMaskLoadPage {
    page_idx: usize,
    /// Source-page pixel size `[w, h]` — the space `blocks` live in. Used to scale
    /// detector boxes into page space for autoclean; `[0, 0]` when unknown.
    source_size: [u32; 2],
    mask_size: [u32; 2],
    mask_alpha: Vec<u8>,
    /// Detector text boxes `[x1, y1, x2, y2]` in source-page pixel space, or `None`
    /// when unknown (manual mask edit, or the disk fallback which has no blocks JSON).
    blocks: Option<Vec<[i32; 4]>>,
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
    regions_partial: usize,
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
    regions_partial: usize,
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
    quick_clean_spread_radius_px: i32,
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
            Box::<SdxlInpaintTool>::default(),
            Box::<FluxFillInpaintTool>::default(),
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
            quick_clean_spread_radius_px: 48,
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

    pub fn current_page_local_view_center(&self) -> Option<(usize, egui::Vec2)> {
        self.canvas.current_page_local_view_center()
    }

    pub fn focus_page(&mut self, page_idx: usize, center_px: Option<egui::Vec2>, zoom: f32) {
        self.canvas.focus_page(page_idx, center_px, zoom);
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
        // NEAREST sampling and the pixel grid switch together from one
        // DPI-correct magnification threshold (device px per source px).
        let pixel_inspection_enabled = self.canvas.pixel_inspection_recommended(ctx);
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
        // Request a repaint only on real activity. A merely open quick-clean panel
        // must not force 60 fps: egui already repaints on panel interaction (drag,
        // resize, hover), and its spinners/progress are gated on the in-progress
        // flags below, so an idle open panel has nothing to animate.
        if self.save_job_in_progress
            || hotkeys_handled
            || history_hotkeys_handled
            || self.text_mask_load_in_progress
            || self.quick_clean_job_in_progress
        {
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
        // PyTorch tools gate on the process-global Torch capability (strict), the
        // same signal their `AiButton` selection buttons use, so the active-tool
        // auto-switch and the button enabled state stay in agreement.
        self.tools.get(idx).is_some_and(|tool| {
            !tool.pytorch_required() || AiRequirement::Torch.satisfied(&AiCaps::current())
        })
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
                            ui.checkbox(&mut overlays_visible, t!("cleaning.tab.show_layer_button"));
                            if ui.button(t!("cleaning.tab.clear_current_layer_button")).clicked() {
                                clear_page = true;
                            }
                            if ui
                                .add_enabled(
                                    !self.save_job_in_progress,
                                    egui::Button::new(t!("cleaning.tab.save_clean_button")),
                                )
                                .clicked()
                            {
                                request_save = true;
                            }
                        });
                        ui.horizontal(|ui| {
                            let quick_button = ui.button(t!("cleaning.tab.quick_clean_heading"));
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
                                        ui.label(save_hint_text());
                                    }
                                },
                            );
                        });

                        ui.small(t!("cleaning.tab.paint_erase_hint"));
                        ui.small(t!("cleaning.tab.scroll_brush_hint"));
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
        let tool_panel_default_pos = egui::pos2(
            (canvas_rect.right()
                - CLEANING_TOOL_PANEL_DEFAULT_WIDTH
                - FLOATING_PANEL_MARGIN
                - CLEANING_TOOL_PANEL_SCROLLBAR_MARGIN)
                .max(canvas_rect.left() + FLOATING_PANEL_MARGIN),
            canvas_rect.top() + FLOATING_PANEL_MARGIN,
        );
        let window = egui::Window::new(t!("cleaning.tab.tools_heading")).id(egui::Id::new("cleaning.tab.tools_heading"))
            .id(egui::Id::new("cleaning_tool_floating_panel"))
            .default_pos(tool_panel_default_pos)
            .default_width(CLEANING_TOOL_PANEL_DEFAULT_WIDTH)
            .collapsible(true)
            .resizable(false)
            .show(ctx, |ui| {
                self.draw_tool_button_group(
                    ui,
                    t!("cleaning.tab.brushes_label"),
                    &BRUSH_TOOL_INDICES,
                    &mut activate_tool_idx,
                );
                ui.add_space(6.0);
                self.draw_tool_button_group(
                    ui,
                    t!("cleaning.tab.mask_removal_label"),
                    &MASK_REMOVAL_TOOL_INDICES,
                    &mut activate_tool_idx,
                );
                // SDXL и FLUX.1 Fill — на отдельной строке инструментов редактирования
                // области, чтобы не растягивать панель в ширину.
                self.draw_tool_button_rows(ui, &AREA_EDIT_TOOL_INDICES, &mut activate_tool_idx);
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

    fn draw_tool_button_group(
        &self,
        ui: &mut egui::Ui,
        title: &str,
        tool_indices: &[usize],
        activate_tool_idx: &mut usize,
    ) {
        ui.label(egui::RichText::new(title).strong());
        self.draw_tool_button_rows(ui, tool_indices, activate_tool_idx);
    }

    fn draw_tool_button_rows(
        &self,
        ui: &mut egui::Ui,
        tool_indices: &[usize],
        activate_tool_idx: &mut usize,
    ) {
        for row in tool_indices.chunks(CLEANING_TOOL_BUTTONS_PER_ROW) {
            ui.horizontal(|ui| {
                for &idx in row {
                    let Some(label) = self.tool_labels.get(idx) else {
                        continue;
                    };
                    let is_selected = *activate_tool_idx == idx;
                    let requires_pytorch =
                        self.tools.get(idx).is_some_and(|tool| tool.pytorch_required());
                    // AI tools use a self-gating `AiButton` (Torch requirement, framed
                    // to match the plain tool buttons) with a "Torch" runtime marker;
                    // it disables itself and shows the reason when Torch is
                    // unavailable. Non-AI tools stay plain always-enabled buttons.
                    let clicked = if requires_pytorch {
                        AiButton::new(label.as_str(), AiRequirement::Torch)
                            .selected(is_selected)
                            .marker("Torch")
                            .draw(ui)
                            .response
                            .clicked()
                    } else {
                        ui.add(egui::Button::new(label.as_str()).selected(is_selected))
                            .clicked()
                    };
                    if clicked {
                        *activate_tool_idx = idx;
                    }
                }
            });
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
        let window = egui::Window::new(t!("cleaning.tab.quick_clean_heading")).id(egui::Id::new("cleaning.tab.quick_clean_heading"))
            .id(egui::Id::new("cleaning_quick_text_mask_panel"))
            .default_pos(canvas_rect.left_top() + egui::vec2(1080.0, 12.0))
            .collapsible(true)
            .resizable(true)
            .open(&mut panel_open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(t!("cleaning.tab.mask_spread_radius_label")).on_hover_text(
                        t!("cleaning.tab.mask_spread_radius_hint"),
                    );
                    ui.add(
                        WheelSlider::new(&mut self.quick_clean_spread_radius_px, 0..=128)
                            .suffix(t!("cleaning.tab.pixels_suffix")),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(t!("cleaning.tab.uneven_background_tool_label"));
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
                            egui::Button::new(t!("cleaning.tab.clean_current_page_button")),
                        )
                        .clicked()
                    {
                        run_current_page = true;
                    }
                    if ui
                        .add_enabled(
                            !self.quick_clean_job_in_progress,
                            egui::Button::new(t!("cleaning.tab.clean_all_pages_button")),
                        )
                        .clicked()
                    {
                        run_all_pages = true;
                    }
                });
                if self.text_mask_load_in_progress {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.small(t!("cleaning.tab.mask_loading_status"));
                    });
                } else if let Some(status) = self.text_mask_load_status.as_ref() {
                    ui.small(status);
                }
                if self.quick_clean_job_in_progress {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.small(t!("cleaning.tab.quick_clean_running_status"));
                    });
                }
                if self.quick_clean_progress.total_pages > 0 {
                    let progress = (self.quick_clean_progress.done_pages as f32
                        / self.quick_clean_progress.total_pages as f32)
                        .clamp(0.0, 1.0);
                    ui.add(egui::ProgressBar::new(progress).text(tf!("cleaning.tab.pages_progress_status", done = self.quick_clean_progress.done_pages, total = self.quick_clean_progress.total_pages)));
                    ui.small(tf!("cleaning.tab.regions_progress_status", filled = self.quick_clean_progress.regions_filled, skipped = self.quick_clean_progress.regions_skipped, partial = self.quick_clean_progress.regions_partial, page_errors = self.quick_clean_progress.failed_pages, missing = self.quick_clean_progress.missing_masks));
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
            self.text_mask_load_status = Some(t!("cleaning.tab.mask_already_loaded_status").to_string());
            return;
        }

        let storage_dir = project.paths.text_detection_dir.clone();
        let (tx, rx) = mpsc::channel::<Result<TextMaskLoadResult, String>>();
        self.text_mask_load_rx = Some(rx);
        self.text_mask_load_in_progress = true;
        self.text_mask_load_status =
            Some(t!("cleaning.tab.mask_load_attempt_status").to_string());
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
                    Some(t!("cleaning.tab.mask_load_aborted_status").to_string());
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
                self.text_mask_load_status = Some(tf!("cleaning.tab.mask_load_progress_status", applied = applied, total = result
                        .loaded
                        .saturating_add(result.missing)
                        .saturating_add(result.failed), loaded = result.loaded, missing = result.missing, failed = result.failed));
            }
            Err(error) => {
                self.text_mask_load_status = Some(tf!("cleaning.tab.mask_load_error", error = error));
            }
        }
    }

    fn start_save_job(&mut self, project: &ProjectData) {
        if self.save_job_in_progress {
            return;
        }
        let Some(model) = self.overlays_model.as_ref().cloned() else {
            self.save_status_text =
                Some(t!("cleaning.tab.save_unavailable_no_model_error").to_string());
            return;
        };
        let save_dir = project.paths.clean_layers_dir.clone();
        let overlay_snapshots = match model.lock() {
            Ok(locked) => locked.save_snapshots(),
            Err(_) => {
                self.save_job_in_progress = false;
                self.save_job_rx = None;
                self.save_status_text =
                    Some(t!("cleaning.tab.overlay_model_lock_error").to_string());
                return;
            }
        };
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        self.save_job_rx = Some(rx);
        self.save_job_in_progress = true;
        self.save_status_text = Some(t!("cleaning.tab.saving_clean_status").to_string());

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
                self.save_status_text = Some(t!("cleaning.tab.clean_saved_status").to_string());
            }
            Ok(Err(err)) => {
                self.save_job_in_progress = false;
                self.save_job_rx = None;
                self.save_status_text = Some(tf!("cleaning.tab.save_clean_error", err = err));
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.save_job_in_progress = false;
                self.save_job_rx = None;
                self.save_status_text = Some(t!("cleaning.tab.save_aborted_status").to_string());
            }
        }
    }

    fn start_quick_text_clean_job(&mut self, project: &ProjectData, page_indices: Vec<usize>) {
        if self.quick_clean_job_in_progress {
            return;
        }
        if page_indices.is_empty() {
            self.quick_clean_status_text = Some(t!("cleaning.tab.no_pages_error").to_string());
            return;
        }
        if self.overlays_model.is_none() {
            self.quick_clean_status_text =
                Some(t!("cleaning.tab.quick_clean_unavailable_no_model_error").to_string());
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
                    source_size: page.source_size,
                    mask_size: page.mask_size,
                    mask_alpha: page.mask_alpha,
                    blocks: page.blocks,
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
            self.quick_clean_status_text = Some(t!("cleaning.tab.no_available_pages_error").to_string());
            return;
        }

        let spread_radius_px = self.quick_clean_spread_radius_px.clamp(0, 128) as usize;
        let uneven_tool = self.quick_clean_uneven_background_tool;
        let (tx, rx) = mpsc::channel::<QuickTextCleanJobEvent>();
        self.quick_clean_job_rx = Some(rx);
        self.quick_clean_job_in_progress = true;
        self.quick_clean_progress = QuickTextCleanProgress::default();
        self.quick_clean_status_text = Some(t!("cleaning.tab.quick_clean_started_status").to_string());

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
                        let result =
                            run_quick_text_clean_on_page(task, spread_radius_px, uneven_tool);
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
                        Some(t!("cleaning.tab.quick_clean_reading_status").to_string());
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
                    self.quick_clean_progress.regions_partial = self
                        .quick_clean_progress
                        .regions_partial
                        .saturating_add(result.regions_partial);
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
                    self.quick_clean_status_text = Some(tf!("cleaning.tab.quick_clean_page_done_status", page = result.page_idx, regions = result.regions_total, filled = result.regions_filled, skipped = result.regions_skipped, partial = result.regions_partial));
                }
                Ok(QuickTextCleanJobEvent::Finished) => {
                    self.quick_clean_job_in_progress = false;
                    self.quick_clean_job_rx = None;
                    self.quick_clean_status_text = Some(tf!("cleaning.tab.quick_clean_finished_status", done = self.quick_clean_progress.done_pages, total = self.quick_clean_progress.total_pages, filled = self.quick_clean_progress.regions_filled, skipped = self.quick_clean_progress.regions_skipped, partial = self.quick_clean_progress.regions_partial, errors = self.quick_clean_progress.failed_pages, missing = self.quick_clean_progress.missing_masks));
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.quick_clean_job_in_progress = false;
                    self.quick_clean_job_rx = None;
                    self.quick_clean_status_text =
                        Some(t!("cleaning.tab.quick_clean_aborted_status").to_string());
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
        ctx.any_popup_open()
            || self.pointer_in_any_panel(pointer_pos)
            || self.canvas.pointer_over_scrollbar(pointer_pos)
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
        let wants_keyboard_input = ctx.egui_wants_keyboard_input();
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
        if ctx.egui_wants_keyboard_input() || self.stroke_active {
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
        let (pointer_pos, modifiers, r_down, scroll_delta) = ctx.input(|i| {
            (
                i.pointer.hover_pos(),
                i.modifiers,
                i.key_down(egui::Key::R),
                i.smooth_scroll_delta,
            )
        });
        let Some(pointer_pos) = pointer_pos else {
            return false;
        };
        if !canvas_rect.contains(pointer_pos) {
            return false;
        }
        if !modifiers.shift && !r_down {
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
        let handled = active_tool.on_wheel_event_with_keys(wheel_delta, modifiers, r_down);
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
        pixel_inspection_nearest: bool,
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
        let texture_options = if pixel_inspection_nearest {
            egui::TextureOptions::NEAREST
        } else {
            egui::TextureOptions::LINEAR
        };
        draw_text_mask_overlay_on_page(TextMaskOverlayDrawParams {
            textures: self.text_mask_textures,
            ctx,
            painter: &painter,
            page_idx,
            page_rect,
            mask_size: mask_page.mask_size,
            mask_alpha: &mask_page.mask_alpha,
            current_frame: ctx.cumulative_frame_nr(),
            texture_options,
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
        zoom: f32,
    ) {
        // Mask sampling follows pixel inspection so a magnified source pixel
        // looks identical across source, clean overlay, and text mask.
        let pixel_inspection_nearest =
            crate::canvas::pixel_inspection_recommended_for(zoom, ctx.pixels_per_point());
        self.draw_text_mask_overlay_on_page_if_enabled(
            ui,
            ctx,
            page_idx,
            image_rect,
            pixel_inspection_nearest,
        );
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
    spread_radius_px: usize,
    uneven_tool: UnevenBackgroundTool,
) -> QuickTextCleanPageResult {
    let page_idx = task.page_idx;
    match run_quick_text_clean_on_page_impl(task, spread_radius_px, uneven_tool) {
        Ok(result) => result,
        Err(error) => QuickTextCleanPageResult {
            page_idx,
            patch: None,
            regions_total: 0,
            regions_filled: 0,
            regions_skipped: 0,
            regions_partial: 0,
            error: Some(error),
            missing_mask: false,
        },
    }
}

fn run_quick_text_clean_on_page_impl(
    task: QuickTextCleanTask,
    spread_radius_px: usize,
    uneven_tool: UnevenBackgroundTool,
) -> Result<QuickTextCleanPageResult, String> {
    let page_idx = task.page_idx;
    let base_rgba = image::open(&task.page_path)
        .map_err(|err| {
            tf!("cleaning.tab.open_page_error", path = task.page_path.display(), err = err)
        })?
        .to_rgba8();
    // `image` dimensions are u32; widening to usize is lossless on the supported 64-bit
    // targets. try_from keeps the narrowing-back path (below) honest and degrades a
    // pathological dimension to 0, which the engine treats as an empty page.
    let width = usize::try_from(base_rgba.width()).unwrap_or(0);
    let height = usize::try_from(base_rgba.height()).unwrap_or(0);
    let Some(mask_page) = resolve_quick_clean_mask_page(&task) else {
        return Ok(QuickTextCleanPageResult {
            page_idx,
            patch: None,
            regions_total: 0,
            regions_filled: 0,
            regions_skipped: 0,
            regions_partial: 0,
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
            regions_partial: 0,
            error: None,
            missing_mask: true,
        });
    }

    // Detector blocks live in source-page pixel space (`mask_page.source_size`).
    // Autoclean operates in page-pixel space (`width`/`height` of the opened page),
    // which is the SAME space the mask is resized into below. Blocks must therefore be
    // scaled by the source->page transform, NOT by the mask->page resize (mask_size ->
    // page). When source_size already equals the page size (the normal case) this is the
    // identity. See the plan's "Data plumbing" note.
    let blocks_page_space = mask_page.blocks.as_ref().map(|blocks| {
        scale_blocks_source_to_page(blocks, mask_page.source_size, width, height)
    });

    // Narrow the page size back to u32 for the mask-size comparison. `width`/`height`
    // originate from a u32 image dimension, so try_from cannot actually fail here; the
    // saturating fallback only guards against a future oversized source.
    let page_size_u32 = [
        u32::try_from(width).unwrap_or(u32::MAX),
        u32::try_from(height).unwrap_or(u32::MAX),
    ];
    let mut binary_mask = mask_page.mask_alpha;
    if mask_page.mask_size != page_size_u32 {
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

    let outcome = autoclean_page(
        &base_rgba,
        &binary_mask,
        width,
        height,
        spread_radius_px,
        uneven_tool,
        blocks_page_space.as_deref(),
    );
    let has_patch = outcome.patch.pixels.iter().any(|px| px.a() > 0);
    Ok(QuickTextCleanPageResult {
        page_idx,
        patch: has_patch.then_some(outcome.patch),
        regions_total: outcome.regions_total,
        regions_filled: outcome.regions_filled,
        regions_skipped: outcome.regions_skipped,
        regions_partial: outcome.regions_partial,
        error: None,
        missing_mask: false,
    })
}

/// Scale detector blocks from source-page pixel space to page-pixel space.
///
/// `blocks` are `[x1, y1, x2, y2]` in `source_size` pixel space. When `source_size`
/// already matches `page_w`/`page_h` (the normal case) the boxes are returned
/// unchanged; otherwise each edge is scaled by the source->page ratio, flooring the
/// min corner and ceiling the max corner to preserve a covering rect. This is the
/// source->page transform only — the mask nearest-resize (mask_size -> page) never
/// touches blocks.
///
/// Returns NO blocks (an empty vector) when the source size is degenerate (zero width
/// or height) — a zero dimension defines no scale factor, so the caller must fall back
/// to the cluster bbox rather than pass the boxes through unscaled. Individual boxes
/// whose scaled edges are non-finite or fall outside the `i32` range are dropped (the
/// same graceful degrade); an all-dropped set also falls back to the cluster bbox.
fn scale_blocks_source_to_page(
    blocks: &[[i32; 4]],
    source_size: [u32; 2],
    page_w: usize,
    page_h: usize,
) -> Vec<[i32; 4]> {
    let (sw, sh) = (source_size[0], source_size[1]);
    // A degenerate source size cannot define a scale factor. Drop the blocks (return
    // none, NOT the unscaled set) so candidate B falls back to the cluster bbox.
    if sw == 0 || sh == 0 {
        return Vec::new();
    }
    // Identity fast path ONLY for an exact nonzero size match.
    if usize::try_from(sw) == Ok(page_w) && usize::try_from(sh) == Ok(page_h) {
        return blocks.to_vec();
    }
    // Page dims come from a decoded image (<= u32::MAX). If they somehow do not fit u32
    // we cannot form a scale — drop the blocks (graceful: cluster-bbox fallback).
    let (Ok(pw), Ok(ph)) = (u32::try_from(page_w), u32::try_from(page_h)) else {
        return Vec::new();
    };
    let (fx, fy) = (f64::from(pw) / f64::from(sw), f64::from(ph) / f64::from(sh));
    blocks
        .iter()
        .filter_map(|&[x1, y1, x2, y2]| {
            Some([
                scale_edge_to_i32(f64::from(x1) * fx, false)?,
                scale_edge_to_i32(f64::from(y1) * fy, false)?,
                scale_edge_to_i32(f64::from(x2) * fx, true)?,
                scale_edge_to_i32(f64::from(y2) * fy, true)?,
            ])
        })
        .collect()
}

/// Round a scaled block edge to `i32`, rejecting values that cannot be represented.
///
/// `ceil` rounds up (max corner) versus down (min corner) so the integer rect still
/// covers the float rect. Returns `None` when `value` is non-finite or its rounded form
/// falls outside the `i32` range, so the caller drops that block instead of wrapping a
/// stray coordinate into a valid-looking box.
fn scale_edge_to_i32(value: f64, ceil: bool) -> Option<i32> {
    if !value.is_finite() {
        return None;
    }
    let rounded = if ceil { value.ceil() } else { value.floor() };
    if rounded < f64::from(i32::MIN) || rounded > f64::from(i32::MAX) {
        return None;
    }
    // Bounds-checked, integral f64 -> i32: exact, no truncation.
    Some(rounded as i32)
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
        // Disk fallback has no source_size/blocks JSON yet (remaining Phase 2 work):
        // report the mask raster size and no blocks, so candidate B uses the cluster
        // bbox fallback. `blocks` are only scaled against source_size, so its value is
        // inert while blocks is None.
        source_size: [w as u32, h as u32],
        mask_size: [w as u32, h as u32],
        mask_alpha: alpha,
        blocks: None,
    })
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
                    // Display-cache load: mask raster only, no detector blocks.
                    source_size: [w, h],
                    mask_size: [w, h],
                    mask_alpha: alpha,
                    blocks: None,
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
        .map_err(|err| tf!("cleaning.tab.create_dir_error", dir = save_dir.display(), err = err))?;
    for (stem, image) in snapshots {
        let dst = save_dir.join(format!("{stem}.png"));
        image
            .save(&dst)
            .map_err(|err| tf!("cleaning.tab.save_clean_file_error", path = dst.display(), err = err))?;
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
    texture_options: egui::TextureOptions,
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
        texture_options,
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

    // Rebuild when size changes or when the active sampling mode flips, so the
    // mask matches source/overlay sampling (mirror of the overlay runtime).
    let needs_rebuild = textures
        .get(&page_idx)
        .map(|page| page.size != [mask_w, mask_h] || page.texture_options != texture_options)
        .unwrap_or(true);
    if needs_rebuild {
        let page_tex = build_text_mask_texture_page(
            ctx,
            page_idx,
            [mask_w, mask_h],
            mask_alpha,
            texture_options,
        );
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
    // Viewport cull: the painter is already clipped to the visible page region,
    // so skip tiles whose destination rect falls outside it. `intersects`
    // keeps partially-visible edge tiles.
    let viewport_rect = painter.clip_rect();
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
        if !dst.intersects(viewport_rect) {
            continue;
        }
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
    texture_options: egui::TextureOptions,
) -> TextMaskTexturePage {
    let w = size[0];
    let h = size[1];
    if w == 0 || h == 0 {
        return TextMaskTexturePage {
            size,
            tiles: Vec::new(),
            last_used_frame: 0,
            texture_options,
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
                texture_options,
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
        texture_options,
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

#[cfg(test)]
mod tests {
    use super::{scale_blocks_source_to_page, scale_edge_to_i32};

    #[test]
    fn scale_blocks_identity_passthrough_on_exact_size_match() {
        let blocks = [[1, 2, 3, 4], [10, 20, 30, 40]];
        // Nonzero source size equal to the page size returns the blocks unchanged.
        let out = scale_blocks_source_to_page(&blocks, [100, 200], 100, 200);
        assert_eq!(out, blocks.to_vec());
    }

    #[test]
    fn scale_blocks_degenerate_source_drops_blocks() {
        let blocks = [[1, 2, 3, 4]];
        // Finding 5: a zero source dimension yields NO blocks (not the unscaled set), so
        // candidate B falls back to the cluster bbox.
        assert!(scale_blocks_source_to_page(&blocks, [0, 200], 100, 200).is_empty());
        assert!(scale_blocks_source_to_page(&blocks, [100, 0], 100, 200).is_empty());
    }

    #[test]
    fn scale_blocks_scales_and_rounds_outward() {
        let blocks = [[10, 20, 30, 40]];
        // Source 100x200 -> page 200x400 doubles every edge; min floored, max ceiled.
        let out = scale_blocks_source_to_page(&blocks, [100, 200], 200, 400);
        assert_eq!(out, vec![[20, 40, 60, 80]]);
    }

    #[test]
    fn scale_edge_rejects_non_finite_and_out_of_range() {
        assert_eq!(scale_edge_to_i32(f64::NAN, false), None);
        assert_eq!(scale_edge_to_i32(f64::INFINITY, true), None);
        assert_eq!(scale_edge_to_i32(f64::from(i32::MAX) + 1.0, true), None);
        assert_eq!(scale_edge_to_i32(f64::from(i32::MIN) - 1.0, false), None);
        assert_eq!(scale_edge_to_i32(2.3, false), Some(2));
        assert_eq!(scale_edge_to_i32(2.3, true), Some(3));
    }
}
