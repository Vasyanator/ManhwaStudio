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
use std::collections::VecDeque;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use ms_thread as thread;

const STROKE_OVERLAY_UPLOAD_MIN_INTERVAL_S: f64 = 1.0 / 30.0;
const TEXT_MASK_TILE_SIDE: usize = 1024;
const TEXT_MASK_VISUAL_ALPHA_MAX: u8 = 96;
// --- Autoclean (продвинутый алгоритм клина текста по однородному фону) --------
// Портирован из ZITS-PlusPlus/dataset_generator_v2/naver_rs/src/autoclean.rs и
// адаптирован под единую бинарную маску текста + связные компоненты MS.
//
// Идея: для каждой области текста заполняются внутренности букв, маска слегка
// утолщается, отбраковываются однородные не-текстовые пятна (лицо/волосы), затем
// маска растёт только в сторону пикселей, отличных от фона (штрихи текста), пока
// весь периметр не станет однородным. Фон — устойчивая (медианная) краска кольца
// вокруг маски. Если маска не сошлась к однородному периметру — пробуем залить
// прямоугольник bbox с защитой `box_interior_fillable`.

/// Поканальная начальная дилатация (часто тонкой) маски текста, пиксели.
const AUTOCLEAN_INITIAL_DILATE: i32 = 2;
/// Запас заливки наружу, пиксели. Заливка расширяется на столько в заведомо
/// фоновую зону, чтобы при LINEAR-фильтрации оверлея на обычном масштабе
/// полупрозрачный край заливки приходился на фон, а не на кромку текста (иначе
/// из-под клина «просвечивает» тёмная кромка исходника). Альфа остаётся строго
/// бинарной (0/255) — дорисовываются только полностью непрозрачные пиксели фона,
/// поэтому в программе и в экспорте всё композитится одинаково.
const AUTOCLEAN_FILL_PADDING: i32 = 2;
/// Поканальный допуск «одинакового цвета». Намеренно маленький и фиксированный:
/// допуск, масштабируемый дисперсией, взорвался бы на разноцветном периметре
/// (волосы + кожа + одежда) и ошибочно счёл бы его однородным.
const AUTOCLEAN_SAME_TOL: i32 = 16;
/// Пока не более этой доли периметра отличается от фона, отличающиеся пиксели
/// считаются штрихами текста и поглощаются ростом. Выше — периметр это реальный
/// контент/градиент, область отвергается сразу.
const AUTOCLEAN_GROW_LIMIT: f32 = 0.30;
/// Мин. доля пикселей маски, которые должны быть «чернилами» (иначе это
/// однородная область, а не текст).
const AUTOCLEAN_MIN_INK_FRAC: f32 = 0.02;
/// Макс. доля пикселей маски, допустимая как «чернила» (выше — это сплошной
/// отличающийся объект, а не разреженный текст на фоне).
const AUTOCLEAN_MAX_INK_FRAC: f32 = 0.65;
/// Мин. доля «чернильных» пикселей на границе чернила/фон. Тонкие штрихи → высоко;
/// сплошная заливка (лицо/волосы) → низко.
const AUTOCLEAN_MIN_EDGE_RATIO: f32 = 0.16;
/// Защита box-fill: макс. доля внутренности прямоугольника, которая может
/// отличаться от фона. Текстовые боксы — в основном фон с редкими чернилами.
const AUTOCLEAN_BOX_INK_LIMIT: f32 = 0.45;
/// Объединять компоненты текста, чьи пиксели в пределах стольких пикселей.
const AUTOCLEAN_CLUSTER_SLACK: usize = 4;
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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum UnevenBackgroundTool {
    NoProcessing,
}

impl UnevenBackgroundTool {
    fn title(self) -> &'static str {
        match self {
            Self::NoProcessing => t!("cleaning.tab.no_processing_title"),
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
    // Sampling mode the tiles were uploaded with. When the active pixel
    // inspection mode flips, the page is rebuilt so the mask matches the
    // source/overlay sampling instead of staying fixed at one filter.
    texture_options: egui::TextureOptions,
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
                    ui.small(tf!("cleaning.tab.regions_progress_status", filled = self.quick_clean_progress.regions_filled, skipped = self.quick_clean_progress.regions_skipped, page_errors = self.quick_clean_progress.failed_pages, missing = self.quick_clean_progress.missing_masks));
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
                    self.quick_clean_status_text = Some(tf!("cleaning.tab.quick_clean_page_done_status", page = result.page_idx, regions = result.regions_total, filled = result.regions_filled, skipped = result.regions_skipped));
                }
                Ok(QuickTextCleanJobEvent::Finished) => {
                    self.quick_clean_job_in_progress = false;
                    self.quick_clean_job_rx = None;
                    self.quick_clean_status_text = Some(tf!("cleaning.tab.quick_clean_finished_status", done = self.quick_clean_progress.done_pages, total = self.quick_clean_progress.total_pages, filled = self.quick_clean_progress.regions_filled, skipped = self.quick_clean_progress.regions_skipped, errors = self.quick_clean_progress.failed_pages, missing = self.quick_clean_progress.missing_masks));
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

    let outcome = autoclean_page(
        &base_rgba,
        &binary_mask,
        width,
        height,
        spread_radius_px,
        uneven_tool,
    );
    let has_patch = outcome.patch.pixels.iter().any(|px| px.a() > 0);
    Ok(QuickTextCleanPageResult {
        page_idx,
        patch: has_patch.then_some(outcome.patch),
        regions_total: outcome.regions_total,
        regions_filled: outcome.regions_filled,
        regions_skipped: outcome.regions_skipped,
        error: None,
        missing_mask: false,
    })
}

/// Результат продвинутого автоклина одной страницы.
struct AutocleanPageOutcome {
    patch: egui::ColorImage,
    regions_total: usize,
    regions_filled: usize,
    regions_skipped: usize,
}

/// Продвинутый автоклин страницы по бинарной маске текста.
///
/// Связные компоненты маски, расширенной на `AUTOCLEAN_CLUSTER_SLACK`, образуют
/// области (кластеры близких штрихов/букв). Для каждой области:
///   1. **mask-fill** — локальная маска из исходных пикселей текста кластера,
///      заполнение внутренностей букв, лёгкая дилатация, отбраковка не-текстовых
///      пятен и рост к однородному периметру;
///   2. **box-fill** — если mask-fill не сошёлся, пробуем залить прямоугольник
///      bbox кластера с проверкой `box_interior_fillable`.
///
/// `spread_radius_px` — радиус «расползания»: на столько пикселей маска готова
/// расползтись вдоль выбивающегося из фона штриха (и на столько же отступить от
/// него), а также на столько наружу зондируется фон при классификации
/// штрих-vs-объект.
fn autoclean_page(
    base_rgba: &image::RgbaImage,
    binary_mask: &[u8],
    width: usize,
    height: usize,
    spread_radius_px: usize,
    uneven_tool: UnevenBackgroundTool,
) -> AutocleanPageOutcome {
    let mut patch =
        egui::ColorImage::filled([width.max(1), height.max(1)], egui::Color32::TRANSPARENT);
    let (mut regions_total, mut regions_filled, mut regions_skipped) = (0usize, 0usize, 0usize);
    if binary_mask.is_empty() || width == 0 || height == 0 {
        return AutocleanPageOutcome {
            patch,
            regions_total,
            regions_filled,
            regions_skipped,
        };
    }

    // Единый радиус расползания управляет ростом, отступлением и зондированием.
    // Зонд (`stroke_probe`) равен бюджету роста: штрих длиной ≤ радиуса находит
    // фон в пределах зонда → классифицируется как штрих → маска расползается по
    // нему; то, что тянется дальше радиуса → объект → маска отступает от пика.
    // Так длинные хвосты букв покрываются, а не ошибочно стираются эрозией.
    let radius = spread_radius_px.min(128) as i32;
    let max_expand = radius;
    let retreat_max = radius;
    let stroke_probe = radius;
    // Запас crop: рост наружу до `radius` + поле для кольца фона.
    let pad = radius + 6;

    // Кластеризация: компоненты исходной маски, раздутой на CLUSTER_SLACK, дают
    // области близкого текста без явного union-find.
    let cluster_mask = if AUTOCLEAN_CLUSTER_SLACK > 0 {
        dilate_binary_mask(binary_mask, width, height, AUTOCLEAN_CLUSTER_SLACK)
    } else {
        binary_mask.to_vec()
    };
    let clusters = extract_connected_components(&cluster_mask, width, height);

    let (sw, sh) = (width as i32, height as i32);
    for (label, cluster_pixels) in clusters.pixels.iter().enumerate() {
        if cluster_pixels.is_empty() {
            continue;
        }
        regions_total = regions_total.saturating_add(1);
        let cluster_label = label as i32;

        // bbox кластера (x2/y2 — эксклюзивны).
        let (mut x1, mut y1, mut x2, mut y2) = (sw, sh, 0i32, 0i32);
        for &idx in cluster_pixels {
            let x = (idx % width) as i32;
            let y = (idx / width) as i32;
            x1 = x1.min(x);
            y1 = y1.min(y);
            x2 = x2.max(x + 1);
            y2 = y2.max(y + 1);
        }

        // Crop с запасом, чтобы внешнее кольцо и рост оставались в реальном фоне.
        let ox = (x1 - pad).max(0);
        let oy = (y1 - pad).max(0);
        let ex = (x2 + pad).min(sw);
        let ey = (y2 + pad).min(sh);
        let (cw, ch) = ((ex - ox) as u32, (ey - oy) as u32);
        if cw == 0 || ch == 0 {
            regions_skipped = regions_skipped.saturating_add(1);
            continue;
        }
        let rgb = crop_rgb_from_rgba(base_rgba, ox, oy, cw, ch);

        // Локальная маска: исходные пиксели текста, принадлежащие этому кластеру.
        let mut mask = image::GrayImage::new(cw, ch);
        for &idx in cluster_pixels {
            if binary_mask.get(idx).copied().unwrap_or(0) == 0 {
                continue;
            }
            if clusters.labels.get(idx).copied().unwrap_or(-1) != cluster_label {
                continue;
            }
            let lx = (idx % width) as i32 - ox;
            let ly = (idx / width) as i32 - oy;
            if lx >= 0 && ly >= 0 && lx < cw as i32 && ly < ch as i32 {
                mask.put_pixel(lx as u32, ly as u32, image::Luma([255]));
            }
        }

        // --- попытка 1: заливка по маске ---------------------------------
        // Клон: оригинальные штрихи текста нужны box-fallback'у как seed
        // интерьера пузыря (см. clip_fill_to_bubble_interior).
        if let Some((final_mask, bg, retreated)) =
            autoclean_try_mask_fill(&rgb, mask.clone(), max_expand, retreat_max, stroke_probe)
        {
            if retreated {
                // Маска отступила с чужого объекта — bbox заливать нельзя (затёрли
                // бы объект), красим только по штрихам маски.
                paint_patch_from_mask(&mut patch, width, height, ox, oy, &final_mask, bg);
            } else {
                paint_autoclean_fill(&mut patch, (width, height), ox, oy, &final_mask, bg);
            }
            regions_filled = regions_filled.saturating_add(1);
            continue;
        }

        // --- попытка 2: заливка прямоугольника bbox ----------------------
        let mut bmask = image::GrayImage::new(cw, ch);
        let bx1 = (x1 - ox).clamp(0, cw as i32);
        let by1 = (y1 - oy).clamp(0, ch as i32);
        let bx2 = (x2 - ox).clamp(0, cw as i32);
        let by2 = (y2 - oy).clamp(0, ch as i32);
        for y in by1..by2 {
            for x in bx1..bx2 {
                bmask.put_pixel(x as u32, y as u32, image::Luma([255]));
            }
        }
        let mut box_filled = false;
        if has_foreground(&bmask)
            && let Some((bg, _retreated)) =
                grow_until_homogeneous(&rgb, &mut bmask, max_expand, retreat_max, stroke_probe)
            && box_interior_fillable(&rgb, bx1, by1, bx2, by2, bg)
        {
            // Запас наружу против «просвечивания» исходника на LINEAR-крае
            // (см. AUTOCLEAN_FILL_PADDING).
            dilate_gray_inplace(&mut bmask, AUTOCLEAN_FILL_PADDING);
            // Прямоугольный box по углам заходит за контур баббла; рост и padding
            // могут расползтись через тонкий контур наружу. Обрезаем заливку по
            // интерьеру пузыря, чтобы не затереть его контур и фон за ним.
            clip_fill_to_bubble_interior(&mut bmask, &rgb, &mask, bg);
            paint_patch_from_mask(&mut patch, width, height, ox, oy, &bmask, bg);
            regions_filled = regions_filled.saturating_add(1);
            box_filled = true;
        }

        if !box_filled {
            match uneven_tool {
                UnevenBackgroundTool::NoProcessing => {
                    regions_skipped = regions_skipped.saturating_add(1);
                }
            }
        }
    }

    AutocleanPageOutcome {
        patch,
        regions_total,
        regions_filled,
        regions_skipped,
    }
}

/// Crop из RGBA-страницы в локальный RGB-буфер (за границами — чёрный).
fn crop_rgb_from_rgba(
    base: &image::RgbaImage,
    ox: i32,
    oy: i32,
    cw: u32,
    ch: u32,
) -> image::RgbImage {
    let (bw, bh) = (base.width() as i32, base.height() as i32);
    let mut out = image::RgbImage::new(cw, ch);
    for y in 0..ch as i32 {
        for x in 0..cw as i32 {
            let (gx, gy) = (ox + x, oy + y);
            if gx >= 0 && gy >= 0 && gx < bw && gy < bh {
                let p = base.get_pixel(gx as u32, gy as u32);
                out.put_pixel(x as u32, y as u32, image::Rgb([p[0], p[1], p[2]]));
            }
        }
    }
    out
}

/// Одна попытка заливки по маске: заполнить внутренности букв, утолстить,
/// отбраковать не-текстовые пятна, затем дорастить до однородного периметра.
/// Возвращает финальную (выросшую) маску, цвет фона и флаг `retreated` (маска
/// где-то отступила с чужого объекта → bbox заливать нельзя) на успехе.
fn autoclean_try_mask_fill(
    rgb: &image::RgbImage,
    mut mask: image::GrayImage,
    max_expand: i32,
    retreat_max: i32,
    stroke_probe: i32,
) -> Option<(image::GrayImage, image::Rgb<u8>, bool)> {
    if !has_foreground(&mask) {
        return None;
    }
    fill_holes(&mut mask);
    dilate_gray_inplace(&mut mask, AUTOCLEAN_INITIAL_DILATE);
    // Отбраковать маски без структуры текста (ложное срабатывание на однородном
    // пятне лица/волос), сверяясь с начальным фоном периметра.
    let ring0 = outer_ring(&mask);
    if ring0.is_empty() {
        return None;
    }
    let bg0 = ring_background(rgb, &ring0);
    if !has_text_structure(rgb, &mask, bg0) {
        return None;
    }
    let (bg, retreated) =
        grow_until_homogeneous(rgb, &mut mask, max_expand, retreat_max, stroke_probe)?;
    Some((mask, bg, retreated))
}

/// Закрасить пиксели маски в patch сплошным цветом фона (по глоб. координатам).
fn paint_patch_from_mask(
    patch: &mut egui::ColorImage,
    pw: usize,
    ph: usize,
    ox: i32,
    oy: i32,
    mask: &image::GrayImage,
    bg: image::Rgb<u8>,
) {
    let color = egui::Color32::from_rgb(bg[0], bg[1], bg[2]);
    for y in 0..mask.height() as i32 {
        for x in 0..mask.width() as i32 {
            if mask.get_pixel(x as u32, y as u32)[0] == 0 {
                continue;
            }
            let (gx, gy) = (ox + x, oy + y);
            if gx >= 0 && gy >= 0 && (gx as usize) < pw && (gy as usize) < ph {
                let didx = gy as usize * pw + gx as usize;
                if let Some(px) = patch.pixels.get_mut(didx) {
                    *px = color;
                }
            }
        }
    }
}

/// Залить успешно проверенную область текста в patch.
///
/// `grow_until_homogeneous` уже доказал, что **весь периметр выросшей маски —
/// единый однородный цвет фона** (возврат `Some` только при нулевом числе
/// отличий). Значит вся область внутри периметра окружена однородным фоном и её
/// можно целиком залить bbox'ом маски: тонкие штрихи не покрывают зазоры между
/// буквами и сглаживающий ореол по краям, и заливка только по штрихам оставляла
/// бы призрачные артефакты текста. Доп. проверка `box_interior_fillable` здесь
/// не нужна (и вредна — она отбраковывала плотные строки текста, чей bbox почти
/// весь «чернила»): структура текста и однородность периметра уже подтверждены.
///
/// bbox дополнительно расширяется на `AUTOCLEAN_FILL_PADDING` в фоновую зону —
/// см. константу: это убирает «просвечивание» исходника на крае при LINEAR.
fn paint_autoclean_fill(
    patch: &mut egui::ColorImage,
    (pw, ph): (usize, usize),
    ox: i32,
    oy: i32,
    mask: &image::GrayImage,
    bg: image::Rgb<u8>,
) {
    let Some((bx1, by1, bx2, by2)) = gray_mask_bbox(mask) else {
        return;
    };
    let (mw, mh) = (mask.width() as i32, mask.height() as i32);
    let p = AUTOCLEAN_FILL_PADDING;
    let (bx1, by1, bx2, by2) = (
        (bx1 - p).max(0),
        (by1 - p).max(0),
        (bx2 + p).min(mw),
        (by2 + p).min(mh),
    );
    let color = egui::Color32::from_rgb(bg[0], bg[1], bg[2]);
    for y in by1..by2 {
        for x in bx1..bx2 {
            let (gx, gy) = (ox + x, oy + y);
            if gx >= 0 && gy >= 0 && (gx as usize) < pw && (gy as usize) < ph {
                let didx = gy as usize * pw + gx as usize;
                if let Some(px) = patch.pixels.get_mut(didx) {
                    *px = color;
                }
            }
        }
    }
}

/// Bounding box (локальные координаты crop, x2/y2 эксклюзивны) пикселей маски.
fn gray_mask_bbox(mask: &image::GrayImage) -> Option<(i32, i32, i32, i32)> {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    let (mut x1, mut y1, mut x2, mut y2) = (w, h, 0i32, 0i32);
    let mut found = false;
    for y in 0..h {
        for x in 0..w {
            if mask.get_pixel(x as u32, y as u32)[0] != 0 {
                found = true;
                x1 = x1.min(x);
                y1 = y1.min(y);
                x2 = x2.max(x + 1);
                y2 = y2.max(y + 1);
            }
        }
    }
    found.then_some((x1, y1, x2, y2))
}

fn has_foreground(mask: &image::GrayImage) -> bool {
    mask.as_raw().iter().any(|&v| v != 0)
}

/// Заполнить фоновые пиксели, полностью окружённые маской (внутренности букв):
/// заливка фона от границы crop, всё недостигнутое — дырка.
fn fill_holes(mask: &mut image::GrayImage) {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    let mut outside = vec![false; (w * h) as usize];
    let mut stack: Vec<(i32, i32)> = Vec::new();
    let push = |x: i32, y: i32, outside: &mut Vec<bool>, stack: &mut Vec<(i32, i32)>| {
        let idx = (y * w + x) as usize;
        if !outside[idx] && mask.get_pixel(x as u32, y as u32)[0] == 0 {
            outside[idx] = true;
            stack.push((x, y));
        }
    };
    for x in 0..w {
        push(x, 0, &mut outside, &mut stack);
        push(x, h - 1, &mut outside, &mut stack);
    }
    for y in 0..h {
        push(0, y, &mut outside, &mut stack);
        push(w - 1, y, &mut outside, &mut stack);
    }
    while let Some((x, y)) = stack.pop() {
        for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
            let (nx, ny) = (x + dx, y + dy);
            if nx >= 0 && ny >= 0 && nx < w && ny < h {
                push(nx, ny, &mut outside, &mut stack);
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            let idx = (y * w + x) as usize;
            if !outside[idx] && mask.get_pixel(x as u32, y as u32)[0] == 0 {
                mask.put_pixel(x as u32, y as u32, image::Luma([255]));
            }
        }
    }
}

/// 8-связная дилатация на `r` пикс. (r итераций роста на 1 пиксель).
fn dilate_gray_inplace(mask: &mut image::GrayImage, r: i32) {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    for _ in 0..r {
        let src = mask.clone();
        for y in 0..h {
            for x in 0..w {
                if src.get_pixel(x as u32, y as u32)[0] != 0 {
                    continue;
                }
                let mut hit = false;
                'n: for dy in -1..=1 {
                    for dx in -1..=1 {
                        let (nx, ny) = (x + dx, y + dy);
                        if nx >= 0
                            && ny >= 0
                            && nx < w
                            && ny < h
                            && src.get_pixel(nx as u32, ny as u32)[0] != 0
                        {
                            hit = true;
                            break 'n;
                        }
                    }
                }
                if hit {
                    mask.put_pixel(x as u32, y as u32, image::Luma([255]));
                }
            }
        }
    }
}

/// 1-px фоновое кольцо, 4-смежное с маской.
fn outer_ring(mask: &image::GrayImage) -> Vec<(u32, u32)> {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    let mut ring = Vec::new();
    for y in 0..h {
        for x in 0..w {
            if mask.get_pixel(x as u32, y as u32)[0] != 0 {
                continue;
            }
            for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                let (nx, ny) = (x + dx, y + dy);
                if nx >= 0
                    && ny >= 0
                    && nx < w
                    && ny < h
                    && mask.get_pixel(nx as u32, ny as u32)[0] != 0
                {
                    ring.push((x as u32, y as u32));
                    break;
                }
            }
        }
    }
    ring
}

fn chan_dist(a: image::Rgb<u8>, b: image::Rgb<u8>) -> i32 {
    let mut d = 0;
    for k in 0..3 {
        d = d.max((a[k] as i32 - b[k] as i32).abs());
    }
    d
}

fn median_u8(values: &mut [u8]) -> u8 {
    values.sort_unstable();
    values[values.len() / 2]
}

/// Цвет фона = поканальная медиана кольца (устойчива к пикселям штрихов текста,
/// затёкшим в кольцо).
fn ring_background(rgb: &image::RgbImage, ring: &[(u32, u32)]) -> image::Rgb<u8> {
    let mut ch: [Vec<u8>; 3] = std::array::from_fn(|_| Vec::with_capacity(ring.len()));
    for &(x, y) in ring {
        let p = rgb.get_pixel(x, y);
        for k in 0..3 {
            ch[k].push(p[k]);
        }
    }
    image::Rgb([
        median_u8(&mut ch[0]),
        median_u8(&mut ch[1]),
        median_u8(&mut ch[2]),
    ])
}

/// Свести периметр к единому однородному цвету фона. Возвращает цвет фона и флаг
/// `retreated` (было ли хоть одно отступление-эрозия). Каждый отличающийся
/// пиксель периметра классифицируется зондированием наружу: фон в пределах
/// `stroke_probe` ⇒ штрих → маска **расползается** на него (≤`max_expand`);
/// иначе разница тянется дальше (чужой объект) ⇒ маска **отступает** от пика
/// (≤`retreat_max`). Несколько пиков обрабатываются за один проход: каждая
/// итерация классифицирует и двигает весь отличающийся фронт сразу.
/// `None`, если маска заполнила crop, если >`AUTOCLEAN_GROW_LIMIT` периметра
/// отличается (контент, не текст), или бюджеты исчерпаны, а периметр всё грязный.
fn grow_until_homogeneous(
    rgb: &image::RgbImage,
    mask: &mut image::GrayImage,
    max_expand: i32,
    retreat_max: i32,
    stroke_probe: i32,
) -> Option<(image::Rgb<u8>, bool)> {
    let (mut grow_used, mut retreat_used) = (0i32, 0i32);
    let mut retreated = false;
    loop {
        let ring = outer_ring(mask);
        if ring.is_empty() {
            return None; // маска заполнила crop — фон не проверить.
        }
        let bg = ring_background(rgb, &ring);
        let mut grow_set = Vec::new();
        let mut retreat_set = Vec::new();
        for &(x, y) in &ring {
            if chan_dist(*rgb.get_pixel(x, y), bg) <= AUTOCLEAN_SAME_TOL {
                continue;
            }
            if probe_outward_bg(rgb, mask, x, y, bg, stroke_probe) {
                grow_set.push((x, y));
            } else {
                retreat_set.push((x, y));
            }
        }
        let diff_count = grow_set.len() + retreat_set.len();
        if diff_count == 0 {
            return Some((bg, retreated)); // весь периметр однороден.
        }
        if diff_count as f32 > AUTOCLEAN_GROW_LIMIT * ring.len() as f32 {
            return None; // периметр в основном не-фон → контент, не текст.
        }

        let mut acted = false;
        if grow_used < max_expand && !grow_set.is_empty() {
            for (x, y) in grow_set {
                mask.put_pixel(x, y, image::Luma([255]));
            }
            grow_used += 1;
            acted = true;
        }
        if retreat_used < retreat_max && !retreat_set.is_empty() {
            for (x, y) in retreat_set {
                erode_around(mask, x, y);
            }
            retreat_used += 1;
            retreated = true;
            acted = true;
        }
        if !acted {
            return None; // бюджеты исчерпаны, периметр всё ещё грязный.
        }
    }
}

/// Зондировать наружу от отличающегося пикселя периметра (прочь от маски): true,
/// если фон возвращается в пределах `stroke_probe` пикс. (ограниченный штрих),
/// false — если разница продолжается или уходит за crop (объект).
fn probe_outward_bg(
    rgb: &image::RgbImage,
    mask: &image::GrayImage,
    x: u32,
    y: u32,
    bg: image::Rgb<u8>,
    stroke_probe: i32,
) -> bool {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    let (xi, yi) = (x as i32, y as i32);
    // Направление наружу = прочь от 4-соседнего пикселя маски.
    let mut dir = None;
    for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
        let (mx, my) = (xi + dx, yi + dy);
        if mx >= 0 && my >= 0 && mx < w && my < h && mask.get_pixel(mx as u32, my as u32)[0] != 0 {
            dir = Some((-dx, -dy));
            break;
        }
    }
    let Some((ox, oy)) = dir else { return true };
    for k in 1..=stroke_probe {
        let (px, py) = (xi + ox * k, yi + oy * k);
        if px < 0 || py < 0 || px >= w || py >= h {
            return false; // ушли за crop, всё ещё отличаясь → объект.
        }
        if chan_dist(*rgb.get_pixel(px as u32, py as u32), bg) <= AUTOCLEAN_SAME_TOL {
            return true; // фон в пределах зонда → ограниченный штрих.
        }
    }
    false
}

/// Отступить границу маски у отличающегося пикселя периметра, стерев его
/// 4-соседние пиксели маски (стянуть маску на 1 пиксель с чужого объекта).
fn erode_around(mask: &mut image::GrayImage, x: u32, y: u32) {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
        let (nx, ny) = (x as i32 + dx, y as i32 + dy);
        if nx >= 0 && ny >= 0 && nx < w && ny < h && mask.get_pixel(nx as u32, ny as u32)[0] != 0 {
            mask.put_pixel(nx as u32, ny as u32, image::Luma([0]));
        }
    }
}

/// true, если пиксели под маской похожи на текст (чернила-на-фоне с тонкими
/// штрихами), а не на однородное пятно лица/волос. «Чернила» = пиксели,
/// отличающиеся от фона `bg` больше `AUTOCLEAN_SAME_TOL`.
fn has_text_structure(rgb: &image::RgbImage, mask: &image::GrayImage, bg: image::Rgb<u8>) -> bool {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    let is_ink = |x: i32, y: i32| -> bool {
        x >= 0
            && y >= 0
            && x < w
            && y < h
            && mask.get_pixel(x as u32, y as u32)[0] != 0
            && chan_dist(*rgb.get_pixel(x as u32, y as u32), bg) > AUTOCLEAN_SAME_TOL
    };
    let (mut area, mut ink, mut edge) = (0u64, 0u64, 0u64);
    for y in 0..h {
        for x in 0..w {
            if mask.get_pixel(x as u32, y as u32)[0] == 0 {
                continue;
            }
            area += 1;
            if !is_ink(x, y) {
                continue;
            }
            ink += 1;
            let on_boundary = [(-1, 0), (1, 0), (0, -1), (0, 1)]
                .iter()
                .any(|&(dx, dy)| !is_ink(x + dx, y + dy));
            if on_boundary {
                edge += 1;
            }
        }
    }
    if area == 0 || ink == 0 {
        return false;
    }
    let ink_frac = ink as f32 / area as f32;
    let edge_ratio = edge as f32 / ink as f32;
    (AUTOCLEAN_MIN_INK_FRAC..=AUTOCLEAN_MAX_INK_FRAC).contains(&ink_frac)
        && edge_ratio >= AUTOCLEAN_MIN_EDGE_RATIO
}

/// true, если прямоугольник в основном цвета фона (редкие чернила текста), так
/// что заливка лишь стирает текст. false для панелей, заполненных контентом.
fn box_interior_fillable(
    rgb: &image::RgbImage,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    bg: image::Rgb<u8>,
) -> bool {
    let (mut total, mut ink) = (0u64, 0u64);
    for y in y1.max(0)..y2.min(rgb.height() as i32) {
        for x in x1.max(0)..x2.min(rgb.width() as i32) {
            total += 1;
            if chan_dist(*rgb.get_pixel(x as u32, y as u32), bg) > AUTOCLEAN_SAME_TOL {
                ink += 1;
            }
        }
    }
    total > 0 && (ink as f32) <= AUTOCLEAN_BOX_INK_LIMIT * (total as f32)
}

/// Обрезать маску box-заливки по интерьеру пузыря, в котором лежит текст.
///
/// Box-fallback заливает bbox кластера сплошным фоном, но прямоугольник по углам
/// заходит за контур баббла (тонкую тёмную кривую) в наружный фон. Рост до
/// однородного периметра и `AUTOCLEAN_FILL_PADDING` классифицируют контур как
/// тонкий штрих и расползаются через него — сплошная заливка затирает контур по
/// углам и фон снаружи. Здесь интерьер пузыря вычисляется заливкой:
///   1. фон, связный со штрихами текста и не пересекающий контур (`interior_bg`);
///   2. «снаружи» — заливка от рамки crop по не-интерьерным пикселям; контур
///      достижим от рамки, поэтому попадает в «снаружи».
///
/// Всё, что оказалось «снаружи» (контур, наружный фон, чужой контент за углами),
/// из заливки убирается; интерьерный фон и замкнутые им буквы остаются.
///
/// Без выраженного контура (текст на открытом фоне) `interior_bg` разливается до
/// рамки, «снаружи» пусто и маска не меняется — заливка прежняя.
fn clip_fill_to_bubble_interior(
    fill: &mut image::GrayImage,
    rgb: &image::RgbImage,
    text: &image::GrayImage,
    bg: image::Rgb<u8>,
) {
    let (w, h) = (rgb.width() as i32, rgb.height() as i32);
    if w == 0 || h == 0 {
        return;
    }
    let at = |x: i32, y: i32| (y * w + x) as usize;
    let near_bg =
        |x: i32, y: i32| chan_dist(*rgb.get_pixel(x as u32, y as u32), bg) <= AUTOCLEAN_SAME_TOL;

    // 1. Интерьерный фон: заливка near-bg пикселей от seed'ов — near-bg соседей
    //    штрихов текста (заведомо внутри пузыря). Контур (тёмный) — стена.
    let mut interior_bg = vec![false; (w * h) as usize];
    let mut stack: Vec<(i32, i32)> = Vec::new();
    let try_push = |x: i32, y: i32, vis: &mut Vec<bool>, st: &mut Vec<(i32, i32)>| {
        if x >= 0 && y >= 0 && x < w && y < h && !vis[at(x, y)] && near_bg(x, y) {
            vis[at(x, y)] = true;
            st.push((x, y));
        }
    };
    for y in 0..h {
        for x in 0..w {
            if text.get_pixel(x as u32, y as u32)[0] == 0 {
                continue;
            }
            for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                try_push(x + dx, y + dy, &mut interior_bg, &mut stack);
            }
        }
    }
    while let Some((x, y)) = stack.pop() {
        for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
            try_push(x + dx, y + dy, &mut interior_bg, &mut stack);
        }
    }
    // Seed'ов не нашлось (текст вплотную окружён не-фоном) — клипать нечем,
    // оставляем box как есть, чтобы не сорвать заливку.
    if !interior_bg.iter().any(|&v| v) {
        return;
    }

    // 2. «Снаружи» = заливка от рамки crop по не-интерьерным пикселям. Контур
    //    пузыря достижим от рамки → «снаружи»; буквы, замкнутые интерьерным
    //    фоном, недостижимы → остаются.
    let mut outside = vec![false; (w * h) as usize];
    let mut q: Vec<(i32, i32)> = Vec::new();
    let seed_out = |x: i32, y: i32, out: &mut Vec<bool>, q: &mut Vec<(i32, i32)>| {
        if x >= 0 && y >= 0 && x < w && y < h && !out[at(x, y)] && !interior_bg[at(x, y)] {
            out[at(x, y)] = true;
            q.push((x, y));
        }
    };
    for x in 0..w {
        seed_out(x, 0, &mut outside, &mut q);
        seed_out(x, h - 1, &mut outside, &mut q);
    }
    for y in 0..h {
        seed_out(0, y, &mut outside, &mut q);
        seed_out(w - 1, y, &mut outside, &mut q);
    }
    while let Some((x, y)) = q.pop() {
        for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
            seed_out(x + dx, y + dy, &mut outside, &mut q);
        }
    }

    // 3. Гасим пиксели заливки, попавшие «снаружи» интерьера пузыря.
    for y in 0..h {
        for x in 0..w {
            if outside[at(x, y)] {
                fill.put_pixel(x as u32, y as u32, image::Luma([0]));
            }
        }
    }
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
