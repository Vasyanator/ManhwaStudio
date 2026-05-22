/*
FILE HEADER (tabs/typing/tab.rs)
- Назначение: состояние вкладки `Текст` на основе `CanvasView` с read-only оверлеями и
  интерактивной деформацией поверх общей high-res surface + созданием новых текстовых оверлеев
  + бинарной маской обрезки страниц.
- Ключевые поля `TypingTabState`:
  - `canvas`: отдельный инстанс холста для вкладки типинга (`editable = false`).
  - `text_overlays`: слой PNG-оверлеев (`text` + `image`) с загрузкой из `text_images/text_info.json`,
    декодирование в фоне, дозированная загрузка текстур в GUI-потоке, выбор, drag,
    загрузка/редактирование сохраняемой `deform_mesh` как общей high-res surface
    and LRU snapshots/eviction for reconstructable display textures while keeping `source_rgba`;
    (legacy `transform_uv`/низкое разрешение читается с конвертацией и ресемплингом),
    контекстное меню ПКМ, удаление (`ПКМ/Del`),
    ручка вращения выделенного оверлея (вне transform-mode), поворот `Ctrl+колесо`
    на `2°` за шаг при выделенном оверлее (иначе событие остаётся у canvas-zoom),
    сдвиг выделенного оверлея стрелками (`1px`, `Shift+стрелки` = `5px`, кроме фокуса
    в текстовом поле панели),
    `Shift+колесо` меняет размер шрифта: в режиме без выделения — на панели `Создание текста`,
    при выделенном `text`-оверлее — в edit-параметрах с live-рендером (в обоих случаях
    с consume wheel-события до `CanvasView`, чтобы не скроллить холст; при наведении на
    `WheelSlider` событие остаётся у слайдера),
    hotkey `C` для выделенного `text`-оверлея запускает фоновый авто-тайп:
    берётся оптический центр оверлея, от него ищется пузырь на composited-странице
    (`src + clean overlay` из shared cache), после чего оверлей центрируется по пузырю;
    при выделении оверлея верхняя панель auto-переключается в режим редактирования,
    изменения текста/параметров рендерятся в тот же PNG в фоне по схеме latest-wins:
    новый запрос сразу вытесняет предыдущий и устаревший результат не применяется,
    а `text_info.json` сохраняется отложенно после снятия выделения;
    масштаб выделенного оверлея через `-` / `=` / `0` (уменьшить/увеличить/сброс), Shift-выделение
    под создание нового текстового оверлея, inline-редактор и фоновый финальный рендер+сохранение;
    новый оверлей после рендера создаётся с `scale = 1.0` (без fit-подгонки под ширину выделения);
    режимы `Perspective`/`Изгиб`/`Рамка`/кистевые warp-инструменты (`Выпуклость`, `Впуклость`,
    `Сдвиг`, `Закрутка`, `Восстановление`, `Разгладить`, `Растянуть`, `Складка`)
    являются только инструментами редактирования общей surface и
    не хранят собственные отдельные параметры влияния; после изменения положения/деформации
    placement сохраняется в `text_info.json`
    через отдельный worker-поток (без блокировки GUI);
    у записей оверлея хранятся placement-поля + `render_data` + флаг `mask_clip_enabled`,
    в `render_data.text_params` сохраняются расширенные поля раскладки
    (`text_layout_mode`, `formula_layout`, `shape_layout`, `drawn_lines_layout`,
    `vector_lines_layout`),
    для legacy `style/static`
    выполняется fallback-конвертация и нормализация файла в новый формат).
  - `top_panel`: состояние верхней фиксированной панели вкладки `Текст`
    (layout вынесен в `panel.rs`, режимы create/edit + сворачивание + кнопка маски).
  - `mask_layer`: слой бинарной маски (`mask_page_{idx}.png`) с фоновыми
    загрузкой/сохранением, кистью рисования/стирания и клипом текстовых PNG.
  - Экспорт в папку: фоновое наложение `src + clean overlay + text overlays`
    с учётом перспективной трансформации и маски обрезки; clean overlay берётся из
    shared `CleanOverlaysModel` (с CPU RGBA-кэшем несохранённых правок), а при
    отсутствии в памяти предварительно догружается из `clean_layers` в модель.
  - Clean overlay visibility in this tab is canvas-local UI state: toggling it must not
    mutate `CleanOverlaysModel` or affect the Cleaning tab.
- Ключевые методы:
  - `set_bubbles_model`: подключение shared-модели пузырей.
  - `set_overlays_model`: подключение shared-модели clean-overlay.
  - `viewport_snapshot/apply_viewport_snapshot`: bridge для общего viewport sync в `MangaApp`.
  - `draw`: кадр вкладки (poll загрузчика, upload текстур по бюджету, рендер `CanvasView`).
  - `draw_canvas_mask_overlay_on_page` / `draw_canvas_overlay_on_page` (в `TypingHooks`):
    yellow mask-preview/input живёт в canvas mask-layer, а текстовые/image оверлеи и
    debug авто-тайпа остаются в additional-elements layer.
  - `draw_canvas_overlay_top_left` (в `TypingHooks`): рендер верхней панели в `panel.rs` +
    обработка Shift-выделения/редактора текста.
*/
use super::auto_typing::{
    TypingAutoTypingDetectionResult, TypingAutoTypingSettings, compute_overlay_visual_center,
    detect_bubble_from_overlay_cache,
};
use super::mask::{TypingMaskExportPage, TypingMaskLayer};
use super::panel::{
    TypingCreateImageRequest, TypingEditorFontSpec, TypingExportUiStatus, TypingOverlayEditRequest,
    TypingOverlayKind, TypingPanelLayout, TypingSelectedOverlayForEdit,
};
use super::render_next::render_text_to_image;
use super::render_next::types::{
    HorizontalAlign, KerningMode, TEXT_FORMULA_USER_VAR_COUNT, TextDrawnLinesLayoutParams,
    TextFormulaLayoutParams, TextLayoutMode, TextLineMode, TextRenderParams,
    TextRenderShapeCompareParams, TextShape, TextVectorLine, TextVectorLineDistanceMode,
    TextVectorLineTextDirection, TextVectorLinesLayoutParams, TextVectorPoint, TextWrapMode,
    VerticalLineDirection,
};
use crate::app::{PageImageInfo, PageTexture};
use crate::canvas::{
    CanvasDrawParams, CanvasHooks, CanvasUiStatus, CanvasView, CanvasViewportSnapshot, RectCoords,
    SourceTextureUploadBudget,
};
use crate::memory_manager::{
    CacheEvictionReport, CacheEvictionRequest, CacheReloadCost, CacheResourceInfo,
    CacheResourceKind, select_eviction_candidates,
};
use crate::models::bubbles_model::BubblesModel;
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::paste_image;
use crate::project::{Bubble, ProjectData};
use crate::tabs::typing::TypingTopPanelState;
use crate::widgets::WheelSlider;
use eframe::egui;
use egui::{Color32, ColorImage, Id, Mesh, Pos2, Rect, Sense, Stroke, TextureOptions, Vec2};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const TEXT_INFO_FILE_NAME: &str = "text_info.json";
const CANVAS_LEFT_TOP_CONTROLS_AREA_ID: &str = "canvas_left_top_controls";
const TEXT_OVERLAY_UPLOAD_TEXTURE_BUDGET_PER_FRAME: usize = 4;
const TEXT_OVERLAY_UPLOAD_BYTES_BUDGET_PER_FRAME: usize = 8 * 1024 * 1024;
const TEXT_OVERLAY_TRANSFORM_HANDLE_RADIUS_PX: f32 = 7.0;
const TEXT_OVERLAY_BEND_HANDLE_RADIUS_PX: f32 = 6.0;
const TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX: f32 = 7.0;
const TEXT_OVERLAY_ROTATE_HANDLE_OFFSET_PX: f32 = 24.0;
const TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX: f32 = 60.0;
const TEXT_OVERLAY_MAX_OUT_OF_BOUNDS_UV: f32 = 0.90;
const TEXT_OVERLAY_MIN_VISIBLE_FRACTION: f32 = 0.10;
const TEXT_CREATE_SELECTION_MIN_SIDE_PX: f32 = 4.0;
const TEXT_EDITOR_MIN_WIDTH_PX: f32 = 120.0;
const TEXT_EDITOR_MIN_HEIGHT_PX: f32 = 72.0;
const TEXT_EDITOR_STATUS_ERROR_SECONDS: f64 = 4.0;
const TEXT_RENDER_DATA_FALLBACK_WIDTH_PX: u32 = 500;
const TEXT_LAYOUT_IMAGE_SUFFIX: &str = "_layout";
const TEXT_SHAPE_VARIANT_GRID_SIDE: usize = 3;
const TEXT_SHAPE_VARIANT_TILE_MAX_WIDTH_PX: f32 = 150.0;
const TEXT_SHAPE_VARIANT_TILE_MAX_HEIGHT_PX: f32 = 120.0;
const TEXT_SHAPE_VARIANT_TILE_GAP_PX: f32 = 8.0;
const TEXT_SHAPE_VARIANT_PANEL_PADDING_PX: f32 = 10.0;
const TEXT_SHAPE_VARIANT_PANEL_MENU_GAP_PX: f32 = 4.0;
const TEXT_SHAPE_VARIANT_CHECKER_SIDE_PX: f32 = 14.0;
const TEXT_LAYOUT_EDITOR_PANEL_WIDTH_PX: f32 = 360.0;
const TEXT_LAYOUT_EDITOR_PANEL_HEIGHT_PX: f32 = 520.0;
const TEXT_LAYOUT_EDITOR_MODE_PANEL_WIDTH_PX: f32 = 300.0;
const TEXT_LAYOUT_EDITOR_FRAME_HANDLE_RADIUS_PX: f32 = 6.0;
const TEXT_LAYOUT_EDITOR_FRAME_MIN_SIDE_PX: f32 = 24.0;
const TEXT_LAYOUT_EDITOR_POINT_RADIUS_PX: f32 = 6.0;
const TEXT_OVERLAY_DEFORM_SURFACE_COLS: usize = 13;
const TEXT_OVERLAY_DEFORM_SURFACE_ROWS: usize = 13;
const TEXT_OVERLAY_BEND_HANDLE_COLS: usize = 5;
const TEXT_OVERLAY_BEND_HANDLE_ROWS: usize = 5;
const TEXT_OVERLAY_FRAME_HANDLE_RADIUS_PX: f32 = 6.0;
const TEXT_OVERLAY_FRAME_HANDLE_SIDE_POINTS_DEFAULT: usize = 6;
const TEXT_OVERLAY_BULGE_PINCH_BRUSH_SCALE: f32 = 0.012;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TypingDeformMode {
    Perspective,
    Bend,
    Frame,
    Grid,
    Bulge,
    Pinch,
    Push,
    Twirl,
    Restore,
    Smooth,
    Stretch,
    Fold,
}

impl TypingDeformMode {
    fn label(self) -> &'static str {
        match self {
            Self::Perspective => "Перспектива",
            Self::Bend => "Изгиб",
            Self::Frame => "Рамка",
            Self::Grid => "Сетка",
            Self::Bulge => "Выпуклость",
            Self::Pinch => "Впуклость",
            Self::Push => "Сдвиг",
            Self::Twirl => "Закрутка",
            Self::Restore => "Восстановление",
            Self::Smooth => "Разгладить",
            Self::Stretch => "Растянуть",
            Self::Fold => "Складка",
        }
    }

    fn is_handle_mode(self) -> bool {
        matches!(
            self,
            Self::Perspective | Self::Bend | Self::Frame | Self::Grid
        )
    }

    fn is_brush_mode(self) -> bool {
        !self.is_handle_mode()
    }
}

#[derive(Debug, Clone)]
struct TypingDeformToolSettings {
    brush_radius_px: f32,
    brush_strength: f32,
}

impl Default for TypingDeformToolSettings {
    fn default() -> Self {
        Self {
            brush_radius_px: 84.0,
            brush_strength: 0.5,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct TypingOverlayDeformMesh {
    cols: usize,
    rows: usize,
    points_px: Vec<[f32; 2]>,
}

impl TypingOverlayDeformMesh {
    fn new(
        cols: usize,
        rows: usize,
        points_px: Vec<[f32; 2]>,
        page_size: [usize; 2],
    ) -> Option<Self> {
        if cols < 2 || rows < 2 || points_px.len() != cols.saturating_mul(rows) {
            return None;
        }
        Some(Self {
            cols,
            rows,
            points_px: points_px
                .into_iter()
                .map(|point| clamp_page_point(point, page_size))
                .collect(),
        })
    }

    fn point_idx(&self, col: usize, row: usize) -> usize {
        row * self.cols + col
    }

    fn point(&self, col: usize, row: usize) -> [f32; 2] {
        self.points_px[self.point_idx(col, row)]
    }

    fn translate(&mut self, dx_px: f32, dy_px: f32, page_size: [usize; 2]) {
        for point in &mut self.points_px {
            point[0] += dx_px;
            point[1] += dy_px;
        }
        for point in &mut self.points_px {
            *point = clamp_page_point(*point, page_size);
        }
    }
}

pub struct TypingTabState {
    canvas: CanvasView,
    text_overlays: TypingTextOverlayLayer,
    top_panel: TypingTopPanelState,
    mask_layer: TypingMaskLayer,
}

impl Default for TypingTabState {
    fn default() -> Self {
        super::render_next::touch_runtime_smoke_contract();
        let mut canvas = CanvasView::default();
        canvas.editable = false;
        Self {
            canvas,
            text_overlays: TypingTextOverlayLayer::default(),
            top_panel: TypingTopPanelState::default(),
            mask_layer: TypingMaskLayer::default(),
        }
    }
}

impl TypingTabState {
    pub fn set_bubbles_model(&mut self, model: Arc<Mutex<BubblesModel>>) {
        self.canvas.set_bubbles_model(model);
    }

    pub fn set_overlays_model(&mut self, model: Arc<Mutex<CleanOverlaysModel>>) {
        self.mask_layer.set_overlays_model(Arc::clone(&model));
        self.text_overlays
            .set_clean_overlays_model(Some(Arc::clone(&model)));
        self.canvas.set_overlays_model(model);
    }

    pub fn set_panel_layout(&mut self, layout: TypingPanelLayout) {
        self.top_panel.set_panel_layout(layout);
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

    pub fn gpu_memory_snapshot(&self, pinned_pages: &BTreeSet<usize>) -> Vec<CacheResourceInfo> {
        let mut snapshot = self.mask_layer.gpu_memory_snapshot(pinned_pages);
        snapshot.extend(self.text_overlays.gpu_memory_snapshot(pinned_pages));
        snapshot
    }

    pub fn evict_gpu_caches(&mut self, request: &CacheEvictionRequest) -> CacheEvictionReport {
        let mut report = self.mask_layer.evict_gpu_cache(request);
        let overlay_report = self.text_overlays.evict_gpu_cache(request);
        report.estimated_freed_bytes = report
            .estimated_freed_bytes
            .saturating_add(overlay_report.estimated_freed_bytes);
        report.resources.extend(overlay_report.resources);
        report
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

    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        project: &ProjectData,
        page_infos: &HashMap<usize, PageImageInfo>,
        texture_cache: &mut HashMap<usize, PageTexture>,
        status: CanvasUiStatus,
    ) {
        let canvas_rect = ui.max_rect();
        self.text_overlays.set_page_count(project.pages.len());
        self.text_overlays.ensure_loader_started(project);
        self.mask_layer.ensure_loader_started(project);
        let mut needs_repaint = false;
        needs_repaint |= self.text_overlays.poll_loader();
        needs_repaint |= self.text_overlays.poll_create_overlay_jobs(ctx);
        needs_repaint |= self.text_overlays.poll_edit_overlay_jobs(ctx);
        needs_repaint |= self.text_overlays.poll_save_jobs(ctx);
        needs_repaint |= self.text_overlays.poll_export_jobs(ctx);
        needs_repaint |= self.mask_layer.poll_loader(ctx);
        needs_repaint |= self.mask_layer.poll_save_jobs(ctx);
        needs_repaint |= self.mask_layer.poll_fill_jobs(ctx);
        for page_idx in self.mask_layer.take_changed_pages() {
            self.text_overlays.mark_page_texture_dirty(page_idx);
            needs_repaint = true;
        }
        needs_repaint |= self
            .text_overlays
            .upload_pending_textures(ctx, &self.mask_layer);
        let layout_editor_active = self.text_overlays.layout_editor_active();
        if !layout_editor_active {
            needs_repaint |=
                self.try_adjust_create_panel_font_size_by_shift_wheel(ctx, canvas_rect);
            needs_repaint |=
                self.try_adjust_selected_overlay_font_size_by_shift_wheel(ctx, canvas_rect);
        }
        if self.top_panel.is_mask_panel_open() {
            self.text_overlays.clear_selection();
        }

        let (canvas, text_overlays, top_panel, mask_layer) = (
            &mut self.canvas,
            &mut self.text_overlays,
            &mut self.top_panel,
            &mut self.mask_layer,
        );
        canvas.set_zoom_blocked(
            !mask_layer.is_panel_open()
                && (text_overlays.has_selected_overlay() || layout_editor_active),
        );
        let mut hooks = TypingHooks {
            text_overlays,
            top_panel,
            mask_layer,
            pending_create_text_from_bubble: None,
            page_overlay_occluders: HashMap::new(),
        };
        hooks.text_overlays.begin_canvas_frame();
        let mut source_upload_budget = SourceTextureUploadBudget::source_page_reupload_default();
        canvas.draw(CanvasDrawParams {
            ctx,
            ui,
            project,
            page_infos,
            texture_cache,
            status,
            source_upload_budget: &mut source_upload_budget,
            hooks: &mut hooks,
        });
        if Self::should_clear_overlay_selection_from_canvas_click(
            ctx,
            canvas_rect,
            hooks.top_panel,
            hooks.text_overlays,
        ) {
            hooks.text_overlays.clear_selection();
            needs_repaint = true;
        }

        if needs_repaint || self.text_overlays.wants_repaint() || self.mask_layer.is_panel_open() {
            ctx.request_repaint();
        }
    }

    fn should_clear_overlay_selection_from_canvas_click(
        ctx: &egui::Context,
        canvas_rect: Rect,
        top_panel: &TypingTopPanelState,
        text_overlays: &TypingTextOverlayLayer,
    ) -> bool {
        if !text_overlays.has_selected_overlay() {
            return false;
        }
        if top_panel.is_mask_panel_open() || top_panel.eyedropper_active() {
            return false;
        }
        if text_overlays.layout_editor_active() {
            return false;
        }
        if top_panel.eyedropper_consumed_primary_click_this_frame() {
            return false;
        }
        if text_overlays.primary_pointer_targets_overlay_this_frame() {
            return false;
        }

        let pointer_over_area = ctx.is_pointer_over_area();
        let popup_open = ctx.is_popup_open();
        ctx.input(|input| {
            input.pointer.primary_clicked()
                && input
                    .pointer
                    .interact_pos()
                    .is_some_and(|pos| canvas_rect.contains(pos))
                && !pointer_over_area
                && !popup_open
        })
    }

    fn try_adjust_create_panel_font_size_by_shift_wheel(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
    ) -> bool {
        if self.top_panel.is_mask_panel_open() {
            return false;
        }
        if self.text_overlays.has_selected_overlay() {
            return false;
        }
        if WheelSlider::pointer_recently_over_any(ctx) {
            return false;
        }

        let (shift_down, raw_scroll_delta, primary_down, hover_pos, interact_pos) =
            ctx.input(|input| {
                (
                    input.modifiers.shift,
                    input.raw_scroll_delta,
                    input.pointer.primary_down(),
                    input.pointer.hover_pos(),
                    input.pointer.interact_pos(),
                )
            });
        if !shift_down || primary_down {
            return false;
        }

        let pointer_pos = interact_pos.or(hover_pos);
        if !pointer_pos.is_some_and(|pos| canvas_rect.contains(pos)) {
            return false;
        }

        // Match panel wheel behavior: use raw delta only (no smooth inertia)
        // and keep one discrete step per wheel event.
        let mut wheel_delta = raw_scroll_delta.y;
        if wheel_delta.abs() <= f32::EPSILON {
            // Some backends convert Shift+wheel into horizontal scroll.
            wheel_delta = raw_scroll_delta.x;
        }
        if wheel_delta.abs() <= f32::EPSILON {
            return false;
        }

        let steps = if wheel_delta > 0.0 { 1 } else { -1 };
        if !self.top_panel.adjust_create_font_size_by_wheel_steps(steps) {
            return false;
        }

        ctx.input_mut(|input| {
            input.smooth_scroll_delta = Vec2::ZERO;
            input.raw_scroll_delta = Vec2::ZERO;
        });
        true
    }

    fn try_adjust_selected_overlay_font_size_by_shift_wheel(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
    ) -> bool {
        if self.top_panel.is_mask_panel_open() {
            return false;
        }
        if !self.text_overlays.has_selected_overlay() {
            return false;
        }
        if self.top_panel.has_focused_text_input(ctx) {
            return false;
        }
        if WheelSlider::pointer_recently_over_any(ctx) {
            return false;
        }

        let (shift_down, raw_scroll_delta, primary_down, hover_pos, interact_pos) =
            ctx.input(|input| {
                (
                    input.modifiers.shift,
                    input.raw_scroll_delta,
                    input.pointer.primary_down(),
                    input.pointer.hover_pos(),
                    input.pointer.interact_pos(),
                )
            });
        if !shift_down || primary_down {
            return false;
        }

        let pointer_pos = interact_pos.or(hover_pos);
        if !pointer_pos.is_some_and(|pos| canvas_rect.contains(pos)) {
            return false;
        }

        let mut wheel_delta = raw_scroll_delta.y;
        if wheel_delta.abs() <= f32::EPSILON {
            wheel_delta = raw_scroll_delta.x;
        }
        if wheel_delta.abs() <= f32::EPSILON {
            return false;
        }

        let steps = if wheel_delta > 0.0 { 1 } else { -1 };
        if !self
            .top_panel
            .adjust_selected_text_overlay_font_size_by_wheel_steps(steps)
        {
            return false;
        }

        ctx.input_mut(|input| {
            input.smooth_scroll_delta = Vec2::ZERO;
            input.raw_scroll_delta = Vec2::ZERO;
        });
        true
    }
}

struct TypingHooks<'a> {
    text_overlays: &'a mut TypingTextOverlayLayer,
    top_panel: &'a mut TypingTopPanelState,
    mask_layer: &'a mut TypingMaskLayer,
    pending_create_text_from_bubble: Option<BubbleCreateTextRequest>,
    page_overlay_occluders: HashMap<usize, Vec<[Pos2; 4]>>,
}

impl CanvasHooks for TypingHooks<'_> {
    fn wants_canvas_shift_drag_selection(&self, ctx: &egui::Context) -> bool {
        self.text_overlays.wants_canvas_shift_drag_selection(ctx)
    }

    fn draw_canvas_mask_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) {
        if self
            .mask_layer
            .draw_page_mask_overlay_and_handle_input(ui, page_idx, image_rect, zoom)
        {
            self.text_overlays.mark_page_texture_dirty(page_idx);
            ctx.request_repaint();
        }
    }

    fn draw_canvas_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) {
        let panel_text_input_focused = self.top_panel.has_focused_text_input(ctx);
        let auto_typing_settings = self.top_panel.auto_typing_settings();
        let eyedropper_blocks_focus_clear = self.top_panel.eyedropper_active()
            || self
                .top_panel
                .eyedropper_consumed_primary_click_this_frame();
        let occluders = self.text_overlays.draw_page_overlays(
            ui,
            ctx,
            page_idx,
            image_rect,
            zoom,
            self.mask_layer.is_panel_open(),
            panel_text_input_focused,
            eyedropper_blocks_focus_clear,
            auto_typing_settings,
            self.top_panel.strict_pixel_movement(),
        );
        self.page_overlay_occluders.insert(page_idx, occluders);
    }

    fn draw_canvas_overlay_top_left(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        canvas: &mut CanvasView,
        project: &ProjectData,
        _status: CanvasUiStatus,
    ) {
        self.text_overlays
            .set_clean_overlays_model(canvas.clean_overlays_model_handle());
        self.text_overlays.flush_edit_save_on_selection_change();
        if self.text_overlays.layout_editor_editing_active() {
            self.top_panel.sync_selected_overlay_for_edit(None);
            self.text_overlays
                .draw_layout_editor_panels(ctx, canvas_rect);
            return;
        }
        if self.top_panel.is_mask_panel_open() {
            self.text_overlays.clear_selection();
            self.top_panel.sync_selected_overlay_for_edit(None);
        } else {
            self.top_panel
                .sync_selected_overlay_for_edit(self.text_overlays.selected_overlay_for_edit());
        }
        self.top_panel
            .sync_clean_overlays_visible_from_canvas(canvas.clean_overlays_visible());
        self.top_panel
            .set_export_default_dir(project.project_dir.clone());
        self.top_panel
            .sync_export_status(self.text_overlays.export_status_for_ui());
        if let Some(request) = self.pending_create_text_from_bubble.take()
            && let Some(page_rect) = canvas.page_scene_rect(request.page_idx)
        {
            let scene_rect = scene_rect_from_rect_coords(page_rect, request.rect_coords);
            if scene_rect.is_positive() {
                self.text_overlays.open_text_editor_for_selection(
                    ctx,
                    canvas,
                    project,
                    self.top_panel,
                    scene_rect,
                );
            }
        }
        if !self.top_panel.is_mask_panel_open() {
            self.text_overlays.draw_create_overlay_ui(
                ctx,
                canvas_rect,
                canvas,
                project,
                self.top_panel,
            );
        }
        self.top_panel.draw(ctx, canvas_rect);
        if self.text_overlays.layout_editor_preview_active() {
            self.text_overlays
                .draw_layout_editor_mode_panel(ctx, canvas_rect);
        }
        self.text_overlays
            .draw_deformation_mode_panel(ctx, canvas_rect);
        if let Some(request) = self.top_panel.take_create_image_request() {
            let center_page_px = viewport_center_page_px_for_page(canvas_rect, canvas, project);
            self.text_overlays.request_create_image_overlay(
                ctx,
                project,
                canvas.current_page_idx(),
                center_page_px,
                request,
            );
        }
        if let Some(export_dir) = self.top_panel.take_export_to_folder_request() {
            let mask_snapshot = self.mask_layer.export_masks_snapshot();
            self.text_overlays
                .request_export_to_folder(ctx, project, mask_snapshot, export_dir);
        }
        if self.top_panel.take_round_text_positions_request() {
            self.text_overlays.round_all_overlay_positions_to_pixels();
        }
        if let Some(visible) = self.top_panel.take_clean_overlays_visible_request() {
            canvas.set_clean_overlays_visible_for_canvas_only(visible);
        }
        self.mask_layer
            .set_panel_open(ctx, self.top_panel.is_mask_panel_open());
        self.mask_layer
            .draw_panel(ctx, canvas_rect, canvas.current_page_idx());
        if self.top_panel.is_mask_panel_open() {
            self.text_overlays.clear_selection();
            self.top_panel.sync_selected_overlay_for_edit(None);
        } else if let Some(request) = self.top_panel.take_edit_request() {
            self.text_overlays
                .queue_selected_overlay_edit_request(ctx, request);
        }
    }

    fn has_bubble_header(&mut self, bubble: &Bubble, _editable: bool) -> bool {
        bubble_rect_coords(bubble).is_some()
    }

    fn build_bubble_header(&mut self, ui: &mut egui::Ui, bubble: &Bubble, _editable: bool) {
        let Some(rect_coords) = bubble_rect_coords(bubble) else {
            return;
        };
        if ui.small_button("Создать текст").clicked() {
            self.pending_create_text_from_bubble = Some(BubbleCreateTextRequest {
                page_idx: bubble.img_idx,
                rect_coords,
            });
        }
    }

    fn readonly_aside_header_width_hint(
        &mut self,
        ui: &egui::Ui,
        bubble: &Bubble,
        _editable: bool,
    ) -> Option<f32> {
        const READONLY_ASIDE_HEADER_WIDTH_SAFETY_PX: f32 = 10.0;

        bubble_rect_coords(bubble)?;
        let font_id = egui::TextStyle::Button.resolve(ui.style());
        let text_color = ui.visuals().widgets.inactive.text_color();
        let text_width = ui.fonts_mut(|fonts| {
            fonts
                .layout_job(egui::text::LayoutJob::simple(
                    "Создать текст".to_owned(),
                    font_id.clone(),
                    text_color,
                    f32::INFINITY,
                ))
                .size()
                .x
        });
        Some(
            text_width
                + ui.spacing().button_padding.x * 2.0
                + READONLY_ASIDE_HEADER_WIDTH_SAFETY_PX,
        )
    }

    fn should_hide_on_top_bubble(
        &mut self,
        page_idx: usize,
        _bubble: &Bubble,
        bubble_rect: Rect,
    ) -> bool {
        let bubble_quad = [
            bubble_rect.left_top(),
            bubble_rect.right_top(),
            bubble_rect.right_bottom(),
            bubble_rect.left_bottom(),
        ];
        self.page_overlay_occluders
            .get(&page_idx)
            .is_some_and(|quads| {
                quads
                    .iter()
                    .any(|overlay_quad| quads_intersect(overlay_quad, &bubble_quad))
            })
    }

    fn should_hide_aside_bubble_line(
        &mut self,
        page_idx: usize,
        _bubble: &Bubble,
        line_start: Pos2,
        line_end: Pos2,
    ) -> bool {
        self.page_overlay_occluders
            .get(&page_idx)
            .is_some_and(|quads| {
                quads
                    .iter()
                    .any(|overlay_quad| segment_intersects_quad(line_start, line_end, overlay_quad))
            })
    }
}

#[derive(Debug, Clone, Copy)]
struct BubbleCreateTextRequest {
    page_idx: usize,
    rect_coords: RectCoords,
}

fn bubble_rect_coords(bubble: &Bubble) -> Option<RectCoords> {
    let raw = bubble.extra.get("rect_coords")?;
    let obj = raw.as_object()?;
    let p1 = obj.get("p1")?.as_object()?;
    let p2 = obj.get("p2")?.as_object()?;
    let u1 = p1.get("img_u")?.as_f64()? as f32;
    let v1 = p1.get("img_v")?.as_f64()? as f32;
    let u2 = p2.get("img_u")?.as_f64()? as f32;
    let v2 = p2.get("img_v")?.as_f64()? as f32;
    Some(RectCoords {
        p1: egui::pos2(u1, v1),
        p2: egui::pos2(u2, v2),
    })
}

fn scene_rect_from_rect_coords(page_rect: Rect, rect_coords: RectCoords) -> Rect {
    let coords = rect_coords.normalized();
    let p1 = egui::pos2(
        page_rect.left() + page_rect.width() * coords.p1.x.clamp(0.0, 1.0),
        page_rect.top() + page_rect.height() * coords.p1.y.clamp(0.0, 1.0),
    );
    let p2 = egui::pos2(
        page_rect.left() + page_rect.width() * coords.p2.x.clamp(0.0, 1.0),
        page_rect.top() + page_rect.height() * coords.p2.y.clamp(0.0, 1.0),
    );
    Rect::from_two_pos(p1, p2)
}

#[derive(Debug, Clone, Copy)]
struct TypingCreateSelection {
    start: Pos2,
    current: Pos2,
}

impl TypingCreateSelection {
    fn rect(self) -> Rect {
        Rect::from_two_pos(self.start, self.current)
    }
}

struct TypingAutoTypingJobState {
    rx: Receiver<Result<TypingAutoTypingWorkerResult, String>>,
    token: u64,
    overlay_idx: usize,
    overlay_file_name: String,
    page_idx: usize,
    overlay_optical_tuv: [f32; 2],
}

struct TypingAutoTypingWorkerResult {
    token: u64,
    page_idx: usize,
    click_uv: [f32; 2],
    detection: TypingAutoTypingDetectionResult,
}

#[derive(Clone)]
struct TypingAutoTypingDebugVisual {
    page_idx: usize,
    accepted: bool,
    overlay_center_uv: [f32; 2],
    bubble_center_uv: Option<[f32; 2]>,
    bubble_bounds_uv: Option<[f32; 4]>,
    bubble_contour_uv: Vec<[f32; 2]>,
}

struct TypingOverlaySceneGeometry {
    quad_scene: [Pos2; 4],
    mesh_scene: Vec<Pos2>,
    mesh_cols: usize,
    mesh_rows: usize,
    bounds_rect: Rect,
}

struct TypingCreateTextEditor {
    page_idx: usize,
    scene_rect: Rect,
    center_page_px: [f32; 2],
    width_px: u32,
    text: String,
    font_family: Option<egui::FontFamily>,
    font_size_px: f32,
    needs_focus: bool,
    window_focused_last_frame: bool,
}

struct TypingCreateRenderState {
    rx: Receiver<Result<TypingOverlayDecoded, String>>,
    scene_rect: Option<Rect>,
}

struct TypingExportRenderState {
    rx: Receiver<TypingExportEvent>,
}

struct TypingCreateOverlayRequest {
    text_images_dir: PathBuf,
    page_idx: usize,
    center_page_px: [f32; 2],
    render_params: TextRenderParams,
    render_data_json: Value,
}

struct TypingCreateImageOverlayRequest {
    text_images_dir: PathBuf,
    page_idx: usize,
    center_page_px: [f32; 2],
    source: TypingCreateImageSource,
}

enum TypingCreateImageSource {
    Clipboard,
    File(PathBuf),
}

struct TypingEditOverlayRequest {
    token: u64,
    latest_token: Arc<AtomicU64>,
    overlay_idx: usize,
    file_name: String,
    text_images_dir: PathBuf,
    user_scale: f32,
    rotation_deg: f32,
    render_params: TextRenderParams,
    render_data_json: Value,
}

struct TypingEditOverlayResult {
    token: u64,
    overlay_idx: usize,
    file_name: String,
    user_scale: f32,
    rotation_deg: f32,
    render_data_json: Value,
    size_px: [usize; 2],
    rgba: Vec<u8>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct TypingShapeVariant {
    row: usize,
    col: usize,
    width_px: u32,
    text_wrap_mode: TextWrapMode,
    shape_min_width_percent: f32,
    shape_variant: u8,
}

struct TypingShapeVariantPreviewTile {
    variant: TypingShapeVariant,
    size_px: [usize; 2],
    rgba: Option<Vec<u8>>,
    texture: Option<egui::TextureHandle>,
}

struct TypingShapeVariantPreviewResult {
    menu_id: u64,
    tiles: Vec<TypingShapeVariantPreviewTile>,
}

struct TypingShapeVariantPreviewState {
    menu_id: u64,
    overlay_idx: usize,
    origin: Pos2,
    menu_rect: Option<Rect>,
    place_above: bool,
    dark_checkerboard: bool,
    slot_size: Vec2,
    gap_px: f32,
    padding_px: f32,
    cancel_render: Arc<AtomicBool>,
    rx: Receiver<Result<TypingShapeVariantPreviewResult, String>>,
    tiles: Option<Vec<TypingShapeVariantPreviewTile>>,
}

impl Drop for TypingShapeVariantPreviewState {
    fn drop(&mut self) {
        self.cancel_render.store(true, Ordering::Relaxed);
    }
}

struct TypingOverlayDecoded {
    kind: TypingOverlayKind,
    page_idx: usize,
    center_page_px: [f32; 2],
    mask_clip_enabled: bool,
    user_scale: f32,
    angle_deg: f32,
    deform_mesh: Option<TypingOverlayDeformMesh>,
    file_name: String,
    #[allow(dead_code)]
    render_data_json: Option<Value>,
    size_px: [usize; 2],
    rgba: Vec<u8>,
    warnings: Vec<String>,
}

struct TypingOverlayRuntime {
    kind: TypingOverlayKind,
    page_idx: usize,
    center_page_px: [f32; 2],
    mask_clip_enabled: bool,
    user_scale: f32,
    angle_deg: f32,
    deform_mesh: Option<TypingOverlayDeformMesh>,
    file_name: String,
    #[allow(dead_code)]
    render_data_json: Option<Value>,
    size_px: [usize; 2],
    source_rgba: Vec<u8>,
    texture: Option<egui::TextureHandle>,
    display_texture_stale: bool,
    last_texture_used_frame: u64,
}

#[derive(Clone)]
struct TypingExportOverlaySnapshot {
    page_idx: usize,
    center_page_px: [f32; 2],
    mask_clip_enabled: bool,
    user_scale: f32,
    angle_deg: f32,
    deform_mesh: Option<TypingOverlayDeformMesh>,
    size_px: [usize; 2],
    source_rgba: Vec<u8>,
}

struct TypingExportPageJob {
    page_idx: usize,
    page_path: PathBuf,
    output_path: PathBuf,
    clean_overlay_path: Option<PathBuf>,
    clean_overlay_rgba: Option<Arc<image::RgbaImage>>,
    overlays: Vec<TypingExportOverlaySnapshot>,
    mask: Option<TypingMaskExportPage>,
}

struct TypingExportResult {
    exported: usize,
    total: usize,
    output_dir: PathBuf,
}

enum TypingExportEvent {
    Progress { done: usize, total: usize },
    Finished(Result<TypingExportResult, String>),
}

#[derive(Debug, Clone, Copy)]
enum TypingOverlayDragMode {
    MoveCenter,
    MoveMesh,
    PerspectiveHandle(usize),
    BendHandle(usize),
    FrameHandle(usize),
    GridHandle(usize),
    BrushStroke(TypingDeformMode),
    Rotate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypingLayoutEditorMode {
    Editing,
    Preview,
}

#[derive(Debug, Clone)]
struct TypingLayoutEditorLine {
    label: String,
    points: Vec<Pos2>,
    corner_smoothing_px: f32,
    text_direction: TextVectorLineTextDirection,
    distance_mode: TextVectorLineDistanceMode,
    flip_text: bool,
}

#[derive(Debug, Clone)]
struct TypingLayoutEditorState {
    overlay_idx: usize,
    page_idx: usize,
    frame_page_rect: Rect,
    mode: TypingLayoutEditorMode,
    active_line_idx: usize,
    lines: Vec<TypingLayoutEditorLine>,
    frame_drag: Option<TypingLayoutFrameDragState>,
    line_drag: Option<TypingLayoutLineDragState>,
}

#[derive(Debug, Clone, Copy)]
struct TypingLayoutFrameDragState {
    handle: TypingLayoutFrameHandle,
    pointer_start_page_px: Pos2,
    start_rect: Rect,
}

#[derive(Debug, Clone, Copy)]
struct TypingLayoutLineDragState {
    line_idx: usize,
    point_idx: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TypingLayoutFrameHandle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

#[derive(Debug, Clone)]
struct TypingOverlayDragState {
    overlay_idx: usize,
    page_idx: usize,
    pointer_start_scene: Pos2,
    mode: TypingOverlayDragMode,
    start_has_mesh: bool,
    start_center_page_px: [f32; 2],
    start_angle_deg: f32,
    start_pointer_angle_rad: f32,
    start_mesh: TypingOverlayDeformMesh,
}

type TypingOverlayLoadResponse = (PathBuf, Result<Vec<TypingOverlayDecoded>, String>);

struct TypingTextOverlayLayer {
    loaded_project_dir: Option<PathBuf>,
    loaded_text_images_dir: Option<PathBuf>,
    /// Directory where new/edited overlays are written (always the unsaved staging dir).
    text_images_save_dir: Option<PathBuf>,
    loading_project_dir: Option<PathBuf>,
    loading_text_images_dir: Option<PathBuf>,
    loading_rx: Option<Receiver<TypingOverlayLoadResponse>>,
    save_rx: Option<Receiver<Result<(), String>>>,
    save_requested_while_busy: bool,
    export_rx: Option<TypingExportRenderState>,
    export_status: TypingExportUiStatus,
    edit_render_rx: Option<Receiver<Result<Option<TypingEditOverlayResult>, String>>>,
    edit_render_latest_token: Arc<AtomicU64>,
    edit_render_next_token: u64,
    edit_render_data_dirty: bool,
    shape_variant_preview_next_id: u64,
    shape_variant_preview: Option<TypingShapeVariantPreviewState>,
    last_selected_overlay_idx: Option<usize>,
    create_selection: Option<TypingCreateSelection>,
    create_editor: Option<TypingCreateTextEditor>,
    create_render_state: Option<TypingCreateRenderState>,
    editor_font_cache: HashMap<(PathBuf, usize), String>,
    editor_font_next_id: u64,
    create_status_error: Option<(String, f64)>,
    create_status_warning: Option<(String, f64)>,
    overlays: Vec<TypingOverlayRuntime>,
    pending_upload_indices: VecDeque<usize>,
    pending_upload_set: HashSet<usize>,
    last_load_error: Option<String>,
    selected_overlay_idx: Option<usize>,
    transform_mode_overlay_idx: Option<usize>,
    layout_editor: Option<TypingLayoutEditorState>,
    deform_mode: TypingDeformMode,
    frame_handle_side_points: usize,
    pull_neighbor_handles: bool,
    deform_tool_settings: TypingDeformToolSettings,
    drag_state: Option<TypingOverlayDragState>,
    drag_has_changes: bool,
    primary_pointer_targets_overlay_this_frame: bool,
    page_count: usize,
    clean_overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    auto_typing_next_token: u64,
    auto_typing_job: Option<TypingAutoTypingJobState>,
    auto_typing_debug_visual: Option<TypingAutoTypingDebugVisual>,
}

impl Default for TypingTextOverlayLayer {
    fn default() -> Self {
        Self {
            loaded_project_dir: None,
            loaded_text_images_dir: None,
            text_images_save_dir: None,
            loading_project_dir: None,
            loading_text_images_dir: None,
            loading_rx: None,
            save_rx: None,
            save_requested_while_busy: false,
            export_rx: None,
            export_status: TypingExportUiStatus::Hidden,
            edit_render_rx: None,
            edit_render_latest_token: Arc::new(AtomicU64::new(0)),
            edit_render_next_token: 0,
            edit_render_data_dirty: false,
            shape_variant_preview_next_id: 0,
            shape_variant_preview: None,
            last_selected_overlay_idx: None,
            create_selection: None,
            create_editor: None,
            create_render_state: None,
            editor_font_cache: HashMap::new(),
            editor_font_next_id: 0,
            create_status_error: None,
            create_status_warning: None,
            overlays: Vec::new(),
            pending_upload_indices: VecDeque::new(),
            pending_upload_set: HashSet::new(),
            last_load_error: None,
            selected_overlay_idx: None,
            transform_mode_overlay_idx: None,
            layout_editor: None,
            deform_mode: TypingDeformMode::Perspective,
            frame_handle_side_points: TEXT_OVERLAY_FRAME_HANDLE_SIDE_POINTS_DEFAULT,
            pull_neighbor_handles: true,
            deform_tool_settings: TypingDeformToolSettings::default(),
            drag_state: None,
            drag_has_changes: false,
            primary_pointer_targets_overlay_this_frame: false,
            page_count: 0,
            clean_overlays_model: None,
            auto_typing_next_token: 0,
            auto_typing_job: None,
            auto_typing_debug_visual: None,
        }
    }
}

impl TypingTextOverlayLayer {
    fn begin_canvas_frame(&mut self) {
        self.primary_pointer_targets_overlay_this_frame = false;
    }

    fn layout_editor_active(&self) -> bool {
        self.layout_editor.is_some()
    }

    fn layout_editor_editing_active(&self) -> bool {
        self.layout_editor
            .as_ref()
            .is_some_and(|editor| editor.mode == TypingLayoutEditorMode::Editing)
    }

    fn layout_editor_preview_active(&self) -> bool {
        self.layout_editor
            .as_ref()
            .is_some_and(|editor| editor.mode == TypingLayoutEditorMode::Preview)
    }

    fn next_shape_variant_preview_id(&mut self) -> u64 {
        self.shape_variant_preview_next_id = self.shape_variant_preview_next_id.wrapping_add(1);
        self.shape_variant_preview_next_id
    }

    fn primary_pointer_targets_overlay_this_frame(&self) -> bool {
        self.primary_pointer_targets_overlay_this_frame
    }

    fn gpu_memory_snapshot(&self, pinned_pages: &BTreeSet<usize>) -> Vec<CacheResourceInfo> {
        self.overlays
            .iter()
            .enumerate()
            .filter(|(_, overlay)| overlay.texture.is_some())
            .map(|(idx, overlay)| CacheResourceInfo {
                id: format!("typing-text-overlay-gpu:{idx}:{}", overlay.file_name),
                kind: CacheResourceKind::TextOverlayGpu,
                page_idx: Some(overlay.page_idx),
                estimated_bytes: u64::try_from(
                    overlay.size_px[0]
                        .saturating_mul(overlay.size_px[1])
                        .saturating_mul(4),
                )
                .unwrap_or(u64::MAX),
                last_used_frame: overlay.last_texture_used_frame,
                reload_cost: CacheReloadCost::RebuildFromModel,
                dirty: false,
                visible: pinned_pages.contains(&overlay.page_idx),
                reconstructable: !overlay.source_rgba.is_empty(),
            })
            .collect()
    }

    fn evict_gpu_cache(&mut self, request: &CacheEvictionRequest) -> CacheEvictionReport {
        let snapshot = self.gpu_memory_snapshot(&request.pinned_pages);
        let candidates = select_eviction_candidates(&snapshot, request);
        let mut evicted = Vec::new();
        let mut freed = 0_u64;
        for resource in candidates.resources {
            let Some(idx) = resource
                .id
                .strip_prefix("typing-text-overlay-gpu:")
                .and_then(|tail| tail.split(':').next())
                .and_then(|raw| raw.parse::<usize>().ok())
            else {
                continue;
            };
            let Some(overlay) = self.overlays.get_mut(idx) else {
                continue;
            };
            if overlay.texture.take().is_some() {
                overlay.display_texture_stale = true;
                overlay.last_texture_used_frame = 0;
                freed = freed.saturating_add(resource.estimated_bytes);
                evicted.push(resource);
            }
        }
        CacheEvictionReport {
            resources: evicted,
            estimated_freed_bytes: freed,
        }
    }

    fn draw_deformation_mode_panel(&mut self, ctx: &egui::Context, canvas_rect: Rect) {
        if self.transform_mode_overlay_idx.is_none() {
            return;
        }
        let area_pos = canvas_rect.left_top() + egui::vec2(16.0, 16.0);
        egui::Area::new("typing_deformation_mode_panel".into())
            .order(egui::Order::Foreground)
            .fixed_pos(area_pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(Color32::from_rgba_unmultiplied(95, 22, 22, 235))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(255, 110, 110)))
                    .show(ui, |ui| {
                        ui.visuals_mut().override_text_color =
                            Some(Color32::from_rgb(255, 235, 235));
                        ui.label(egui::RichText::new("Режим деформации").strong());
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            for mode in [
                                TypingDeformMode::Perspective,
                                TypingDeformMode::Bend,
                                TypingDeformMode::Frame,
                                TypingDeformMode::Grid,
                                TypingDeformMode::Bulge,
                                TypingDeformMode::Pinch,
                                TypingDeformMode::Push,
                                TypingDeformMode::Twirl,
                                TypingDeformMode::Restore,
                                TypingDeformMode::Smooth,
                                TypingDeformMode::Stretch,
                                TypingDeformMode::Fold,
                            ] {
                                ui.selectable_value(&mut self.deform_mode, mode, mode.label());
                            }
                        });
                        if matches!(
                            self.deform_mode,
                            TypingDeformMode::Frame | TypingDeformMode::Grid
                        ) {
                            ui.add_space(6.0);
                            ui.label("Плотность точек");
                            ui.horizontal_wrapped(|ui| {
                                let max_side_points = TEXT_OVERLAY_DEFORM_SURFACE_COLS
                                    .min(TEXT_OVERLAY_DEFORM_SURFACE_ROWS);
                                for side_points in 3..=max_side_points {
                                    ui.selectable_value(
                                        &mut self.frame_handle_side_points,
                                        side_points,
                                        format!("{side_points}*{side_points}"),
                                    );
                                }
                            });
                            ui.checkbox(&mut self.pull_neighbor_handles, "Тянуть соседние ручки");
                        }
                        if self.deform_mode.is_brush_mode() {
                            ui.add_space(6.0);
                            ui.add(
                                WheelSlider::new(
                                    &mut self.deform_tool_settings.brush_radius_px,
                                    16.0..=280.0,
                                )
                                .text("Радиус"),
                            );
                            ui.add(
                                WheelSlider::new(
                                    &mut self.deform_tool_settings.brush_strength,
                                    0.05..=1.5,
                                )
                                .text("Сила"),
                            );
                        }
                    });
            });
    }

    fn draw_layout_editor_panels(&mut self, ctx: &egui::Context, canvas_rect: Rect) {
        if self.layout_editor.is_none() {
            return;
        }
        self.draw_layout_editor_mode_panel(ctx, canvas_rect);
        if self.layout_editor_editing_active() {
            self.draw_layout_editor_lines_panel(ctx, canvas_rect);
        }
    }

    fn draw_layout_editor_mode_panel(&mut self, ctx: &egui::Context, canvas_rect: Rect) {
        let controls_rect =
            ctx.memory(|mem| mem.area_rect(Id::new(CANVAS_LEFT_TOP_CONTROLS_AREA_ID)));
        let default_pos = controls_rect
            .map(|rect| egui::pos2(rect.left(), rect.bottom() + 8.0))
            .unwrap_or(canvas_rect.left_top() + Vec2::new(16.0, 16.0));
        egui::Area::new("typing_layout_editor_mode_panel".into())
            .order(egui::Order::Foreground)
            .movable(true)
            .interactable(true)
            .default_pos(default_pos)
            .show(ctx, |ui| {
                ui.set_width(TEXT_LAYOUT_EDITOR_MODE_PANEL_WIDTH_PX);
                egui::Frame::popup(ui.style())
                    .fill(Color32::from_rgba_unmultiplied(36, 36, 44, 240))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(120, 140, 180)))
                    .show(ui, |ui| {
                        ui.set_width(TEXT_LAYOUT_EDITOR_MODE_PANEL_WIDTH_PX);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Редактирование раскладки")
                                    .strong()
                                    .color(Color32::from_rgb(245, 245, 255)),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let exit = egui::Button::new(
                                        egui::RichText::new("Выйти").strong().color(Color32::WHITE),
                                    )
                                    .fill(Color32::from_rgb(180, 38, 38));
                                    if ui.add(exit).clicked() {
                                        self.exit_layout_editor();
                                    }
                                },
                            );
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            let mode = self
                                .layout_editor
                                .as_ref()
                                .map(|editor| editor.mode)
                                .unwrap_or(TypingLayoutEditorMode::Editing);
                            if ui
                                .selectable_label(
                                    mode == TypingLayoutEditorMode::Editing,
                                    "Редактирование",
                                )
                                .clicked()
                            {
                                self.enter_layout_editor_editing();
                            }
                            if ui
                                .selectable_label(
                                    mode == TypingLayoutEditorMode::Preview,
                                    "Предпросмотр",
                                )
                                .clicked()
                            {
                                self.enter_layout_editor_preview(ctx);
                            }
                        });
                    });
            });
    }

    fn draw_layout_editor_lines_panel(&mut self, ctx: &egui::Context, canvas_rect: Rect) {
        let panel_w =
            TEXT_LAYOUT_EDITOR_PANEL_WIDTH_PX.min((canvas_rect.width() - 24.0).max(220.0));
        let panel_h =
            TEXT_LAYOUT_EDITOR_PANEL_HEIGHT_PX.min((canvas_rect.height() - 24.0).max(220.0));
        let default_pos = egui::pos2(
            canvas_rect.right() - panel_w - 12.0,
            canvas_rect.top() + 12.0,
        );
        egui::Area::new("typing_layout_editor_lines_panel".into())
            .order(egui::Order::Foreground)
            .movable(true)
            .interactable(true)
            .default_pos(default_pos)
            .show(ctx, |ui| {
                ui.set_width(panel_w);
                ui.set_min_width(panel_w);
                ui.set_max_width(panel_w);
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(panel_w);
                    ui.set_min_height(panel_h);
                    let Some(editor) = self.layout_editor.as_mut() else {
                        return;
                    };
                    ui.label(egui::RichText::new("Векторные").strong());
                    ui.separator();
                    draw_layout_editor_vector_lines_tab(ui, editor);
                });
            });
    }

    fn begin_layout_editor_for_overlay(&mut self, overlay_idx: usize, image_rect: Rect, zoom: f32) {
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            return;
        };
        let geometry = overlay_scene_geometry(overlay, image_rect, zoom);
        let page_size = page_size_from_image_rect(image_rect, zoom);
        let saved_vector_layout = overlay.render_data_json.as_ref().and_then(|render_data| {
            text_render_params_from_render_data(render_data)
                .map(|params| params.vector_lines_layout)
        });
        let frame_page_rect = saved_vector_layout
            .as_ref()
            .filter(|layout| {
                layout.width_px > 1 || layout.height_px > 1 || !layout.lines.is_empty()
            })
            .map(|layout| {
                let center = geometry.bounds_rect.center();
                let center_page = page_px_from_scene(image_rect, zoom, center);
                frame_rect_from_center_and_size(
                    Pos2::new(center_page[0], center_page[1]),
                    Vec2::new(
                        layout.width_px.max(1) as f32,
                        layout.height_px.max(1) as f32,
                    ),
                    page_size,
                )
            })
            .unwrap_or_else(|| {
                let min_page = page_px_from_scene(image_rect, zoom, geometry.bounds_rect.min);
                let max_page = page_px_from_scene(image_rect, zoom, geometry.bounds_rect.max);
                Rect::from_min_max(
                    Pos2::new(
                        min_page[0].clamp(0.0, page_size[0].max(1) as f32),
                        min_page[1].clamp(0.0, page_size[1].max(1) as f32),
                    ),
                    Pos2::new(
                        max_page[0].clamp(0.0, page_size[0].max(1) as f32),
                        max_page[1].clamp(0.0, page_size[1].max(1) as f32),
                    ),
                )
            });
        let loaded_lines = saved_vector_layout
            .map(layout_editor_lines_from_vector_layout)
            .filter(|lines| !lines.is_empty())
            .unwrap_or_else(|| {
                vec![TypingLayoutEditorLine {
                    label: "Строка 1".to_string(),
                    points: Vec::new(),
                    corner_smoothing_px: 0.0,
                    text_direction: TextVectorLineTextDirection::LeftToRight,
                    distance_mode: TextVectorLineDistanceMode::ByLineLength,
                    flip_text: false,
                }]
            });
        self.layout_editor = Some(TypingLayoutEditorState {
            overlay_idx,
            page_idx: overlay.page_idx,
            frame_page_rect,
            mode: TypingLayoutEditorMode::Editing,
            active_line_idx: 0,
            lines: loaded_lines,
            frame_drag: None,
            line_drag: None,
        });
        self.selected_overlay_idx = Some(overlay_idx);
        self.transform_mode_overlay_idx = None;
        self.drag_state = None;
        self.drag_has_changes = false;
    }

    fn exit_layout_editor(&mut self) {
        if self.edit_render_data_dirty {
            self.request_overlay_placement_save();
            self.edit_render_data_dirty = false;
        }
        self.layout_editor = None;
    }

    fn enter_layout_editor_editing(&mut self) {
        if let Some(editor) = self.layout_editor.as_mut() {
            editor.mode = TypingLayoutEditorMode::Editing;
        }
    }

    fn enter_layout_editor_preview(&mut self, ctx: &egui::Context) {
        let Some(editor) = self.layout_editor.as_mut() else {
            return;
        };
        editor.mode = TypingLayoutEditorMode::Preview;
        let overlay_idx = editor.overlay_idx;
        let vector_layout = vector_lines_layout_from_editor(editor);
        let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
            self.layout_editor = None;
            return;
        };
        if overlay.kind != TypingOverlayKind::Text {
            return;
        }
        let Some(render_data_json) = overlay
            .render_data_json
            .as_ref()
            .and_then(|render_data| render_data_with_vector_layout(render_data, &vector_layout))
        else {
            self.set_create_error(ctx, "Не удалось обновить параметры векторной раскладки.");
            return;
        };
        let Some(render_params) = text_render_params_from_render_data(&render_data_json) else {
            self.set_create_error(ctx, "Не удалось собрать параметры рендера предпросмотра.");
            return;
        };
        let Some(text_images_dir) = self.text_images_save_dir.clone() else {
            self.set_create_error(
                ctx,
                "Не найдена папка text_images для предпросмотра раскладки.",
            );
            return;
        };

        overlay.render_data_json = Some(render_data_json.clone());
        overlay.user_scale = 1.0;
        overlay.size_px = [
            usize::try_from(vector_layout.width_px).unwrap_or(usize::MAX),
            usize::try_from(vector_layout.height_px).unwrap_or(usize::MAX),
        ];
        self.edit_render_data_dirty = true;
        let edit_request = TypingEditOverlayRequest {
            token: 0,
            latest_token: Arc::clone(&self.edit_render_latest_token),
            overlay_idx,
            file_name: overlay.file_name.clone(),
            text_images_dir,
            user_scale: 1.0,
            rotation_deg: overlay.angle_deg,
            render_params,
            render_data_json,
        };
        self.start_edit_overlay_render_job(edit_request);
    }

    fn draw_layout_editor_on_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        clip_rect: Rect,
    ) {
        let Some(editor) = self.layout_editor.as_mut() else {
            return;
        };
        if editor.page_idx != page_idx {
            return;
        }
        if editor.mode != TypingLayoutEditorMode::Editing {
            return;
        }
        if editor.overlay_idx >= self.overlays.len() {
            self.layout_editor = None;
            return;
        }
        ensure_layout_editor_has_line(editor);
        let page_size = page_size_from_image_rect(image_rect, zoom);
        let frame_scene = layout_editor_frame_scene_rect(editor.frame_page_rect, image_rect, zoom);
        let line_rect_response = ui.interact(
            frame_scene,
            Id::new(("typing_layout_editor_lines", editor.overlay_idx)),
            Sense::click_and_drag(),
        );
        let active_line_idx = editor
            .active_line_idx
            .min(editor.lines.len().saturating_sub(1));
        editor.active_line_idx = active_line_idx;
        handle_layout_editor_vector_canvas_input(
            editor,
            active_line_idx,
            frame_scene,
            image_rect,
            zoom,
            &line_rect_response,
            ctx,
        );

        let frame_scene = layout_editor_frame_scene_rect(editor.frame_page_rect, image_rect, zoom);
        for (handle, handle_pos) in layout_frame_handle_points(frame_scene) {
            let handle_rect = Rect::from_center_size(
                handle_pos,
                Vec2::splat(TEXT_LAYOUT_EDITOR_FRAME_HANDLE_RADIUS_PX * 4.0),
            );
            let response = ui.interact(
                handle_rect,
                Id::new((
                    "typing_layout_editor_frame_handle",
                    editor.overlay_idx,
                    handle,
                )),
                Sense::drag(),
            );
            let pointer_page = response.interact_pointer_pos().map(|pos| {
                let page = page_px_from_scene(image_rect, zoom, pos);
                Pos2::new(page[0], page[1])
            });
            if response.drag_started()
                && let Some(pointer_page) = pointer_page
            {
                editor.frame_drag = Some(TypingLayoutFrameDragState {
                    handle,
                    pointer_start_page_px: pointer_page,
                    start_rect: editor.frame_page_rect,
                });
            }
            if response.dragged()
                && let (Some(drag), Some(pointer_page)) = (editor.frame_drag, pointer_page)
                && drag.handle == handle
            {
                let delta = pointer_page - drag.pointer_start_page_px;
                editor.frame_page_rect =
                    apply_layout_frame_drag(drag.start_rect, drag.handle, delta, page_size);
                clamp_layout_editor_points_to_frame(editor);
                ctx.request_repaint();
            }
            if response.drag_stopped()
                && editor.frame_drag.is_some_and(|drag| drag.handle == handle)
            {
                editor.frame_drag = None;
            }
        }

        let painter = ui.painter().with_clip_rect(clip_rect);
        draw_layout_editor_frame(&painter, frame_scene);
        draw_layout_editor_vector_lines(&painter, frame_scene, zoom, editor);
    }

    fn next_edit_render_token(&mut self) -> u64 {
        self.edit_render_next_token = self.edit_render_next_token.wrapping_add(1);
        self.edit_render_latest_token
            .store(self.edit_render_next_token, Ordering::Release);
        self.edit_render_next_token
    }

    fn cancel_active_edit_overlay_render(&mut self) {
        self.next_edit_render_token();
        self.edit_render_rx = None;
    }

    fn set_page_count(&mut self, page_count: usize) {
        self.page_count = page_count;
    }

    fn set_clean_overlays_model(&mut self, model: Option<Arc<Mutex<CleanOverlaysModel>>>) {
        self.clean_overlays_model = model;
    }

    fn ensure_loader_started(&mut self, project: &ProjectData) {
        let project_dir = project.project_dir.clone();
        if self.loaded_project_dir.as_ref() == Some(&project_dir) {
            return;
        }
        if self.loading_project_dir.as_ref() == Some(&project_dir) {
            return;
        }

        self.overlays.clear();
        self.pending_upload_indices.clear();
        self.pending_upload_set.clear();
        self.last_load_error = None;
        self.create_selection = None;
        self.create_editor = None;
        self.create_render_state = None;
        self.create_status_error = None;
        self.save_rx = None;
        self.save_requested_while_busy = false;
        self.export_rx = None;
        self.export_status = TypingExportUiStatus::Hidden;
        self.cancel_active_edit_overlay_render();
        self.edit_render_data_dirty = false;
        self.last_selected_overlay_idx = None;
        self.selected_overlay_idx = None;
        self.transform_mode_overlay_idx = None;
        self.drag_state = None;
        self.drag_has_changes = false;
        self.auto_typing_job = None;
        self.auto_typing_debug_visual = None;
        self.auto_typing_next_token = 0;
        self.loaded_project_dir = None;
        self.loaded_text_images_dir = None;

        // Saves always go to the unsaved staging dir.
        self.text_images_save_dir = Some(project.paths.unsaved_text_images_dir.clone());

        // For loading: prefer the unsaved dir when its text_info.json exists
        // (crash-recovery mode), and fall back to individual PNGs from the main dir.
        let unsaved_text_images_dir = project.paths.unsaved_text_images_dir.clone();
        let main_text_images_dir = project.paths.text_images_dir.clone();
        let load_from_unsaved = unsaved_text_images_dir.join(TEXT_INFO_FILE_NAME).is_file();
        let (primary_load_dir, fallback_load_dir) = if load_from_unsaved {
            (unsaved_text_images_dir, Some(main_text_images_dir))
        } else {
            (main_text_images_dir, None)
        };

        let page_paths = project
            .pages
            .iter()
            .map(|page| (page.idx, page.path.clone()))
            .collect::<Vec<_>>();
        let (tx, rx) = mpsc::channel::<TypingOverlayLoadResponse>();
        let project_dir_for_thread = project_dir.clone();
        let primary_load_dir_for_thread = primary_load_dir.clone();
        thread::spawn(move || {
            let page_sizes = load_typing_page_sizes(&page_paths);
            let result = load_typing_overlays_from_dir(
                &primary_load_dir_for_thread,
                fallback_load_dir.as_deref(),
                &page_sizes,
            );
            let _ = tx.send((project_dir_for_thread, result));
        });
        self.loading_project_dir = Some(project_dir);
        self.loading_text_images_dir = Some(primary_load_dir);
        self.loading_rx = Some(rx);
    }

    fn poll_loader(&mut self) -> bool {
        let Some(rx) = self.loading_rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok((project_dir, result)) => {
                self.loading_rx = None;
                self.loading_project_dir = None;
                self.loaded_project_dir = Some(project_dir);
                match result {
                    Ok(decoded) => {
                        self.loaded_text_images_dir = self.loading_text_images_dir.take();
                        self.overlays = decoded
                            .into_iter()
                            .map(|entry| TypingOverlayRuntime {
                                kind: entry.kind,
                                page_idx: entry.page_idx,
                                center_page_px: entry.center_page_px,
                                mask_clip_enabled: entry.mask_clip_enabled,
                                user_scale: entry.user_scale,
                                angle_deg: entry.angle_deg,
                                deform_mesh: entry.deform_mesh,
                                file_name: entry.file_name,
                                render_data_json: entry.render_data_json,
                                size_px: entry.size_px,
                                source_rgba: entry.rgba,
                                texture: None,
                                display_texture_stale: true,
                                last_texture_used_frame: 0,
                            })
                            .collect();
                        self.pending_upload_indices.clear();
                        self.pending_upload_set.clear();
                        for idx in 0..self.overlays.len() {
                            self.queue_overlay_texture_upload(idx);
                        }
                        self.export_rx = None;
                        self.export_status = TypingExportUiStatus::Hidden;
                        self.last_load_error = None;
                        self.cancel_active_edit_overlay_render();
                        self.edit_render_data_dirty = false;
                        self.last_selected_overlay_idx = None;
                        self.selected_overlay_idx = None;
                        self.transform_mode_overlay_idx = None;
                        self.drag_state = None;
                        self.drag_has_changes = false;
                        self.auto_typing_job = None;
                        self.auto_typing_debug_visual = None;
                    }
                    Err(err) => {
                        self.loading_text_images_dir = None;
                        self.loaded_text_images_dir = None;
                        self.overlays.clear();
                        self.pending_upload_indices.clear();
                        self.pending_upload_set.clear();
                        self.export_rx = None;
                        self.export_status = TypingExportUiStatus::Hidden;
                        self.last_load_error = Some(err);
                        self.cancel_active_edit_overlay_render();
                        self.edit_render_data_dirty = false;
                        self.last_selected_overlay_idx = None;
                        self.selected_overlay_idx = None;
                        self.transform_mode_overlay_idx = None;
                        self.drag_state = None;
                        self.drag_has_changes = false;
                        self.auto_typing_job = None;
                        self.auto_typing_debug_visual = None;
                    }
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.loading_rx = None;
                self.loading_project_dir = None;
                self.loading_text_images_dir = None;
                self.loaded_text_images_dir = None;
                self.last_load_error =
                    Some("Не удалось получить результат загрузки text_info.json.".to_string());
                self.cancel_active_edit_overlay_render();
                self.edit_render_data_dirty = false;
                self.last_selected_overlay_idx = None;
                self.selected_overlay_idx = None;
                self.transform_mode_overlay_idx = None;
                self.drag_state = None;
                self.drag_has_changes = false;
                self.auto_typing_job = None;
                self.auto_typing_debug_visual = None;
                self.pending_upload_indices.clear();
                self.pending_upload_set.clear();
                self.export_rx = None;
                self.export_status = TypingExportUiStatus::Hidden;
                true
            }
        }
    }

    fn poll_create_overlay_jobs(&mut self, ctx: &egui::Context) -> bool {
        let recv_result = {
            let Some(state) = self.create_render_state.as_ref() else {
                return false;
            };
            match state.rx.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    "Фоновый рендер текста завершился с ошибкой канала.".to_string(),
                )),
            }
        };

        let Some(recv_result) = recv_result else {
            return false;
        };
        self.create_render_state = None;

        match recv_result {
            Ok(Ok(decoded)) => {
                if !decoded.warnings.is_empty() {
                    self.set_create_warning(ctx, decoded.warnings.join("; "));
                }
                self.insert_runtime_overlay(decoded);
                self.request_overlay_placement_save();
                true
            }
            Ok(Err(err)) | Err(err) => {
                self.set_create_error(ctx, err);
                true
            }
        }
    }

    fn poll_edit_overlay_jobs(&mut self, ctx: &egui::Context) -> bool {
        let recv_result = {
            let Some(rx) = self.edit_render_rx.as_ref() else {
                return false;
            };
            match rx.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    "Фоновый рендер редактирования оверлея завершился с ошибкой канала."
                        .to_string(),
                )),
            }
        };
        let Some(recv_result) = recv_result else {
            return false;
        };

        self.edit_render_rx = None;
        let mut repainted = false;
        match recv_result {
            Ok(Ok(Some(result))) => {
                if !result.warnings.is_empty() {
                    self.set_create_warning(ctx, result.warnings.join("; "));
                }
                repainted |= self.apply_edit_overlay_render_result(result);
            }
            Ok(Ok(None)) => {}
            Ok(Err(err)) | Err(err) => {
                self.set_create_error(ctx, err);
                repainted = true;
            }
        }

        if self.edit_render_rx.is_none()
            && self.save_requested_while_busy
            && self.save_rx.is_none()
            && self.create_render_state.is_none()
        {
            self.save_requested_while_busy = false;
            self.spawn_overlay_placement_save();
            repainted = true;
        }

        repainted
    }

    fn apply_edit_overlay_render_result(&mut self, result: TypingEditOverlayResult) -> bool {
        if self.edit_render_latest_token.load(Ordering::Acquire) != result.token {
            return false;
        }
        {
            let Some(overlay) = self.overlays.get_mut(result.overlay_idx) else {
                return false;
            };
            if overlay.file_name != result.file_name {
                return false;
            }

            overlay.user_scale = result.user_scale.clamp(0.05, 20.0);
            overlay.angle_deg = normalize_angle_deg(result.rotation_deg);
            overlay.render_data_json = Some(result.render_data_json);
            overlay.size_px = result.size_px;
            overlay.source_rgba = result.rgba;
        }
        self.mark_overlay_pixels_dirty(result.overlay_idx);
        self.edit_render_data_dirty = true;
        true
    }

    fn queue_selected_overlay_edit_request(
        &mut self,
        ctx: &egui::Context,
        request: TypingOverlayEditRequest,
    ) {
        let Some(selected_idx) = self.selected_overlay_idx else {
            return;
        };
        match request {
            TypingOverlayEditRequest::ImageTransform {
                overlay_idx,
                user_scale,
                rotation_deg,
            } => {
                if selected_idx != overlay_idx {
                    return;
                }
                {
                    let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
                        return;
                    };
                    if overlay.kind != TypingOverlayKind::Image {
                        return;
                    }
                    overlay.user_scale = user_scale.clamp(0.05, 20.0);
                    overlay.angle_deg = normalize_angle_deg(rotation_deg);
                }
                self.mark_overlay_geometry_changed(overlay_idx, false);
                self.request_overlay_placement_save();
            }
            TypingOverlayEditRequest::Text {
                overlay_idx,
                render_params,
                render_data_json,
                user_scale,
                rotation_deg,
            } => {
                let render_params = *render_params;
                // Re-render writes to the unsaved staging dir.
                let Some(text_images_dir) = self.text_images_save_dir.clone() else {
                    self.set_create_error(
                        ctx,
                        "Не найдена папка text_images для редактирования оверлея.",
                    );
                    return;
                };
                if selected_idx != overlay_idx {
                    return;
                }
                let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
                    return;
                };
                if overlay.kind != TypingOverlayKind::Text {
                    return;
                }
                overlay.user_scale = user_scale.clamp(0.05, 20.0);
                overlay.angle_deg = normalize_angle_deg(rotation_deg);

                let edit_request = TypingEditOverlayRequest {
                    token: 0,
                    latest_token: Arc::clone(&self.edit_render_latest_token),
                    overlay_idx,
                    file_name: overlay.file_name.clone(),
                    text_images_dir,
                    user_scale: overlay.user_scale,
                    rotation_deg: overlay.angle_deg,
                    render_params,
                    render_data_json,
                };

                self.start_edit_overlay_render_job(edit_request);
            }
        }
    }

    fn start_edit_overlay_render_job(&mut self, mut request: TypingEditOverlayRequest) {
        request.token = self.next_edit_render_token();
        let (tx, rx) = mpsc::channel::<Result<Option<TypingEditOverlayResult>, String>>();
        thread::spawn(move || {
            let result = render_and_store_edited_overlay(request);
            let _ = tx.send(result);
        });
        self.edit_render_rx = Some(rx);
    }

    fn start_shape_variant_preview_if_available(
        &mut self,
        ctx: &egui::Context,
        overlay_idx: usize,
        origin: Pos2,
    ) {
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            self.shape_variant_preview = None;
            return;
        };
        let Some(render_data_json) = overlay.render_data_json.as_ref() else {
            self.shape_variant_preview = None;
            return;
        };
        let Some(base_params) = text_render_params_from_render_data(render_data_json) else {
            self.shape_variant_preview = None;
            return;
        };
        let overlay_kind = overlay.kind;
        let overlay_size_px = overlay.size_px;
        if !shape_variant_preview_available(overlay_kind) {
            self.shape_variant_preview = None;
            return;
        }

        let variants = build_shape_variant_grid(&base_params);
        let dark_checkerboard = use_dark_shape_variant_checkerboard(base_params.text_color);
        let menu_id = self.next_shape_variant_preview_id();
        let cancel_render = Arc::new(AtomicBool::new(false));
        let worker_cancel_render = Arc::clone(&cancel_render);
        let (tx, rx) = mpsc::channel::<Result<TypingShapeVariantPreviewResult, String>>();
        thread::spawn(move || {
            if worker_cancel_render.load(Ordering::Relaxed) {
                return;
            }
            let tiles =
                render_shape_variant_preview_tiles(base_params, variants, &worker_cancel_render);
            if worker_cancel_render.load(Ordering::Relaxed) {
                return;
            }
            let _ = tx.send(Ok(TypingShapeVariantPreviewResult { menu_id, tiles }));
        });

        let slot_size = shape_variant_slot_size(overlay_size_px);
        let screen_rect = ctx.content_rect();
        self.shape_variant_preview = Some(TypingShapeVariantPreviewState {
            menu_id,
            overlay_idx,
            origin,
            menu_rect: None,
            place_above: origin.y >= screen_rect.center().y,
            dark_checkerboard,
            slot_size,
            gap_px: TEXT_SHAPE_VARIANT_TILE_GAP_PX,
            padding_px: TEXT_SHAPE_VARIANT_PANEL_PADDING_PX,
            cancel_render,
            rx,
            tiles: None,
        });
    }

    fn poll_shape_variant_preview(&mut self, ctx: &egui::Context) {
        if !ctx.is_popup_open() {
            self.shape_variant_preview = None;
            return;
        }
        let Some(state) = self.shape_variant_preview.as_mut() else {
            return;
        };
        let Ok(message) = state.rx.try_recv() else {
            return;
        };
        match message {
            Ok(result) if result.menu_id == state.menu_id => {
                state.tiles = Some(result.tiles);
                ctx.request_repaint();
            }
            Ok(_) => {}
            Err(err) => {
                eprintln!(
                    "ERROR typing::shape_variant_preview overlay_idx={} err={}",
                    state.overlay_idx, err
                );
                self.shape_variant_preview = None;
            }
        }
    }

    fn update_shape_variant_preview_menu_rect(&mut self, overlay_idx: usize, menu_rect: Rect) {
        let Some(state) = self.shape_variant_preview.as_mut() else {
            return;
        };
        if state.overlay_idx == overlay_idx {
            state.menu_rect = Some(menu_rect);
        }
    }

    fn draw_shape_variant_preview(&mut self, ctx: &egui::Context) -> Option<TypingShapeVariant> {
        if !ctx.is_popup_open() {
            self.shape_variant_preview = None;
            return None;
        }
        let state = self.shape_variant_preview.as_mut()?;
        if self.selected_overlay_idx != Some(state.overlay_idx) {
            self.shape_variant_preview = None;
            return None;
        }
        let tiles = state.tiles.as_mut()?;
        if tiles.is_empty() {
            return None;
        }

        for tile in tiles.iter_mut().filter(|tile| tile.texture.is_none()) {
            let Some(rgba) = tile.rgba.take() else {
                continue;
            };
            let image = ColorImage::from_rgba_unmultiplied(tile.size_px, rgba.as_slice());
            tile.texture = Some(ctx.load_texture(
                format!(
                    "typing_shape_variant_{}_{}_{}",
                    state.menu_id, tile.variant.row, tile.variant.col
                ),
                image,
                TextureOptions::LINEAR,
            ));
        }

        let panel_size = shape_variant_panel_size(state.slot_size, state.gap_px, state.padding_px);
        let screen_rect = ctx.content_rect();
        let anchor_rect = state
            .menu_rect
            .unwrap_or_else(|| Rect::from_min_size(state.origin, Vec2::ZERO));
        let mut pos =
            shape_variant_panel_pos(anchor_rect, panel_size, screen_rect, state.place_above);
        pos.x = pos.x.clamp(
            screen_rect.left(),
            (screen_rect.right() - panel_size.x).max(screen_rect.left()),
        );
        pos.y = pos.y.clamp(
            screen_rect.top(),
            (screen_rect.bottom() - panel_size.y).max(screen_rect.top()),
        );

        let mut clicked_variant = None;
        egui::Area::new(Id::new(("typing_shape_variant_preview", state.menu_id)))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                ui.set_min_size(panel_size);
                let panel_rect = Rect::from_min_size(ui.min_rect().min, panel_size);
                paint_shape_variant_checkerboard(
                    ui.painter(),
                    panel_rect,
                    8.0,
                    state.dark_checkerboard,
                );

                for tile in tiles.iter() {
                    let Some(texture) = tile.texture.as_ref() else {
                        continue;
                    };
                    let slot_min = Pos2::new(
                        panel_rect.left()
                            + state.padding_px
                            + tile.variant.col as f32 * (state.slot_size.x + state.gap_px),
                        panel_rect.top()
                            + state.padding_px
                            + tile.variant.row as f32 * (state.slot_size.y + state.gap_px),
                    );
                    let slot_rect = Rect::from_min_size(slot_min, state.slot_size);
                    let response = ui.interact(
                        slot_rect,
                        Id::new((
                            "typing_shape_variant_tile",
                            state.menu_id,
                            tile.variant.row,
                            tile.variant.col,
                        )),
                        Sense::click(),
                    );
                    let scale = if response.hovered() { 1.06 } else { 1.0 };
                    let draw_size = fit_size_to_box(tile.size_px, state.slot_size * scale);
                    let draw_rect = Rect::from_center_size(slot_rect.center(), draw_size);
                    ui.painter().image(
                        texture.id(),
                        draw_rect,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    if response.hovered() {
                        ui.painter().rect_stroke(
                            draw_rect.expand(3.0),
                            6.0,
                            Stroke::new(2.0, Color32::WHITE),
                            egui::StrokeKind::Outside,
                        );
                    }
                    if response.clicked() {
                        clicked_variant = Some(tile.variant.clone());
                    }
                }
            });

        clicked_variant
    }

    fn apply_shape_variant_to_overlay(&mut self, ctx: &egui::Context, variant: TypingShapeVariant) {
        let Some(overlay_idx) = self.selected_overlay_idx else {
            return;
        };
        let Some(text_images_dir) = self.text_images_save_dir.clone() else {
            self.set_create_error(
                ctx,
                "Не найдена папка text_images для редактирования оверлея.",
            );
            return;
        };
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            return;
        };
        if overlay.kind != TypingOverlayKind::Text {
            return;
        }
        let Some(current_render_data) = overlay.render_data_json.as_ref() else {
            return;
        };
        let Some((render_params, render_data_json)) =
            build_shape_variant_apply_payload(current_render_data, &variant)
        else {
            return;
        };

        let edit_request = TypingEditOverlayRequest {
            token: 0,
            latest_token: Arc::clone(&self.edit_render_latest_token),
            overlay_idx,
            file_name: overlay.file_name.clone(),
            text_images_dir,
            user_scale: overlay.user_scale,
            rotation_deg: overlay.angle_deg,
            render_params,
            render_data_json,
        };
        self.shape_variant_preview = None;
        self.start_edit_overlay_render_job(edit_request);
    }

    fn poll_save_jobs(&mut self, ctx: &egui::Context) -> bool {
        let recv_result = {
            let Some(rx) = self.save_rx.as_ref() else {
                return false;
            };
            match rx.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    "Фоновое сохранение text_info.json завершилось с ошибкой канала.".to_string(),
                )),
            }
        };

        let Some(recv_result) = recv_result else {
            return false;
        };

        self.save_rx = None;
        match recv_result {
            Ok(Ok(())) => {}
            Ok(Err(err)) | Err(err) => self.set_create_error(ctx, err),
        }

        if self.save_requested_while_busy {
            self.save_requested_while_busy = false;
            self.spawn_overlay_placement_save();
        }
        true
    }

    fn poll_export_jobs(&mut self, ctx: &egui::Context) -> bool {
        let Some(state) = self.export_rx.as_ref() else {
            return false;
        };
        let mut changed = false;
        loop {
            match state.rx.try_recv() {
                Ok(TypingExportEvent::Progress { done, total }) => {
                    self.export_status = TypingExportUiStatus::Running { done, total };
                    changed = true;
                }
                Ok(TypingExportEvent::Finished(result)) => {
                    self.export_rx = None;
                    match result {
                        Ok(result) => {
                            self.create_status_error = None;
                            self.export_status = TypingExportUiStatus::Success {
                                done: result.exported,
                                total: result.total,
                            };
                            let _ = result.output_dir;
                        }
                        Err(err) => {
                            self.export_status = TypingExportUiStatus::Error {
                                message: err.clone(),
                            };
                            self.set_create_error(ctx, err);
                        }
                    }
                    changed = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.export_rx = None;
                    let err = "Фоновый экспорт завершился с ошибкой канала.".to_string();
                    self.export_status = TypingExportUiStatus::Error {
                        message: err.clone(),
                    };
                    self.set_create_error(ctx, err);
                    changed = true;
                    break;
                }
            }
        }
        changed
    }

    fn request_export_to_folder(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        masks_snapshot: HashMap<usize, TypingMaskExportPage>,
        output_dir: PathBuf,
    ) {
        if self.export_rx.is_some() {
            self.set_create_error(ctx, "Экспорт уже выполняется.");
            return;
        }
        if project.pages.is_empty() {
            self.set_create_error(ctx, "В проекте нет страниц для экспорта.");
            return;
        }
        let clean_overlays_model = self.clean_overlays_model.clone();

        let mut overlays_by_page = HashMap::<usize, Vec<TypingExportOverlaySnapshot>>::new();
        for overlay in &self.overlays {
            if overlay.size_px[0] == 0 || overlay.size_px[1] == 0 {
                continue;
            }
            if overlay.source_rgba.len() != overlay.size_px[0] * overlay.size_px[1] * 4 {
                continue;
            }
            overlays_by_page.entry(overlay.page_idx).or_default().push(
                TypingExportOverlaySnapshot {
                    page_idx: overlay.page_idx,
                    center_page_px: overlay.center_page_px,
                    mask_clip_enabled: overlay.mask_clip_enabled,
                    user_scale: overlay.user_scale,
                    angle_deg: overlay.angle_deg,
                    deform_mesh: overlay.deform_mesh.clone(),
                    size_px: overlay.size_px,
                    source_rgba: overlay.source_rgba.clone(),
                },
            );
        }

        let jobs = project
            .pages
            .iter()
            .map(|page| {
                let stem = page
                    .path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("page");
                let clean_overlay_path = project.paths.clean_layers_dir.join(format!("{stem}.png"));
                let out_name = format!("{stem}.png");
                TypingExportPageJob {
                    page_idx: page.idx,
                    page_path: page.path.clone(),
                    output_path: output_dir.join(out_name),
                    clean_overlay_path: clean_overlay_path.is_file().then_some(clean_overlay_path),
                    clean_overlay_rgba: None,
                    overlays: overlays_by_page.remove(&page.idx).unwrap_or_default(),
                    mask: masks_snapshot.get(&page.idx).cloned(),
                }
            })
            .collect::<Vec<_>>();
        let total_pages = jobs.len();
        self.export_status = TypingExportUiStatus::Running {
            done: 0,
            total: total_pages,
        };
        let (tx, rx) = mpsc::channel::<TypingExportEvent>();
        thread::spawn(move || {
            let result =
                export_typing_pages_to_folder(jobs, output_dir, clean_overlays_model, tx.clone());
            let _ = tx.send(TypingExportEvent::Finished(result));
        });
        self.export_rx = Some(TypingExportRenderState { rx });
    }

    fn export_status_for_ui(&self) -> TypingExportUiStatus {
        self.export_status.clone()
    }

    fn request_overlay_placement_save(&mut self) {
        if self.save_rx.is_some()
            || self.create_render_state.is_some()
            || self.edit_render_rx.is_some()
        {
            self.save_requested_while_busy = true;
            return;
        }
        self.spawn_overlay_placement_save();
    }

    fn spawn_overlay_placement_save(&mut self) {
        // Placement saves always go to the unsaved staging dir.
        let Some(text_images_dir) = self.text_images_save_dir.clone() else {
            return;
        };

        let snapshot = self
            .overlays
            .iter()
            .map(|overlay| {
                let render_data = if overlay.kind == TypingOverlayKind::Text {
                    Some(overlay.render_data_json.clone().unwrap_or_else(|| {
                        default_render_data_for_text("", overlay.size_px[0].max(1) as u32)
                    }))
                } else {
                    None
                };
                build_storage_overlay_entry(
                    overlay.kind,
                    overlay.page_idx,
                    overlay.file_name.as_str(),
                    overlay.center_page_px,
                    overlay.mask_clip_enabled,
                    overlay.angle_deg,
                    overlay.user_scale,
                    overlay.deform_mesh.clone(),
                    render_data,
                )
            })
            .collect::<Vec<_>>();

        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        thread::spawn(move || {
            let result = write_overlay_items_to_text_info(&text_images_dir, &snapshot);
            let _ = tx.send(result);
        });
        self.save_rx = Some(rx);
    }

    fn round_all_overlay_positions_to_pixels(&mut self) {
        let mut changed_indices = Vec::new();
        for (idx, overlay) in self.overlays.iter_mut().enumerate() {
            let previous_center = overlay.center_page_px;
            overlay.center_page_px = [
                overlay.center_page_px[0].round(),
                overlay.center_page_px[1].round(),
            ];
            if overlay.center_page_px != previous_center {
                changed_indices.push(idx);
            }
        }
        if changed_indices.is_empty() {
            return;
        }
        for idx in changed_indices {
            self.mark_overlay_geometry_changed(idx, false);
        }
        self.request_overlay_placement_save();
    }

    fn wants_canvas_shift_drag_selection(&self, ctx: &egui::Context) -> bool {
        self.create_selection.is_some()
            || self.create_editor.is_some()
            || self.create_render_state.is_some()
            || ctx.input(|i| i.modifiers.shift)
    }

    fn draw_create_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        canvas: &CanvasView,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
    ) {
        let now_s = ctx.input(|i| i.time);
        if self
            .create_status_error
            .as_ref()
            .is_some_and(|(_, hide_at)| now_s >= *hide_at)
        {
            self.create_status_error = None;
        }
        if self
            .create_status_warning
            .as_ref()
            .is_some_and(|(_, hide_at)| now_s >= *hide_at)
        {
            self.create_status_warning = None;
        }

        self.capture_shift_drag_selection(ctx, canvas_rect, canvas, project, top_panel);
        self.draw_active_shift_selection(ctx);
        self.draw_text_editor(ctx, project, top_panel);
        self.draw_render_inflight_hint(ctx);
        self.draw_status_error(ctx, canvas_rect);
        self.draw_status_warning(ctx, canvas_rect);
    }

    fn capture_shift_drag_selection(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        canvas: &CanvasView,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
    ) {
        if self.loading_rx.is_some()
            || self.create_editor.is_some()
            || self.create_render_state.is_some()
        {
            return;
        }
        let shift_down = ctx.input(|i| i.modifiers.shift);
        let selection_active = self.create_selection.is_some();
        if !shift_down && !selection_active {
            return;
        }

        egui::Area::new("typing_text_create_shift_capture".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.min)
            .show(ctx, |ui| {
                ui.set_min_size(canvas_rect.size());
                let local_rect = Rect::from_min_size(Pos2::ZERO, canvas_rect.size());
                let sense = if shift_down {
                    egui::Sense::click_and_drag()
                } else {
                    egui::Sense::hover()
                };
                let response =
                    ui.interact(local_rect, ui.id().with("typing_text_shift_drag"), sense);

                if shift_down
                    && response.drag_started()
                    && let Some(pos) = response.interact_pointer_pos()
                    && contains_any_page(canvas, project, pos)
                {
                    self.create_selection = Some(TypingCreateSelection {
                        start: pos,
                        current: pos,
                    });
                }

                if let Some(selection) = self.create_selection.as_mut()
                    && let Some(pos) = ctx.input(|i| i.pointer.latest_pos())
                {
                    selection.current = pos;
                }

                let should_finish =
                    self.create_selection.is_some() && (response.drag_stopped() || !shift_down);
                if should_finish && let Some(selection) = self.create_selection.take() {
                    let rect = selection.rect();
                    if rect.width() >= TEXT_CREATE_SELECTION_MIN_SIDE_PX
                        && rect.height() >= TEXT_CREATE_SELECTION_MIN_SIDE_PX
                    {
                        self.open_text_editor_for_selection(ctx, canvas, project, top_panel, rect);
                    }
                }
            });
    }

    fn draw_active_shift_selection(&self, ctx: &egui::Context) {
        let Some(selection) = self.create_selection else {
            return;
        };
        let rect = selection.rect();
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("typing_text_shift_selection_painter"),
        ));
        painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(245, 210, 60, 52));
        painter.rect_stroke(
            rect,
            0.0,
            Stroke::new(2.0, Color32::from_rgb(245, 210, 60)),
            egui::StrokeKind::Outside,
        );
    }

    fn open_text_editor_for_selection(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
        scene_selection_rect: Rect,
    ) {
        let Some((page_idx, page_rect, scene_rect)) =
            resolve_selection_to_page(canvas, project, scene_selection_rect)
        else {
            self.set_create_error(
                ctx,
                "Выделение должно пересекать хотя бы одну страницу холста.",
            );
            return;
        };

        let width_px = selection_width_in_source_px(canvas, page_idx, page_rect, scene_rect);
        if width_px == 0 {
            self.set_create_error(ctx, "Не удалось определить ширину выделения в пикселях.");
            return;
        }

        let center_page_px = selection_center_page_px(page_rect, scene_rect, canvas.zoom());
        let seed_text = pick_bubble_text_for_selection(project, page_idx, scene_rect, page_rect)
            .unwrap_or_default();

        let mut font_family = None;
        let mut font_size_px = 24.0;
        if let Some(spec) = top_panel.create_editor_font_spec() {
            font_family = self.ensure_editor_font(ctx, &spec);
            font_size_px = spec.ui_font_size_px.clamp(8.0, 128.0);
        }

        self.create_editor = Some(TypingCreateTextEditor {
            page_idx,
            scene_rect,
            center_page_px,
            width_px,
            text: seed_text,
            font_family,
            font_size_px,
            needs_focus: true,
            window_focused_last_frame: ctx.input(|input| input.viewport().focused.unwrap_or(true)),
        });
        self.create_status_error = None;
    }

    fn ensure_editor_font(
        &mut self,
        ctx: &egui::Context,
        spec: &TypingEditorFontSpec,
    ) -> Option<egui::FontFamily> {
        let cache_key = (spec.font_path.clone(), spec.face_index);
        if let Some(name) = self.editor_font_cache.get(&cache_key) {
            return Some(egui::FontFamily::Name(name.clone().into()));
        }

        let font_bytes = fs::read(&spec.font_path).ok()?;
        self.editor_font_next_id = self.editor_font_next_id.saturating_add(1);
        let font_name = format!("typing-editor-font-{}", self.editor_font_next_id);
        let mut font_data = egui::FontData::from_owned(font_bytes);
        font_data.index = spec.face_index as u32;
        ctx.add_font(egui::epaint::text::FontInsert::new(
            font_name.as_str(),
            font_data,
            vec![egui::epaint::text::InsertFontFamily {
                family: egui::FontFamily::Name(font_name.clone().into()),
                priority: egui::epaint::text::FontPriority::Highest,
            }],
        ));
        self.editor_font_cache.insert(cache_key, font_name.clone());
        Some(egui::FontFamily::Name(font_name.into()))
    }

    fn draw_text_editor(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
    ) {
        if self.create_editor.is_none() {
            return;
        }

        let editor_rect = {
            let editor = self.create_editor.as_mut().expect("checked above");
            let desired_rect = Rect::from_min_size(
                editor.scene_rect.min,
                egui::vec2(
                    editor.scene_rect.width().max(TEXT_EDITOR_MIN_WIDTH_PX),
                    editor.scene_rect.height().max(TEXT_EDITOR_MIN_HEIGHT_PX),
                ),
            );
            let text_edit_id = Id::new((
                "typing_text_editor_input",
                editor.page_idx,
                editor.scene_rect.min.x.to_bits(),
                editor.scene_rect.min.y.to_bits(),
            ));
            let area_response = egui::Area::new(Id::new((
                "typing_text_editor_area",
                editor.page_idx,
                editor.scene_rect.min.x.to_bits(),
                editor.scene_rect.min.y.to_bits(),
            )))
            .order(egui::Order::Foreground)
            .fixed_pos(desired_rect.min)
            .show(ctx, |ui| {
                ui.set_min_size(desired_rect.size());
                ui.set_max_size(desired_rect.size());
                egui::Frame::popup(ui.style())
                    .stroke(Stroke::new(1.0, Color32::from_rgb(235, 200, 85)))
                    .show(ui, |ui| {
                        ui.set_min_size(desired_rect.size());
                        let family = editor
                            .font_family
                            .clone()
                            .filter(|family| is_font_family_bound(ctx, family))
                            .unwrap_or(egui::FontFamily::Proportional);
                        let edit = egui::TextEdit::multiline(&mut editor.text)
                            .id(text_edit_id)
                            .font(egui::FontId::new(editor.font_size_px, family))
                            .desired_width(f32::INFINITY)
                            .desired_rows(1)
                            .lock_focus(true)
                            .frame(false);
                        let output = edit.show(ui);
                        let viewport_focused =
                            ctx.input(|input| input.viewport().focused.unwrap_or(true));
                        let clicked_inside_editor = ctx.input(|input| {
                            input.pointer.primary_clicked()
                                && input
                                    .pointer
                                    .interact_pos()
                                    .is_some_and(|pos| desired_rect.contains(pos))
                        });
                        if editor.needs_focus
                            || (viewport_focused && !editor.window_focused_last_frame)
                            || (clicked_inside_editor && !output.response.has_focus())
                        {
                            output.response.request_focus();
                            editor.needs_focus = false;
                        }
                        editor.window_focused_last_frame = viewport_focused;
                    });
            });
            area_response.response.rect
        };

        let clicked_outside = ctx.input(|i| {
            i.pointer.primary_clicked()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|pos| !editor_rect.contains(pos))
        });
        if clicked_outside && let Some(finished_editor) = self.create_editor.take() {
            self.start_create_overlay_render(ctx, project, top_panel, finished_editor);
        }
    }

    fn start_create_overlay_render(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
        editor: TypingCreateTextEditor,
    ) {
        if editor.text.trim().is_empty() {
            self.create_status_error = None;
            return;
        }

        let (render_params, render_data_json) =
            match top_panel.build_create_text_render_bundle(editor.text.clone(), editor.width_px) {
                Ok(bundle) => bundle,
                Err(err) => {
                    self.set_create_error(ctx, err);
                    return;
                }
            };

        let request = TypingCreateOverlayRequest {
            text_images_dir: project.paths.unsaved_text_images_dir.clone(),
            page_idx: editor.page_idx,
            center_page_px: editor.center_page_px,
            render_params,
            render_data_json,
        };
        let (tx, rx) = mpsc::channel::<Result<TypingOverlayDecoded, String>>();
        thread::spawn(move || {
            let result = render_and_store_created_overlay(request);
            let _ = tx.send(result);
        });
        self.create_render_state = Some(TypingCreateRenderState {
            rx,
            scene_rect: Some(editor.scene_rect),
        });
        self.create_status_error = None;
    }

    fn request_create_image_overlay(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        page_idx: usize,
        center_page_px: [f32; 2],
        request: TypingCreateImageRequest,
    ) {
        if self.loading_rx.is_some() || self.create_render_state.is_some() {
            self.set_create_error(ctx, "Сначала дождитесь завершения текущей операции.");
            return;
        }
        if project.pages.is_empty() {
            self.set_create_error(ctx, "В проекте нет страниц.");
            return;
        }
        let target_page_idx = page_idx.min(project.pages.len().saturating_sub(1));
        let source = match request {
            TypingCreateImageRequest::FromClipboard => TypingCreateImageSource::Clipboard,
            TypingCreateImageRequest::FromFile(path) => TypingCreateImageSource::File(path),
        };
        let create_request = TypingCreateImageOverlayRequest {
            text_images_dir: project.paths.unsaved_text_images_dir.clone(),
            page_idx: target_page_idx,
            center_page_px,
            source,
        };
        let (tx, rx) = mpsc::channel::<Result<TypingOverlayDecoded, String>>();
        thread::spawn(move || {
            let result = render_and_store_created_image_overlay(create_request);
            let _ = tx.send(result);
        });
        self.create_render_state = Some(TypingCreateRenderState {
            rx,
            scene_rect: None,
        });
        self.create_status_error = None;
    }

    fn draw_render_inflight_hint(&self, ctx: &egui::Context) {
        let Some(state) = self.create_render_state.as_ref() else {
            return;
        };
        let Some(scene_rect) = state.scene_rect else {
            return;
        };
        let hint_pos = scene_rect.center() - egui::vec2(76.0, 18.0);
        egui::Area::new("typing_text_editor_render_hint".into())
            .order(egui::Order::Foreground)
            .fixed_pos(hint_pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Рендер текста...");
                    });
                });
            });
    }

    fn draw_status_error(&self, ctx: &egui::Context, canvas_rect: Rect) {
        let Some((message, _)) = self.create_status_error.as_ref() else {
            return;
        };
        egui::Area::new("typing_text_editor_error".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.center_top() + egui::vec2(-220.0, 16.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .stroke(Stroke::new(1.0, Color32::from_rgb(240, 110, 110)))
                    .show(ui, |ui| {
                        ui.colored_label(Color32::from_rgb(240, 110, 110), message);
                    });
            });
    }

    fn draw_status_warning(&self, ctx: &egui::Context, canvas_rect: Rect) {
        let Some((message, _)) = self.create_status_warning.as_ref() else {
            return;
        };
        egui::Area::new("typing_text_editor_warning".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.center_top() + egui::vec2(-220.0, 52.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .stroke(Stroke::new(1.0, Color32::from_rgb(232, 188, 66)))
                    .show(ui, |ui| {
                        ui.colored_label(Color32::from_rgb(232, 188, 66), message);
                    });
            });
    }

    fn set_create_error(&mut self, ctx: &egui::Context, message: impl Into<String>) {
        let now_s = ctx.input(|i| i.time);
        self.create_status_error = Some((message.into(), now_s + TEXT_EDITOR_STATUS_ERROR_SECONDS));
    }

    fn set_create_warning(&mut self, ctx: &egui::Context, message: impl Into<String>) {
        let now_s = ctx.input(|i| i.time);
        self.create_status_warning =
            Some((message.into(), now_s + TEXT_EDITOR_STATUS_ERROR_SECONDS));
    }

    fn insert_runtime_overlay(&mut self, decoded: TypingOverlayDecoded) {
        let idx = self.overlays.len();
        self.overlays.push(TypingOverlayRuntime {
            kind: decoded.kind,
            page_idx: decoded.page_idx,
            center_page_px: decoded.center_page_px,
            mask_clip_enabled: decoded.mask_clip_enabled,
            user_scale: decoded.user_scale,
            angle_deg: decoded.angle_deg,
            deform_mesh: decoded.deform_mesh,
            file_name: decoded.file_name,
            render_data_json: decoded.render_data_json,
            size_px: decoded.size_px,
            source_rgba: decoded.rgba,
            texture: None,
            display_texture_stale: true,
            last_texture_used_frame: 0,
        });
        self.queue_overlay_texture_upload(idx);
        self.selected_overlay_idx = Some(idx);
        self.transform_mode_overlay_idx = None;
        self.drag_state = None;
    }

    fn upload_pending_textures(
        &mut self,
        ctx: &egui::Context,
        mask_layer: &TypingMaskLayer,
    ) -> bool {
        let mut uploaded_any = false;
        let mut uploaded_textures = 0usize;
        let mut uploaded_bytes = 0usize;

        while uploaded_textures < TEXT_OVERLAY_UPLOAD_TEXTURE_BUDGET_PER_FRAME
            && uploaded_bytes < TEXT_OVERLAY_UPLOAD_BYTES_BUDGET_PER_FRAME
        {
            let Some(idx) = self.pending_upload_indices.pop_front() else {
                break;
            };
            self.pending_upload_set.remove(&idx);
            let Some(overlay) = self.overlays.get_mut(idx) else {
                continue;
            };
            if overlay.texture.is_some() && !overlay.display_texture_stale {
                continue;
            }
            if overlay.source_rgba.is_empty() {
                continue;
            };
            if overlay.size_px[0] == 0 || overlay.size_px[1] == 0 {
                continue;
            }
            if overlay.source_rgba.len() != overlay.size_px[0] * overlay.size_px[1] * 4 {
                continue;
            }

            let display_rgba = if overlay.mask_clip_enabled {
                if let Some(page_size) = mask_layer.page_mask_size(overlay.page_idx) {
                    let deform_mesh = overlay_deform_mesh_for_page(overlay, page_size);
                    let deform_mesh_points_uv = deform_mesh
                        .points_px
                        .iter()
                        .map(|&point| page_px_to_uv(point, page_size))
                        .collect::<Vec<_>>();
                    mask_layer
                        .clip_overlay_rgba_if_needed(
                            overlay.page_idx,
                            overlay.size_px,
                            &overlay.source_rgba,
                            deform_mesh.cols,
                            deform_mesh.rows,
                            deform_mesh_points_uv.as_slice(),
                        )
                        .unwrap_or_else(|| overlay.source_rgba.clone())
                } else {
                    overlay.source_rgba.clone()
                }
            } else {
                overlay.source_rgba.clone()
            };

            let image = egui::ColorImage::from_rgba_unmultiplied(
                [overlay.size_px[0], overlay.size_px[1]],
                &display_rgba,
            );
            if let Some(texture) = overlay.texture.as_mut() {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                let texture = ctx.load_texture(
                    format!(
                        "typing-text-overlay-{}-{}-{}",
                        overlay.page_idx, idx, overlay.file_name
                    ),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                overlay.texture = Some(texture);
            }
            overlay.display_texture_stale = false;

            uploaded_any = true;
            uploaded_textures += 1;
            uploaded_bytes += display_rgba.len();
        }

        uploaded_any
    }

    fn ensure_overlay_deform_mesh(
        &mut self,
        overlay_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) -> bool {
        let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
            return false;
        };
        let page_size = page_size_from_image_rect(image_rect, zoom);
        if overlay.deform_mesh.is_none() {
            overlay.deform_mesh = Some(default_overlay_deform_mesh(overlay, image_rect, zoom));
        } else if let Some(mesh) = overlay.deform_mesh.as_ref() {
            let normalized = normalize_deform_mesh_resolution(mesh, page_size);
            if &normalized != mesh {
                overlay.deform_mesh = Some(normalized);
            }
        }
        sync_overlay_center_from_deform_mesh(overlay, page_size);
        true
    }

    fn queue_overlay_texture_upload(&mut self, idx: usize) {
        if idx >= self.overlays.len() {
            return;
        }
        if self.pending_upload_set.insert(idx) {
            self.pending_upload_indices.push_back(idx);
        }
    }

    fn mark_overlay_pixels_dirty(&mut self, idx: usize) {
        if let Some(overlay) = self.overlays.get_mut(idx) {
            overlay.display_texture_stale = true;
        } else {
            return;
        }
        self.queue_overlay_texture_upload(idx);
    }

    fn mark_overlay_geometry_changed(&mut self, idx: usize, defer_mask_refresh: bool) {
        let should_refresh = if let Some(overlay) = self.overlays.get_mut(idx) {
            if !overlay.mask_clip_enabled {
                false
            } else {
                overlay.display_texture_stale = true;
                true
            }
        } else {
            return;
        };
        if should_refresh && !defer_mask_refresh {
            self.queue_overlay_texture_upload(idx);
        }
    }

    fn flush_overlay_texture_if_stale(&mut self, idx: usize) {
        if self
            .overlays
            .get(idx)
            .is_some_and(|overlay| overlay.display_texture_stale)
        {
            self.queue_overlay_texture_upload(idx);
        }
    }

    fn mark_page_texture_dirty(&mut self, page_idx: usize) {
        for idx in 0..self.overlays.len() {
            if self.overlays[idx].page_idx == page_idx && self.overlays[idx].mask_clip_enabled {
                self.mark_overlay_pixels_dirty(idx);
            }
        }
    }

    fn clear_selection(&mut self) {
        self.selected_overlay_idx = None;
        self.transform_mode_overlay_idx = None;
        self.drag_state = None;
        self.drag_has_changes = false;
        self.shape_variant_preview = None;
    }

    fn has_selected_overlay(&self) -> bool {
        self.selected_overlay_idx
            .and_then(|idx| self.overlays.get(idx))
            .is_some()
    }

    fn selected_overlay_for_edit(&self) -> Option<TypingSelectedOverlayForEdit> {
        let overlay_idx = self.selected_overlay_idx?;
        let overlay = self.overlays.get(overlay_idx)?;
        let width_px_hint = overlay_render_data_width_hint(
            overlay.render_data_json.as_ref(),
            (overlay.size_px[0] as f32 * overlay.user_scale.max(0.01))
                .round()
                .max(1.0) as u32,
        );
        Some(TypingSelectedOverlayForEdit {
            overlay_idx,
            overlay_kind: overlay.kind,
            render_data_json: overlay.render_data_json.clone(),
            width_px_hint,
            user_scale: overlay.user_scale,
            rotation_deg: overlay.angle_deg,
        })
    }

    fn flush_edit_save_on_selection_change(&mut self) {
        if self.last_selected_overlay_idx == self.selected_overlay_idx {
            return;
        }
        if self.last_selected_overlay_idx.is_some() && self.edit_render_data_dirty {
            self.request_overlay_placement_save();
            self.edit_render_data_dirty = false;
        }
        self.last_selected_overlay_idx = self.selected_overlay_idx;
    }

    fn remove_overlay(&mut self, overlay_idx: usize) {
        if overlay_idx >= self.overlays.len() {
            return;
        }
        self.overlays.remove(overlay_idx);
        self.shape_variant_preview = None;

        self.pending_upload_indices = self
            .pending_upload_indices
            .iter()
            .filter_map(|&idx| {
                if idx == overlay_idx {
                    None
                } else if idx > overlay_idx {
                    Some(idx - 1)
                } else {
                    Some(idx)
                }
            })
            .collect();
        self.pending_upload_set = self.pending_upload_indices.iter().copied().collect();

        shift_index_after_remove(&mut self.selected_overlay_idx, overlay_idx);
        shift_index_after_remove(&mut self.transform_mode_overlay_idx, overlay_idx);
        shift_index_after_remove(&mut self.last_selected_overlay_idx, overlay_idx);
        if let Some(mut drag_state) = self.drag_state.take() {
            if drag_state.overlay_idx == overlay_idx {
                self.drag_state = None;
            } else {
                if drag_state.overlay_idx > overlay_idx {
                    drag_state.overlay_idx -= 1;
                }
                self.drag_state = Some(drag_state);
            }
        }
        if let Some(mut auto_job) = self.auto_typing_job.take() {
            if auto_job.overlay_idx == overlay_idx {
                self.auto_typing_job = None;
            } else {
                if auto_job.overlay_idx > overlay_idx {
                    auto_job.overlay_idx -= 1;
                }
                self.auto_typing_job = Some(auto_job);
            }
        }
        self.drag_has_changes = false;
        self.edit_render_data_dirty = false;
        self.request_overlay_placement_save();
    }

    fn try_rotate_selected_overlay_by_ctrl_wheel(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) {
        let Some(selected_idx) = self.selected_overlay_idx else {
            return;
        };
        if self.transform_mode_overlay_idx == Some(selected_idx) {
            return;
        }

        let Some(selected_overlay) = self.overlays.get(selected_idx) else {
            return;
        };
        if selected_overlay.page_idx != page_idx {
            return;
        }

        let (ctrl_or_command, raw_scroll_delta_y) = ui.ctx().input(|input| {
            (
                input.modifiers.ctrl || input.modifiers.command,
                input.raw_scroll_delta.y,
            )
        });
        if !ctrl_or_command || raw_scroll_delta_y.abs() <= f32::EPSILON {
            return;
        }

        let steps: f32 = if raw_scroll_delta_y > 0.0 { 1.0 } else { -1.0 };
        let delta_deg: f32 = steps * 2.0;
        let delta_rad = delta_deg.to_radians();

        let (start_angle_deg, start_mesh_scene, start_mesh_dims, had_mesh) = {
            let overlay = &self.overlays[selected_idx];
            let geometry = overlay_scene_geometry(overlay, image_rect, zoom);
            (
                overlay.angle_deg,
                geometry.mesh_scene,
                (geometry.mesh_cols, geometry.mesh_rows),
                overlay.deform_mesh.is_some(),
            )
        };

        if let Some(overlay) = self.overlays.get_mut(selected_idx) {
            if had_mesh {
                let center_scene = deform_mesh_center_scene(&start_mesh_scene);
                let rotated_scene = rotate_mesh_scene(&start_mesh_scene, center_scene, delta_rad);
                let page_size = page_size_from_image_rect(image_rect, zoom);
                let rotated_page_px = rotated_scene
                    .into_iter()
                    .map(|scene| page_px_from_scene(image_rect, zoom, scene))
                    .collect::<Vec<_>>();
                overlay.deform_mesh = TypingOverlayDeformMesh::new(
                    start_mesh_dims.0,
                    start_mesh_dims.1,
                    rotated_page_px,
                    page_size,
                );
                sync_overlay_center_from_deform_mesh(overlay, page_size);
            } else {
                overlay.angle_deg = normalize_angle_deg(start_angle_deg + delta_deg);
            }
        }

        ui.ctx().input_mut(|input| {
            input.smooth_scroll_delta = Vec2::ZERO;
            input.raw_scroll_delta = Vec2::ZERO;
        });
        self.mark_overlay_geometry_changed(selected_idx, false);
        self.request_overlay_placement_save();
    }

    fn try_scale_selected_overlay_by_shortcuts(&mut self, ui: &mut egui::Ui, page_idx: usize) {
        // Do not hijack typing in any focused text field.
        if ui.ctx().wants_keyboard_input() {
            return;
        }

        let Some(selected_idx) = self.selected_overlay_idx else {
            return;
        };
        let Some(selected_overlay) = self.overlays.get(selected_idx) else {
            return;
        };
        if selected_overlay.page_idx != page_idx || selected_overlay.deform_mesh.is_some() {
            return;
        }

        let (increase, decrease, reset) = ui.ctx().input_mut(|input| {
            (
                input.consume_key(egui::Modifiers::NONE, egui::Key::Equals)
                    || input.consume_key(egui::Modifiers::NONE, egui::Key::Plus)
                    || input.consume_key(egui::Modifiers::SHIFT, egui::Key::Equals),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Minus),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Num0),
            )
        });

        if !increase && !decrease && !reset {
            return;
        }

        let mut changed = false;
        if let Some(overlay) = self.overlays.get_mut(selected_idx) {
            let prev_scale = overlay.user_scale;
            if reset {
                overlay.user_scale = 1.0;
            } else {
                let factor = if increase {
                    1.1
                } else if decrease {
                    1.0 / 1.1
                } else {
                    1.0
                };
                overlay.user_scale = (overlay.user_scale * factor).clamp(0.05, 20.0);
            }
            changed = (overlay.user_scale - prev_scale).abs() > 1e-6;
        }

        if changed {
            self.mark_overlay_geometry_changed(selected_idx, false);
            self.request_overlay_placement_save();
            ui.ctx().request_repaint();
        }
    }

    fn try_move_selected_overlay_by_arrow_shortcuts(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        panel_text_input_focused: bool,
        strict_pixel_movement: bool,
    ) {
        if panel_text_input_focused {
            return;
        }

        let Some(selected_idx) = self.selected_overlay_idx else {
            return;
        };
        let Some(selected_overlay) = self.overlays.get(selected_idx) else {
            return;
        };
        if selected_overlay.page_idx != page_idx {
            return;
        }

        let (left_1, right_1, up_1, down_1, left_5, right_5, up_5, down_5) =
            ui.ctx().input_mut(|input| {
                (
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowLeft),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowRight),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowUp),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowDown),
                )
            });

        let delta_x_px = (right_1 as i32 - left_1 as i32) + (right_5 as i32 - left_5 as i32) * 5;
        let delta_y_px = (down_1 as i32 - up_1 as i32) + (down_5 as i32 - up_5 as i32) * 5;
        if delta_x_px == 0 && delta_y_px == 0 {
            return;
        }

        let page_delta = [delta_x_px as f32, delta_y_px as f32];
        let page_size = page_size_from_image_rect(image_rect, zoom);
        if let Some(overlay) = self.overlays.get_mut(selected_idx) {
            if let Some(mesh) = overlay.deform_mesh.as_mut() {
                mesh.translate(page_delta[0], page_delta[1], page_size);
                sync_overlay_center_from_deform_mesh(overlay, page_size);
            } else {
                overlay.center_page_px = clamp_page_point(
                    [
                        overlay.center_page_px[0] + page_delta[0],
                        overlay.center_page_px[1] + page_delta[1],
                    ],
                    page_size,
                );
            }
            snap_overlay_center_to_pixels_if_enabled(overlay, strict_pixel_movement, page_size);
        }

        let _ = self.enforce_overlay_visibility_limit(
            selected_idx,
            image_rect,
            zoom,
            strict_pixel_movement,
        );
        self.request_overlay_placement_save();
        ui.ctx().request_repaint();
    }

    fn try_trigger_selected_overlay_auto_typing_by_hotkey(
        &mut self,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        panel_text_input_focused: bool,
        settings: TypingAutoTypingSettings,
    ) {
        if panel_text_input_focused || ctx.wants_keyboard_input() {
            return;
        }
        if self.auto_typing_job.is_some() {
            return;
        }
        if !ctx.input(|input| input.key_pressed(egui::Key::C)) {
            return;
        }

        let Some(clean_model) = self.clean_overlays_model.clone() else {
            self.set_create_error(
                ctx,
                "Авто-тайп недоступен: модель clean overlay не подключена.",
            );
            return;
        };
        let Some(selected_idx) = self.selected_overlay_idx else {
            return;
        };
        let Some(overlay) = self.overlays.get(selected_idx) else {
            return;
        };
        if overlay.kind != TypingOverlayKind::Text || overlay.page_idx != page_idx {
            return;
        }

        let Some(local_center_px) = compute_overlay_visual_center(
            overlay.size_px,
            overlay.source_rgba.as_slice(),
            settings.extra_downward_shift_percent,
        ) else {
            self.set_create_error(
                ctx,
                "Авто-тайп: у оверлея не найден оптический центр (прозрачный слой).",
            );
            return;
        };
        let overlay_tuv = [
            (local_center_px[0] / overlay.size_px[0].max(1) as f32).clamp(0.0, 1.0),
            (local_center_px[1] / overlay.size_px[1].max(1) as f32).clamp(0.0, 1.0),
        ];
        let overlay_file_name = overlay.file_name.clone();
        let quad_scene = overlay_quad_scene(overlay, image_rect, zoom);
        let click_scene = bilinear_quad_point(quad_scene, overlay_tuv[0], overlay_tuv[1]);
        let mut click_uv = uv_from_scene(image_rect, click_scene);
        click_uv[0] = click_uv[0].clamp(0.0, 1.0);
        click_uv[1] = click_uv[1].clamp(0.0, 1.0);
        ctx.input_mut(|input| {
            let _ = input.consume_key(egui::Modifiers::NONE, egui::Key::C);
        });

        self.auto_typing_next_token = self.auto_typing_next_token.wrapping_add(1);
        let token = self.auto_typing_next_token;
        let (tx, rx) = mpsc::channel::<Result<TypingAutoTypingWorkerResult, String>>();
        thread::spawn(move || {
            let result = detect_bubble_from_overlay_cache(&clean_model, page_idx, click_uv).map(
                |detection| TypingAutoTypingWorkerResult {
                    token,
                    page_idx,
                    click_uv,
                    detection,
                },
            );
            let _ = tx.send(result);
        });

        self.auto_typing_job = Some(TypingAutoTypingJobState {
            rx,
            token,
            overlay_idx: selected_idx,
            overlay_file_name,
            page_idx,
            overlay_optical_tuv: overlay_tuv,
        });
    }

    fn poll_auto_typing_job(&mut self, ctx: &egui::Context) -> bool {
        let recv_result = {
            let Some(state) = self.auto_typing_job.as_ref() else {
                return false;
            };
            match state.rx.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    "Фоновый авто-тайп завершился с ошибкой канала.".to_string(),
                )),
            }
        };
        let Some(recv_result) = recv_result else {
            return false;
        };

        let Some(job_state) = self.auto_typing_job.take() else {
            return false;
        };
        match recv_result {
            Ok(Ok(result)) => self.apply_auto_typing_result(ctx, job_state, result),
            Ok(Err(err)) | Err(err) => {
                self.set_create_error(ctx, err);
                true
            }
        }
    }

    fn apply_auto_typing_result(
        &mut self,
        ctx: &egui::Context,
        job: TypingAutoTypingJobState,
        result: TypingAutoTypingWorkerResult,
    ) -> bool {
        if result.token != job.token || result.page_idx != job.page_idx {
            return false;
        }

        self.auto_typing_debug_visual = Some(TypingAutoTypingDebugVisual {
            page_idx: result.page_idx,
            accepted: result.detection.accepted,
            overlay_center_uv: result.click_uv,
            bubble_center_uv: result.detection.bubble_center_uv,
            bubble_bounds_uv: result.detection.bubble_bounds_uv,
            bubble_contour_uv: result.detection.bubble_contour_uv.clone(),
        });

        if !result.detection.accepted {
            self.set_create_error(ctx, format!("Авто-тайп: {}", result.detection.status));
            return true;
        }
        let Some(target_center_uv) = result.detection.bubble_center_uv else {
            self.set_create_error(
                ctx,
                "Авто-тайп: пузырь найден без центра, выравнивание пропущено.",
            );
            return true;
        };

        let page_size = result.detection.page_size;
        let delta_page_px = {
            let Some(overlay) = self.overlays.get(job.overlay_idx) else {
                return true;
            };
            if overlay.file_name != job.overlay_file_name
                || overlay.kind != TypingOverlayKind::Text
                || overlay.page_idx != job.page_idx
            {
                return true;
            }

            let deform_mesh = overlay_deform_mesh_for_page(overlay, page_size);
            let current_center_uv = sample_deform_mesh_uv(
                &deform_mesh,
                job.overlay_optical_tuv[0],
                job.overlay_optical_tuv[1],
                page_size,
            );
            [
                target_center_uv[0] - current_center_uv[0],
                target_center_uv[1] - current_center_uv[1],
            ]
        };
        let delta_page_px = [
            delta_page_px[0] * page_size[0].max(1) as f32,
            delta_page_px[1] * page_size[1].max(1) as f32,
        ];
        if delta_page_px[0].abs() <= 1e-6 && delta_page_px[1].abs() <= 1e-6 {
            return true;
        }

        if let Some(overlay) = self.overlays.get_mut(job.overlay_idx) {
            if let Some(mesh) = overlay.deform_mesh.as_mut() {
                mesh.translate(delta_page_px[0], delta_page_px[1], page_size);
                sync_overlay_center_from_deform_mesh(overlay, page_size);
            } else {
                overlay.center_page_px = clamp_page_point(
                    [
                        overlay.center_page_px[0] + delta_page_px[0],
                        overlay.center_page_px[1] + delta_page_px[1],
                    ],
                    page_size,
                );
            }
        }
        self.mark_overlay_geometry_changed(job.overlay_idx, false);
        self.request_overlay_placement_save();
        true
    }

    fn draw_auto_typing_debug_visuals(
        &self,
        painter: &egui::Painter,
        page_idx: usize,
        image_rect: Rect,
        settings: TypingAutoTypingSettings,
    ) {
        if !settings.debug_visuals {
            return;
        }
        let Some(debug) = self.auto_typing_debug_visual.as_ref() else {
            return;
        };
        if debug.page_idx != page_idx {
            return;
        }

        if debug.bubble_contour_uv.len() >= 2 {
            let stroke_color = if debug.accepted {
                Color32::from_rgb(102, 255, 153)
            } else {
                Color32::from_rgb(255, 160, 160)
            };
            for idx in 0..debug.bubble_contour_uv.len() {
                let a_uv = debug.bubble_contour_uv[idx];
                let b_uv = debug.bubble_contour_uv[(idx + 1) % debug.bubble_contour_uv.len()];
                let a = scene_from_uv(image_rect, a_uv[0], a_uv[1]);
                let b = scene_from_uv(image_rect, b_uv[0], b_uv[1]);
                painter.line_segment([a, b], Stroke::new(1.5, stroke_color));
            }
        }

        if let Some(bounds_uv) = debug.bubble_bounds_uv {
            let min = scene_from_uv(image_rect, bounds_uv[0], bounds_uv[1]);
            let max = scene_from_uv(image_rect, bounds_uv[2], bounds_uv[3]);
            let rect = Rect::from_min_max(min, max);
            let stroke_color = if debug.accepted {
                Color32::from_rgba_unmultiplied(140, 255, 140, 120)
            } else {
                Color32::from_rgba_unmultiplied(255, 140, 140, 120)
            };
            painter.rect_stroke(
                rect,
                0.0,
                Stroke::new(1.0, stroke_color),
                egui::StrokeKind::Outside,
            );
        }

        if let Some(center_uv) = debug.bubble_center_uv {
            let center = scene_from_uv(image_rect, center_uv[0], center_uv[1]);
            let color = Color32::RED;
            painter.line_segment(
                [center + Vec2::new(-8.0, 0.0), center + Vec2::new(8.0, 0.0)],
                Stroke::new(2.0, color),
            );
            painter.line_segment(
                [center + Vec2::new(0.0, -8.0), center + Vec2::new(0.0, 8.0)],
                Stroke::new(2.0, color),
            );
            painter.circle_stroke(center, 12.0, Stroke::new(1.5, color));
        }

        let overlay_center = scene_from_uv(
            image_rect,
            debug.overlay_center_uv[0],
            debug.overlay_center_uv[1],
        );
        let overlay_color = Color32::from_rgb(80, 210, 255);
        painter.line_segment(
            [
                overlay_center + Vec2::new(-6.0, 0.0),
                overlay_center + Vec2::new(6.0, 0.0),
            ],
            Stroke::new(1.5, overlay_color),
        );
        painter.line_segment(
            [
                overlay_center + Vec2::new(0.0, -6.0),
                overlay_center + Vec2::new(0.0, 6.0),
            ],
            Stroke::new(1.5, overlay_color),
        );
    }

    // All parameters are distinct pixel-buffer or layout properties; grouping would obscure rendering intent.
    #[allow(clippy::too_many_arguments)]
    fn draw_page_overlays(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        mask_panel_open: bool,
        panel_text_input_focused: bool,
        eyedropper_blocks_focus_clear: bool,
        auto_typing_settings: TypingAutoTypingSettings,
        strict_pixel_movement: bool,
    ) -> Vec<[Pos2; 4]> {
        if self
            .selected_overlay_idx
            .is_some_and(|idx| idx >= self.overlays.len())
        {
            self.selected_overlay_idx = None;
        }
        if self
            .transform_mode_overlay_idx
            .is_some_and(|idx| idx >= self.overlays.len())
        {
            self.transform_mode_overlay_idx = None;
        }
        if self
            .drag_state
            .as_ref()
            .is_some_and(|state| state.overlay_idx >= self.overlays.len())
        {
            self.drag_state = None;
            self.drag_has_changes = false;
        }
        if mask_panel_open {
            if let Some(selected_idx) = self.selected_overlay_idx {
                let should_validate = self
                    .overlays
                    .get(selected_idx)
                    .is_some_and(|overlay| overlay.page_idx == page_idx);
                if should_validate
                    && self.enforce_overlay_visibility_limit(
                        selected_idx,
                        image_rect,
                        zoom,
                        strict_pixel_movement,
                    )
                {
                    self.mark_overlay_geometry_changed(selected_idx, false);
                    self.request_overlay_placement_save();
                }
            }
            self.clear_selection();
        }

        if !ui.input(|i| i.pointer.primary_down()) {
            if self.drag_state.is_some() && self.drag_has_changes {
                if let Some(state) = self.drag_state.as_ref() {
                    self.flush_overlay_texture_if_stale(state.overlay_idx);
                }
                self.request_overlay_placement_save();
            }
            self.drag_state = None;
            self.drag_has_changes = false;
        }

        let clip_rect = ui.clip_rect().intersect(image_rect);
        if self.poll_auto_typing_job(ctx) {
            ctx.request_repaint();
        }
        if !clip_rect.is_positive() {
            return Vec::new();
        }
        let layout_editor_active = self.layout_editor.is_some();
        if !mask_panel_open && !layout_editor_active {
            self.try_trigger_selected_overlay_auto_typing_by_hotkey(
                ctx,
                page_idx,
                image_rect,
                zoom,
                panel_text_input_focused,
                auto_typing_settings,
            );
            self.try_rotate_selected_overlay_by_ctrl_wheel(ui, page_idx, image_rect, zoom);
            self.try_scale_selected_overlay_by_shortcuts(ui, page_idx);
            self.try_move_selected_overlay_by_arrow_shortcuts(
                ui,
                page_idx,
                image_rect,
                zoom,
                panel_text_input_focused,
                strict_pixel_movement,
            );
        }
        let mut adjusted_by_visibility_limit = false;
        for idx in 0..self.overlays.len() {
            let Some(overlay) = self.overlays.get(idx) else {
                continue;
            };
            if overlay.page_idx != page_idx {
                continue;
            }
            if self
                .drag_state
                .as_ref()
                .is_some_and(|state| state.overlay_idx == idx && state.page_idx == page_idx)
            {
                continue;
            }
            if self.enforce_overlay_visibility_limit(idx, image_rect, zoom, strict_pixel_movement) {
                self.mark_overlay_geometry_changed(idx, false);
                adjusted_by_visibility_limit = true;
            }
        }
        if adjusted_by_visibility_limit {
            self.request_overlay_placement_save();
        }
        let painter = ui.painter().with_clip_rect(clip_rect);
        let mut needs_texture_upload = Vec::new();
        for (idx, overlay) in self.overlays.iter().enumerate() {
            if overlay.page_idx == page_idx
                && (overlay.texture.is_none() || overlay.display_texture_stale)
            {
                needs_texture_upload.push(idx);
            }
        }
        for idx in needs_texture_upload {
            self.queue_overlay_texture_upload(idx);
        }
        if !self.pending_upload_indices.is_empty() {
            ctx.request_repaint();
        }

        struct OverlayDrawEntry {
            idx: usize,
            bounds_rect: Rect,
            selection_bounds_rect: Rect,
            quad_scene: [Pos2; 4],
            mesh_scene: Vec<Pos2>,
            selection_mesh_scene: Vec<Pos2>,
            mesh_cols: usize,
            mesh_rows: usize,
            occluder_quads: Vec<[Pos2; 4]>,
            texture: egui::TextureHandle,
        }

        let mut draw_entries: Vec<OverlayDrawEntry> = Vec::new();
        let current_frame = ui.ctx().cumulative_frame_nr();
        for idx in 0..self.overlays.len() {
            let Some(overlay) = self.overlays.get(idx) else {
                continue;
            };
            if overlay.page_idx != page_idx || overlay.texture.is_none() {
                continue;
            }
            if self.layout_editor.as_ref().is_some_and(|editor| {
                editor.mode == TypingLayoutEditorMode::Editing
                    && editor.overlay_idx == idx
                    && editor.page_idx == page_idx
            }) {
                continue;
            }
            let geometry = overlay_scene_geometry(overlay, image_rect, zoom);
            if geometry.bounds_rect.width() <= 0.5 || geometry.bounds_rect.height() <= 0.5 {
                continue;
            }
            if !geometry.bounds_rect.intersects(clip_rect) {
                continue;
            }
            if let Some(overlay) = self.overlays.get_mut(idx) {
                overlay.last_texture_used_frame = current_frame;
            }
            let Some(overlay) = self.overlays.get(idx) else {
                continue;
            };
            let is_selected_text =
                self.selected_overlay_idx == Some(idx) && overlay.kind == TypingOverlayKind::Text;
            let selection_mesh_scene = if is_selected_text {
                expand_selection_mesh_to_min_screen_side(
                    &geometry.mesh_scene,
                    geometry.mesh_cols,
                    geometry.mesh_rows,
                )
            } else {
                geometry.mesh_scene.clone()
            };
            let selection_bounds_rect = if is_selected_text {
                deform_mesh_bounds(&selection_mesh_scene)
            } else {
                geometry.bounds_rect
            };
            draw_entries.push(OverlayDrawEntry {
                idx,
                bounds_rect: geometry.bounds_rect,
                selection_bounds_rect,
                quad_scene: geometry.quad_scene,
                occluder_quads: build_mesh_occluder_quads(
                    &geometry.mesh_scene,
                    geometry.mesh_cols,
                    geometry.mesh_rows,
                ),
                mesh_scene: geometry.mesh_scene,
                selection_mesh_scene,
                mesh_cols: geometry.mesh_cols,
                mesh_rows: geometry.mesh_rows,
                texture: overlay.texture.as_ref().expect("checked above").clone(),
            });
        }

        if !draw_entries.is_empty() && !mask_panel_open && !layout_editor_active {
            let mut clicked_overlay_idx: Option<usize> = None;
            let mut pending_delete_overlay_idx: Option<usize> = None;
            let mut pending_enter_layout_editor_idx: Option<usize> = None;
            let popup_open_before = ui.ctx().is_popup_open();
            for entry in &draw_entries {
                let is_transform_mode = self.transform_mode_overlay_idx == Some(entry.idx);
                let show_rotate_handle =
                    self.selected_overlay_idx == Some(entry.idx) && !is_transform_mode;
                let rotate_handle_pos = if show_rotate_handle {
                    Some(rotation_handle_scene(&entry.quad_scene, image_rect))
                } else {
                    None
                };
                let mut interact_rect = if is_transform_mode {
                    entry
                        .bounds_rect
                        .expand(TEXT_OVERLAY_TRANSFORM_HANDLE_RADIUS_PX * 2.0 + 2.0)
                } else if self.selected_overlay_idx == Some(entry.idx) {
                    entry.selection_bounds_rect
                } else {
                    entry.bounds_rect
                };
                if let Some(handle_pos) = rotate_handle_pos {
                    let handle_rect = Rect::from_center_size(
                        handle_pos,
                        Vec2::splat(TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX * 4.0),
                    );
                    interact_rect = interact_rect.union(handle_rect);
                }
                let response = ui.interact(
                    interact_rect,
                    Id::new(("typing_text_overlay", entry.idx)),
                    Sense::click_and_drag(),
                );
                let pointer_pos = response.interact_pointer_pos();
                let pointer_inside_visual = pointer_pos.is_some_and(|pos| {
                    deform_mesh_contains_point(
                        &entry.mesh_scene,
                        entry.mesh_cols,
                        entry.mesh_rows,
                        pos,
                    )
                });
                let pointer_inside_grab_area = pointer_pos.is_some_and(|pos| {
                    let hit_mesh = if self.selected_overlay_idx == Some(entry.idx) {
                        &entry.selection_mesh_scene
                    } else {
                        &entry.mesh_scene
                    };
                    deform_mesh_contains_point(hit_mesh, entry.mesh_cols, entry.mesh_rows, pos)
                });
                let pointer_on_handle = pointer_pos.and_then(|pos| {
                    if !is_transform_mode || !self.deform_mode.is_handle_mode() {
                        return None;
                    }
                    match self.deform_mode {
                        TypingDeformMode::Perspective => {
                            hit_test_transform_handle(pos, &entry.quad_scene)
                        }
                        TypingDeformMode::Bend => hit_test_bend_handle(
                            pos,
                            &entry.mesh_scene,
                            entry.mesh_cols,
                            entry.mesh_rows,
                        ),
                        TypingDeformMode::Frame => hit_test_frame_handle(
                            pos,
                            &entry.mesh_scene,
                            entry.mesh_cols,
                            entry.mesh_rows,
                            self.frame_handle_side_points,
                        ),
                        TypingDeformMode::Grid => hit_test_grid_handle(
                            pos,
                            &entry.mesh_scene,
                            entry.mesh_cols,
                            entry.mesh_rows,
                            self.frame_handle_side_points,
                        ),
                        _ => None,
                    }
                });
                let pointer_on_rotate_handle =
                    pointer_pos
                        .zip(rotate_handle_pos)
                        .is_some_and(|(pointer, handle)| {
                            pointer.distance(handle) <= TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX * 2.0
                        });
                let pointer_targets_overlay = pointer_inside_grab_area
                    || pointer_on_handle.is_some()
                    || pointer_on_rotate_handle;

                if response.clicked() && pointer_targets_overlay {
                    clicked_overlay_idx = Some(entry.idx);
                    self.selected_overlay_idx = Some(entry.idx);
                    self.primary_pointer_targets_overlay_this_frame = true;
                }
                if response.secondary_clicked() && pointer_inside_visual {
                    self.selected_overlay_idx = Some(entry.idx);
                    if let Some(origin) = pointer_pos {
                        self.start_shape_variant_preview_if_available(ui.ctx(), entry.idx, origin);
                    }
                }

                response.context_menu(|menu_ui| {
                    if self.selected_overlay_idx != Some(entry.idx) {
                        menu_ui.label("Выделите оверлей ЛКМ.");
                        return;
                    }
                    if self
                        .shape_variant_preview
                        .as_ref()
                        .is_none_or(|state| state.overlay_idx != entry.idx)
                    {
                        let origin = menu_ui
                            .ctx()
                            .pointer_latest_pos()
                            .unwrap_or_else(|| menu_ui.min_rect().left_top());
                        self.start_shape_variant_preview_if_available(
                            menu_ui.ctx(),
                            entry.idx,
                            origin,
                        );
                    }
                    if menu_ui
                        .button("Войти в режим изменения раскладки")
                        .clicked()
                    {
                        pending_enter_layout_editor_idx = Some(entry.idx);
                        menu_ui.close();
                    }
                    menu_ui.separator();
                    if !is_transform_mode {
                        if menu_ui.button("Войти в режим трансформации").clicked()
                        {
                            if self.ensure_overlay_deform_mesh(entry.idx, image_rect, zoom) {
                                self.transform_mode_overlay_idx = Some(entry.idx);
                                self.deform_mode = TypingDeformMode::Perspective;
                                self.drag_state = None;
                            }
                            menu_ui.close();
                        }
                    } else {
                        if menu_ui.button("Выйти из режима трансформации").clicked()
                        {
                            if self.transform_mode_overlay_idx == Some(entry.idx) {
                                self.transform_mode_overlay_idx = None;
                            }
                            self.drag_state = None;
                            self.drag_has_changes = false;
                            menu_ui.close();
                        }
                        if menu_ui.button("Сбросить трансформацию").clicked() {
                            if let Some(overlay) = self.overlays.get_mut(entry.idx) {
                                overlay.deform_mesh = None;
                            }
                            self.mark_overlay_geometry_changed(entry.idx, false);
                            self.request_overlay_placement_save();
                            self.drag_state = None;
                            self.drag_has_changes = false;
                            menu_ui.close();
                        }
                    }
                    menu_ui.separator();
                    if let Some(overlay) = self.overlays.get(entry.idx) {
                        let toggle_label = if overlay.mask_clip_enabled {
                            "Выключить обрезание маской"
                        } else {
                            "Включить обрезание маской"
                        };
                        if menu_ui.button(toggle_label).clicked() {
                            if let Some(overlay) = self.overlays.get_mut(entry.idx) {
                                overlay.mask_clip_enabled = !overlay.mask_clip_enabled;
                            }
                            self.mark_overlay_pixels_dirty(entry.idx);
                            self.request_overlay_placement_save();
                            menu_ui.close();
                        }
                    }
                    menu_ui.separator();
                    if menu_ui.button("Удалить оверлей").clicked() {
                        pending_delete_overlay_idx = Some(entry.idx);
                        menu_ui.close();
                    }
                    self.update_shape_variant_preview_menu_rect(entry.idx, menu_ui.min_rect());
                });

                if response.drag_started() && pointer_targets_overlay {
                    self.primary_pointer_targets_overlay_this_frame = true;
                    if let Some(pointer_pos) = pointer_pos {
                        let Some((
                            mut start_center_page_px,
                            start_angle_deg,
                            has_mesh,
                            mut start_mesh,
                        )) = self.overlays.get(entry.idx).map(|overlay| {
                            (
                                overlay.center_page_px,
                                overlay.angle_deg,
                                overlay.deform_mesh.is_some(),
                                overlay.deform_mesh.clone().unwrap_or_else(|| {
                                    default_overlay_quad_mesh(overlay, image_rect, zoom)
                                }),
                            )
                        })
                        else {
                            continue;
                        };

                        self.selected_overlay_idx = Some(entry.idx);
                        let mut mode = if pointer_on_rotate_handle {
                            TypingOverlayDragMode::Rotate
                        } else if has_mesh {
                            TypingOverlayDragMode::MoveMesh
                        } else {
                            TypingOverlayDragMode::MoveCenter
                        };
                        let start_mesh_scene = scene_mesh_points(&start_mesh, image_rect, zoom);
                        let start_center_scene = deform_mesh_center_scene(&start_mesh_scene);
                        let start_pointer_angle_rad =
                            pointer_angle_rad(start_center_scene, pointer_pos);

                        if self.transform_mode_overlay_idx == Some(entry.idx) {
                            let _ = self.ensure_overlay_deform_mesh(entry.idx, image_rect, zoom);
                            if let Some(current_mesh) = self
                                .overlays
                                .get(entry.idx)
                                .and_then(|overlay| overlay.deform_mesh.clone())
                            {
                                mode = TypingOverlayDragMode::MoveMesh;
                                if let Some(handle_idx) = pointer_on_handle {
                                    mode = match self.deform_mode {
                                        TypingDeformMode::Perspective => {
                                            TypingOverlayDragMode::PerspectiveHandle(handle_idx)
                                        }
                                        TypingDeformMode::Bend => {
                                            TypingOverlayDragMode::BendHandle(handle_idx)
                                        }
                                        TypingDeformMode::Frame => {
                                            TypingOverlayDragMode::FrameHandle(handle_idx)
                                        }
                                        TypingDeformMode::Grid => {
                                            TypingOverlayDragMode::GridHandle(handle_idx)
                                        }
                                        _ => TypingOverlayDragMode::MoveMesh,
                                    };
                                } else if self.deform_mode.is_brush_mode() && pointer_inside_visual
                                {
                                    mode = TypingOverlayDragMode::BrushStroke(self.deform_mode);
                                }
                                let snapped_on_drag_start =
                                    if matches!(mode, TypingOverlayDragMode::MoveMesh) {
                                        let page_size = page_size_from_image_rect(image_rect, zoom);
                                        self.snap_overlay_to_pixel_position(
                                            entry.idx, page_size, true,
                                        )
                                    } else {
                                        false
                                    };
                                let current_mesh = if snapped_on_drag_start {
                                    self.overlays
                                        .get(entry.idx)
                                        .and_then(|overlay| overlay.deform_mesh.clone())
                                        .unwrap_or(current_mesh)
                                } else {
                                    current_mesh
                                };
                                if snapped_on_drag_start
                                    && let Some(overlay) = self.overlays.get(entry.idx)
                                {
                                    start_center_page_px = overlay.center_page_px;
                                }
                                self.drag_state = Some(TypingOverlayDragState {
                                    overlay_idx: entry.idx,
                                    page_idx,
                                    pointer_start_scene: pointer_pos,
                                    mode,
                                    start_has_mesh: has_mesh,
                                    start_center_page_px,
                                    start_angle_deg,
                                    start_pointer_angle_rad,
                                    start_mesh: current_mesh,
                                });
                                self.drag_has_changes = snapped_on_drag_start;
                                continue;
                            }
                        }

                        let snapped_on_drag_start = if matches!(
                            mode,
                            TypingOverlayDragMode::MoveCenter | TypingOverlayDragMode::MoveMesh
                        ) {
                            let page_size = page_size_from_image_rect(image_rect, zoom);
                            self.snap_overlay_to_pixel_position(entry.idx, page_size, true)
                        } else {
                            false
                        };
                        if snapped_on_drag_start && let Some(overlay) = self.overlays.get(entry.idx)
                        {
                            start_center_page_px = overlay.center_page_px;
                            start_mesh = overlay.deform_mesh.clone().unwrap_or_else(|| {
                                default_overlay_quad_mesh(overlay, image_rect, zoom)
                            });
                        }
                        self.drag_state = Some(TypingOverlayDragState {
                            overlay_idx: entry.idx,
                            page_idx,
                            pointer_start_scene: pointer_pos,
                            mode,
                            start_has_mesh: has_mesh,
                            start_center_page_px,
                            start_angle_deg,
                            start_pointer_angle_rad,
                            start_mesh,
                        });
                        self.drag_has_changes = snapped_on_drag_start;
                    }
                }

                if response.dragged() {
                    let Some(mut state) = self.drag_state.take() else {
                        continue;
                    };
                    if state.overlay_idx != entry.idx || state.page_idx != page_idx {
                        self.drag_state = Some(state);
                        continue;
                    }
                    let Some(pointer_pos) = pointer_pos else {
                        self.drag_state = Some(state);
                        continue;
                    };

                    let page_size = page_size_from_image_rect(image_rect, zoom);
                    let raw_delta_page_px = [
                        (pointer_pos.x - state.pointer_start_scene.x) / zoom.max(f32::EPSILON),
                        (pointer_pos.y - state.pointer_start_scene.y) / zoom.max(f32::EPSILON),
                    ];
                    let delta_page_px = match state.mode {
                        TypingOverlayDragMode::MoveCenter | TypingOverlayDragMode::MoveMesh => {
                            quantize_drag_page_delta(raw_delta_page_px, strict_pixel_movement)
                        }
                        TypingOverlayDragMode::PerspectiveHandle(_)
                        | TypingOverlayDragMode::BendHandle(_)
                        | TypingOverlayDragMode::FrameHandle(_)
                        | TypingOverlayDragMode::GridHandle(_)
                        | TypingOverlayDragMode::BrushStroke(_)
                        | TypingOverlayDragMode::Rotate => raw_delta_page_px,
                    };
                    let move_center_transition = match state.mode {
                        TypingOverlayDragMode::MoveCenter => {
                            Some(self.remap_drag_vertical_page_transition(
                                state.page_idx,
                                state.start_center_page_px[1] + delta_page_px[1],
                                page_size,
                            ))
                        }
                        _ => None,
                    };
                    let move_mesh_transition = match state.mode {
                        TypingOverlayDragMode::MoveMesh => {
                            let mut raw_mesh = state.start_mesh.clone();
                            raw_mesh.translate(delta_page_px[0], delta_page_px[1], page_size);
                            let center_y =
                                raw_mesh.points_px.iter().map(|point| point[1]).sum::<f32>()
                                    / raw_mesh.points_px.len().max(1) as f32;
                            let (next_page_idx, next_center_v) = self
                                .remap_drag_vertical_page_transition(
                                    state.page_idx,
                                    center_y,
                                    page_size,
                                );
                            Some((raw_mesh, center_y, next_page_idx, next_center_v))
                        }
                        _ => None,
                    };
                    let mut overlay_changed = false;
                    let mut page_changed = false;
                    if let Some(overlay) = self.overlays.get_mut(entry.idx) {
                        let prev_center_page_px = overlay.center_page_px;
                        let prev_angle = overlay.angle_deg;
                        let prev_mesh = overlay.deform_mesh.clone();
                        let prev_page_idx = overlay.page_idx;
                        match state.mode {
                            TypingOverlayDragMode::MoveCenter => {
                                let (next_page_idx, next_y_px) =
                                    move_center_transition.unwrap_or((
                                        state.page_idx,
                                        clamp_overlay_page_coord(
                                            state.start_center_page_px[1] + delta_page_px[1],
                                            page_size[1],
                                        ),
                                    ));
                                overlay.center_page_px = clamp_page_point(
                                    [state.start_center_page_px[0] + delta_page_px[0], next_y_px],
                                    page_size,
                                );
                                overlay.page_idx = next_page_idx;
                                page_changed = overlay.page_idx != prev_page_idx;
                            }
                            TypingOverlayDragMode::MoveMesh => {
                                let (mut deform_mesh, center_y, next_page_idx, next_center_y) =
                                    move_mesh_transition.unwrap_or((
                                        state.start_mesh.clone(),
                                        state
                                            .start_mesh
                                            .points_px
                                            .iter()
                                            .map(|point| point[1])
                                            .sum::<f32>()
                                            / state.start_mesh.points_px.len().max(1) as f32,
                                        state.page_idx,
                                        state
                                            .start_mesh
                                            .points_px
                                            .iter()
                                            .map(|point| point[1])
                                            .sum::<f32>()
                                            / state.start_mesh.points_px.len().max(1) as f32,
                                    ));
                                if next_page_idx != state.page_idx {
                                    let shift_y = next_center_y - center_y;
                                    deform_mesh.translate(0.0, shift_y, page_size);
                                }
                                overlay.deform_mesh = Some(deform_mesh);
                                overlay.page_idx = next_page_idx;
                                page_changed = overlay.page_idx != prev_page_idx;
                                sync_overlay_center_from_deform_mesh(overlay, page_size);
                            }
                            TypingOverlayDragMode::PerspectiveHandle(handle_idx) => {
                                if handle_idx < 4 {
                                    overlay.deform_mesh = Some(apply_perspective_corner_drag(
                                        &state.start_mesh,
                                        handle_idx,
                                        delta_page_px,
                                        page_size,
                                    ));
                                    sync_overlay_center_from_deform_mesh(overlay, page_size);
                                }
                            }
                            TypingOverlayDragMode::BendHandle(handle_idx) => {
                                if handle_idx < bend_handle_count() {
                                    overlay.deform_mesh = Some(apply_bend_handle_drag(
                                        &state.start_mesh,
                                        handle_idx,
                                        delta_page_px,
                                        page_size,
                                    ));
                                    sync_overlay_center_from_deform_mesh(overlay, page_size);
                                }
                            }
                            TypingOverlayDragMode::FrameHandle(handle_idx) => {
                                if handle_idx < frame_handle_count(self.frame_handle_side_points) {
                                    overlay.deform_mesh = Some(apply_sampled_handle_drag(
                                        &state.start_mesh,
                                        SampledHandleMode::Frame,
                                        self.frame_handle_side_points,
                                        handle_idx,
                                        self.pull_neighbor_handles,
                                        delta_page_px,
                                        page_size,
                                    ));
                                    sync_overlay_center_from_deform_mesh(overlay, page_size);
                                }
                            }
                            TypingOverlayDragMode::GridHandle(handle_idx) => {
                                if handle_idx < grid_handle_count(self.frame_handle_side_points) {
                                    overlay.deform_mesh = Some(apply_sampled_handle_drag(
                                        &state.start_mesh,
                                        SampledHandleMode::Grid,
                                        self.frame_handle_side_points,
                                        handle_idx,
                                        self.pull_neighbor_handles,
                                        delta_page_px,
                                        page_size,
                                    ));
                                    sync_overlay_center_from_deform_mesh(overlay, page_size);
                                }
                            }
                            TypingOverlayDragMode::BrushStroke(mode) => {
                                let default_mesh =
                                    default_overlay_deform_mesh(overlay, image_rect, zoom);
                                overlay.deform_mesh = Some(apply_brush_deform_drag(
                                    mode,
                                    &state.start_mesh,
                                    &default_mesh,
                                    state.pointer_start_scene,
                                    pointer_pos,
                                    image_rect,
                                    zoom,
                                    &self.deform_tool_settings,
                                ));
                                sync_overlay_center_from_deform_mesh(overlay, page_size);
                            }
                            TypingOverlayDragMode::Rotate => {
                                let start_mesh_scene =
                                    scene_mesh_points(&state.start_mesh, image_rect, zoom);
                                let center_scene = deform_mesh_center_scene(&start_mesh_scene);
                                let current_angle = pointer_angle_rad(center_scene, pointer_pos);
                                let delta_angle = normalize_angle_rad(
                                    current_angle - state.start_pointer_angle_rad,
                                );
                                if state.start_has_mesh {
                                    let rotated_scene = rotate_mesh_scene(
                                        &start_mesh_scene,
                                        center_scene,
                                        delta_angle,
                                    );
                                    let rotated_uv = rotated_scene
                                        .into_iter()
                                        .map(|scene| page_px_from_scene(image_rect, zoom, scene))
                                        .collect::<Vec<_>>();
                                    overlay.deform_mesh = TypingOverlayDeformMesh::new(
                                        state.start_mesh.cols,
                                        state.start_mesh.rows,
                                        rotated_uv,
                                        page_size,
                                    );
                                    sync_overlay_center_from_deform_mesh(overlay, page_size);
                                } else {
                                    overlay.angle_deg = normalize_angle_deg(
                                        state.start_angle_deg + delta_angle.to_degrees(),
                                    );
                                }
                            }
                        }
                        if overlay.center_page_px != prev_center_page_px
                            || (overlay.angle_deg - prev_angle).abs() > 1e-4
                            || overlay.deform_mesh != prev_mesh
                            || overlay.page_idx != prev_page_idx
                        {
                            self.drag_has_changes = true;
                            overlay_changed = true;
                        }
                    }
                    if !page_changed
                        && self.enforce_overlay_visibility_limit(
                            entry.idx,
                            image_rect,
                            zoom,
                            strict_pixel_movement,
                        )
                    {
                        self.drag_has_changes = true;
                        overlay_changed = true;
                    }
                    if overlay_changed {
                        self.mark_overlay_geometry_changed(entry.idx, true);
                    }
                    let brush_continue =
                        matches!(state.mode, TypingOverlayDragMode::BrushStroke(_));
                    if (page_changed || brush_continue)
                        && let Some(overlay) = self.overlays.get(entry.idx)
                    {
                        state.page_idx = overlay.page_idx;
                        state.pointer_start_scene = pointer_pos;
                        state.start_center_page_px = overlay.center_page_px;
                        state.start_angle_deg = overlay.angle_deg;
                        if let Some(mesh) = overlay.deform_mesh.clone() {
                            state.start_mesh = mesh;
                        }
                    }
                    self.drag_state = Some(state);
                }

                if response.drag_stopped()
                    && self
                        .drag_state
                        .as_ref()
                        .is_some_and(|state| state.overlay_idx == entry.idx)
                {
                    if self.drag_has_changes {
                        self.flush_overlay_texture_if_stale(entry.idx);
                        self.request_overlay_placement_save();
                    }
                    self.drag_state = None;
                    self.drag_has_changes = false;
                }
            }

            self.poll_shape_variant_preview(ui.ctx());
            if let Some(variant) = self.draw_shape_variant_preview(ui.ctx()) {
                self.apply_shape_variant_to_overlay(ctx, variant);
            }

            if let Some(delete_idx) = pending_delete_overlay_idx {
                self.remove_overlay(delete_idx);
                return Vec::new();
            }
            if let Some(editor_idx) = pending_enter_layout_editor_idx {
                self.begin_layout_editor_for_overlay(editor_idx, image_rect, zoom);
                ctx.request_repaint();
            }
            let popup_open_after = ui.ctx().is_popup_open();
            let popup_open = popup_open_before || popup_open_after;
            let delete_pressed = ui.input(|i| i.key_pressed(egui::Key::Delete));
            if delete_pressed
                && !ui.ctx().wants_keyboard_input()
                && let Some(selected_idx) = self.selected_overlay_idx
                && self
                    .overlays
                    .get(selected_idx)
                    .is_some_and(|overlay| overlay.page_idx == page_idx)
            {
                self.remove_overlay(selected_idx);
                return Vec::new();
            }

            let clicked_on_image_without_overlay = ui.input(|i| {
                i.pointer.primary_clicked()
                    && i.pointer
                        .interact_pos()
                        .is_some_and(|pos| image_rect.contains(pos))
                    && clicked_overlay_idx.is_none()
            }) && !popup_open
                && !ui.ctx().is_pointer_over_area()
                && !eyedropper_blocks_focus_clear;
            if clicked_on_image_without_overlay {
                if self
                    .selected_overlay_idx
                    .and_then(|idx| self.overlays.get(idx))
                    .is_some_and(|overlay| overlay.page_idx == page_idx)
                {
                    if let Some(selected_idx) = self.selected_overlay_idx
                        && self.enforce_overlay_visibility_limit(
                            selected_idx,
                            image_rect,
                            zoom,
                            strict_pixel_movement,
                        )
                    {
                        snap_overlay_center_to_pixels_if_enabled(
                            self.overlays
                                .get_mut(selected_idx)
                                .expect("selected overlay exists after visibility enforcement"),
                            strict_pixel_movement,
                            page_size_from_image_rect(image_rect, zoom),
                        );
                        self.mark_overlay_geometry_changed(selected_idx, false);
                        self.request_overlay_placement_save();
                    }
                    if self.transform_mode_overlay_idx == self.selected_overlay_idx {
                        self.transform_mode_overlay_idx = None;
                    }
                    self.selected_overlay_idx = None;
                }
                if self
                    .drag_state
                    .as_ref()
                    .is_some_and(|state| state.page_idx == page_idx)
                {
                    self.drag_state = None;
                    self.drag_has_changes = false;
                }
            }
            if self
                .transform_mode_overlay_idx
                .is_some_and(|idx| self.selected_overlay_idx != Some(idx))
                && !popup_open
            {
                self.transform_mode_overlay_idx = None;
            }
        }

        for entry in &draw_entries {
            draw_textured_deform_mesh(
                &painter,
                entry.texture.id(),
                &entry.mesh_scene,
                entry.mesh_cols,
                entry.mesh_rows,
            );
            if !mask_panel_open && self.selected_overlay_idx == Some(entry.idx) {
                let selection_path = mesh_boundary_path(
                    &entry.selection_mesh_scene,
                    entry.mesh_cols,
                    entry.mesh_rows,
                );
                draw_dashed_selection_path(&painter, &selection_path);
                if self.transform_mode_overlay_idx == Some(entry.idx) {
                    match self.deform_mode {
                        TypingDeformMode::Perspective => {
                            draw_perspective_handles(&painter, &entry.quad_scene)
                        }
                        TypingDeformMode::Bend => draw_bend_handles(
                            &painter,
                            &entry.mesh_scene,
                            entry.mesh_cols,
                            entry.mesh_rows,
                        ),
                        TypingDeformMode::Frame => draw_frame_handles(
                            &painter,
                            &entry.mesh_scene,
                            entry.mesh_cols,
                            entry.mesh_rows,
                            self.frame_handle_side_points,
                        ),
                        TypingDeformMode::Grid => draw_grid_handles(
                            &painter,
                            &entry.mesh_scene,
                            entry.mesh_cols,
                            entry.mesh_rows,
                            self.frame_handle_side_points,
                        ),
                        _ => {}
                    }
                } else {
                    draw_rotation_handle(&painter, &entry.quad_scene, image_rect);
                }
            }
        }
        if self.layout_editor.is_some() && !mask_panel_open {
            self.draw_layout_editor_on_page(ui, ctx, page_idx, image_rect, zoom, clip_rect);
        }
        if let Some(selected_idx) = self.transform_mode_overlay_idx
            && self.selected_overlay_idx == Some(selected_idx)
            && self.deform_mode.is_brush_mode()
            && let Some(selected_entry) =
                draw_entries.iter().find(|entry| entry.idx == selected_idx)
            && let Some(pointer_pos) = ui.ctx().input(|i| i.pointer.latest_pos())
            && deform_mesh_contains_point(
                &selected_entry.mesh_scene,
                selected_entry.mesh_cols,
                selected_entry.mesh_rows,
                pointer_pos,
            )
        {
            draw_brush_preview(
                &painter,
                pointer_pos,
                self.deform_tool_settings.brush_radius_px,
            );
        }
        self.draw_auto_typing_debug_visuals(&painter, page_idx, image_rect, auto_typing_settings);
        draw_entries
            .into_iter()
            .flat_map(|entry| entry.occluder_quads.into_iter())
            .collect()
    }

    fn wants_repaint(&self) -> bool {
        self.loading_rx.is_some()
            || self.create_selection.is_some()
            || self.create_editor.is_some()
            || self.create_render_state.is_some()
            || self.edit_render_rx.is_some()
            || self.auto_typing_job.is_some()
            || self.export_rx.is_some()
            || self.create_status_error.is_some()
            || self.create_status_warning.is_some()
            || self.save_rx.is_some()
            || !self.pending_upload_indices.is_empty()
            || self.drag_state.is_some()
            || self.layout_editor.is_some()
    }

    fn snap_overlay_to_pixel_position(
        &mut self,
        overlay_idx: usize,
        page_size: [usize; 2],
        defer_mask_refresh: bool,
    ) -> bool {
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            return false;
        };
        let previous_center = overlay.center_page_px;
        let previous_mesh = overlay.deform_mesh.clone();
        let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
            return false;
        };
        snap_overlay_center_to_pixels_if_enabled(overlay, true, page_size);
        let changed =
            overlay.center_page_px != previous_center || overlay.deform_mesh != previous_mesh;
        if changed {
            self.mark_overlay_geometry_changed(overlay_idx, defer_mask_refresh);
        }
        changed
    }

    fn enforce_overlay_visibility_limit(
        &mut self,
        overlay_idx: usize,
        image_rect: Rect,
        zoom: f32,
        strict_pixel_movement: bool,
    ) -> bool {
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            return false;
        };
        if !image_rect.is_positive() || overlay.size_px[0] == 0 || overlay.size_px[1] == 0 {
            return false;
        }

        let bounds = if overlay.deform_mesh.is_some() {
            let deform_mesh = overlay_deform_mesh(overlay, image_rect, zoom);
            let page_size = page_size_from_image_rect(image_rect, zoom);
            let bounds_uv = deform_mesh_bounds_uv(&deform_mesh, page_size);
            if !bounds_uv.is_positive() {
                return false;
            }
            Rect::from_min_max(
                scene_from_uv(image_rect, bounds_uv.min.x, bounds_uv.min.y),
                scene_from_uv(image_rect, bounds_uv.max.x, bounds_uv.max.y),
            )
        } else {
            quad_bounds(&default_overlay_quad_scene(overlay, image_rect, zoom))
        };

        let min_visible_w = bounds.width() * TEXT_OVERLAY_MIN_VISIBLE_FRACTION;
        let min_visible_h = bounds.height() * TEXT_OVERLAY_MIN_VISIBLE_FRACTION;

        let target_left = bounds.left().clamp(
            image_rect.left() + min_visible_w - bounds.width(),
            image_rect.right() - min_visible_w,
        );
        let target_top = bounds.top().clamp(
            image_rect.top() + min_visible_h - bounds.height(),
            image_rect.bottom() - min_visible_h,
        );
        let dx = target_left - bounds.left();
        let dy = target_top - bounds.top();
        if dx.abs() <= 1e-6 && dy.abs() <= 1e-6 {
            return false;
        }

        let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
            return false;
        };
        let page_size = page_size_from_image_rect(image_rect, zoom);
        if let Some(deform_mesh) = overlay.deform_mesh.as_mut() {
            let dx_px = dx / zoom.max(f32::EPSILON);
            let dy_px = dy / zoom.max(f32::EPSILON);
            deform_mesh.translate(dx_px, dy_px, page_size);
            sync_overlay_center_from_deform_mesh(overlay, page_size);
        } else {
            let dx_px = dx / zoom.max(f32::EPSILON);
            let dy_px = dy / zoom.max(f32::EPSILON);
            overlay.center_page_px = clamp_page_point(
                [
                    overlay.center_page_px[0] + dx_px,
                    overlay.center_page_px[1] + dy_px,
                ],
                page_size,
            );
        }
        snap_overlay_center_to_pixels_if_enabled(overlay, strict_pixel_movement, page_size);
        true
    }

    fn remap_drag_vertical_page_transition(
        &self,
        mut page_idx: usize,
        mut y_px: f32,
        page_size: [usize; 2],
    ) -> (usize, f32) {
        let min_v = overlay_uv_min() * page_size[1].max(1) as f32;
        let max_v = overlay_uv_max() * page_size[1].max(1) as f32;
        loop {
            if y_px > max_v && page_idx + 1 < self.page_count {
                y_px = min_v + (y_px - max_v);
                page_idx += 1;
                continue;
            }
            if y_px < min_v && page_idx > 0 {
                y_px = max_v - (min_v - y_px);
                page_idx -= 1;
                continue;
            }
            break;
        }
        (page_idx, clamp_overlay_page_coord(y_px, page_size[1]))
    }
}

fn draw_dashed_selection_path(painter: &egui::Painter, path: &[Pos2]) {
    if path.len() < 2 {
        return;
    }
    let dash_length = 8.0;
    let gap_length = 6.0;
    let white_offset = dash_length;
    let mut shapes = Vec::new();
    for segment in path.windows(2) {
        egui::Shape::dashed_line_many(
            segment,
            Stroke::new(2.0, Color32::BLACK),
            dash_length,
            gap_length,
            &mut shapes,
        );
        egui::Shape::dashed_line_many_with_offset(
            segment,
            Stroke::new(2.0, Color32::WHITE),
            &[dash_length],
            &[gap_length],
            white_offset,
            &mut shapes,
        );
    }
    painter.extend(shapes);
}

fn mesh_boundary_path(mesh_scene: &[Pos2], cols: usize, rows: usize) -> Vec<Pos2> {
    if cols < 2 || rows < 2 || mesh_scene.len() != cols.saturating_mul(rows) {
        return Vec::new();
    }

    let idx = |col: usize, row: usize| -> usize { row * cols + col };
    let mut path = Vec::with_capacity(cols.saturating_mul(2) + rows.saturating_mul(2) + 1);

    for col in 0..cols {
        path.push(mesh_scene[idx(col, 0)]);
    }
    for row in 1..rows {
        path.push(mesh_scene[idx(cols - 1, row)]);
    }
    if rows > 1 {
        for col in (0..(cols - 1)).rev() {
            path.push(mesh_scene[idx(col, rows - 1)]);
        }
    }
    if cols > 1 {
        for row in (1..(rows - 1)).rev() {
            path.push(mesh_scene[idx(0, row)]);
        }
    }
    if let Some(first) = path.first().copied() {
        path.push(first);
    }
    path
}

fn expand_selection_mesh_to_min_screen_side(
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
) -> Vec<Pos2> {
    if cols < 2 || rows < 2 || mesh_scene.len() != cols.saturating_mul(rows) {
        return mesh_scene.to_vec();
    }

    if cols == 2 && rows == 2 {
        return expand_quad_selection_mesh_to_min_screen_side(mesh_scene);
    }

    expand_axis_aligned_selection_mesh_to_min_screen_side(mesh_scene)
}

fn expand_quad_selection_mesh_to_min_screen_side(mesh_scene: &[Pos2]) -> Vec<Pos2> {
    let quad = [mesh_scene[0], mesh_scene[1], mesh_scene[3], mesh_scene[2]];
    let width = ((quad[0].distance(quad[1]) + quad[3].distance(quad[2])) * 0.5).max(f32::EPSILON);
    let height = ((quad[0].distance(quad[3]) + quad[1].distance(quad[2])) * 0.5).max(f32::EPSILON);
    if width >= TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX
        && height >= TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX
    {
        return mesh_scene.to_vec();
    }

    let scale_x = (TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX / width).max(1.0);
    let scale_y = (TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX / height).max(1.0);
    let center = quad_center_scene(&quad);
    let top_axis = normalized_or_none(quad[1] - quad[0]);
    let left_axis = normalized_or_none(quad[3] - quad[0]);
    let (Some(x_axis), Some(y_axis)) = (top_axis, left_axis) else {
        return expand_axis_aligned_selection_mesh_to_min_screen_side(mesh_scene);
    };

    mesh_scene
        .iter()
        .map(|point| {
            let delta = *point - center;
            center + x_axis * delta.dot(x_axis) * scale_x + y_axis * delta.dot(y_axis) * scale_y
        })
        .collect()
}

fn expand_axis_aligned_selection_mesh_to_min_screen_side(mesh_scene: &[Pos2]) -> Vec<Pos2> {
    let bounds = deform_mesh_bounds(mesh_scene);
    if !bounds.is_positive() {
        return mesh_scene.to_vec();
    }
    let width = bounds.width().max(f32::EPSILON);
    let height = bounds.height().max(f32::EPSILON);
    if width >= TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX
        && height >= TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX
    {
        return mesh_scene.to_vec();
    }

    let center = bounds.center();
    let scale_x = (TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX / width).max(1.0);
    let scale_y = (TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX / height).max(1.0);
    mesh_scene
        .iter()
        .map(|point| {
            Pos2::new(
                center.x + (point.x - center.x) * scale_x,
                center.y + (point.y - center.y) * scale_y,
            )
        })
        .collect()
}

fn normalized_or_none(vector: Vec2) -> Option<Vec2> {
    let len = vector.length();
    if len <= f32::EPSILON {
        None
    } else {
        Some(vector / len)
    }
}

fn draw_perspective_handles(painter: &egui::Painter, quad: &[Pos2; 4]) {
    for corner in quad {
        painter.circle_filled(
            *corner,
            TEXT_OVERLAY_TRANSFORM_HANDLE_RADIUS_PX,
            Color32::from_rgba_unmultiplied(255, 80, 80, 230),
        );
        painter.circle_stroke(
            *corner,
            TEXT_OVERLAY_TRANSFORM_HANDLE_RADIUS_PX,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 200)),
        );
    }
}

fn draw_bend_handles(painter: &egui::Painter, mesh_scene: &[Pos2], cols: usize, rows: usize) {
    for handle_idx in 0..bend_handle_count() {
        let Some((surface_col, surface_row)) = bend_handle_surface_coord(handle_idx, cols, rows)
        else {
            continue;
        };
        let point = mesh_scene[surface_row * cols + surface_col];
        painter.circle_filled(
            point,
            TEXT_OVERLAY_BEND_HANDLE_RADIUS_PX,
            Color32::from_rgba_unmultiplied(255, 110, 110, 215),
        );
        painter.circle_stroke(
            point,
            TEXT_OVERLAY_BEND_HANDLE_RADIUS_PX,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 180)),
        );
    }
}

fn draw_frame_handles(
    painter: &egui::Painter,
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
    side_points: usize,
) {
    for handle_idx in 0..frame_handle_count(side_points) {
        let Some((surface_col, surface_row)) =
            frame_handle_surface_coord(handle_idx, side_points, cols, rows)
        else {
            continue;
        };
        let point = mesh_scene[surface_row * cols + surface_col];
        painter.circle_filled(
            point,
            TEXT_OVERLAY_FRAME_HANDLE_RADIUS_PX,
            Color32::from_rgba_unmultiplied(255, 140, 110, 220),
        );
        painter.circle_stroke(
            point,
            TEXT_OVERLAY_FRAME_HANDLE_RADIUS_PX,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 180)),
        );
    }
}

fn draw_grid_handles(
    painter: &egui::Painter,
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
    side_points: usize,
) {
    for handle_idx in 0..grid_handle_count(side_points) {
        let Some((surface_col, surface_row)) =
            grid_handle_surface_coord(handle_idx, side_points, cols, rows)
        else {
            continue;
        };
        let point = mesh_scene[surface_row * cols + surface_col];
        painter.circle_filled(
            point,
            TEXT_OVERLAY_FRAME_HANDLE_RADIUS_PX,
            Color32::from_rgba_unmultiplied(255, 180, 110, 225),
        );
        painter.circle_stroke(
            point,
            TEXT_OVERLAY_FRAME_HANDLE_RADIUS_PX,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 180)),
        );
    }
}

fn draw_rotation_handle(painter: &egui::Painter, quad: &[Pos2; 4], image_rect: Rect) {
    let (corner, handle) = rotation_handle_scene_with_corner(quad, image_rect);
    painter.line_segment(
        [corner, handle],
        Stroke::new(2.0, Color32::from_rgba_unmultiplied(0, 0, 0, 180)),
    );
    painter.circle_filled(
        handle,
        TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX,
        Color32::from_rgba_unmultiplied(90, 185, 255, 235),
    );
    painter.circle_stroke(
        handle,
        TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 210)),
    );
}

fn draw_brush_preview(painter: &egui::Painter, center: Pos2, radius_px: f32) {
    painter.circle_stroke(
        center,
        radius_px.max(2.0),
        Stroke::new(1.5, Color32::from_rgba_unmultiplied(255, 215, 120, 220)),
    );
    painter.circle_stroke(
        center,
        3.0,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 245, 210, 180)),
    );
}

fn hit_test_transform_handle(pointer_scene: Pos2, quad_scene: &[Pos2; 4]) -> Option<usize> {
    for (idx, corner) in quad_scene.iter().enumerate() {
        if pointer_scene.distance(*corner) <= TEXT_OVERLAY_TRANSFORM_HANDLE_RADIUS_PX * 2.0 {
            return Some(idx);
        }
    }
    None
}

fn hit_test_bend_handle(
    pointer_scene: Pos2,
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
) -> Option<usize> {
    for handle_idx in 0..bend_handle_count() {
        let Some((surface_col, surface_row)) = bend_handle_surface_coord(handle_idx, cols, rows)
        else {
            continue;
        };
        let point_idx = surface_row * cols + surface_col;
        if pointer_scene.distance(mesh_scene[point_idx]) <= TEXT_OVERLAY_BEND_HANDLE_RADIUS_PX * 2.0
        {
            return Some(handle_idx);
        }
    }
    None
}

fn hit_test_frame_handle(
    pointer_scene: Pos2,
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
    side_points: usize,
) -> Option<usize> {
    for handle_idx in 0..frame_handle_count(side_points) {
        let Some((surface_col, surface_row)) =
            frame_handle_surface_coord(handle_idx, side_points, cols, rows)
        else {
            continue;
        };
        let point_idx = surface_row * cols + surface_col;
        if pointer_scene.distance(mesh_scene[point_idx])
            <= TEXT_OVERLAY_FRAME_HANDLE_RADIUS_PX * 2.0
        {
            return Some(handle_idx);
        }
    }
    None
}

fn hit_test_grid_handle(
    pointer_scene: Pos2,
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
    side_points: usize,
) -> Option<usize> {
    for handle_idx in 0..grid_handle_count(side_points) {
        let Some((surface_col, surface_row)) =
            grid_handle_surface_coord(handle_idx, side_points, cols, rows)
        else {
            continue;
        };
        let point_idx = surface_row * cols + surface_col;
        if pointer_scene.distance(mesh_scene[point_idx])
            <= TEXT_OVERLAY_FRAME_HANDLE_RADIUS_PX * 2.0
        {
            return Some(handle_idx);
        }
    }
    None
}

fn bend_handle_count() -> usize {
    TEXT_OVERLAY_BEND_HANDLE_COLS
        .saturating_sub(2)
        .saturating_mul(TEXT_OVERLAY_BEND_HANDLE_ROWS.saturating_sub(2))
}

fn frame_handle_count(side_points: usize) -> usize {
    if side_points < 3 {
        0
    } else {
        side_points.saturating_sub(1).saturating_mul(4)
    }
}

fn grid_handle_count(side_points: usize) -> usize {
    if side_points < 2 {
        0
    } else {
        side_points.saturating_mul(side_points)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SampledHandleMode {
    Frame,
    Grid,
}

fn bend_handle_surface_coord(
    handle_idx: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> Option<(usize, usize)> {
    if surface_cols < 3
        || surface_rows < 3
        || TEXT_OVERLAY_BEND_HANDLE_COLS < 3
        || TEXT_OVERLAY_BEND_HANDLE_ROWS < 3
    {
        return None;
    }
    let handle_cols = TEXT_OVERLAY_BEND_HANDLE_COLS - 2;
    let handle_rows = TEXT_OVERLAY_BEND_HANDLE_ROWS - 2;
    if handle_idx >= handle_cols.saturating_mul(handle_rows) {
        return None;
    }
    let handle_row = handle_idx / handle_cols + 1;
    let handle_col = handle_idx % handle_cols + 1;
    Some((
        sample_control_axis_to_surface(handle_col, TEXT_OVERLAY_BEND_HANDLE_COLS, surface_cols),
        sample_control_axis_to_surface(handle_row, TEXT_OVERLAY_BEND_HANDLE_ROWS, surface_rows),
    ))
}

fn frame_handle_surface_coord(
    handle_idx: usize,
    side_points: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> Option<(usize, usize)> {
    if side_points < 3 || surface_cols < 2 || surface_rows < 2 {
        return None;
    }

    let side_points = side_points.min(surface_cols.min(surface_rows));
    let top_count = side_points;
    let right_count = side_points - 1;
    let bottom_count = side_points - 1;
    let left_count = side_points - 2;
    let total = top_count + right_count + bottom_count + left_count;
    if handle_idx >= total {
        return None;
    }

    if handle_idx < top_count {
        return Some((
            sample_control_axis_to_surface(handle_idx, side_points, surface_cols),
            0,
        ));
    }
    let idx = handle_idx - top_count;
    if idx < right_count {
        return Some((
            surface_cols - 1,
            sample_control_axis_to_surface(idx + 1, side_points, surface_rows),
        ));
    }
    let idx = idx - right_count;
    if idx < bottom_count {
        return Some((
            sample_control_axis_to_surface(side_points - 2 - idx, side_points, surface_cols),
            surface_rows - 1,
        ));
    }
    let idx = idx - bottom_count;
    if idx < left_count {
        return Some((
            0,
            sample_control_axis_to_surface(side_points - 2 - idx, side_points, surface_rows),
        ));
    }
    None
}

fn grid_handle_surface_coord(
    handle_idx: usize,
    side_points: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> Option<(usize, usize)> {
    if side_points < 2 || surface_cols < 2 || surface_rows < 2 {
        return None;
    }
    let side_points = side_points.min(surface_cols.min(surface_rows));
    let total = side_points.saturating_mul(side_points);
    if handle_idx >= total {
        return None;
    }
    let row = handle_idx / side_points;
    let col = handle_idx % side_points;
    Some((
        sample_control_axis_to_surface(col, side_points, surface_cols),
        sample_control_axis_to_surface(row, side_points, surface_rows),
    ))
}

fn is_frame_handle_surface_point(
    col: usize,
    row: usize,
    side_points: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> bool {
    (0..frame_handle_count(side_points)).any(|handle_idx| {
        frame_handle_surface_coord(handle_idx, side_points, surface_cols, surface_rows)
            .is_some_and(|coord| coord == (col, row))
    })
}

fn is_grid_handle_surface_point(
    col: usize,
    row: usize,
    side_points: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> bool {
    (0..grid_handle_count(side_points)).any(|handle_idx| {
        grid_handle_surface_coord(handle_idx, side_points, surface_cols, surface_rows)
            .is_some_and(|coord| coord == (col, row))
    })
}

fn sampled_handle_surface_coord(
    mode: SampledHandleMode,
    handle_idx: usize,
    side_points: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> Option<(usize, usize)> {
    match mode {
        SampledHandleMode::Frame => {
            frame_handle_surface_coord(handle_idx, side_points, surface_cols, surface_rows)
        }
        SampledHandleMode::Grid => {
            grid_handle_surface_coord(handle_idx, side_points, surface_cols, surface_rows)
        }
    }
}

fn is_sampled_handle_surface_point(
    mode: SampledHandleMode,
    col: usize,
    row: usize,
    side_points: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> bool {
    match mode {
        SampledHandleMode::Frame => {
            is_frame_handle_surface_point(col, row, side_points, surface_cols, surface_rows)
        }
        SampledHandleMode::Grid => {
            is_grid_handle_surface_point(col, row, side_points, surface_cols, surface_rows)
        }
    }
}

fn sample_control_axis_to_surface(
    control_idx: usize,
    control_count: usize,
    surface_count: usize,
) -> usize {
    if control_count <= 1 || surface_count <= 1 {
        return 0;
    }
    (((surface_count - 1) as f32 * control_idx as f32) / (control_count - 1) as f32)
        .round()
        .clamp(0.0, (surface_count - 1) as f32) as usize
}

fn draw_textured_deform_mesh(
    painter: &egui::Painter,
    texture_id: egui::TextureId,
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
) {
    let mut mesh = Mesh::with_texture(texture_id);
    mesh.reserve_vertices(mesh_scene.len());
    mesh.reserve_triangles((cols.saturating_sub(1)) * (rows.saturating_sub(1)) * 2);

    if cols < 2 || rows < 2 || mesh_scene.len() != cols.saturating_mul(rows) {
        return;
    }

    for row in 0..rows {
        let t = row as f32 / (rows - 1) as f32;
        for col in 0..cols {
            let s = col as f32 / (cols - 1) as f32;
            mesh.vertices.push(egui::epaint::Vertex {
                pos: mesh_scene[row * cols + col],
                uv: Pos2::new(s, t),
                color: Color32::WHITE,
            });
        }
    }

    for row in 0..(rows - 1) {
        for col in 0..(cols - 1) {
            let i0 = (row * cols + col) as u32;
            let i1 = i0 + 1;
            let i2 = ((row + 1) * cols + col) as u32;
            let i3 = i2 + 1;
            mesh.add_triangle(i0, i1, i2);
            mesh.add_triangle(i2, i1, i3);
        }
    }

    painter.add(egui::Shape::mesh(mesh));
}

fn bilinear_quad_point(quad: [Pos2; 4], s: f32, t: f32) -> Pos2 {
    let top = quad[0].lerp(quad[1], s);
    let bottom = quad[3].lerp(quad[2], s);
    top.lerp(bottom, t)
}

fn point_in_quad(point: Pos2, quad: &[Pos2; 4]) -> bool {
    point_in_triangle(point, quad[0], quad[1], quad[2])
        || point_in_triangle(point, quad[0], quad[2], quad[3])
}

fn point_in_triangle(point: Pos2, a: Pos2, b: Pos2, c: Pos2) -> bool {
    fn edge_sign(p: Pos2, p1: Pos2, p2: Pos2) -> f32 {
        (p.x - p2.x) * (p1.y - p2.y) - (p1.x - p2.x) * (p.y - p2.y)
    }

    let d1 = edge_sign(point, a, b);
    let d2 = edge_sign(point, b, c);
    let d3 = edge_sign(point, c, a);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}

fn segment_intersects_quad(start: Pos2, end: Pos2, quad: &[Pos2; 4]) -> bool {
    if point_in_quad(start, quad) || point_in_quad(end, quad) {
        return true;
    }
    for edge_idx in 0..4 {
        let edge_start = quad[edge_idx];
        let edge_end = quad[(edge_idx + 1) % 4];
        if line_segments_intersect(start, end, edge_start, edge_end) {
            return true;
        }
    }
    false
}

fn quads_intersect(a: &[Pos2; 4], b: &[Pos2; 4]) -> bool {
    if !quad_bounds(a).intersects(quad_bounds(b)) {
        return false;
    }
    if a.iter().any(|point| point_in_quad(*point, b))
        || b.iter().any(|point| point_in_quad(*point, a))
    {
        return true;
    }
    for a_idx in 0..4 {
        let a_start = a[a_idx];
        let a_end = a[(a_idx + 1) % 4];
        for b_idx in 0..4 {
            let b_start = b[b_idx];
            let b_end = b[(b_idx + 1) % 4];
            if line_segments_intersect(a_start, a_end, b_start, b_end) {
                return true;
            }
        }
    }
    false
}

fn line_segments_intersect(a1: Pos2, a2: Pos2, b1: Pos2, b2: Pos2) -> bool {
    const EPS: f32 = 0.001;

    fn cross(origin: Pos2, a: Pos2, b: Pos2) -> f32 {
        (a.x - origin.x) * (b.y - origin.y) - (a.y - origin.y) * (b.x - origin.x)
    }

    fn on_segment(a: Pos2, p: Pos2, b: Pos2) -> bool {
        p.x >= a.x.min(b.x) - EPS
            && p.x <= a.x.max(b.x) + EPS
            && p.y >= a.y.min(b.y) - EPS
            && p.y <= a.y.max(b.y) + EPS
    }

    let d1 = cross(a1, a2, b1);
    let d2 = cross(a1, a2, b2);
    let d3 = cross(b1, b2, a1);
    let d4 = cross(b1, b2, a2);

    if ((d1 > EPS && d2 < -EPS) || (d1 < -EPS && d2 > EPS))
        && ((d3 > EPS && d4 < -EPS) || (d3 < -EPS && d4 > EPS))
    {
        return true;
    }

    (d1.abs() <= EPS && on_segment(a1, b1, a2))
        || (d2.abs() <= EPS && on_segment(a1, b2, a2))
        || (d3.abs() <= EPS && on_segment(b1, a1, b2))
        || (d4.abs() <= EPS && on_segment(b1, a2, b2))
}

fn quad_bounds(quad: &[Pos2; 4]) -> Rect {
    let mut min_x = quad[0].x;
    let mut min_y = quad[0].y;
    let mut max_x = quad[0].x;
    let mut max_y = quad[0].y;
    for point in quad.iter().skip(1) {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    Rect::from_min_max(Pos2::new(min_x, min_y), Pos2::new(max_x, max_y))
}

fn quad_center_scene(quad: &[Pos2; 4]) -> Pos2 {
    let (sum_x, sum_y) = quad.iter().fold((0.0f32, 0.0f32), |(acc_x, acc_y), p| {
        (acc_x + p.x, acc_y + p.y)
    });
    Pos2::new(sum_x / 4.0, sum_y / 4.0)
}

fn rotation_handle_scene(quad: &[Pos2; 4], image_rect: Rect) -> Pos2 {
    rotation_handle_scene_with_corner(quad, image_rect).1
}

fn rotation_handle_scene_with_corner(quad: &[Pos2; 4], image_rect: Rect) -> (Pos2, Pos2) {
    let corner_idx = select_rotation_handle_corner(quad, image_rect);
    let corner = quad[corner_idx];
    let center = quad_center_scene(quad);
    let dir = corner - center;
    let len_sq = dir.length_sq();
    if len_sq <= f32::EPSILON {
        return (
            corner,
            corner + Vec2::new(TEXT_OVERLAY_ROTATE_HANDLE_OFFSET_PX, 0.0),
        );
    }
    (
        corner,
        corner + dir / len_sq.sqrt() * TEXT_OVERLAY_ROTATE_HANDLE_OFFSET_PX,
    )
}

fn select_rotation_handle_corner(quad: &[Pos2; 4], image_rect: Rect) -> usize {
    const ROTATION_HANDLE_CORNER_ORDER: [usize; 4] = [1, 0, 3, 2];

    for corner_idx in ROTATION_HANDLE_CORNER_ORDER {
        let handle = rotation_handle_scene_for_corner(quad, corner_idx);
        let handle_rect = Rect::from_center_size(
            handle,
            Vec2::splat(TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX * 2.0),
        );
        if image_rect.contains_rect(handle_rect) {
            return corner_idx;
        }
    }

    1
}

fn rotation_handle_scene_for_corner(quad: &[Pos2; 4], corner_idx: usize) -> Pos2 {
    let corner = quad[corner_idx];
    let center = quad_center_scene(quad);
    let dir = corner - center;
    let len_sq = dir.length_sq();
    if len_sq <= f32::EPSILON {
        return corner + Vec2::new(TEXT_OVERLAY_ROTATE_HANDLE_OFFSET_PX, 0.0);
    }
    corner + dir / len_sq.sqrt() * TEXT_OVERLAY_ROTATE_HANDLE_OFFSET_PX
}

fn pointer_angle_rad(center: Pos2, pointer: Pos2) -> f32 {
    (pointer.y - center.y).atan2(pointer.x - center.x)
}

fn normalize_angle_rad(angle: f32) -> f32 {
    let two_pi = std::f32::consts::TAU;
    ((angle + std::f32::consts::PI).rem_euclid(two_pi)) - std::f32::consts::PI
}

fn normalize_angle_deg(angle: f32) -> f32 {
    ((angle + 180.0).rem_euclid(360.0)) - 180.0
}

fn overlay_quad_scene(overlay: &TypingOverlayRuntime, image_rect: Rect, zoom: f32) -> [Pos2; 4] {
    if overlay.deform_mesh.is_none() {
        return default_overlay_quad_scene(overlay, image_rect, zoom);
    }
    let mesh = overlay_deform_mesh(overlay, image_rect, zoom);
    [
        scene_from_page_px(image_rect, zoom, mesh.point(0, 0)),
        scene_from_page_px(image_rect, zoom, mesh.point(mesh.cols - 1, 0)),
        scene_from_page_px(image_rect, zoom, mesh.point(mesh.cols - 1, mesh.rows - 1)),
        scene_from_page_px(image_rect, zoom, mesh.point(0, mesh.rows - 1)),
    ]
}

fn overlay_scene_geometry(
    overlay: &TypingOverlayRuntime,
    image_rect: Rect,
    zoom: f32,
) -> TypingOverlaySceneGeometry {
    if overlay.deform_mesh.is_none() {
        let quad_scene = default_overlay_quad_scene(overlay, image_rect, zoom);
        return TypingOverlaySceneGeometry {
            quad_scene,
            mesh_scene: vec![quad_scene[0], quad_scene[1], quad_scene[3], quad_scene[2]],
            mesh_cols: 2,
            mesh_rows: 2,
            bounds_rect: quad_bounds(&quad_scene),
        };
    }

    let deform_mesh = overlay_deform_mesh(overlay, image_rect, zoom);
    let quad_scene = [
        scene_from_page_px(image_rect, zoom, deform_mesh.point(0, 0)),
        scene_from_page_px(image_rect, zoom, deform_mesh.point(deform_mesh.cols - 1, 0)),
        scene_from_page_px(
            image_rect,
            zoom,
            deform_mesh.point(deform_mesh.cols - 1, deform_mesh.rows - 1),
        ),
        scene_from_page_px(image_rect, zoom, deform_mesh.point(0, deform_mesh.rows - 1)),
    ];
    let mesh_scene = scene_mesh_points(&deform_mesh, image_rect, zoom);
    let bounds_rect = deform_mesh_bounds(&mesh_scene);
    TypingOverlaySceneGeometry {
        quad_scene,
        mesh_scene,
        mesh_cols: deform_mesh.cols,
        mesh_rows: deform_mesh.rows,
        bounds_rect,
    }
}

fn shift_index_after_remove(index: &mut Option<usize>, removed_idx: usize) {
    if let Some(current_idx) = *index {
        *index = if current_idx == removed_idx {
            None
        } else if current_idx > removed_idx {
            Some(current_idx - 1)
        } else {
            Some(current_idx)
        };
    }
}

fn default_overlay_quad_scene(
    overlay: &TypingOverlayRuntime,
    image_rect: Rect,
    zoom: f32,
) -> [Pos2; 4] {
    let center_page_px = clamp_page_point(
        overlay.center_page_px,
        page_size_from_image_rect(image_rect, zoom),
    );
    let scale = overlay.user_scale.max(0.01);
    let center = scene_from_page_px(image_rect, zoom, center_page_px);
    let size = Vec2::new(
        overlay.size_px[0] as f32 * zoom * scale,
        overlay.size_px[1] as f32 * zoom * scale,
    );
    let rect = Rect::from_center_size(center, size);
    let mut quad = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ];
    if overlay.angle_deg.abs() > f32::EPSILON {
        let radians = overlay.angle_deg.to_radians();
        let (sin_a, cos_a) = radians.sin_cos();
        for point in &mut quad {
            let dx = point.x - center.x;
            let dy = point.y - center.y;
            point.x = center.x + dx * cos_a - dy * sin_a;
            point.y = center.y + dx * sin_a + dy * cos_a;
        }
    }
    quad
}

fn default_overlay_quad_uv(
    overlay: &TypingOverlayRuntime,
    image_rect: Rect,
    zoom: f32,
) -> [[f32; 2]; 4] {
    default_overlay_quad_scene(overlay, image_rect, zoom).map(|point| {
        page_px_to_uv(
            page_px_from_scene(image_rect, zoom, point),
            page_size_from_image_rect(image_rect, zoom),
        )
    })
}

fn default_overlay_quad_mesh(
    overlay: &TypingOverlayRuntime,
    image_rect: Rect,
    zoom: f32,
) -> TypingOverlayDeformMesh {
    let quad_uv = default_overlay_quad_uv(overlay, image_rect, zoom);
    let page_size = page_size_from_image_rect(image_rect, zoom);
    let quad_px = quad_uv.map(|point| uv_to_page_px(point, page_size));
    TypingOverlayDeformMesh::new(
        2,
        2,
        vec![quad_px[0], quad_px[1], quad_px[3], quad_px[2]],
        page_size,
    )
    .unwrap_or_else(|| {
        default_deform_mesh_for_page(overlay.center_page_px, [1, 1], 1.0, 0.0, [1, 1])
    })
}

fn overlay_deform_mesh(
    overlay: &TypingOverlayRuntime,
    image_rect: Rect,
    zoom: f32,
) -> Cow<'_, TypingOverlayDeformMesh> {
    overlay.deform_mesh.as_ref().map_or_else(
        || Cow::Owned(default_overlay_deform_mesh(overlay, image_rect, zoom)),
        Cow::Borrowed,
    )
}

fn overlay_deform_mesh_for_page(
    overlay: &TypingOverlayRuntime,
    page_size: [usize; 2],
) -> Cow<'_, TypingOverlayDeformMesh> {
    overlay.deform_mesh.as_ref().map_or_else(
        || {
            Cow::Owned(default_deform_mesh_for_page(
                overlay.center_page_px,
                overlay.size_px,
                overlay.user_scale,
                overlay.angle_deg,
                page_size,
            ))
        },
        Cow::Borrowed,
    )
}

fn page_size_from_image_rect(image_rect: Rect, zoom: f32) -> [usize; 2] {
    let zoom = zoom.max(f32::EPSILON);
    [
        (image_rect.width() / zoom).round().max(1.0) as usize,
        (image_rect.height() / zoom).round().max(1.0) as usize,
    ]
}

fn scene_from_page_px(image_rect: Rect, zoom: f32, page_px: [f32; 2]) -> Pos2 {
    let page_size = page_size_from_image_rect(image_rect, zoom);
    let clamped = clamp_page_point(page_px, page_size);
    Pos2::new(
        image_rect.left() + clamped[0] * zoom,
        image_rect.top() + clamped[1] * zoom,
    )
}

fn page_px_from_scene(image_rect: Rect, zoom: f32, point: Pos2) -> [f32; 2] {
    let zoom = zoom.max(f32::EPSILON);
    [
        (point.x - image_rect.left()) / zoom,
        (point.y - image_rect.top()) / zoom,
    ]
}

fn scene_from_uv(image_rect: Rect, u: f32, v: f32) -> Pos2 {
    Pos2::new(
        image_rect.left() + u * image_rect.width(),
        image_rect.top() + v * image_rect.height(),
    )
}

fn uv_from_scene(image_rect: Rect, point: Pos2) -> [f32; 2] {
    let w = image_rect.width().max(1.0);
    let h = image_rect.height().max(1.0);
    [
        (point.x - image_rect.left()) / w,
        (point.y - image_rect.top()) / h,
    ]
}

fn sync_overlay_center_from_deform_mesh(overlay: &mut TypingOverlayRuntime, page_size: [usize; 2]) {
    let Some(mesh) = overlay.deform_mesh.as_ref() else {
        return;
    };
    let (sum_x, sum_y) = mesh
        .points_px
        .iter()
        .fold((0.0f32, 0.0f32), |(acc_x, acc_y), p| {
            (acc_x + p[0], acc_y + p[1])
        });
    let count = mesh.points_px.len().max(1) as f32;
    overlay.center_page_px = clamp_page_point([sum_x / count, sum_y / count], page_size);
}

fn snap_overlay_center_to_pixels_if_enabled(
    overlay: &mut TypingOverlayRuntime,
    strict_pixel_movement: bool,
    page_size: [usize; 2],
) {
    if !strict_pixel_movement {
        return;
    }
    let snapped_center = [
        overlay.center_page_px[0].round(),
        overlay.center_page_px[1].round(),
    ];
    if let Some(mesh) = overlay.deform_mesh.as_mut() {
        let dx_px = snapped_center[0] - overlay.center_page_px[0];
        let dy_px = snapped_center[1] - overlay.center_page_px[1];
        if dx_px.abs() > f32::EPSILON || dy_px.abs() > f32::EPSILON {
            mesh.translate(dx_px, dy_px, page_size);
            sync_overlay_center_from_deform_mesh(overlay, page_size);
        }
    } else {
        overlay.center_page_px = clamp_page_point(snapped_center, page_size);
    }
}

fn quantize_drag_page_delta(delta_page_px: [f32; 2], strict_pixel_movement: bool) -> [f32; 2] {
    if !strict_pixel_movement {
        return delta_page_px;
    }
    [
        quantize_drag_page_delta_axis(delta_page_px[0]),
        quantize_drag_page_delta_axis(delta_page_px[1]),
    ]
}

fn quantize_drag_page_delta_axis(delta_page_px: f32) -> f32 {
    if delta_page_px.is_sign_negative() {
        delta_page_px.ceil()
    } else {
        delta_page_px.floor()
    }
}

fn default_overlay_deform_mesh(
    overlay: &TypingOverlayRuntime,
    image_rect: Rect,
    zoom: f32,
) -> TypingOverlayDeformMesh {
    deform_mesh_from_quad(
        default_overlay_quad_uv(overlay, image_rect, zoom),
        TEXT_OVERLAY_DEFORM_SURFACE_COLS,
        TEXT_OVERLAY_DEFORM_SURFACE_ROWS,
        page_size_from_image_rect(image_rect, zoom),
    )
}

fn default_deform_mesh_for_page(
    center_page_px: [f32; 2],
    overlay_size_px: [usize; 2],
    user_scale: f32,
    angle_deg: f32,
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    deform_mesh_from_quad(
        default_quad_uv_for_page(
            center_page_px,
            overlay_size_px,
            user_scale,
            angle_deg,
            page_size,
        ),
        TEXT_OVERLAY_DEFORM_SURFACE_COLS,
        TEXT_OVERLAY_DEFORM_SURFACE_ROWS,
        page_size,
    )
}

fn deform_mesh_from_quad(
    quad_uv: [[f32; 2]; 4],
    cols: usize,
    rows: usize,
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    let mut points_px = Vec::with_capacity(cols.saturating_mul(rows));
    for row in 0..rows {
        let tv = row as f32 / (rows - 1) as f32;
        for col in 0..cols {
            let tu = col as f32 / (cols - 1) as f32;
            points_px.push(uv_to_page_px(
                projective_quad_uv(quad_uv, tu, tv),
                page_size,
            ));
        }
    }
    TypingOverlayDeformMesh::new(cols, rows, points_px, page_size).unwrap_or_else(|| {
        TypingOverlayDeformMesh {
            cols: 2,
            rows: 2,
            points_px: quad_uv
                .into_iter()
                .map(|point| uv_to_page_px(point, page_size))
                .collect(),
        }
    })
}

fn normalize_deform_mesh_resolution(
    mesh: &TypingOverlayDeformMesh,
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    if mesh.cols == TEXT_OVERLAY_DEFORM_SURFACE_COLS
        && mesh.rows == TEXT_OVERLAY_DEFORM_SURFACE_ROWS
    {
        return mesh.clone();
    }

    let mut points_px = Vec::with_capacity(
        TEXT_OVERLAY_DEFORM_SURFACE_COLS.saturating_mul(TEXT_OVERLAY_DEFORM_SURFACE_ROWS),
    );
    for row in 0..TEXT_OVERLAY_DEFORM_SURFACE_ROWS {
        let tv = row as f32 / (TEXT_OVERLAY_DEFORM_SURFACE_ROWS - 1) as f32;
        for col in 0..TEXT_OVERLAY_DEFORM_SURFACE_COLS {
            let tu = col as f32 / (TEXT_OVERLAY_DEFORM_SURFACE_COLS - 1) as f32;
            points_px.push(sample_deform_mesh_page_px_for_size(mesh, tu, tv, page_size));
        }
    }

    TypingOverlayDeformMesh::new(
        TEXT_OVERLAY_DEFORM_SURFACE_COLS,
        TEXT_OVERLAY_DEFORM_SURFACE_ROWS,
        points_px,
        page_size,
    )
    .unwrap_or_else(|| default_deform_mesh_for_page([0.5, 0.5], [1, 1], 1.0, 0.0, [1, 1]))
}

fn scene_mesh_points(mesh: &TypingOverlayDeformMesh, image_rect: Rect, zoom: f32) -> Vec<Pos2> {
    mesh.points_px
        .iter()
        .map(|&point| scene_from_page_px(image_rect, zoom, point))
        .collect()
}

fn mesh_page_size_hint(mesh: &TypingOverlayDeformMesh) -> [usize; 2] {
    let bounds = deform_mesh_bounds_px(mesh);
    [
        bounds.max.x.ceil().max(1.0) as usize,
        bounds.max.y.ceil().max(1.0) as usize,
    ]
}

fn deform_mesh_bounds_px(mesh: &TypingOverlayDeformMesh) -> Rect {
    let Some(first) = mesh.points_px.first().copied() else {
        return Rect::NOTHING;
    };
    let mut min_x = first[0];
    let mut max_x = first[0];
    let mut min_y = first[1];
    let mut max_y = first[1];
    for point in mesh.points_px.iter().skip(1) {
        min_x = min_x.min(point[0]);
        max_x = max_x.max(point[0]);
        min_y = min_y.min(point[1]);
        max_y = max_y.max(point[1]);
    }
    Rect::from_min_max(Pos2::new(min_x, min_y), Pos2::new(max_x, max_y))
}

fn uv_to_page_px(uv: [f32; 2], page_size: [usize; 2]) -> [f32; 2] {
    [
        clamp_overlay_uv_coord(uv[0]) * page_size[0].max(1) as f32,
        clamp_overlay_uv_coord(uv[1]) * page_size[1].max(1) as f32,
    ]
}

fn page_px_to_uv(page_px: [f32; 2], page_size: [usize; 2]) -> [f32; 2] {
    let clamped = clamp_page_point(page_px, page_size);
    [
        clamped[0] / page_size[0].max(1) as f32,
        clamped[1] / page_size[1].max(1) as f32,
    ]
}

fn clamp_page_point(point: [f32; 2], page_size: [usize; 2]) -> [f32; 2] {
    [
        clamp_overlay_page_coord(point[0], page_size[0]),
        clamp_overlay_page_coord(point[1], page_size[1]),
    ]
}

fn clamp_quad_uv(quad: [[f32; 2]; 4]) -> [[f32; 2]; 4] {
    quad.map(clamp_uv_point)
}

fn clamp_uv_point(point: [f32; 2]) -> [f32; 2] {
    [
        clamp_overlay_uv_coord(point[0]),
        clamp_overlay_uv_coord(point[1]),
    ]
}

fn deform_mesh_bounds_uv(mesh: &TypingOverlayDeformMesh, page_size: [usize; 2]) -> Rect {
    let Some(first) = mesh.points_px.first().copied() else {
        return Rect::NOTHING;
    };
    let first_uv = page_px_to_uv(first, page_size);
    let mut min_u = first_uv[0];
    let mut max_u = first_uv[0];
    let mut min_v = first_uv[1];
    let mut max_v = first_uv[1];
    for point in mesh.points_px.iter().skip(1) {
        let uv = page_px_to_uv(*point, page_size);
        min_u = min_u.min(uv[0]);
        max_u = max_u.max(uv[0]);
        min_v = min_v.min(uv[1]);
        max_v = max_v.max(uv[1]);
    }
    Rect::from_min_max(Pos2::new(min_u, min_v), Pos2::new(max_u, max_v))
}

fn mesh_cell_quad_scene(mesh_scene: &[Pos2], cols: usize, col: usize, row: usize) -> [Pos2; 4] {
    let idx = |c: usize, r: usize| -> usize { r * cols + c };
    [
        mesh_scene[idx(col, row)],
        mesh_scene[idx(col + 1, row)],
        mesh_scene[idx(col + 1, row + 1)],
        mesh_scene[idx(col, row + 1)],
    ]
}

fn build_mesh_occluder_quads(mesh_scene: &[Pos2], cols: usize, rows: usize) -> Vec<[Pos2; 4]> {
    if cols < 2 || rows < 2 {
        return Vec::new();
    }
    let mut quads = Vec::with_capacity(
        cols.saturating_sub(1)
            .saturating_mul(rows.saturating_sub(1)),
    );
    for row in 0..(rows - 1) {
        for col in 0..(cols - 1) {
            quads.push(mesh_cell_quad_scene(mesh_scene, cols, col, row));
        }
    }
    quads
}

fn deform_mesh_contains_point(mesh_scene: &[Pos2], cols: usize, rows: usize, point: Pos2) -> bool {
    if cols < 2 || rows < 2 {
        return false;
    }
    if !deform_mesh_bounds(mesh_scene).contains(point) {
        return false;
    }
    for row in 0..(rows - 1) {
        for col in 0..(cols - 1) {
            if point_in_quad(point, &mesh_cell_quad_scene(mesh_scene, cols, col, row)) {
                return true;
            }
        }
    }
    false
}

fn sample_deform_mesh_page_px(mesh: &TypingOverlayDeformMesh, tu: f32, tv: f32) -> [f32; 2] {
    sample_deform_mesh_page_px_for_size(mesh, tu, tv, mesh_page_size_hint(mesh))
}

fn sample_deform_mesh_page_px_for_size(
    mesh: &TypingOverlayDeformMesh,
    tu: f32,
    tv: f32,
    page_size: [usize; 2],
) -> [f32; 2] {
    if mesh.cols < 2 || mesh.rows < 2 {
        return [0.5, 0.5];
    }
    let u = tu.clamp(0.0, 1.0) * (mesh.cols - 1) as f32;
    let v = tv.clamp(0.0, 1.0) * (mesh.rows - 1) as f32;
    let col0 = u.floor().clamp(0.0, (mesh.cols - 2) as f32) as usize;
    let row0 = v.floor().clamp(0.0, (mesh.rows - 2) as f32) as usize;
    let col1 = (col0 + 1).min(mesh.cols - 1);
    let row1 = (row0 + 1).min(mesh.rows - 1);
    let local_u = u - col0 as f32;
    let local_v = v - row0 as f32;
    let quad = [
        mesh.point(col0, row0),
        mesh.point(col1, row0),
        mesh.point(col1, row1),
        mesh.point(col0, row1),
    ];
    clamp_page_point(bilinear_quad_page_px(quad, local_u, local_v), page_size)
}

fn sample_deform_mesh_uv(
    mesh: &TypingOverlayDeformMesh,
    tu: f32,
    tv: f32,
    page_size: [usize; 2],
) -> [f32; 2] {
    page_px_to_uv(
        sample_deform_mesh_page_px_for_size(mesh, tu, tv, page_size),
        page_size,
    )
}

fn mesh_grid_tuv(mesh: &TypingOverlayDeformMesh, col: usize, row: usize) -> [f32; 2] {
    let tu = if mesh.cols <= 1 {
        0.0
    } else {
        col as f32 / (mesh.cols - 1) as f32
    };
    let tv = if mesh.rows <= 1 {
        0.0
    } else {
        row as f32 / (mesh.rows - 1) as f32
    };
    [tu, tv]
}

fn apply_bend_handle_drag(
    mesh: &TypingOverlayDeformMesh,
    handle_idx: usize,
    delta_page_px: [f32; 2],
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    let Some((handle_col, handle_row)) =
        bend_handle_surface_coord(handle_idx, mesh.cols, mesh.rows)
    else {
        return mesh.clone();
    };

    let center_tuv = mesh_grid_tuv(mesh, handle_col, handle_row);
    let radius_u = 1.35 / (TEXT_OVERLAY_BEND_HANDLE_COLS.saturating_sub(1)).max(1) as f32;
    let radius_v = 1.35 / (TEXT_OVERLAY_BEND_HANDLE_ROWS.saturating_sub(1)).max(1) as f32;
    let mut next_points = mesh.points_px.clone();

    for row in 0..mesh.rows {
        for col in 0..mesh.cols {
            let [tu, tv] = mesh_grid_tuv(mesh, col, row);
            let du = (tu - center_tuv[0]) / radius_u.max(1e-4);
            let dv = (tv - center_tuv[1]) / radius_v.max(1e-4);
            let dist = (du * du + dv * dv).sqrt();
            if dist >= 1.0 {
                continue;
            }
            let influence = 1.0 - dist;
            let weight = influence * influence * (3.0 - 2.0 * influence);
            let point_idx = row * mesh.cols + col;
            next_points[point_idx] = clamp_page_point(
                [
                    next_points[point_idx][0] + delta_page_px[0] * weight,
                    next_points[point_idx][1] + delta_page_px[1] * weight,
                ],
                page_size,
            );
        }
    }

    TypingOverlayDeformMesh::new(mesh.cols, mesh.rows, next_points, page_size)
        .unwrap_or_else(|| mesh.clone())
}

fn apply_sampled_handle_drag(
    mesh: &TypingOverlayDeformMesh,
    mode: SampledHandleMode,
    side_points: usize,
    handle_idx: usize,
    pull_neighbor_handles: bool,
    delta_page_px: [f32; 2],
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    let Some((handle_col, handle_row)) =
        sampled_handle_surface_coord(mode, handle_idx, side_points, mesh.cols, mesh.rows)
    else {
        return mesh.clone();
    };

    let center_tuv = mesh_grid_tuv(mesh, handle_col, handle_row);
    let spacing = 1.0 / (side_points.saturating_sub(1)).max(1) as f32;
    let radius_u = (spacing * 1.75).max(1e-4);
    let radius_v = (spacing * 1.75).max(1e-4);
    let mut next_points = mesh.points_px.clone();

    for row in 0..mesh.rows {
        for col in 0..mesh.cols {
            if !pull_neighbor_handles
                && (col != handle_col || row != handle_row)
                && is_sampled_handle_surface_point(
                    mode,
                    col,
                    row,
                    side_points,
                    mesh.cols,
                    mesh.rows,
                )
            {
                continue;
            }
            let [tu, tv] = mesh_grid_tuv(mesh, col, row);
            let du = (tu - center_tuv[0]) / radius_u;
            let dv = (tv - center_tuv[1]) / radius_v;
            let dist = (du * du + dv * dv).sqrt();
            if dist >= 1.0 {
                continue;
            }
            let influence = 1.0 - dist;
            let weight = influence * influence * (3.0 - 2.0 * influence);
            let point_idx = row * mesh.cols + col;
            next_points[point_idx] = clamp_page_point(
                [
                    next_points[point_idx][0] + delta_page_px[0] * weight,
                    next_points[point_idx][1] + delta_page_px[1] * weight,
                ],
                page_size,
            );
        }
    }

    TypingOverlayDeformMesh::new(mesh.cols, mesh.rows, next_points, page_size)
        .unwrap_or_else(|| mesh.clone())
}

fn apply_perspective_corner_drag(
    mesh: &TypingOverlayDeformMesh,
    handle_idx: usize,
    delta_page_px: [f32; 2],
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    if handle_idx >= 4 || mesh.cols < 2 || mesh.rows < 2 {
        return mesh.clone();
    }

    let mut next_points = Vec::with_capacity(mesh.points_px.len());
    for row in 0..mesh.rows {
        for col in 0..mesh.cols {
            let [tu, tv] = mesh_grid_tuv(mesh, col, row);
            let weights = [
                (1.0 - tu) * (1.0 - tv),
                tu * (1.0 - tv),
                tu * tv,
                (1.0 - tu) * tv,
            ];
            let influence = weights[handle_idx];
            next_points.push(clamp_page_point(
                [
                    mesh.point(col, row)[0] + delta_page_px[0] * influence,
                    mesh.point(col, row)[1] + delta_page_px[1] * influence,
                ],
                page_size,
            ));
        }
    }

    TypingOverlayDeformMesh::new(mesh.cols, mesh.rows, next_points, page_size)
        .unwrap_or_else(|| mesh.clone())
}

// Brush deformation depends on distinct input spaces (scene pointer, mesh state, page rect, zoom, tool settings).
#[allow(clippy::too_many_arguments)]
fn apply_brush_deform_drag(
    mode: TypingDeformMode,
    mesh: &TypingOverlayDeformMesh,
    default_mesh: &TypingOverlayDeformMesh,
    brush_center_scene: Pos2,
    pointer_scene: Pos2,
    image_rect: Rect,
    zoom: f32,
    settings: &TypingDeformToolSettings,
) -> TypingOverlayDeformMesh {
    if !mode.is_brush_mode() || mesh.cols < 2 || mesh.rows < 2 {
        return mesh.clone();
    }

    let page_size = page_size_from_image_rect(image_rect, zoom);
    let delta_page_px = [
        pointer_scene.x - brush_center_scene.x,
        pointer_scene.y - brush_center_scene.y,
    ];
    let delta_scene = pointer_scene - brush_center_scene;
    let radius_px = settings.brush_radius_px.max(4.0);
    let strength = settings.brush_strength.max(0.01);
    let center_page_px = page_px_from_scene(image_rect, zoom, brush_center_scene);
    let radial_drag = (delta_scene.length() / radius_px).min(1.0);
    let mut next_points = mesh.points_px.clone();

    for row in 0..mesh.rows {
        for col in 0..mesh.cols {
            let idx = row * mesh.cols + col;
            let point_page_px = mesh.point(col, row);
            let point_scene = scene_from_page_px(image_rect, zoom, point_page_px);
            let to_center = point_scene - brush_center_scene;
            let dist_px = to_center.length();
            if dist_px > radius_px {
                continue;
            }
            let influence = 1.0 - dist_px / radius_px;
            let weight = influence * influence * (3.0 - 2.0 * influence) * strength;
            let next_page_px = match mode {
                TypingDeformMode::Bulge => {
                    let dir = normalize_or_zero_page([
                        point_page_px[0] - center_page_px[0],
                        point_page_px[1] - center_page_px[1],
                    ]);
                    let amount = TEXT_OVERLAY_BULGE_PINCH_BRUSH_SCALE
                        * weight
                        * radial_drag
                        * page_size[0].max(page_size[1]).max(1) as f32;
                    [
                        point_page_px[0] + dir[0] * amount,
                        point_page_px[1] + dir[1] * amount,
                    ]
                }
                TypingDeformMode::Pinch => {
                    let dir = normalize_or_zero_page([
                        center_page_px[0] - point_page_px[0],
                        center_page_px[1] - point_page_px[1],
                    ]);
                    let amount = TEXT_OVERLAY_BULGE_PINCH_BRUSH_SCALE
                        * weight
                        * radial_drag
                        * page_size[0].max(page_size[1]).max(1) as f32;
                    [
                        point_page_px[0] + dir[0] * amount,
                        point_page_px[1] + dir[1] * amount,
                    ]
                }
                TypingDeformMode::Push => [
                    point_page_px[0] + delta_page_px[0] * weight,
                    point_page_px[1] + delta_page_px[1] * weight,
                ],
                TypingDeformMode::Twirl => {
                    let angle = delta_scene.x / radius_px * 1.6 * weight;
                    rotate_page_around_center(point_page_px, center_page_px, angle)
                }
                TypingDeformMode::Restore => {
                    let target = sample_deform_mesh_page_px(
                        default_mesh,
                        mesh_grid_tuv(mesh, col, row)[0],
                        mesh_grid_tuv(mesh, col, row)[1],
                    );
                    [
                        lerp(point_page_px[0], target[0], weight.min(1.0)),
                        lerp(point_page_px[1], target[1], weight.min(1.0)),
                    ]
                }
                TypingDeformMode::Smooth => {
                    let target = smooth_mesh_point(mesh, default_mesh, col, row);
                    [
                        lerp(point_page_px[0], target[0], (weight * 0.85).min(1.0)),
                        lerp(point_page_px[1], target[1], (weight * 0.85).min(1.0)),
                    ]
                }
                TypingDeformMode::Stretch => {
                    let dir = normalize_or_zero_scene(delta_scene);
                    let stretch = (delta_scene.length() / radius_px).min(1.0) * 0.08 * weight;
                    let offset = [
                        (point_page_px[0] - center_page_px[0])
                            * dir.x.abs()
                            * stretch
                            * delta_scene.x.signum(),
                        (point_page_px[1] - center_page_px[1])
                            * dir.y.abs()
                            * stretch
                            * delta_scene.y.signum(),
                    ];
                    [point_page_px[0] + offset[0], point_page_px[1] + offset[1]]
                }
                TypingDeformMode::Fold => {
                    let axis = normalize_or_zero_scene(delta_scene);
                    let signed_side = if dist_px <= f32::EPSILON {
                        0.0
                    } else {
                        (to_center.x * axis.y - to_center.y * axis.x).signum()
                    };
                    let fold_dir = egui::vec2(-axis.y, axis.x) * signed_side;
                    [
                        point_page_px[0] + fold_dir.x * 0.06 * weight,
                        point_page_px[1] + fold_dir.y * 0.06 * weight,
                    ]
                }
                _ => point_page_px,
            };
            next_points[idx] = clamp_page_point(next_page_px, page_size);
        }
    }

    TypingOverlayDeformMesh::new(mesh.cols, mesh.rows, next_points, page_size)
        .unwrap_or_else(|| mesh.clone())
}

fn smooth_mesh_point(
    mesh: &TypingOverlayDeformMesh,
    default_mesh: &TypingOverlayDeformMesh,
    col: usize,
    row: usize,
) -> [f32; 2] {
    let mut sum = [0.0f32; 2];
    let mut count = 0.0f32;
    let row_start = row.saturating_sub(1);
    let row_end = (row + 1).min(mesh.rows - 1);
    let col_start = col.saturating_sub(1);
    let col_end = (col + 1).min(mesh.cols - 1);
    for rr in row_start..=row_end {
        for cc in col_start..=col_end {
            let point = mesh.point(cc, rr);
            sum[0] += point[0];
            sum[1] += point[1];
            count += 1.0;
        }
    }
    if count <= 0.0 {
        return mesh.point(col, row);
    }
    let avg = [sum[0] / count, sum[1] / count];
    let default_point = sample_deform_mesh_page_px(
        default_mesh,
        mesh_grid_tuv(mesh, col, row)[0],
        mesh_grid_tuv(mesh, col, row)[1],
    );
    [
        lerp(avg[0], default_point[0], 0.15),
        lerp(avg[1], default_point[1], 0.15),
    ]
}

fn rotate_page_around_center(
    point_page_px: [f32; 2],
    center_page_px: [f32; 2],
    angle_rad: f32,
) -> [f32; 2] {
    let dx = point_page_px[0] - center_page_px[0];
    let dy = point_page_px[1] - center_page_px[1];
    let (sin_a, cos_a) = angle_rad.sin_cos();
    [
        center_page_px[0] + dx * cos_a - dy * sin_a,
        center_page_px[1] + dx * sin_a + dy * cos_a,
    ]
}

fn normalize_or_zero_page(v: [f32; 2]) -> [f32; 2] {
    let len = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if len <= 1e-6 {
        [0.0, 0.0]
    } else {
        [v[0] / len, v[1] / len]
    }
}

fn normalize_or_zero_scene(v: Vec2) -> Vec2 {
    let len = v.length();
    if len <= 1e-6 { Vec2::ZERO } else { v / len }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

fn projective_quad_uv(quad_uv: [[f32; 2]; 4], tu: f32, tv: f32) -> [f32; 2] {
    let p0 = quad_uv[0];
    let p1 = quad_uv[1];
    let p2 = quad_uv[2];
    let p3 = quad_uv[3];

    let a1 = p2[0] - p1[0];
    let b1 = p2[0] - p3[0];
    let c1 = p1[0] + p3[0] - p0[0] - p2[0];
    let a2 = p2[1] - p1[1];
    let b2 = p2[1] - p3[1];
    let c2 = p1[1] + p3[1] - p0[1] - p2[1];
    let det = a1 * b2 - a2 * b1;

    if det.abs() <= 1e-6 {
        return export_bilinear_quad_uv(quad_uv, tu, tv);
    }

    let g = (c1 * b2 - c2 * b1) / det;
    let h = (a1 * c2 - a2 * c1) / det;

    let a = p1[0] * (g + 1.0) - p0[0];
    let b = p3[0] * (h + 1.0) - p0[0];
    let c = p0[0];
    let d = p1[1] * (g + 1.0) - p0[1];
    let e = p3[1] * (h + 1.0) - p0[1];
    let f = p0[1];

    let u = tu.clamp(0.0, 1.0);
    let v = tv.clamp(0.0, 1.0);
    let denom = g * u + h * v + 1.0;
    if denom.abs() <= 1e-6 {
        return export_bilinear_quad_uv(quad_uv, u, v);
    }
    [(a * u + b * v + c) / denom, (d * u + e * v + f) / denom]
}

fn deform_mesh_bounds(mesh_scene: &[Pos2]) -> Rect {
    let Some(first) = mesh_scene.first().copied() else {
        return Rect::NOTHING;
    };
    let mut min_x = first.x;
    let mut min_y = first.y;
    let mut max_x = first.x;
    let mut max_y = first.y;
    for point in mesh_scene.iter().skip(1) {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    Rect::from_min_max(Pos2::new(min_x, min_y), Pos2::new(max_x, max_y))
}

fn deform_mesh_center_scene(mesh_scene: &[Pos2]) -> Pos2 {
    let (sum_x, sum_y) = mesh_scene
        .iter()
        .fold((0.0f32, 0.0f32), |(acc_x, acc_y), p| {
            (acc_x + p.x, acc_y + p.y)
        });
    let count = mesh_scene.len().max(1) as f32;
    Pos2::new(sum_x / count, sum_y / count)
}

fn rotate_mesh_scene(mesh_scene: &[Pos2], center: Pos2, angle_rad: f32) -> Vec<Pos2> {
    let (sin_a, cos_a) = angle_rad.sin_cos();
    mesh_scene
        .iter()
        .map(|point| {
            let dx = point.x - center.x;
            let dy = point.y - center.y;
            Pos2::new(
                center.x + dx * cos_a - dy * sin_a,
                center.y + dx * sin_a + dy * cos_a,
            )
        })
        .collect()
}

fn overlay_uv_min() -> f32 {
    -TEXT_OVERLAY_MAX_OUT_OF_BOUNDS_UV
}

fn overlay_uv_max() -> f32 {
    1.0 + TEXT_OVERLAY_MAX_OUT_OF_BOUNDS_UV
}

fn clamp_overlay_uv_coord(value: f32) -> f32 {
    value.clamp(overlay_uv_min(), overlay_uv_max())
}

fn clamp_overlay_page_coord(value: f32, side_px: usize) -> f32 {
    let side_px = side_px.max(1) as f32;
    value.clamp(overlay_uv_min() * side_px, overlay_uv_max() * side_px)
}

fn draw_layout_editor_vector_lines_tab(ui: &mut egui::Ui, editor: &mut TypingLayoutEditorState) {
    ensure_layout_editor_has_line(editor);
    ui.label(egui::RichText::new("Строки").strong());
    ui.add_space(6.0);
    egui::ScrollArea::vertical()
        .id_salt("typing_layout_editor_vector_lines_scroll")
        .show(ui, |ui| {
            let mut remove_idx: Option<usize> = None;
            for idx in 0..editor.lines.len() {
                let selected = editor.active_line_idx == idx;
                let frame = if selected {
                    egui::Frame::default()
                        .fill(Color32::from_rgb(45, 72, 98))
                        .stroke(Stroke::new(1.4, Color32::from_rgb(120, 210, 255)))
                } else {
                    egui::Frame::default()
                        .fill(Color32::from_rgb(38, 40, 44))
                        .stroke(Stroke::new(1.0, Color32::from_rgb(86, 90, 98)))
                };
                frame
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let label = editor
                                .lines
                                .get(idx)
                                .map(|line| line.label.as_str())
                                .unwrap_or("Строка");
                            if ui.selectable_label(selected, label).clicked() {
                                editor.active_line_idx = idx;
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if selected && ui.small_button("×").clicked() {
                                        remove_idx = Some(idx);
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(5.0);
            }
            if let Some(idx) = remove_idx {
                remove_layout_editor_line(editor, idx);
            }
            let plus_response = egui::Frame::default()
                .fill(Color32::from_rgb(34, 35, 38))
                .stroke(Stroke::new(1.0, Color32::from_rgb(92, 96, 105)))
                .inner_margin(egui::Margin::symmetric(8, 8))
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        if ui.button("+").clicked() {
                            let next_idx = editor.lines.len() + 1;
                            editor.lines.push(TypingLayoutEditorLine {
                                label: format!("Строка {next_idx}"),
                                points: Vec::new(),
                                corner_smoothing_px: 0.0,
                                text_direction: TextVectorLineTextDirection::LeftToRight,
                                distance_mode: TextVectorLineDistanceMode::ByLineLength,
                                flip_text: false,
                            });
                            editor.active_line_idx = editor.lines.len().saturating_sub(1);
                        }
                    });
                });
            if plus_response.response.clicked() {
                let next_idx = editor.lines.len() + 1;
                editor.lines.push(TypingLayoutEditorLine {
                    label: format!("Строка {next_idx}"),
                    points: Vec::new(),
                    corner_smoothing_px: 0.0,
                    text_direction: TextVectorLineTextDirection::LeftToRight,
                    distance_mode: TextVectorLineDistanceMode::ByLineLength,
                    flip_text: false,
                });
                editor.active_line_idx = editor.lines.len().saturating_sub(1);
            }
        });
    ui.separator();
    ui.label(egui::RichText::new("Параметры строки").strong());
    if let Some(line) = editor.lines.get_mut(editor.active_line_idx) {
        ui.add(WheelSlider::new(&mut line.corner_smoothing_px, 0.0..=256.0).text("Сглаживание"));
        egui::ComboBox::from_label("Направление текста")
            .selected_text(vector_line_text_direction_label(line.text_direction))
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut line.text_direction,
                    TextVectorLineTextDirection::LeftToRight,
                    vector_line_text_direction_label(TextVectorLineTextDirection::LeftToRight),
                );
                ui.selectable_value(
                    &mut line.text_direction,
                    TextVectorLineTextDirection::RightToLeft,
                    vector_line_text_direction_label(TextVectorLineTextDirection::RightToLeft),
                );
            });
        egui::ComboBox::from_label("Режим расстояния")
            .selected_text(vector_line_distance_mode_label(line.distance_mode))
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut line.distance_mode,
                    TextVectorLineDistanceMode::ByLineLength,
                    vector_line_distance_mode_label(TextVectorLineDistanceMode::ByLineLength),
                );
                ui.selectable_value(
                    &mut line.distance_mode,
                    TextVectorLineDistanceMode::MinimumPreviousDistance,
                    vector_line_distance_mode_label(
                        TextVectorLineDistanceMode::MinimumPreviousDistance,
                    ),
                );
            });
        ui.checkbox(&mut line.flip_text, "Перевернуть текст");
    }
}

fn vector_line_text_direction_label(direction: TextVectorLineTextDirection) -> &'static str {
    match direction {
        TextVectorLineTextDirection::LeftToRight => "Слева направо",
        TextVectorLineTextDirection::RightToLeft => "Справа налево",
    }
}

fn vector_line_distance_mode_label(mode: TextVectorLineDistanceMode) -> &'static str {
    match mode {
        TextVectorLineDistanceMode::ByLineLength => "По длине линии",
        TextVectorLineDistanceMode::MinimumPreviousDistance => "Мин. расстояние до символа",
    }
}

fn ensure_layout_editor_has_line(editor: &mut TypingLayoutEditorState) {
    if editor.lines.is_empty() {
        editor.lines.push(TypingLayoutEditorLine {
            label: "Строка 1".to_string(),
            points: Vec::new(),
            corner_smoothing_px: 0.0,
            text_direction: TextVectorLineTextDirection::LeftToRight,
            distance_mode: TextVectorLineDistanceMode::ByLineLength,
            flip_text: false,
        });
    }
    editor.active_line_idx = editor
        .active_line_idx
        .min(editor.lines.len().saturating_sub(1));
}

fn remove_layout_editor_line(editor: &mut TypingLayoutEditorState, idx: usize) {
    if editor.lines.len() <= 1 {
        if let Some(line) = editor.lines.first_mut() {
            line.points.clear();
            line.corner_smoothing_px = 0.0;
            line.text_direction = TextVectorLineTextDirection::LeftToRight;
            line.distance_mode = TextVectorLineDistanceMode::ByLineLength;
            line.flip_text = false;
        }
        editor.active_line_idx = 0;
        return;
    }
    if idx < editor.lines.len() {
        editor.lines.remove(idx);
    }
    for (line_idx, line) in editor.lines.iter_mut().enumerate() {
        line.label = format!("Строка {}", line_idx + 1);
    }
    editor.active_line_idx = editor
        .active_line_idx
        .min(editor.lines.len().saturating_sub(1));
}

fn layout_editor_lines_from_vector_layout(
    layout: TextVectorLinesLayoutParams,
) -> Vec<TypingLayoutEditorLine> {
    layout
        .lines
        .into_iter()
        .enumerate()
        .map(|(idx, line)| TypingLayoutEditorLine {
            label: format!("Строка {}", idx + 1),
            points: line
                .points
                .into_iter()
                .map(|point| egui::pos2(point.x, point.y))
                .collect(),
            corner_smoothing_px: line.corner_smoothing_px.clamp(0.0, 256.0),
            text_direction: line.text_direction,
            distance_mode: line.distance_mode,
            flip_text: line.flip_text,
        })
        .collect()
}

fn vector_lines_layout_from_editor(
    editor: &TypingLayoutEditorState,
) -> TextVectorLinesLayoutParams {
    let width_px = rounded_positive_f32_to_u32(editor.frame_page_rect.width());
    let height_px = rounded_positive_f32_to_u32(editor.frame_page_rect.height());
    let max_x = width_px as f32;
    let max_y = height_px as f32;
    let lines = editor
        .lines
        .iter()
        .map(|line| TextVectorLine {
            points: line
                .points
                .iter()
                .map(|point| TextVectorPoint {
                    x: point.x.clamp(0.0, max_x),
                    y: point.y.clamp(0.0, max_y),
                })
                .collect(),
            corner_smoothing_px: line.corner_smoothing_px.clamp(0.0, 256.0),
            text_direction: line.text_direction,
            distance_mode: line.distance_mode,
            flip_text: line.flip_text,
        })
        .collect();
    TextVectorLinesLayoutParams {
        width_px,
        height_px,
        lines,
        ..TextVectorLinesLayoutParams::default()
    }
}

fn render_data_with_vector_layout(
    render_data: &Value,
    layout: &TextVectorLinesLayoutParams,
) -> Option<Value> {
    let mut updated = render_data.clone();
    let obj = updated.as_object_mut()?;
    let text_params = obj.get_mut("text_params")?.as_object_mut()?;
    text_params.insert(
        "text_layout_mode".to_string(),
        Value::from("custom_vector_lines"),
    );
    text_params.insert("text_line_mode".to_string(), Value::from("horizontal"));
    text_params.insert("width_px".to_string(), Value::from(layout.width_px.max(1)));
    text_params.insert(
        "vector_lines_layout".to_string(),
        vector_lines_layout_to_value_for_render_data(layout),
    );
    Some(updated)
}

fn vector_lines_layout_to_value_for_render_data(layout: &TextVectorLinesLayoutParams) -> Value {
    let lines = layout
        .lines
        .iter()
        .map(|line| {
            let points = line
                .points
                .iter()
                .map(|point| json!({ "x": point.x, "y": point.y }))
                .collect::<Vec<_>>();
            json!({
                "points": points,
                "corner_smoothing_px": line.corner_smoothing_px,
                "text_direction": vector_line_text_direction_to_str(line.text_direction),
                "distance_mode": vector_line_distance_mode_to_str(line.distance_mode),
                "flip_text": line.flip_text,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "width_px": layout.width_px.max(1),
        "height_px": layout.height_px.max(1),
        "use_tangent_rotation": layout.use_tangent_rotation,
        "static_rotation_rad": layout.static_rotation_rad,
        "normal_offset_px": layout.normal_offset_px,
        "letter_spacing_mul": layout.letter_spacing_mul,
        "letter_spacing_px": layout.letter_spacing_px,
        "lines": lines,
    })
}

fn vector_line_text_direction_to_str(direction: TextVectorLineTextDirection) -> &'static str {
    match direction {
        TextVectorLineTextDirection::LeftToRight => "left_to_right",
        TextVectorLineTextDirection::RightToLeft => "right_to_left",
    }
}

fn vector_line_text_direction_from_value(value: Option<&Value>) -> TextVectorLineTextDirection {
    match value.and_then(Value::as_str).unwrap_or("left_to_right") {
        "right_to_left" | "rtl" => TextVectorLineTextDirection::RightToLeft,
        "left_to_right" | "ltr" => TextVectorLineTextDirection::LeftToRight,
        _ => TextVectorLineTextDirection::LeftToRight,
    }
}

fn vector_line_distance_mode_to_str(mode: TextVectorLineDistanceMode) -> &'static str {
    match mode {
        TextVectorLineDistanceMode::ByLineLength => "by_line_length",
        TextVectorLineDistanceMode::MinimumPreviousDistance => "minimum_previous_distance",
    }
}

fn vector_line_distance_mode_from_value(value: Option<&Value>) -> TextVectorLineDistanceMode {
    match value.and_then(Value::as_str).unwrap_or("by_line_length") {
        "minimum_previous_distance" | "min_previous_distance" | "minimum_distance" => {
            TextVectorLineDistanceMode::MinimumPreviousDistance
        }
        "by_line_length" | "line_length" => TextVectorLineDistanceMode::ByLineLength,
        _ => TextVectorLineDistanceMode::ByLineLength,
    }
}

fn rounded_positive_f32_to_u32(value: f32) -> u32 {
    let rounded = value.round().clamp(1.0, u32::MAX as f32);
    rounded as u32
}

fn frame_rect_from_center_and_size(center: Pos2, size: Vec2, page_size: [usize; 2]) -> Rect {
    let page_w = page_size[0].max(1) as f32;
    let page_h = page_size[1].max(1) as f32;
    let width = size.x.clamp(1.0, page_w);
    let height = size.y.clamp(1.0, page_h);
    let min_x = (center.x - width * 0.5).clamp(0.0, (page_w - width).max(0.0));
    let min_y = (center.y - height * 0.5).clamp(0.0, (page_h - height).max(0.0));
    Rect::from_min_size(Pos2::new(min_x, min_y), Vec2::new(width, height))
}

fn layout_editor_frame_scene_rect(frame_page_rect: Rect, image_rect: Rect, zoom: f32) -> Rect {
    Rect::from_min_max(
        scene_from_page_px(
            image_rect,
            zoom,
            [frame_page_rect.min.x, frame_page_rect.min.y],
        ),
        scene_from_page_px(
            image_rect,
            zoom,
            [frame_page_rect.max.x, frame_page_rect.max.y],
        ),
    )
}

fn layout_frame_handle_points(rect: Rect) -> [(TypingLayoutFrameHandle, Pos2); 8] {
    [
        (TypingLayoutFrameHandle::TopLeft, rect.left_top()),
        (
            TypingLayoutFrameHandle::Top,
            egui::pos2(rect.center().x, rect.top()),
        ),
        (TypingLayoutFrameHandle::TopRight, rect.right_top()),
        (
            TypingLayoutFrameHandle::Right,
            egui::pos2(rect.right(), rect.center().y),
        ),
        (TypingLayoutFrameHandle::BottomRight, rect.right_bottom()),
        (
            TypingLayoutFrameHandle::Bottom,
            egui::pos2(rect.center().x, rect.bottom()),
        ),
        (TypingLayoutFrameHandle::BottomLeft, rect.left_bottom()),
        (
            TypingLayoutFrameHandle::Left,
            egui::pos2(rect.left(), rect.center().y),
        ),
    ]
}

fn apply_layout_frame_drag(
    start_rect: Rect,
    handle: TypingLayoutFrameHandle,
    delta: Vec2,
    page_size: [usize; 2],
) -> Rect {
    let mut min = start_rect.min;
    let mut max = start_rect.max;
    match handle {
        TypingLayoutFrameHandle::TopLeft => {
            min += delta;
        }
        TypingLayoutFrameHandle::Top => {
            min.y += delta.y;
        }
        TypingLayoutFrameHandle::TopRight => {
            max.x += delta.x;
            min.y += delta.y;
        }
        TypingLayoutFrameHandle::Right => {
            max.x += delta.x;
        }
        TypingLayoutFrameHandle::BottomRight => {
            max += delta;
        }
        TypingLayoutFrameHandle::Bottom => {
            max.y += delta.y;
        }
        TypingLayoutFrameHandle::BottomLeft => {
            min.x += delta.x;
            max.y += delta.y;
        }
        TypingLayoutFrameHandle::Left => {
            min.x += delta.x;
        }
    }
    let page_w = page_size[0].max(1) as f32;
    let page_h = page_size[1].max(1) as f32;
    min.x = min.x.clamp(0.0, page_w);
    max.x = max.x.clamp(0.0, page_w);
    min.y = min.y.clamp(0.0, page_h);
    max.y = max.y.clamp(0.0, page_h);
    if max.x - min.x < TEXT_LAYOUT_EDITOR_FRAME_MIN_SIDE_PX {
        match handle {
            TypingLayoutFrameHandle::TopLeft
            | TypingLayoutFrameHandle::Left
            | TypingLayoutFrameHandle::BottomLeft => {
                min.x = (max.x - TEXT_LAYOUT_EDITOR_FRAME_MIN_SIDE_PX).max(0.0);
            }
            TypingLayoutFrameHandle::TopRight
            | TypingLayoutFrameHandle::Right
            | TypingLayoutFrameHandle::BottomRight => {
                max.x = (min.x + TEXT_LAYOUT_EDITOR_FRAME_MIN_SIDE_PX).min(page_w);
            }
            TypingLayoutFrameHandle::Top | TypingLayoutFrameHandle::Bottom => {}
        }
    }
    if max.y - min.y < TEXT_LAYOUT_EDITOR_FRAME_MIN_SIDE_PX {
        match handle {
            TypingLayoutFrameHandle::TopLeft
            | TypingLayoutFrameHandle::Top
            | TypingLayoutFrameHandle::TopRight => {
                min.y = (max.y - TEXT_LAYOUT_EDITOR_FRAME_MIN_SIDE_PX).max(0.0);
            }
            TypingLayoutFrameHandle::BottomLeft
            | TypingLayoutFrameHandle::Bottom
            | TypingLayoutFrameHandle::BottomRight => {
                max.y = (min.y + TEXT_LAYOUT_EDITOR_FRAME_MIN_SIDE_PX).min(page_h);
            }
            TypingLayoutFrameHandle::Left | TypingLayoutFrameHandle::Right => {}
        }
    }
    Rect::from_min_max(min, max)
}

fn handle_layout_editor_vector_canvas_input(
    editor: &mut TypingLayoutEditorState,
    line_idx: usize,
    frame_scene: Rect,
    image_rect: Rect,
    zoom: f32,
    response: &egui::Response,
    ctx: &egui::Context,
) {
    if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Delete))
        && let Some(line) = editor.lines.get_mut(line_idx)
    {
        let _ = line.points.pop();
        ctx.request_repaint();
    }

    let Some(pointer_scene) = response.interact_pointer_pos() else {
        return;
    };
    let pointer_page = page_px_from_scene(image_rect, zoom, pointer_scene);
    let local = egui::pos2(
        (pointer_page[0] - editor.frame_page_rect.left())
            .clamp(0.0, editor.frame_page_rect.width().max(1.0)),
        (pointer_page[1] - editor.frame_page_rect.top())
            .clamp(0.0, editor.frame_page_rect.height().max(1.0)),
    );
    if response.clicked()
        && frame_scene.contains(pointer_scene)
        && let Some(line) = editor.lines.get_mut(line_idx)
    {
        let shift_creates_next = ctx.input(|input| input.modifiers.shift)
            && hit_test_layout_editor_line_point(line, frame_scene, zoom, pointer_scene)
                == line.points.len().checked_sub(1);
        if line.points.is_empty() || shift_creates_next {
            line.points.push(local);
            ctx.request_repaint();
        }
    }
    if response.drag_started()
        && let Some(line) = editor.lines.get_mut(line_idx)
    {
        let hit_point_idx =
            hit_test_layout_editor_line_point(line, frame_scene, zoom, pointer_scene);
        let shift_pressed = ctx.input(|input| input.modifiers.shift);
        let last_point_idx = line.points.len().checked_sub(1);
        if shift_pressed && hit_point_idx.is_some() && hit_point_idx == last_point_idx {
            line.points.push(local);
            editor.line_drag = Some(TypingLayoutLineDragState {
                line_idx,
                point_idx: line.points.len().saturating_sub(1),
            });
            ctx.request_repaint();
        } else if let Some(point_idx) = hit_point_idx {
            editor.line_drag = Some(TypingLayoutLineDragState {
                line_idx,
                point_idx,
            });
            ctx.request_repaint();
        }
    }
    if response.dragged()
        && let Some(drag) = editor.line_drag
        && let Some(line) = editor.lines.get_mut(drag.line_idx)
        && let Some(point) = line.points.get_mut(drag.point_idx)
    {
        *point = local;
        ctx.request_repaint();
    }
    if response.drag_stopped() {
        editor.line_drag = None;
    }
}

fn clamp_layout_editor_points_to_frame(editor: &mut TypingLayoutEditorState) {
    let max_x = editor.frame_page_rect.width().max(1.0);
    let max_y = editor.frame_page_rect.height().max(1.0);
    for line in &mut editor.lines {
        for point in &mut line.points {
            point.x = point.x.clamp(0.0, max_x);
            point.y = point.y.clamp(0.0, max_y);
        }
    }
}

fn hit_test_layout_editor_line_point(
    line: &TypingLayoutEditorLine,
    frame_scene: Rect,
    zoom: f32,
    pointer_scene: Pos2,
) -> Option<usize> {
    line.points
        .iter()
        .enumerate()
        .rev()
        .find(|(_, point)| {
            layout_line_point_scene(frame_scene, **point, zoom).distance(pointer_scene)
                <= TEXT_LAYOUT_EDITOR_POINT_RADIUS_PX * 2.2
        })
        .map(|(point_idx, _)| point_idx)
}

fn layout_line_point_scene(frame_scene: Rect, point: Pos2, zoom: f32) -> Pos2 {
    egui::pos2(
        frame_scene.left() + point.x * zoom,
        frame_scene.top() + point.y * zoom,
    )
}

fn draw_layout_editor_frame(painter: &egui::Painter, rect: Rect) {
    painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(20, 32, 46, 36));
    painter.rect_stroke(
        rect,
        0.0,
        Stroke::new(2.0, Color32::from_rgb(92, 210, 255)),
        egui::StrokeKind::Outside,
    );
    for (handle, pos) in layout_frame_handle_points(rect) {
        let is_corner = matches!(
            handle,
            TypingLayoutFrameHandle::TopLeft
                | TypingLayoutFrameHandle::TopRight
                | TypingLayoutFrameHandle::BottomRight
                | TypingLayoutFrameHandle::BottomLeft
        );
        let color = if is_corner {
            Color32::from_rgb(255, 220, 90)
        } else {
            Color32::from_rgb(118, 225, 255)
        };
        painter.rect_filled(Rect::from_center_size(pos, Vec2::splat(10.0)), 1.5, color);
        painter.rect_stroke(
            Rect::from_center_size(pos, Vec2::splat(10.0)),
            1.5,
            Stroke::new(1.0, Color32::from_rgb(12, 20, 28)),
            egui::StrokeKind::Outside,
        );
    }
}

fn draw_layout_editor_vector_lines(
    painter: &egui::Painter,
    frame_scene: Rect,
    zoom: f32,
    editor: &TypingLayoutEditorState,
) {
    for (line_idx, line) in editor.lines.iter().enumerate() {
        let active = line_idx == editor.active_line_idx;
        let line_color = if active {
            layout_editor_active_line_color(line_idx)
        } else {
            Color32::from_rgba_unmultiplied(165, 170, 178, 145)
        };
        let point_color = if active {
            Color32::from_rgb(255, 245, 110)
        } else {
            Color32::from_rgba_unmultiplied(178, 182, 188, 150)
        };
        let raw_line_color = if active {
            Color32::from_rgba_unmultiplied(line_color.r(), line_color.g(), line_color.b(), 110)
        } else {
            Color32::from_rgba_unmultiplied(140, 145, 152, 85)
        };
        for pair in line.points.windows(2) {
            painter.line_segment(
                [
                    layout_line_point_scene(frame_scene, pair[0], zoom),
                    layout_line_point_scene(frame_scene, pair[1], zoom),
                ],
                Stroke::new(if active { 1.2 } else { 0.9 }, raw_line_color),
            );
        }
        let smoothed_points = smoothed_layout_editor_line_points(line);
        for pair in smoothed_points.windows(2) {
            painter.line_segment(
                [
                    layout_line_point_scene(frame_scene, pair[0], zoom),
                    layout_line_point_scene(frame_scene, pair[1], zoom),
                ],
                Stroke::new(if active { 2.8 } else { 1.4 }, line_color),
            );
        }
        for (point_idx, point) in line.points.iter().enumerate() {
            let scene = layout_line_point_scene(frame_scene, *point, zoom);
            draw_layout_editor_line_point(
                painter,
                scene,
                point_color,
                point_idx,
                line.points.len(),
                active,
            );
        }
    }
}

fn smoothed_layout_editor_line_points(line: &TypingLayoutEditorLine) -> Vec<Pos2> {
    let points = line
        .points
        .iter()
        .map(|point| TextVectorPoint {
            x: point.x,
            y: point.y,
        })
        .collect::<Vec<_>>();
    super::render_next::drawn_lines::smooth_vector_points(
        points.as_slice(),
        line.corner_smoothing_px,
    )
    .into_iter()
    .map(|point| Pos2::new(point.x, point.y))
    .collect()
}

fn draw_layout_editor_line_point(
    painter: &egui::Painter,
    center: Pos2,
    color: Color32,
    point_idx: usize,
    point_count: usize,
    active: bool,
) {
    let radius = if active {
        TEXT_LAYOUT_EDITOR_POINT_RADIUS_PX
    } else {
        TEXT_LAYOUT_EDITOR_POINT_RADIUS_PX - 1.5
    };
    if point_idx == 0 && point_count > 1 {
        painter.circle_filled(center, radius + 2.0, Color32::from_rgb(20, 28, 38));
        painter.circle_stroke(center, radius + 2.0, Stroke::new(2.0, color));
        painter.circle_filled(center, radius - 2.0, color);
    } else if point_idx + 1 == point_count {
        painter.rect_filled(
            Rect::from_center_size(center, Vec2::splat(radius * 2.0)),
            1.5,
            color,
        );
        painter.rect_stroke(
            Rect::from_center_size(center, Vec2::splat(radius * 2.0)),
            1.5,
            Stroke::new(1.0, Color32::from_rgb(20, 28, 38)),
            egui::StrokeKind::Outside,
        );
    } else {
        painter.circle_filled(center, radius, color);
        painter.circle_stroke(
            center,
            radius,
            Stroke::new(1.0, Color32::from_rgb(20, 28, 38)),
        );
    }
}

fn layout_editor_active_line_color(line_idx: usize) -> Color32 {
    const COLORS: [Color32; 12] = [
        Color32::from_rgb(255, 64, 64),
        Color32::from_rgb(255, 150, 40),
        Color32::from_rgb(240, 205, 70),
        Color32::from_rgb(74, 220, 96),
        Color32::from_rgb(35, 220, 190),
        Color32::from_rgb(70, 190, 255),
        Color32::from_rgb(80, 110, 255),
        Color32::from_rgb(170, 90, 255),
        Color32::from_rgb(255, 70, 170),
        Color32::from_rgb(180, 115, 60),
        Color32::from_rgb(190, 195, 205),
        Color32::from_rgb(170, 35, 70),
    ];
    COLORS[line_idx % COLORS.len()]
}

fn contains_any_page(canvas: &CanvasView, project: &ProjectData, pos: Pos2) -> bool {
    project.pages.iter().any(|page| {
        canvas
            .page_scene_rect(page.idx)
            .map(|rect| rect.contains(pos))
            .unwrap_or(false)
    })
}

fn viewport_center_page_px_for_page(
    canvas_rect: Rect,
    canvas: &CanvasView,
    project: &ProjectData,
) -> [f32; 2] {
    let current_idx = canvas.current_page_idx();
    let page_rect = canvas.page_scene_rect(current_idx).or_else(|| {
        project
            .pages
            .first()
            .and_then(|p| canvas.page_scene_rect(p.idx))
    });
    let Some(page_rect) = page_rect else {
        return [0.5, 0.5];
    };
    if !page_rect.is_positive() {
        return [0.5, 0.5];
    }
    let center_scene = canvas_rect.center();
    let clamped_scene = Pos2::new(
        center_scene.x.clamp(page_rect.left(), page_rect.right()),
        center_scene.y.clamp(page_rect.top(), page_rect.bottom()),
    );
    page_px_from_scene(page_rect, canvas.zoom(), clamped_scene)
}

fn resolve_selection_to_page(
    canvas: &CanvasView,
    project: &ProjectData,
    selection_rect: Rect,
) -> Option<(usize, Rect, Rect)> {
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
    let scene_rect = selection_rect.intersect(page_rect);
    if !scene_rect.is_positive() {
        return None;
    }
    Some((page_idx, page_rect, scene_rect))
}

fn selection_width_in_source_px(
    canvas: &CanvasView,
    page_idx: usize,
    page_rect: Rect,
    scene_rect: Rect,
) -> u32 {
    if !page_rect.is_positive() || !scene_rect.is_positive() {
        return 0;
    }

    let source_w = canvas
        .overlay_size(page_idx)
        .map(|size| size[0] as f32)
        .unwrap_or_else(|| {
            let zoom = canvas.state.zoom.max(f32::EPSILON);
            (page_rect.width() / zoom).max(1.0)
        });
    let ratio = (scene_rect.width() / page_rect.width().max(1.0)).clamp(0.0, 1.0);
    (source_w * ratio).round().max(1.0) as u32
}

fn selection_center_page_px(page_rect: Rect, scene_rect: Rect, zoom: f32) -> [f32; 2] {
    page_px_from_scene(page_rect, zoom, scene_rect.center())
}

fn is_font_family_bound(ctx: &egui::Context, family: &egui::FontFamily) -> bool {
    ctx.fonts(|fonts| fonts.definitions().families.contains_key(family))
}

fn pick_bubble_text_for_selection(
    project: &ProjectData,
    page_idx: usize,
    scene_rect: Rect,
    page_rect: Rect,
) -> Option<String> {
    let selection_center = scene_rect.center();
    let mut best: Option<(f32, String)> = None;

    for bubble in project
        .bubbles
        .iter()
        .filter(|bubble| bubble.img_idx == page_idx)
    {
        let bubble_pos = scene_from_uv(page_rect, bubble.img_u, bubble.img_v);
        if !scene_rect.contains(bubble_pos) {
            continue;
        }
        let text = preferred_bubble_seed_text(bubble);
        if text.is_empty() {
            continue;
        }
        let dist_sq = selection_center.distance_sq(bubble_pos);
        let should_replace = match best.as_ref() {
            Some((best_dist, _)) => dist_sq < *best_dist,
            None => true,
        };
        if should_replace {
            best = Some((dist_sq, text));
        }
    }

    best.map(|(_, text)| text)
}

fn preferred_bubble_seed_text(bubble: &crate::project::Bubble) -> String {
    let translated = bubble.text.trim();
    if !translated.is_empty() {
        return translated.to_string();
    }
    bubble.original_text.trim().to_string()
}

fn render_and_store_created_overlay(
    request: TypingCreateOverlayRequest,
) -> Result<TypingOverlayDecoded, String> {
    fs::create_dir_all(&request.text_images_dir).map_err(|err| {
        format!(
            "Не удалось создать папку {}: {err}",
            request.text_images_dir.display()
        )
    })?;
    let file_name = next_created_overlay_file_name(&request.text_images_dir, request.page_idx);
    let render_params = render_params_with_adjacent_layout_path(
        &request.text_images_dir,
        &file_name,
        &request.render_params,
    );
    let rendered = render_text_to_image(&render_params, None).map_err(|err| {
        eprintln!(
            "ERROR typing::create_overlay_render layout={:?} shape={:?} wrap={:?} line_mode={:?} width_px={} page_idx={} err={}",
            render_params.text_layout_mode,
            render_params.text_shape,
            render_params.text_wrap_mode,
            render_params.text_line_mode,
            render_params.width_px,
            request.page_idx,
            err
        );
        err
    })?;
    if rendered.width == 0 || rendered.height == 0 {
        return Err("Рендер вернул изображение нулевого размера.".to_string());
    }

    let image_path = request.text_images_dir.join(&file_name);
    image::save_buffer(
        &image_path,
        rendered.rgba.as_slice(),
        rendered.width,
        rendered.height,
        image::ColorType::Rgba8,
    )
    .map_err(|err| format!("Не удалось сохранить {}: {err}", image_path.display()))?;
    let layout_image_path = save_drawn_lines_layout_image_if_needed(
        &request.text_images_dir,
        &file_name,
        &render_params,
        rendered.width,
        rendered.height,
    )?;

    // Для нового оверлея не подгоняем PNG под выделение: показываем в исходном масштабе.
    let user_scale = 1.0_f32;
    if let Err(err) = append_created_overlay_info(
        &request.text_images_dir,
        request.page_idx,
        request.center_page_px,
        true,
        user_scale,
        &file_name,
        TypingOverlayKind::Text,
        Some(request.render_data_json.clone()),
    ) {
        let _ = fs::remove_file(&image_path);
        if let Some(path) = layout_image_path {
            let _ = fs::remove_file(path);
        }
        return Err(err);
    }

    Ok(TypingOverlayDecoded {
        kind: TypingOverlayKind::Text,
        page_idx: request.page_idx,
        center_page_px: request.center_page_px,
        mask_clip_enabled: true,
        user_scale,
        angle_deg: 0.0,
        deform_mesh: None,
        file_name,
        render_data_json: Some(request.render_data_json),
        size_px: [rendered.width as usize, rendered.height as usize],
        rgba: rendered.rgba,
        warnings: rendered.warnings,
    })
}

fn render_and_store_created_image_overlay(
    request: TypingCreateImageOverlayRequest,
) -> Result<TypingOverlayDecoded, String> {
    let (rgba, width, height) = match request.source {
        TypingCreateImageSource::Clipboard => read_image_rgba_from_clipboard()?,
        TypingCreateImageSource::File(path) => read_image_rgba_from_file(path.as_path())?,
    };
    if width == 0 || height == 0 {
        return Err("Изображение нулевого размера.".to_string());
    }
    if rgba.len() != width.saturating_mul(height).saturating_mul(4) {
        return Err("Некорректный буфер RGBA изображения.".to_string());
    }

    fs::create_dir_all(&request.text_images_dir).map_err(|err| {
        format!(
            "Не удалось создать папку {}: {err}",
            request.text_images_dir.display()
        )
    })?;
    let file_name = next_created_overlay_file_name(&request.text_images_dir, request.page_idx);
    let image_path = request.text_images_dir.join(&file_name);
    image::save_buffer(
        &image_path,
        rgba.as_slice(),
        width as u32,
        height as u32,
        image::ColorType::Rgba8,
    )
    .map_err(|err| format!("Не удалось сохранить {}: {err}", image_path.display()))?;

    if let Err(err) = append_created_overlay_info(
        &request.text_images_dir,
        request.page_idx,
        request.center_page_px,
        true,
        1.0,
        &file_name,
        TypingOverlayKind::Image,
        None,
    ) {
        let _ = fs::remove_file(&image_path);
        return Err(err);
    }

    Ok(TypingOverlayDecoded {
        kind: TypingOverlayKind::Image,
        page_idx: request.page_idx,
        center_page_px: request.center_page_px,
        mask_clip_enabled: true,
        user_scale: 1.0,
        angle_deg: 0.0,
        deform_mesh: None,
        file_name,
        render_data_json: None,
        size_px: [width, height],
        rgba,
        warnings: Vec::new(),
    })
}

fn render_and_store_edited_overlay(
    request: TypingEditOverlayRequest,
) -> Result<Option<TypingEditOverlayResult>, String> {
    if request.latest_token.load(Ordering::Acquire) != request.token {
        return Ok(None);
    }

    let render_params = render_params_with_adjacent_layout_path(
        &request.text_images_dir,
        &request.file_name,
        &request.render_params,
    );
    let rendered = match render_text_to_image(
        &render_params,
        Some((&request.latest_token, request.token)),
    ) {
        Ok(rendered) => rendered,
        Err(err) if err == "render_next render cancelled" => return Ok(None),
        Err(err) => {
            eprintln!(
                "ERROR typing::edit_overlay_render layout={:?} shape={:?} wrap={:?} line_mode={:?} width_px={} token={} err={}",
                render_params.text_layout_mode,
                render_params.text_shape,
                render_params.text_wrap_mode,
                render_params.text_line_mode,
                render_params.width_px,
                request.token,
                err
            );
            return Err(err);
        }
    };
    if rendered.width == 0 || rendered.height == 0 {
        return Err("Рендер редактирования вернул изображение нулевого размера.".to_string());
    }

    if request.latest_token.load(Ordering::Acquire) != request.token {
        return Ok(None);
    }

    fs::create_dir_all(&request.text_images_dir).map_err(|err| {
        format!(
            "Не удалось создать папку {}: {err}",
            request.text_images_dir.display()
        )
    })?;
    let image_path = request.text_images_dir.join(&request.file_name);
    if request.latest_token.load(Ordering::Acquire) != request.token {
        return Ok(None);
    }
    image::save_buffer(
        &image_path,
        rendered.rgba.as_slice(),
        rendered.width,
        rendered.height,
        image::ColorType::Rgba8,
    )
    .map_err(|err| format!("Не удалось сохранить {}: {err}", image_path.display()))?;
    save_drawn_lines_layout_image_if_needed(
        &request.text_images_dir,
        &request.file_name,
        &render_params,
        rendered.width,
        rendered.height,
    )?;

    Ok(Some(TypingEditOverlayResult {
        token: request.token,
        overlay_idx: request.overlay_idx,
        file_name: request.file_name,
        user_scale: request.user_scale.max(0.05),
        rotation_deg: request.rotation_deg,
        render_data_json: request.render_data_json,
        size_px: [rendered.width as usize, rendered.height as usize],
        rgba: rendered.rgba,
        warnings: rendered.warnings,
    }))
}

fn shape_variant_slot_size(current_size_px: [usize; 2]) -> Vec2 {
    fit_size_to_box(
        current_size_px,
        Vec2::new(
            TEXT_SHAPE_VARIANT_TILE_MAX_WIDTH_PX,
            TEXT_SHAPE_VARIANT_TILE_MAX_HEIGHT_PX,
        ),
    )
}

fn shape_variant_panel_size(slot_size: Vec2, gap_px: f32, padding_px: f32) -> Vec2 {
    let grid_side = TEXT_SHAPE_VARIANT_GRID_SIDE as f32;
    Vec2::new(
        padding_px * 2.0 + slot_size.x * grid_side + gap_px * (grid_side - 1.0),
        padding_px * 2.0 + slot_size.y * grid_side + gap_px * (grid_side - 1.0),
    )
}

fn shape_variant_panel_pos(
    menu_rect: Rect,
    panel_size: Vec2,
    viewport_rect: Rect,
    place_above: bool,
) -> Pos2 {
    let viewport_center_x = viewport_rect.center().x;
    let x = if menu_rect.center().x >= viewport_center_x {
        menu_rect.right() - panel_size.x
    } else {
        menu_rect.left()
    };
    let y = if place_above {
        menu_rect.top() - panel_size.y - TEXT_SHAPE_VARIANT_PANEL_MENU_GAP_PX
    } else {
        menu_rect.bottom() + TEXT_SHAPE_VARIANT_PANEL_MENU_GAP_PX
    };
    Pos2::new(x, y)
}

fn use_dark_shape_variant_checkerboard(text_color: [u8; 4]) -> bool {
    let r = f32::from(text_color[0]);
    let g = f32::from(text_color[1]);
    let b = f32::from(text_color[2]);
    let a = f32::from(text_color[3]) / 255.0;
    let luminance = (0.2126 * r + 0.7152 * g + 0.0722 * b) * a + 255.0 * (1.0 - a);
    luminance >= 140.0
}

fn paint_shape_variant_checkerboard(
    painter: &egui::Painter,
    rect: Rect,
    rounding: f32,
    dark: bool,
) {
    let (base_color, alternate_color, stroke_color) = if dark {
        (
            Color32::from_rgb(64, 64, 64),
            Color32::from_rgb(88, 88, 88),
            Color32::from_rgb(115, 115, 115),
        )
    } else {
        (
            Color32::from_rgb(232, 232, 232),
            Color32::from_rgb(198, 198, 198),
            Color32::from_rgb(150, 150, 150),
        )
    };

    painter.rect_filled(rect, rounding, base_color);
    let clip_rect = rect.shrink(1.0);
    let clipped = painter.with_clip_rect(clip_rect);
    let side = TEXT_SHAPE_VARIANT_CHECKER_SIDE_PX.max(1.0);
    let cols = (rect.width() / side).ceil().max(1.0) as usize;
    let rows = (rect.height() / side).ceil().max(1.0) as usize;

    for row in 0..rows {
        for col in 0..cols {
            if (row + col) % 2 == 0 {
                continue;
            }
            let min = Pos2::new(
                rect.left() + col as f32 * side,
                rect.top() + row as f32 * side,
            );
            let cell = Rect::from_min_size(min, Vec2::splat(side)).intersect(rect);
            clipped.rect_filled(cell, 0.0, alternate_color);
        }
    }

    painter.rect_stroke(
        rect,
        rounding,
        Stroke::new(1.0, stroke_color),
        egui::StrokeKind::Inside,
    );
}

fn fit_size_to_box(source_size: [usize; 2], box_size: Vec2) -> Vec2 {
    let src_w = source_size[0].max(1) as f32;
    let src_h = source_size[1].max(1) as f32;
    let scale = (box_size.x.max(1.0) / src_w)
        .min(box_size.y.max(1.0) / src_h)
        .min(1.0);
    Vec2::new((src_w * scale).max(1.0), (src_h * scale).max(1.0))
}

fn build_shape_variant_grid(base_params: &TextRenderParams) -> Vec<TypingShapeVariant> {
    const WRAP_MODES: [TextWrapMode; 3] = [
        TextWrapMode::Minimal,
        TextWrapMode::Moderate,
        TextWrapMode::Aggressive,
    ];
    const SOFT_PEAK_VARIANTS: [u8; 3] = [3, 9, 6];
    let min_width_available = shape_min_width_available(base_params.text_shape);
    let mut out = Vec::with_capacity(TEXT_SHAPE_VARIANT_GRID_SIDE * TEXT_SHAPE_VARIANT_GRID_SIDE);

    for row in 0..TEXT_SHAPE_VARIANT_GRID_SIDE {
        for (col, text_wrap_mode) in WRAP_MODES.iter().copied().enumerate() {
            let (width_px, shape_min_width_percent) = if min_width_available {
                let percent = match row {
                    0 => 50.0,
                    1 => 75.0,
                    2 => 90.0,
                    _ => base_params.shape_min_width_percent,
                };
                (base_params.width_px.max(1), percent)
            } else if base_params.text_shape == TextShape::SoftPeak {
                (
                    base_params.width_px.max(1),
                    base_params.shape_min_width_percent,
                )
            } else {
                let scale = match row {
                    0 => 0.95,
                    1 => 1.0,
                    2 => 1.05,
                    _ => 1.0,
                };
                (
                    ((base_params.width_px.max(1) as f32) * scale)
                        .round()
                        .max(1.0) as u32,
                    base_params.shape_min_width_percent,
                )
            };
            out.push(TypingShapeVariant {
                row,
                col,
                width_px,
                text_wrap_mode,
                shape_min_width_percent,
                shape_variant: if base_params.text_shape == TextShape::SoftPeak {
                    SOFT_PEAK_VARIANTS
                        .get(row)
                        .copied()
                        .unwrap_or(base_params.shape_variant)
                } else {
                    base_params.shape_variant
                },
            });
        }
    }

    out
}

fn shape_variant_preview_available(overlay_kind: TypingOverlayKind) -> bool {
    overlay_kind == TypingOverlayKind::Text
}

fn render_shape_variant_preview_tiles(
    base_params: TextRenderParams,
    variants: Vec<TypingShapeVariant>,
    cancel_render: &Arc<AtomicBool>,
) -> Vec<TypingShapeVariantPreviewTile> {
    let mut indexed_variants = variants.into_iter().enumerate();
    let mut indexed_tiles = Vec::<(usize, Option<TypingShapeVariantPreviewTile>)>::new();

    loop {
        if cancel_render.load(Ordering::Relaxed) {
            break;
        }
        let batch = indexed_variants
            .by_ref()
            .take(TEXT_SHAPE_VARIANT_GRID_SIDE)
            .collect::<Vec<_>>();
        if batch.is_empty() {
            break;
        }

        let (tx, rx) = mpsc::channel::<(usize, Option<TypingShapeVariantPreviewTile>)>();
        let mut handles = Vec::with_capacity(batch.len());

        for (index, variant) in batch {
            let tx = tx.clone();
            let base_params = base_params.clone();
            let cancel_render = Arc::clone(cancel_render);
            let worker_name = format!(
                "typing-shape-variant-render-{}-{}",
                variant.row, variant.col
            );
            match thread::Builder::new().name(worker_name).spawn(move || {
                if cancel_render.load(Ordering::Relaxed) {
                    return;
                }
                let tile = render_shape_variant_preview_tile(base_params, variant);
                if cancel_render.load(Ordering::Relaxed) {
                    return;
                }
                if let Err(err) = tx.send((index, tile)) {
                    eprintln!(
                        "ERROR typing::shape_variant_preview_render_send index={} err={}",
                        index, err
                    );
                }
            }) {
                Ok(handle) => handles.push(handle),
                Err(err) => {
                    eprintln!(
                        "ERROR typing::shape_variant_preview_spawn index={} err={}",
                        index, err
                    );
                }
            }
        }
        drop(tx);

        indexed_tiles.extend(rx);
        for handle in handles {
            if handle.join().is_err() {
                eprintln!("ERROR typing::shape_variant_preview_worker_panicked");
            }
        }
    }
    indexed_tiles.sort_by_key(|(index, _)| *index);
    indexed_tiles
        .into_iter()
        .filter_map(|(_, tile)| tile)
        .collect()
}

fn render_shape_variant_preview_tile(
    base_params: TextRenderParams,
    variant: TypingShapeVariant,
) -> Option<TypingShapeVariantPreviewTile> {
    let mut params = base_params.clone();
    params.width_px = variant.width_px;
    params.text_wrap_mode = variant.text_wrap_mode;
    params.shape_min_width_percent = variant.shape_min_width_percent;
    params.shape_variant = variant.shape_variant;
    params.compare_shape_with = Some(TextRenderShapeCompareParams {
        width_px: base_params.width_px,
        text_wrap_mode: base_params.text_wrap_mode,
        shape_min_width_percent: base_params.shape_min_width_percent,
        shape_variant: base_params.shape_variant,
        cancel_render_if_layout_text_unchanged: true,
    });

    match render_text_to_image(&params, None) {
        Ok(rendered) if rendered.width > 0 && rendered.height > 0 => {
            let width = match usize::try_from(rendered.width) {
                Ok(width) => width,
                Err(err) => {
                    eprintln!(
                        "ERROR typing::shape_variant_preview_width row={} col={} width={} err={}",
                        variant.row, variant.col, rendered.width, err
                    );
                    return None;
                }
            };
            let height = match usize::try_from(rendered.height) {
                Ok(height) => height,
                Err(err) => {
                    eprintln!(
                        "ERROR typing::shape_variant_preview_height row={} col={} height={} err={}",
                        variant.row, variant.col, rendered.height, err
                    );
                    return None;
                }
            };
            Some(TypingShapeVariantPreviewTile {
                variant,
                size_px: [width, height],
                rgba: Some(rendered.rgba),
                texture: None,
            })
        }
        Ok(_) => None,
        Err(err) => {
            eprintln!(
                "ERROR typing::shape_variant_preview_render row={} col={} err={}",
                variant.row, variant.col, err
            );
            None
        }
    }
}

fn build_shape_variant_apply_payload(
    render_data: &Value,
    variant: &TypingShapeVariant,
) -> Option<(TextRenderParams, Value)> {
    let mut updated = render_data.clone();
    let text_params = updated
        .as_object_mut()?
        .get_mut("text_params")?
        .as_object_mut()?;
    text_params.insert(
        "text_wrap_mode".to_string(),
        Value::String(text_wrap_mode_to_config_str(variant.text_wrap_mode).to_string()),
    );
    text_params.insert("width_px".to_string(), Value::from(variant.width_px));
    text_params.insert(
        "shape_min_width_percent".to_string(),
        Value::from(variant.shape_min_width_percent),
    );
    text_params.insert(
        "shape_variant".to_string(),
        Value::from(variant.shape_variant),
    );
    let render_params = text_render_params_from_render_data(&updated)?;
    Some((render_params, updated))
}

fn shape_min_width_available(shape: TextShape) -> bool {
    matches!(shape, TextShape::Oval | TextShape::Hexagon)
}

fn text_render_params_from_render_data(render_data: &Value) -> Option<TextRenderParams> {
    let render_obj = render_data.as_object()?;
    let text_params = render_obj.get("text_params")?.as_object()?;
    let font_path = text_params
        .get("font_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)?;
    let effects_json = render_obj
        .get("effects")
        .and_then(Value::as_array)
        .map(|effects| Value::Array(effects.clone()))
        .and_then(|effects| serde_json::to_string(&effects).ok())
        .unwrap_or_default();

    Some(TextRenderParams {
        text: text_params
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        text_color: text_params
            .get("text_color")
            .and_then(parse_rgba_value)
            .unwrap_or([0, 0, 0, 255]),
        font_path,
        available_inline_fonts: Vec::new(),
        font_size_px: text_params
            .get("font_size_px")
            .and_then(value_as_f32)
            .unwrap_or(24.0)
            .max(1.0),
        line_spacing_px: text_params
            .get("line_spacing_px")
            .and_then(value_as_f32)
            .unwrap_or(4.0),
        line_spacing_percent: text_params
            .get("line_spacing_percent")
            .and_then(value_as_f32)
            .unwrap_or(50.0),
        kerning_mode: text_params
            .get("kerning_mode")
            .and_then(Value::as_str)
            .and_then(parse_kerning_mode_config_str)
            .unwrap_or(KerningMode::Metric),
        kerning_px: text_params
            .get("kerning_px")
            .and_then(value_as_f32)
            .unwrap_or(0.0),
        kerning_percent: text_params
            .get("kerning_percent")
            .and_then(value_as_f32)
            .unwrap_or(0.0),
        glyph_height_percent: text_params
            .get("glyph_height_percent")
            .and_then(value_as_f32)
            .unwrap_or(100.0),
        glyph_width_percent: text_params
            .get("glyph_width_percent")
            .and_then(value_as_f32)
            .unwrap_or(100.0),
        width_px: text_params
            .get("width_px")
            .and_then(value_as_f32)
            .map(|value| value.round().max(1.0) as u32)
            .unwrap_or(TEXT_RENDER_DATA_FALLBACK_WIDTH_PX),
        align: text_params
            .get("align")
            .and_then(Value::as_str)
            .and_then(parse_align_config_str)
            .unwrap_or(HorizontalAlign::Center),
        selected_face_index: text_params
            .get("selected_face_index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0),
        force_bold: text_params
            .get("force_bold")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        force_italic: text_params
            .get("force_italic")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        uppercase_text: text_params
            .get("uppercase_text")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        trim_extra_spaces: text_params
            .get("trim_extra_spaces")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        hanging_punctuation: text_params
            .get("hanging_punctuation")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        new_line_after_sentence: text_params
            .get("new_line_after_sentence")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        enable_inline_style_tags: text_params
            .get("enable_inline_style_tags")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        text_wrap_mode: text_params
            .get("text_wrap_mode")
            .and_then(Value::as_str)
            .and_then(parse_text_wrap_mode_config_str)
            .unwrap_or(TextWrapMode::Aggressive),
        text_shape: text_params
            .get("text_shape")
            .and_then(Value::as_str)
            .and_then(parse_text_shape_config_str)
            .unwrap_or(TextShape::Rectangle),
        shape_min_width_percent: text_params
            .get("shape_min_width_percent")
            .and_then(value_as_f32)
            .unwrap_or(50.0),
        shape_variant: text_params
            .get("shape_variant")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(5)
            .clamp(1, 9),
        compare_shape_with: None,
        allow_moderate_trees: text_params
            .get("allow_moderate_trees")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        text_line_mode: text_params
            .get("text_line_mode")
            .and_then(Value::as_str)
            .and_then(parse_text_line_mode_config_str)
            .unwrap_or(TextLineMode::Horizontal),
        vertical_line_direction: text_params
            .get("vertical_line_direction")
            .and_then(Value::as_str)
            .and_then(parse_vertical_line_direction_config_str)
            .unwrap_or(VerticalLineDirection::RightToLeft),
        text_layout_mode: text_params
            .get("text_layout_mode")
            .and_then(Value::as_str)
            .and_then(parse_text_layout_mode_config_str)
            .unwrap_or(TextLayoutMode::Normal),
        formula_layout: text_formula_layout_params_from_value(text_params.get("formula_layout")),
        drawn_lines_layout: text_drawn_lines_layout_params_from_value(
            text_params.get("drawn_lines_layout"),
        ),
        vector_lines_layout: text_vector_lines_layout_params_from_value(
            text_params.get("vector_lines_layout"),
        ),
        effects_json,
    })
}

fn text_formula_layout_params_from_value(value: Option<&Value>) -> TextFormulaLayoutParams {
    let Some(obj) = value.and_then(Value::as_object) else {
        return TextFormulaLayoutParams::default();
    };
    let defaults = TextFormulaLayoutParams::default();
    let mut vars = defaults.vars;
    if let Some(raw_vars) = obj.get("vars").and_then(Value::as_array) {
        for (idx, value) in raw_vars
            .iter()
            .take(TEXT_FORMULA_USER_VAR_COUNT)
            .enumerate()
        {
            if let Some(parsed) = value_as_f32(value) {
                vars[idx] = parsed;
            }
        }
    }
    TextFormulaLayoutParams {
        x_expr: obj
            .get("x_expr")
            .and_then(Value::as_str)
            .unwrap_or(defaults.x_expr.as_str())
            .to_string(),
        y_expr: obj
            .get("y_expr")
            .and_then(Value::as_str)
            .unwrap_or(defaults.y_expr.as_str())
            .to_string(),
        rotation_expr: obj
            .get("rotation_expr")
            .and_then(Value::as_str)
            .unwrap_or(defaults.rotation_expr.as_str())
            .to_string(),
        use_tangent_rotation: obj
            .get("use_tangent_rotation")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.use_tangent_rotation),
        t_start: obj
            .get("t_start")
            .and_then(value_as_f32)
            .unwrap_or(defaults.t_start),
        t_end: obj
            .get("t_end")
            .and_then(value_as_f32)
            .unwrap_or(defaults.t_end),
        offset_x_px: obj
            .get("offset_x_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.offset_x_px),
        offset_y_px: obj
            .get("offset_y_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.offset_y_px),
        scale_x: obj
            .get("scale_x")
            .and_then(value_as_f32)
            .unwrap_or(defaults.scale_x),
        scale_y: obj
            .get("scale_y")
            .and_then(value_as_f32)
            .unwrap_or(defaults.scale_y),
        normal_offset_px: obj
            .get("normal_offset_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.normal_offset_px),
        letter_spacing_mul: obj
            .get("letter_spacing_mul")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_mul),
        letter_spacing_px: obj
            .get("letter_spacing_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_px),
        vars,
    }
}

fn text_drawn_lines_layout_params_from_value(value: Option<&Value>) -> TextDrawnLinesLayoutParams {
    let Some(obj) = value.and_then(Value::as_object) else {
        return TextDrawnLinesLayoutParams::default();
    };
    let defaults = TextDrawnLinesLayoutParams::default();
    TextDrawnLinesLayoutParams {
        image_path: None,
        use_tangent_rotation: obj
            .get("use_tangent_rotation")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.use_tangent_rotation),
        static_rotation_rad: obj
            .get("static_rotation_rad")
            .and_then(value_as_f32)
            .unwrap_or(defaults.static_rotation_rad),
        normal_offset_px: obj
            .get("normal_offset_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.normal_offset_px),
        letter_spacing_mul: obj
            .get("letter_spacing_mul")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_mul)
            .clamp(0.0, 8.0),
        letter_spacing_px: obj
            .get("letter_spacing_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_px)
            .clamp(-10_000.0, 10_000.0),
        color_tolerance: obj
            .get("color_tolerance")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(defaults.color_tolerance),
        continuation_alpha: obj
            .get("continuation_alpha")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(defaults.continuation_alpha),
        start_alpha: obj
            .get("start_alpha")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(defaults.start_alpha),
    }
}

fn text_vector_lines_layout_params_from_value(
    value: Option<&Value>,
) -> TextVectorLinesLayoutParams {
    let Some(obj) = value.and_then(Value::as_object) else {
        return TextVectorLinesLayoutParams::default();
    };
    let defaults = TextVectorLinesLayoutParams::default();
    let lines = obj
        .get("lines")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(text_vector_line_params_from_value)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    TextVectorLinesLayoutParams {
        width_px: obj
            .get("width_px")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(defaults.width_px)
            .max(1),
        height_px: obj
            .get("height_px")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(defaults.height_px)
            .max(1),
        use_tangent_rotation: obj
            .get("use_tangent_rotation")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.use_tangent_rotation),
        static_rotation_rad: obj
            .get("static_rotation_rad")
            .and_then(value_as_f32)
            .unwrap_or(defaults.static_rotation_rad),
        normal_offset_px: obj
            .get("normal_offset_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.normal_offset_px),
        letter_spacing_mul: obj
            .get("letter_spacing_mul")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_mul)
            .clamp(0.0, 8.0),
        letter_spacing_px: obj
            .get("letter_spacing_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_px)
            .clamp(-10_000.0, 10_000.0),
        lines,
    }
}

fn text_vector_line_params_from_value(value: &Value) -> Option<TextVectorLine> {
    let obj = value.as_object()?;
    let points = obj
        .get("points")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(text_vector_point_params_from_value)
        .collect::<Vec<_>>();
    Some(TextVectorLine {
        points,
        corner_smoothing_px: obj
            .get("corner_smoothing_px")
            .and_then(value_as_f32)
            .unwrap_or(0.0)
            .clamp(0.0, 256.0),
        text_direction: vector_line_text_direction_from_value(obj.get("text_direction")),
        distance_mode: vector_line_distance_mode_from_value(obj.get("distance_mode")),
        flip_text: obj
            .get("flip_text")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn text_vector_point_params_from_value(value: &Value) -> Option<TextVectorPoint> {
    let obj = value.as_object()?;
    Some(TextVectorPoint {
        x: obj.get("x").and_then(value_as_f32)?,
        y: obj.get("y").and_then(value_as_f32)?,
    })
}

fn parse_align_config_str(raw: &str) -> Option<HorizontalAlign> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "left" => Some(HorizontalAlign::Left),
        "center" => Some(HorizontalAlign::Center),
        "right" => Some(HorizontalAlign::Right),
        "justify" => Some(HorizontalAlign::Justify),
        _ => None,
    }
}

fn parse_kerning_mode_config_str(raw: &str) -> Option<KerningMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "metric" => Some(KerningMode::Metric),
        "optical" => Some(KerningMode::Optical),
        _ => None,
    }
}

fn parse_text_shape_config_str(raw: &str) -> Option<TextShape> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "free" => Some(TextShape::Free),
        "rectangle" => Some(TextShape::Rectangle),
        "oval" => Some(TextShape::Oval),
        "hexagon" => Some(TextShape::Hexagon),
        "soft_peak" | "soft" | "no_trees" => Some(TextShape::SoftPeak),
        _ => None,
    }
}

fn parse_text_wrap_mode_config_str(raw: &str) -> Option<TextWrapMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "none" => Some(TextWrapMode::None),
        "whole_words" | "words" | "word" => Some(TextWrapMode::WholeWords),
        "minimal" => Some(TextWrapMode::Minimal),
        "moderate" => Some(TextWrapMode::Moderate),
        "aggressive" | "smart" => Some(TextWrapMode::Aggressive),
        _ => None,
    }
}

fn text_wrap_mode_to_config_str(mode: TextWrapMode) -> &'static str {
    match mode {
        TextWrapMode::None => "none",
        TextWrapMode::WholeWords => "whole_words",
        TextWrapMode::Minimal => "minimal",
        TextWrapMode::Moderate => "moderate",
        TextWrapMode::Aggressive => "aggressive",
    }
}

fn parse_text_line_mode_config_str(raw: &str) -> Option<TextLineMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "horizontal" => Some(TextLineMode::Horizontal),
        "vertical" => Some(TextLineMode::Vertical),
        _ => None,
    }
}

fn parse_vertical_line_direction_config_str(raw: &str) -> Option<VerticalLineDirection> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "left_to_right" | "ltr" => Some(VerticalLineDirection::LeftToRight),
        "right_to_left" | "rtl" => Some(VerticalLineDirection::RightToLeft),
        _ => None,
    }
}

fn parse_text_layout_mode_config_str(raw: &str) -> Option<TextLayoutMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "normal" => Some(TextLayoutMode::Normal),
        "formula" => Some(TextLayoutMode::Formula),
        "shape" => Some(TextLayoutMode::Shape),
        "drawn_lines"
        | "drawn-lines"
        | "drawnlines"
        | "custom_raster_lines"
        | "custom-raster-lines"
        | "customrasterlines" => Some(TextLayoutMode::CustomRasterLines),
        "vector_lines"
        | "vector-lines"
        | "vectorlines"
        | "custom_vector_lines"
        | "custom-vector-lines"
        | "customvectorlines" => Some(TextLayoutMode::CustomVectorLines),
        _ => None,
    }
}

fn export_typing_pages_to_folder(
    mut jobs: Vec<TypingExportPageJob>,
    output_dir: PathBuf,
    clean_overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    progress_tx: mpsc::Sender<TypingExportEvent>,
) -> Result<TypingExportResult, String> {
    fs::create_dir_all(&output_dir)
        .map_err(|err| format!("Не удалось создать папку {}: {err}", output_dir.display()))?;
    let total = jobs.len();
    if jobs.is_empty() {
        return Ok(TypingExportResult {
            exported: 0,
            total,
            output_dir,
        });
    }
    prepare_export_clean_overlay_snapshots(&mut jobs, clean_overlays_model)?;

    let worker_count = thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1)
        .saturating_sub(1)
        .max(1)
        .min(jobs.len());
    let queue = Arc::new(Mutex::new(VecDeque::from(jobs)));
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let mut worker_handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let tx = tx.clone();
        let queue = Arc::clone(&queue);
        worker_handles.push(thread::spawn(move || {
            loop {
                let job = {
                    let mut locked = queue.lock().unwrap_or_else(|p| p.into_inner());
                    locked.pop_front()
                };
                let Some(job) = job else {
                    break;
                };
                if tx.send(export_typing_single_page(job)).is_err() {
                    break;
                }
            }
        }));
    }
    drop(tx);

    let mut exported = 0usize;
    let mut processed = 0usize;
    let mut first_error: Option<String> = None;
    for result in rx {
        processed = processed.saturating_add(1);
        match result {
            Ok(()) => exported = exported.saturating_add(1),
            Err(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
        let _ = progress_tx.send(TypingExportEvent::Progress {
            done: processed,
            total,
        });
    }
    for handle in worker_handles {
        let _ = handle.join();
    }
    if let Some(err) = first_error {
        return Err(err);
    }
    Ok(TypingExportResult {
        exported,
        total,
        output_dir,
    })
}

fn export_typing_single_page(job: TypingExportPageJob) -> Result<(), String> {
    let mut base = image::open(&job.page_path)
        .map_err(|err| {
            format!(
                "Не удалось открыть страницу {}: {err}",
                job.page_path.display()
            )
        })?
        .to_rgba8();
    let base_w = base.width() as usize;
    let base_h = base.height() as usize;
    let base_rgba = base.as_mut();

    if let Some(clean) = job.clean_overlay_rgba.as_ref() {
        composite_overlay_full_image_over(
            base_rgba,
            [base_w, base_h],
            clean.as_raw(),
            [clean.width() as usize, clean.height() as usize],
        );
    }

    for overlay in &job.overlays {
        if overlay.page_idx != job.page_idx {
            continue;
        }
        let deform_mesh = export_overlay_deform_mesh_for_page(overlay, [base_w, base_h]);
        let clipped_rgba = if overlay.mask_clip_enabled {
            job.mask
                .as_ref()
                .and_then(|mask| {
                    export_clip_overlay_rgba_if_needed(
                        mask,
                        overlay.size_px,
                        overlay.source_rgba.as_slice(),
                        &deform_mesh,
                    )
                })
                .unwrap_or_else(|| overlay.source_rgba.clone())
        } else {
            overlay.source_rgba.clone()
        };
        if let Some(top_left_px) = direct_overlay_blit_top_left_px(overlay) {
            composite_overlay_at_page_position_over(
                base_rgba,
                [base_w, base_h],
                clipped_rgba.as_slice(),
                overlay.size_px,
                top_left_px,
            );
        } else {
            composite_overlay_mesh_over_page(
                base_rgba,
                [base_w, base_h],
                clipped_rgba.as_slice(),
                overlay.size_px,
                &deform_mesh,
            );
        }
    }

    image::save_buffer(
        &job.output_path,
        base_rgba,
        base_w as u32,
        base_h as u32,
        image::ColorType::Rgba8,
    )
    .map_err(|err| {
        format!(
            "Не удалось сохранить страницу {}: {err}",
            job.output_path.display()
        )
    })
}

fn prepare_export_clean_overlay_snapshots(
    jobs: &mut [TypingExportPageJob],
    clean_overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
) -> Result<(), String> {
    for job in jobs {
        job.clean_overlay_rgba = load_clean_overlay_snapshot_for_export(
            clean_overlays_model.as_ref(),
            job.page_idx,
            job.clean_overlay_path.as_deref(),
        )?;
    }
    Ok(())
}

fn load_clean_overlay_snapshot_for_export(
    clean_overlays_model: Option<&Arc<Mutex<CleanOverlaysModel>>>,
    page_idx: usize,
    clean_overlay_path: Option<&Path>,
) -> Result<Option<Arc<image::RgbaImage>>, String> {
    let Some(model) = clean_overlays_model else {
        return load_clean_overlay_rgba_from_disk(clean_overlay_path)
            .map(|image| image.map(Arc::new));
    };
    if let Ok(locked) = model.lock()
        && let Some(image) = locked.overlay_rgba(page_idx)
    {
        return Ok(Some(image));
    }
    let Some(decoded) = load_clean_overlay_rgba_from_disk(clean_overlay_path)? else {
        return Ok(None);
    };
    if let Ok(mut locked) = model.lock() {
        if let Some(image) = locked.overlay_rgba(page_idx) {
            return Ok(Some(image));
        }
        locked.replace_from_rgba(page_idx, decoded.clone());
        if let Some(image) = locked.overlay_rgba(page_idx) {
            return Ok(Some(image));
        }
    }
    Ok(Some(Arc::new(decoded)))
}

fn load_clean_overlay_rgba_from_disk(
    clean_overlay_path: Option<&Path>,
) -> Result<Option<image::RgbaImage>, String> {
    let Some(clean_overlay_path) = clean_overlay_path else {
        return Ok(None);
    };
    let clean = image::open(clean_overlay_path)
        .map_err(|err| {
            format!(
                "Не удалось открыть clean overlay {}: {err}",
                clean_overlay_path.display()
            )
        })?
        .to_rgba8();
    Ok(Some(clean))
}

fn composite_overlay_full_image_over(
    base_rgba: &mut [u8],
    base_size: [usize; 2],
    overlay_rgba: &[u8],
    overlay_size: [usize; 2],
) {
    if base_size[0] == 0 || base_size[1] == 0 || overlay_size[0] == 0 || overlay_size[1] == 0 {
        return;
    }
    if base_rgba.len() != base_size[0] * base_size[1] * 4 {
        return;
    }
    if overlay_rgba.len() != overlay_size[0] * overlay_size[1] * 4 {
        return;
    }
    let w = base_size[0].min(overlay_size[0]);
    let h = base_size[1].min(overlay_size[1]);
    for y in 0..h {
        for x in 0..w {
            let dst_idx = (y * base_size[0] + x) * 4;
            let src_idx = (y * overlay_size[0] + x) * 4;
            blend_source_over(
                &mut base_rgba[dst_idx..dst_idx + 4],
                &overlay_rgba[src_idx..src_idx + 4],
            );
        }
    }
}

fn composite_overlay_at_page_position_over(
    base_rgba: &mut [u8],
    base_size: [usize; 2],
    overlay_rgba: &[u8],
    overlay_size: [usize; 2],
    top_left_px: [i32; 2],
) {
    if base_size[0] == 0 || base_size[1] == 0 || overlay_size[0] == 0 || overlay_size[1] == 0 {
        return;
    }
    if base_rgba.len() != base_size[0] * base_size[1] * 4 {
        return;
    }
    if overlay_rgba.len() != overlay_size[0] * overlay_size[1] * 4 {
        return;
    }

    let base_w_i32 = i32::try_from(base_size[0]).unwrap_or(i32::MAX);
    let base_h_i32 = i32::try_from(base_size[1]).unwrap_or(i32::MAX);
    let overlay_w_i32 = i32::try_from(overlay_size[0]).unwrap_or(i32::MAX);
    let overlay_h_i32 = i32::try_from(overlay_size[1]).unwrap_or(i32::MAX);
    let start_x = top_left_px[0].max(0);
    let start_y = top_left_px[1].max(0);
    let end_x = top_left_px[0].saturating_add(overlay_w_i32).min(base_w_i32);
    let end_y = top_left_px[1].saturating_add(overlay_h_i32).min(base_h_i32);
    if start_x >= end_x || start_y >= end_y {
        return;
    }

    for dst_y in start_y..end_y {
        let src_y = dst_y - top_left_px[1];
        for dst_x in start_x..end_x {
            let src_x = dst_x - top_left_px[0];
            let dst_idx = (dst_y as usize * base_size[0] + dst_x as usize) * 4;
            let src_idx = (src_y as usize * overlay_size[0] + src_x as usize) * 4;
            blend_source_over(
                &mut base_rgba[dst_idx..dst_idx + 4],
                &overlay_rgba[src_idx..src_idx + 4],
            );
        }
    }
}

fn composite_overlay_mesh_over_page(
    base_rgba: &mut [u8],
    base_size: [usize; 2],
    overlay_rgba: &[u8],
    overlay_size: [usize; 2],
    deform_mesh: &TypingOverlayDeformMesh,
) {
    if base_size[0] == 0 || base_size[1] == 0 || overlay_size[0] == 0 || overlay_size[1] == 0 {
        return;
    }
    if base_rgba.len() != base_size[0] * base_size[1] * 4 {
        return;
    }
    if overlay_rgba.len() != overlay_size[0] * overlay_size[1] * 4 {
        return;
    }
    if deform_mesh.cols < 2 || deform_mesh.rows < 2 {
        return;
    }

    for row in 0..(deform_mesh.rows - 1) {
        let t0 = row as f32 / (deform_mesh.rows - 1) as f32;
        let t1 = (row + 1) as f32 / (deform_mesh.rows - 1) as f32;
        for col in 0..(deform_mesh.cols - 1) {
            let s0 = col as f32 / (deform_mesh.cols - 1) as f32;
            let s1 = (col + 1) as f32 / (deform_mesh.cols - 1) as f32;
            let p00 = export_scene_from_page_px(base_size, deform_mesh.point(col, row));
            let p10 = export_scene_from_page_px(base_size, deform_mesh.point(col + 1, row));
            let p01 = export_scene_from_page_px(base_size, deform_mesh.point(col, row + 1));
            let p11 = export_scene_from_page_px(base_size, deform_mesh.point(col + 1, row + 1));

            rasterize_textured_triangle(
                base_rgba,
                base_size,
                overlay_rgba,
                overlay_size,
                (p00, [s0, t0]),
                (p10, [s1, t0]),
                (p01, [s0, t1]),
            );
            rasterize_textured_triangle(
                base_rgba,
                base_size,
                overlay_rgba,
                overlay_size,
                (p01, [s0, t1]),
                (p10, [s1, t0]),
                (p11, [s1, t1]),
            );
        }
    }
}

fn rasterize_textured_triangle(
    base_rgba: &mut [u8],
    base_size: [usize; 2],
    overlay_rgba: &[u8],
    overlay_size: [usize; 2],
    v0: ([f32; 2], [f32; 2]),
    v1: ([f32; 2], [f32; 2]),
    v2: ([f32; 2], [f32; 2]),
) {
    fn edge(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
        (p[0] - a[0]) * (b[1] - a[1]) - (p[1] - a[1]) * (b[0] - a[0])
    }

    let area = edge(v0.0, v1.0, v2.0);
    if area.abs() <= f32::EPSILON {
        return;
    }
    let min_x = v0.0[0].min(v1.0[0]).min(v2.0[0]).floor().max(0.0) as i32;
    let max_x = v0.0[0]
        .max(v1.0[0])
        .max(v2.0[0])
        .ceil()
        .min(base_size[0].saturating_sub(1) as f32) as i32;
    let min_y = v0.0[1].min(v1.0[1]).min(v2.0[1]).floor().max(0.0) as i32;
    let max_y = v0.0[1]
        .max(v1.0[1])
        .max(v2.0[1])
        .ceil()
        .min(base_size[1].saturating_sub(1) as f32) as i32;
    if min_x > max_x || min_y > max_y {
        return;
    }

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let p = [x as f32 + 0.5, y as f32 + 0.5];
            let w0 = edge(v1.0, v2.0, p) / area;
            let w1 = edge(v2.0, v0.0, p) / area;
            let w2 = edge(v0.0, v1.0, p) / area;
            if w0 < -0.0001 || w1 < -0.0001 || w2 < -0.0001 {
                continue;
            }

            let s = (w0 * v0.1[0] + w1 * v1.1[0] + w2 * v2.1[0]).clamp(0.0, 1.0);
            let t = (w0 * v0.1[1] + w1 * v1.1[1] + w2 * v2.1[1]).clamp(0.0, 1.0);
            let src = sample_overlay_bilinear_rgba(overlay_rgba, overlay_size, s, t);
            if src[3] == 0 {
                continue;
            }

            let dst_idx = (y as usize * base_size[0] + x as usize) * 4;
            blend_source_over(&mut base_rgba[dst_idx..dst_idx + 4], &src);
        }
    }
}

fn direct_overlay_blit_top_left_px(overlay: &TypingExportOverlaySnapshot) -> Option<[i32; 2]> {
    if overlay.deform_mesh.is_some()
        || overlay.angle_deg.abs() > 1e-4
        || (overlay.user_scale - 1.0).abs() > 1e-4
    {
        return None;
    }
    Some([
        (overlay.center_page_px[0] - overlay.size_px[0] as f32 * 0.5).round() as i32,
        (overlay.center_page_px[1] - overlay.size_px[1] as f32 * 0.5).round() as i32,
    ])
}

fn sample_overlay_bilinear_rgba(rgba: &[u8], size: [usize; 2], s: f32, t: f32) -> [u8; 4] {
    let w = size[0].max(1);
    let h = size[1].max(1);
    if rgba.len() != w * h * 4 {
        return [0, 0, 0, 0];
    }
    if w == 1 || h == 1 {
        let x = if w == 1 {
            0
        } else {
            (s.clamp(0.0, 1.0) * (w.saturating_sub(1)) as f32).round() as usize
        };
        let y = if h == 1 {
            0
        } else {
            (t.clamp(0.0, 1.0) * (h.saturating_sub(1)) as f32).round() as usize
        };
        let idx = (y * w + x) * 4;
        return [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]];
    }

    let fx = (s.clamp(0.0, 1.0) * w as f32 - 0.5).clamp(0.0, (w - 1) as f32);
    let fy = (t.clamp(0.0, 1.0) * h as f32 - 0.5).clamp(0.0, (h - 1) as f32);
    let x0 = fx.floor().clamp(0.0, (w - 1) as f32) as usize;
    let y0 = fy.floor().clamp(0.0, (h - 1) as f32) as usize;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let tx = fx - x0 as f32;
    let ty = fy - y0 as f32;

    let i00 = (y0 * w + x0) * 4;
    let i10 = (y0 * w + x1) * 4;
    let i01 = (y1 * w + x0) * 4;
    let i11 = (y1 * w + x1) * 4;

    let bilerp = |v00: f32, v10: f32, v01: f32, v11: f32| {
        let top = v00 + (v10 - v00) * tx;
        let bot = v01 + (v11 - v01) * tx;
        top + (bot - top) * ty
    };

    // Interpolate in premultiplied alpha to avoid matte-color fringing
    // on semi-transparent glyph edges during export.
    let a00 = rgba[i00 + 3] as f32 / 255.0;
    let a10 = rgba[i10 + 3] as f32 / 255.0;
    let a01 = rgba[i01 + 3] as f32 / 255.0;
    let a11 = rgba[i11 + 3] as f32 / 255.0;
    let out_a = bilerp(a00, a10, a01, a11).clamp(0.0, 1.0);
    if out_a <= f32::EPSILON {
        return [0, 0, 0, 0];
    }

    let mut out = [0u8; 4];
    for c in 0..3 {
        let p00 = (rgba[i00 + c] as f32 / 255.0) * a00;
        let p10 = (rgba[i10 + c] as f32 / 255.0) * a10;
        let p01 = (rgba[i01 + c] as f32 / 255.0) * a01;
        let p11 = (rgba[i11 + c] as f32 / 255.0) * a11;
        let out_p = bilerp(p00, p10, p01, p11).clamp(0.0, 1.0);
        let out_c = (out_p / out_a).clamp(0.0, 1.0);
        out[c] = (out_c * 255.0).round() as u8;
    }
    out[3] = (out_a * 255.0).round() as u8;
    out
}

fn blend_source_over(dst: &mut [u8], src: &[u8]) {
    if dst.len() < 4 || src.len() < 4 {
        return;
    }
    let sa = src[3] as f32 / 255.0;
    if sa <= 0.0 {
        return;
    }
    let da = dst[3] as f32 / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        dst[0] = 0;
        dst[1] = 0;
        dst[2] = 0;
        dst[3] = 0;
        return;
    }

    for c in 0..3 {
        let s = src[c] as f32 / 255.0;
        let d = dst[c] as f32 / 255.0;
        let out = (s * sa + d * da * (1.0 - sa)) / out_a;
        dst[c] = (out * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    dst[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

fn export_overlay_deform_mesh_for_page(
    overlay: &TypingExportOverlaySnapshot,
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    overlay.deform_mesh.clone().unwrap_or_else(|| {
        default_deform_mesh_for_page(
            overlay.center_page_px,
            overlay.size_px,
            overlay.user_scale,
            overlay.angle_deg,
            page_size,
        )
    })
}

fn default_quad_uv_for_page(
    center_page_px: [f32; 2],
    overlay_size_px: [usize; 2],
    user_scale: f32,
    angle_deg: f32,
    page_size: [usize; 2],
) -> [[f32; 2]; 4] {
    let page_w = page_size[0].max(1) as f32;
    let page_h = page_size[1].max(1) as f32;
    let center_scene = clamp_page_point(center_page_px, page_size);
    let half_w = overlay_size_px[0] as f32 * user_scale.max(0.01) * 0.5;
    let half_h = overlay_size_px[1] as f32 * user_scale.max(0.01) * 0.5;
    let mut quad_scene = [
        [center_scene[0] - half_w, center_scene[1] - half_h],
        [center_scene[0] + half_w, center_scene[1] - half_h],
        [center_scene[0] + half_w, center_scene[1] + half_h],
        [center_scene[0] - half_w, center_scene[1] + half_h],
    ];
    if angle_deg.abs() > f32::EPSILON {
        let angle = angle_deg.to_radians();
        let (sin_a, cos_a) = angle.sin_cos();
        for point in &mut quad_scene {
            let dx = point[0] - center_scene[0];
            let dy = point[1] - center_scene[1];
            point[0] = center_scene[0] + dx * cos_a - dy * sin_a;
            point[1] = center_scene[1] + dx * sin_a + dy * cos_a;
        }
    }

    let quad_uv = quad_scene.map(|point| [point[0] / page_w, point[1] / page_h]);
    clamp_quad_uv(quad_uv)
}

fn export_scene_from_page_px(page_size: [usize; 2], page_px: [f32; 2]) -> [f32; 2] {
    clamp_page_point(page_px, page_size)
}

fn export_bilinear_quad_uv(quad_uv: [[f32; 2]; 4], tu: f32, tv: f32) -> [f32; 2] {
    let t = tu.clamp(0.0, 1.0);
    let v = tv.clamp(0.0, 1.0);
    let top_u = quad_uv[0][0] + (quad_uv[1][0] - quad_uv[0][0]) * t;
    let top_v = quad_uv[0][1] + (quad_uv[1][1] - quad_uv[0][1]) * t;
    let bot_u = quad_uv[3][0] + (quad_uv[2][0] - quad_uv[3][0]) * t;
    let bot_v = quad_uv[3][1] + (quad_uv[2][1] - quad_uv[3][1]) * t;
    [top_u + (bot_u - top_u) * v, top_v + (bot_v - top_v) * v]
}

fn bilinear_quad_page_px(quad_px: [[f32; 2]; 4], tu: f32, tv: f32) -> [f32; 2] {
    let t = tu.clamp(0.0, 1.0);
    let v = tv.clamp(0.0, 1.0);
    let top_x = quad_px[0][0] + (quad_px[1][0] - quad_px[0][0]) * t;
    let top_y = quad_px[0][1] + (quad_px[1][1] - quad_px[0][1]) * t;
    let bot_x = quad_px[3][0] + (quad_px[2][0] - quad_px[3][0]) * t;
    let bot_y = quad_px[3][1] + (quad_px[2][1] - quad_px[3][1]) * t;
    [top_x + (bot_x - top_x) * v, top_y + (bot_y - top_y) * v]
}

fn export_clip_overlay_rgba_if_needed(
    mask: &TypingMaskExportPage,
    overlay_size: [usize; 2],
    overlay_rgba: &[u8],
    overlay_deform_mesh: &TypingOverlayDeformMesh,
) -> Option<Vec<u8>> {
    if overlay_size[0] == 0 || overlay_size[1] == 0 {
        return None;
    }
    if overlay_rgba.len() != overlay_size[0] * overlay_size[1] * 4 {
        return None;
    }
    if mask.width == 0 || mask.height == 0 || mask.data.len() != mask.width * mask.height {
        return None;
    }

    let mut out = overlay_rgba.to_vec();
    let mut touched_active = false;
    for y in 0..overlay_size[1] {
        let tv = (y as f32 + 0.5) / overlay_size[1] as f32;
        for x in 0..overlay_size[0] {
            let tu = (x as f32 + 0.5) / overlay_size[0] as f32;
            let px_idx = (y * overlay_size[0] + x) * 4;
            if out[px_idx + 3] == 0 {
                continue;
            }
            let uv = sample_deform_mesh_uv(overlay_deform_mesh, tu, tv, [mask.width, mask.height]);
            let active = export_sample_mask_active(mask, uv[0], uv[1]);
            if active {
                touched_active = true;
            } else {
                out[px_idx + 3] = 0;
            }
        }
    }
    if touched_active { Some(out) } else { None }
}

fn export_sample_mask_active(mask: &TypingMaskExportPage, u: f32, v: f32) -> bool {
    if mask.width == 0 || mask.height == 0 {
        return false;
    }
    let x = (u.clamp(0.0, 1.0) * (mask.width.saturating_sub(1)) as f32).round() as usize;
    let y = (v.clamp(0.0, 1.0) * (mask.height.saturating_sub(1)) as f32).round() as usize;
    mask.data
        .get(y.saturating_mul(mask.width).saturating_add(x))
        .is_some_and(|v| *v > 0)
}

fn next_created_overlay_file_name(text_images_dir: &Path, page_idx: usize) -> String {
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|dur| dur.as_millis())
        .unwrap_or(0);
    let page_token = page_idx.saturating_add(1);
    for attempt in 0..10_000usize {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("_{attempt}")
        };
        let candidate = format!("typing_overlay_p{page_token:04}_{unix_ms}{suffix}.png");
        if !text_images_dir.join(&candidate).exists() {
            return candidate;
        }
    }
    format!("typing_overlay_p{page_token:04}_{unix_ms}_fallback.png")
}

fn layout_image_file_name_for_overlay(file_name: &str) -> String {
    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .and_then(|raw| raw.to_str())
        .filter(|raw| !raw.is_empty())
        .unwrap_or(file_name);
    let extension = path
        .extension()
        .and_then(|raw| raw.to_str())
        .filter(|raw| !raw.is_empty())
        .unwrap_or("png");
    format!("{stem}{TEXT_LAYOUT_IMAGE_SUFFIX}.{extension}")
}

fn render_params_with_adjacent_layout_path(
    text_images_dir: &Path,
    overlay_file_name: &str,
    render_params: &TextRenderParams,
) -> TextRenderParams {
    let mut out = render_params.clone();
    if out.text_layout_mode == TextLayoutMode::CustomRasterLines {
        out.drawn_lines_layout.image_path =
            Some(text_images_dir.join(layout_image_file_name_for_overlay(overlay_file_name)));
    }
    out
}

fn save_drawn_lines_layout_image_if_needed(
    text_images_dir: &Path,
    overlay_file_name: &str,
    render_params: &TextRenderParams,
    width: u32,
    height: u32,
) -> Result<Option<PathBuf>, String> {
    if render_params.text_layout_mode != TextLayoutMode::CustomRasterLines {
        return Ok(None);
    }
    let pixel_count = usize::try_from(width)
        .ok()
        .and_then(|width_usize| {
            usize::try_from(height)
                .ok()
                .map(|height_usize| width_usize.saturating_mul(height_usize))
        })
        .ok_or_else(|| "Размер layout-изображения не помещается в память.".to_string())?;
    let layout_path = text_images_dir.join(layout_image_file_name_for_overlay(overlay_file_name));
    if layout_path.is_file() {
        return Ok(Some(layout_path));
    }
    let rgba = vec![0u8; pixel_count.saturating_mul(4)];
    image::save_buffer(
        &layout_path,
        rgba.as_slice(),
        width.max(1),
        height.max(1),
        image::ColorType::Rgba8,
    )
    .map_err(|err| format!("Не удалось сохранить {}: {err}", layout_path.display()))?;
    Ok(Some(layout_path))
}

fn read_image_rgba_from_file(path: &Path) -> Result<(Vec<u8>, usize, usize), String> {
    let img = image::open(path)
        .map_err(|err| format!("Не удалось открыть {}: {err}", path.display()))?
        .to_rgba8();
    let width = img.width() as usize;
    let height = img.height() as usize;
    Ok((img.into_raw(), width, height))
}

fn read_image_rgba_from_clipboard() -> Result<(Vec<u8>, usize, usize), String> {
    let image = paste_image::read_image_from_clipboard()?;
    Ok((image.rgba, image.width, image.height))
}

fn parse_effects_json_array(raw: &str) -> Vec<Value> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

// Parameters represent distinct required inputs with no natural grouping.
#[allow(clippy::too_many_arguments)]
fn append_created_overlay_info(
    text_images_dir: &Path,
    page_idx: usize,
    center_page_px: [f32; 2],
    mask_clip_enabled: bool,
    user_scale: f32,
    file_name: &str,
    kind: TypingOverlayKind,
    render_data_json: Option<Value>,
) -> Result<(), String> {
    let text_info_path = text_images_dir.join(TEXT_INFO_FILE_NAME);
    let mut items = if text_info_path.is_file() {
        let raw = fs::read_to_string(&text_info_path)
            .map_err(|err| format!("Не удалось прочитать {}: {err}", text_info_path.display()))?;
        let parsed: Value = serde_json::from_str(&raw)
            .map_err(|err| format!("Не удалось распарсить {}: {err}", text_info_path.display()))?;
        parsed.as_array().cloned().ok_or_else(|| {
            format!(
                "Файл {} должен содержать JSON-массив.",
                text_info_path.display()
            )
        })?
    } else {
        Vec::new()
    };

    items.push(build_storage_overlay_entry(
        kind,
        page_idx,
        file_name,
        center_page_px,
        mask_clip_enabled,
        0.0,
        user_scale,
        None,
        render_data_json,
    ));

    write_overlay_items_to_text_info(text_images_dir, &items)
}

fn write_overlay_items_to_text_info(text_images_dir: &Path, items: &[Value]) -> Result<(), String> {
    fs::create_dir_all(text_images_dir).map_err(|err| {
        format!(
            "Не удалось создать папку {}: {err}",
            text_images_dir.display()
        )
    })?;

    let text_info_path = text_images_dir.join(TEXT_INFO_FILE_NAME);
    let raw = serde_json::to_string_pretty(items).map_err(|err| {
        format!(
            "Не удалось сериализовать {}: {err}",
            text_info_path.display()
        )
    })?;
    fs::write(&text_info_path, raw)
        .map_err(|err| format!("Не удалось записать {}: {err}", text_info_path.display()))
}

// Parameters represent distinct required inputs with no natural grouping.
#[allow(clippy::too_many_arguments)]
fn build_storage_overlay_entry(
    kind: TypingOverlayKind,
    page_idx: usize,
    file_name: &str,
    center_page_px: [f32; 2],
    mask_clip_enabled: bool,
    rotation_deg: f32,
    scale: f32,
    deform_mesh: Option<TypingOverlayDeformMesh>,
    render_data: Option<Value>,
) -> Value {
    let mut out = serde_json::Map::<String, Value>::new();
    out.insert(
        "overlay_type".to_string(),
        Value::String(
            match kind {
                TypingOverlayKind::Text => "text",
                TypingOverlayKind::Image => "image",
            }
            .to_string(),
        ),
    );
    out.insert("img_idx".to_string(), Value::from(page_idx as u64));
    out.insert("file".to_string(), Value::String(file_name.to_string()));
    out.insert("img_x_px".to_string(), Value::from(center_page_px[0]));
    out.insert("img_y_px".to_string(), Value::from(center_page_px[1]));
    out.insert(
        "mask_clip_enabled".to_string(),
        Value::from(mask_clip_enabled),
    );
    out.insert("rotation_deg".to_string(), Value::from(rotation_deg));
    out.insert("scale".to_string(), Value::from(scale.max(0.01)));
    if let Some(mesh) = deform_mesh {
        let points = mesh
            .points_px
            .iter()
            .map(|[x, y]| Value::Array(vec![Value::from(*x), Value::from(*y)]))
            .collect::<Vec<_>>();
        out.insert(
            "deform_mesh".to_string(),
            json!({
                "cols": mesh.cols,
                "rows": mesh.rows,
                "points_px": points,
            }),
        );
    }
    if let Some(render_data) = render_data {
        out.insert("render_data".to_string(), render_data);
    }
    Value::Object(out)
}

fn parse_overlay_render_data_json(
    obj: &serde_json::Map<String, Value>,
    fallback_width_px: u32,
) -> Option<Value> {
    if let Some(render_data_value) = obj.get("render_data")
        && let Some(normalized) = normalize_render_data_value(render_data_value, fallback_width_px)
    {
        return Some(normalized);
    }
    if let Some(render_params) = obj.get("render_params").and_then(Value::as_object) {
        return Some(render_params_object_to_render_data(
            render_params,
            fallback_width_px,
        ));
    }
    parse_legacy_static_render_data(obj, fallback_width_px)
}

fn normalize_render_data_value(value: &Value, fallback_width_px: u32) -> Option<Value> {
    let obj = value.as_object()?;
    if obj.get("text_params").and_then(Value::as_object).is_some() {
        let text_params_obj = obj
            .get("text_params")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let text_params = normalize_text_params_object(&text_params_obj, fallback_width_px);
        let effects = obj
            .get("effects")
            .and_then(Value::as_array)
            .cloned()
            .or_else(|| {
                obj.get("effects_json")
                    .and_then(Value::as_str)
                    .map(parse_effects_json_array)
            })
            .unwrap_or_default();
        return Some(json!({
            "schema_version": 2,
            "text_params": text_params,
            "effects": effects,
        }));
    }
    Some(render_params_object_to_render_data(obj, fallback_width_px))
}

fn render_params_object_to_render_data(
    obj: &serde_json::Map<String, Value>,
    fallback_width_px: u32,
) -> Value {
    let text_params = normalize_text_params_object(obj, fallback_width_px);
    let effects = parse_effects_list_from_render_params_object(obj);
    json!({
        "schema_version": 2,
        "text_params": text_params,
        "effects": effects,
    })
}

fn normalize_text_params_object(
    obj: &serde_json::Map<String, Value>,
    fallback_width_px: u32,
) -> Value {
    let text = obj
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let text_color = obj
        .get("text_color")
        .and_then(parse_rgba_value)
        .or_else(|| obj.get("font_color_rgba").and_then(parse_rgba_value))
        .or_else(|| obj.get("color").and_then(parse_rgba_value))
        .unwrap_or([0, 0, 0, 255]);
    let font_path = obj
        .get("font_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let font_label = obj
        .get("font_label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            obj.get("font_family")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            obj.get("font")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        });
    let width_px = obj
        .get("width_px")
        .and_then(value_as_f32)
        .map(|v| v.round().max(1.0) as u32)
        .unwrap_or_else(|| fallback_width_px.max(1));
    let align =
        normalize_align_legacy(obj.get("align").and_then(Value::as_str).unwrap_or("center"));
    let text_shape = normalize_text_shape_legacy(
        obj.get("text_shape")
            .and_then(Value::as_str)
            .unwrap_or("rectangle"),
    );
    let text_line_mode = normalize_text_line_mode_legacy(
        obj.get("text_line_mode")
            .and_then(Value::as_str)
            .unwrap_or("horizontal"),
    );
    let text_layout_mode = normalize_text_layout_mode_legacy(
        obj.get("text_layout_mode")
            .and_then(Value::as_str)
            .unwrap_or("normal"),
    );
    let text_wrap_mode = normalize_text_wrap_mode_legacy(
        obj.get("text_wrap_mode").and_then(Value::as_str),
        obj.get("aggressive_word_breaks").and_then(Value::as_bool),
        obj.get("allow_moderate_trees").and_then(Value::as_bool),
    );
    let formula_layout =
        normalize_formula_layout_object(obj.get("formula_layout").and_then(Value::as_object));
    let shape_layout =
        normalize_shape_layout_object(obj.get("shape_layout").and_then(Value::as_object));
    let drawn_lines_layout = normalize_drawn_lines_layout_object(
        obj.get("drawn_lines_layout").and_then(Value::as_object),
    );
    let vector_lines_layout = normalize_vector_lines_layout_object(
        obj.get("vector_lines_layout").and_then(Value::as_object),
    );
    let selected_face_index = obj
        .get("selected_face_index")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0usize);

    json!({
        "text": text,
        "text_color": text_color,
        "font_path": font_path,
        "font_label": font_label,
        "font_size_px": obj.get("font_size_px").and_then(value_as_f32).or_else(|| obj.get("font_size").and_then(value_as_f32)).or_else(|| obj.get("size").and_then(value_as_f32)).unwrap_or(24.0).max(1.0),
        "line_spacing_px": obj.get("line_spacing_px").and_then(value_as_f32).or_else(|| obj.get("line_spacing").and_then(value_as_f32)).unwrap_or(4.0),
        "line_spacing_percent": obj.get("line_spacing_percent").and_then(value_as_f32).unwrap_or(50.0),
        "width_px": width_px,
        "align": align,
        "text_line_mode": text_line_mode,
        "text_layout_mode": text_layout_mode,
        "formula_layout": formula_layout,
        "shape_layout": shape_layout,
        "drawn_lines_layout": drawn_lines_layout,
        "vector_lines_layout": vector_lines_layout,
        "selected_face_index": selected_face_index,
        "force_bold": obj.get("force_bold").and_then(Value::as_bool).unwrap_or(false),
        "force_italic": obj.get("force_italic").and_then(Value::as_bool).unwrap_or(false),
        "uppercase_text": obj.get("uppercase_text").and_then(Value::as_bool).unwrap_or(false),
        "enable_inline_style_tags": obj.get("enable_inline_style_tags").and_then(Value::as_bool).unwrap_or(false),
        "text_wrap_mode": text_wrap_mode,
        "allow_moderate_trees": obj.get("allow_moderate_trees").and_then(Value::as_bool).unwrap_or(false),
        "text_shape": text_shape,
        "shape_min_width_percent": obj.get("shape_min_width_percent").and_then(value_as_f32).unwrap_or(50.0),
        "shape_variant": obj.get("shape_variant").and_then(Value::as_u64).unwrap_or(5).clamp(1, 9),
    })
}

fn parse_effects_list_from_render_params_object(
    obj: &serde_json::Map<String, Value>,
) -> Vec<Value> {
    if let Some(effects) = obj.get("effects").and_then(Value::as_array) {
        return effects.clone();
    }
    if let Some(effects_json) = obj.get("effects_json").and_then(Value::as_str) {
        return parse_effects_json_array(effects_json);
    }
    Vec::new()
}

fn parse_legacy_static_render_data(
    obj: &serde_json::Map<String, Value>,
    fallback_width_px: u32,
) -> Option<Value> {
    let style = obj.get("style").and_then(Value::as_object);
    let text = obj
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if text.is_empty() && style.is_none() {
        return None;
    }

    let font_label = overlay_param_str(style, obj, "font_family")
        .or_else(|| overlay_param_str(style, obj, "font"))
        .unwrap_or_default();
    let font_size_px = overlay_param_f32(style, obj, "font_size")
        .or_else(|| overlay_param_f32(style, obj, "size"))
        .unwrap_or(24.0);
    let text_color = overlay_param_rgba(style, obj, "font_color_rgba")
        .or_else(|| overlay_param_rgba(style, obj, "color"))
        .unwrap_or([0, 0, 0, 255]);
    let line_spacing_px = overlay_param_f32(style, obj, "line_spacing").unwrap_or(4.0);
    let line_spacing_percent =
        overlay_param_f32(style, obj, "line_spacing_percent").unwrap_or(50.0);
    let align = normalize_align_legacy(
        overlay_param_str(style, obj, "align")
            .unwrap_or_else(|| "center".to_string())
            .as_str(),
    );
    let text_shape = normalize_text_shape_legacy(
        overlay_param_str(style, obj, "text_shape")
            .unwrap_or_else(|| "rectangle".to_string())
            .as_str(),
    );
    let width_px = overlay_param_f32(style, obj, "width_px")
        .or_else(|| obj.get("width_px").and_then(value_as_f32))
        .map(|v| v.round().max(1.0) as u32)
        .unwrap_or_else(|| fallback_width_px.max(1));

    let effects = build_legacy_effects_json(style, obj);
    Some(json!({
        "schema_version": 2,
        "source": "legacy_static_style",
        "text_params": {
            "text": text,
            "text_color": text_color,
            "font_path": Value::Null,
            "font_label": font_label,
            "font_size_px": font_size_px.max(1.0),
            "line_spacing_px": line_spacing_px,
            "line_spacing_percent": line_spacing_percent,
            "width_px": width_px,
            "align": align,
            "text_line_mode": "horizontal",
            "text_layout_mode": "normal",
            "formula_layout": normalize_formula_layout_object(None),
            "drawn_lines_layout": normalize_drawn_lines_layout_object(None),
            "vector_lines_layout": normalize_vector_lines_layout_object(None),
            "selected_face_index": 0,
            "force_bold": false,
            "force_italic": false,
            "uppercase_text": false,
            "enable_inline_style_tags": false,
            "text_wrap_mode": "aggressive",
            "text_shape": text_shape,
            "shape_min_width_percent": 50.0,
            "shape_variant": 5,
        },
        "effects": effects,
    }))
}

fn build_legacy_effects_json(
    style: Option<&serde_json::Map<String, Value>>,
    obj: &serde_json::Map<String, Value>,
) -> Vec<Value> {
    let mut out = Vec::<Value>::new();

    let stroke_width = overlay_param_f32(style, obj, "stroke_width").unwrap_or(0.0);
    if stroke_width > 0.0 {
        out.push(json!({
            "effect": "stroke",
            "enabled": true,
            "width": stroke_width,
            "color": overlay_param_rgba(style, obj, "stroke_color_rgba").unwrap_or([0, 0, 0, 255]),
            "opacity_mode": "static",
            "transparency": 0.0,
            "opacity": 100.0,
        }));
    }

    if let Some(shadow_color) = overlay_param_rgba(style, obj, "shadow_color_rgba") {
        out.push(json!({
            "effect": "shadow",
            "enabled": true,
            "offset_x": overlay_param_i32(style, obj, "shadow_dx").unwrap_or(0),
            "offset_y": overlay_param_i32(style, obj, "shadow_dy").unwrap_or(0),
            "transparency": 0.0,
            "opacity": 100.0,
            "mode": "single",
            "use_source_color": false,
            "color": shadow_color,
        }));
    }

    let glow_radius = overlay_param_f32(style, obj, "glow_radius").unwrap_or(0.0);
    if glow_radius > 0.0
        && let Some(glow_color) = overlay_param_rgba(style, obj, "glow_color_rgba")
    {
        out.push(json!({
            "effect": "glow_v1",
            "enabled": true,
            "radius": glow_radius,
            "color": glow_color,
            "opacity_mode": "static",
            "transparency": 0.0,
            "opacity": 100.0,
            "fade_strength": 0.0,
            "fade_shift": 0.0,
        }));
    }

    let grad2_c1 = overlay_param_rgba(style, obj, "grad2_c1_rgba");
    let grad2_c2 = overlay_param_rgba(style, obj, "grad2_c2_rgba");
    if let (Some(c1), Some(c2)) = (grad2_c1, grad2_c2) {
        out.push(json!({
            "effect": "gradient2",
            "enabled": true,
            "color1": c1,
            "color2": c2,
            "angle_deg": overlay_param_f32(style, obj, "grad_angle_deg").unwrap_or(90.0),
            "respect_source_alpha": true,
            "fill_mode": "all_opaque",
        }));
    }

    let grad4_tl = overlay_param_rgba(style, obj, "grad4_tl_rgba");
    let grad4_tr = overlay_param_rgba(style, obj, "grad4_tr_rgba");
    let grad4_bl = overlay_param_rgba(style, obj, "grad4_bl_rgba");
    let grad4_br = overlay_param_rgba(style, obj, "grad4_br_rgba");
    if let (Some(tl), Some(tr), Some(bl), Some(br)) = (grad4_tl, grad4_tr, grad4_bl, grad4_br) {
        out.push(json!({
            "effect": "gradient4",
            "enabled": true,
            "color_top_left": tl,
            "color_top_right": tr,
            "color_bottom_left": bl,
            "color_bottom_right": br,
            "respect_source_alpha": true,
            "fill_mode": "all_opaque",
        }));
    }

    if let Some(axis_raw) = overlay_param_str(style, obj, "reflect") {
        let axis = axis_raw.trim().to_ascii_lowercase();
        if axis == "x" || axis == "y" {
            out.push(json!({
                "effect": "reflect",
                "enabled": true,
                "axis": axis,
            }));
        }
    }

    if overlay_param_bool(style, obj, "shake_enabled").unwrap_or(false) {
        out.push(json!({
            "effect": "shake",
            "enabled": true,
            "angle_deg": overlay_param_f32(style, obj, "shake_angle_deg").unwrap_or(90.0),
            "up": overlay_param_f32(style, obj, "shake_up").unwrap_or(0.0),
            "down": overlay_param_f32(style, obj, "shake_down").unwrap_or(40.0),
            "steps": overlay_param_i32(style, obj, "shake_steps").unwrap_or(12).max(0) as u32,
            "base_fade": overlay_param_f32(style, obj, "shake_base_fade").unwrap_or(0.30),
            "decay": overlay_param_f32(style, obj, "shake_decay").unwrap_or(0.15),
            "blur": overlay_param_i32(style, obj, "shake_blur").unwrap_or(2).max(0) as u32,
            "autogrow": true,
            "grow_margin": 0,
        }));
    }

    out
}

fn overlay_param_value<'a>(
    style: Option<&'a serde_json::Map<String, Value>>,
    obj: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Option<&'a Value> {
    style.and_then(|map| map.get(key)).or_else(|| obj.get(key))
}

fn overlay_param_str(
    style: Option<&serde_json::Map<String, Value>>,
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<String> {
    overlay_param_value(style, obj, key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn overlay_param_bool(
    style: Option<&serde_json::Map<String, Value>>,
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<bool> {
    overlay_param_value(style, obj, key).and_then(Value::as_bool)
}

fn overlay_param_f32(
    style: Option<&serde_json::Map<String, Value>>,
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<f32> {
    overlay_param_value(style, obj, key).and_then(value_as_f32)
}

fn overlay_param_i32(
    style: Option<&serde_json::Map<String, Value>>,
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<i32> {
    let value = overlay_param_value(style, obj, key)?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))
        .or_else(|| value.as_f64().map(|v| v.round() as i64))
        .and_then(|v| i32::try_from(v).ok())
}

fn overlay_param_rgba(
    style: Option<&serde_json::Map<String, Value>>,
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<[u8; 4]> {
    overlay_param_value(style, obj, key).and_then(parse_rgba_value)
}

fn parse_rgba_value(value: &Value) -> Option<[u8; 4]> {
    let arr = value.as_array()?;
    if arr.len() < 3 {
        return None;
    }
    let r = value_as_u8(arr.first()?)?;
    let g = value_as_u8(arr.get(1)?)?;
    let b = value_as_u8(arr.get(2)?)?;
    let a = arr.get(3).and_then(value_as_u8).unwrap_or(255);
    Some([r, g, b, a])
}

fn value_as_u8(value: &Value) -> Option<u8> {
    if let Some(v) = value.as_u64() {
        return u8::try_from(v).ok();
    }
    value.as_f64().map(|v| v.round().clamp(0.0, 255.0) as u8)
}

fn value_as_f32(value: &Value) -> Option<f32> {
    value.as_f64().map(|v| v as f32)
}

fn normalize_align_legacy(value: &str) -> &'static str {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "left" | "center" | "right" | "justify" => normalized_to_static(&normalized),
        _ => "center",
    }
}

fn normalize_text_shape_legacy(value: &str) -> &'static str {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "free" | "rectangle" | "oval" | "hexagon" | "soft_peak" => {
            normalized_to_static(&normalized)
        }
        "soft" | "no_trees" => "soft_peak",
        _ => "rectangle",
    }
}

fn normalize_text_line_mode_legacy(value: &str) -> &'static str {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "horizontal" | "vertical" => normalized_to_static(&normalized),
        _ => "horizontal",
    }
}

fn normalize_text_layout_mode_legacy(value: &str) -> &'static str {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "normal" | "formula" | "shape" | "custom_raster_lines" | "custom_vector_lines" => {
            normalized_to_static(&normalized)
        }
        "drawn_lines"
        | "drawn-lines"
        | "drawnlines"
        | "custom-raster-lines"
        | "customrasterlines" => "custom_raster_lines",
        "vector_lines"
        | "vector-lines"
        | "vectorlines"
        | "custom-vector-lines"
        | "customvectorlines" => "custom_vector_lines",
        _ => "normal",
    }
}

fn normalize_text_wrap_mode_legacy(
    value: Option<&str>,
    aggressive_word_breaks: Option<bool>,
    allow_moderate_trees: Option<bool>,
) -> &'static str {
    let normalized = value
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(str::to_ascii_lowercase);
    match normalized.as_deref() {
        Some("none") => "none",
        Some("whole_words" | "words" | "word") => "whole_words",
        Some("minimal") => "minimal",
        Some("moderate") => "moderate",
        Some("aggressive") => "aggressive",
        Some("smart") => match aggressive_word_breaks {
            Some(true) => "aggressive",
            Some(false) => "minimal",
            None if allow_moderate_trees.unwrap_or(false) => "minimal",
            None => "aggressive",
        },
        _ => "aggressive",
    }
}

fn normalize_shape_layout_object(
    obj: Option<&serde_json::Map<String, Value>>,
) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::<String, Value>::new();
    out.insert("kind".to_string(), Value::String("arc".to_string()));
    out.insert(
        "width_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("width_px"))
                .and_then(value_as_f32)
                .unwrap_or(320.0),
        ),
    );
    out.insert(
        "height_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("height_px"))
                .and_then(value_as_f32)
                .unwrap_or(80.0),
        ),
    );
    out.insert(
        "frequency".to_string(),
        Value::from(
            obj.and_then(|v| v.get("frequency"))
                .and_then(value_as_f32)
                .unwrap_or(1.0),
        ),
    );
    out
}

fn normalize_formula_layout_object(
    obj: Option<&serde_json::Map<String, Value>>,
) -> serde_json::Map<String, Value> {
    let defaults = TextFormulaLayoutParams::default();
    let mut out = serde_json::Map::<String, Value>::new();
    out.insert(
        "x_expr".to_string(),
        Value::String(
            obj.and_then(|v| v.get("x_expr"))
                .and_then(Value::as_str)
                .unwrap_or(defaults.x_expr.as_str())
                .to_string(),
        ),
    );
    out.insert(
        "y_expr".to_string(),
        Value::String(
            obj.and_then(|v| v.get("y_expr"))
                .and_then(Value::as_str)
                .unwrap_or(defaults.y_expr.as_str())
                .to_string(),
        ),
    );
    out.insert(
        "rotation_expr".to_string(),
        Value::String(
            obj.and_then(|v| v.get("rotation_expr"))
                .and_then(Value::as_str)
                .unwrap_or(defaults.rotation_expr.as_str())
                .to_string(),
        ),
    );
    out.insert(
        "use_tangent_rotation".to_string(),
        Value::from(
            obj.and_then(|v| v.get("use_tangent_rotation"))
                .and_then(Value::as_bool)
                .unwrap_or(defaults.use_tangent_rotation),
        ),
    );
    out.insert(
        "t_start".to_string(),
        Value::from(
            obj.and_then(|v| v.get("t_start"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.t_start),
        ),
    );
    out.insert(
        "t_end".to_string(),
        Value::from(
            obj.and_then(|v| v.get("t_end"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.t_end),
        ),
    );
    out.insert(
        "offset_x_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("offset_x_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.offset_x_px),
        ),
    );
    out.insert(
        "offset_y_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("offset_y_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.offset_y_px),
        ),
    );
    out.insert(
        "scale_x".to_string(),
        Value::from(
            obj.and_then(|v| v.get("scale_x"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.scale_x),
        ),
    );
    out.insert(
        "scale_y".to_string(),
        Value::from(
            obj.and_then(|v| v.get("scale_y"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.scale_y),
        ),
    );
    out.insert(
        "normal_offset_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("normal_offset_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.normal_offset_px),
        ),
    );
    out.insert(
        "letter_spacing_mul".to_string(),
        Value::from(
            obj.and_then(|v| v.get("letter_spacing_mul"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.letter_spacing_mul),
        ),
    );
    out.insert(
        "letter_spacing_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("letter_spacing_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.letter_spacing_px),
        ),
    );
    out.insert(
        "vars".to_string(),
        Value::Array(normalize_formula_vars_array(
            obj.and_then(|v| v.get("vars")).and_then(Value::as_array),
            defaults.vars,
        )),
    );
    out
}

fn normalize_drawn_lines_layout_object(
    obj: Option<&serde_json::Map<String, Value>>,
) -> serde_json::Map<String, Value> {
    let defaults = TextDrawnLinesLayoutParams::default();
    let mut out = serde_json::Map::<String, Value>::new();
    out.insert(
        "use_tangent_rotation".to_string(),
        Value::from(
            obj.and_then(|v| v.get("use_tangent_rotation"))
                .and_then(Value::as_bool)
                .unwrap_or(defaults.use_tangent_rotation),
        ),
    );
    out.insert(
        "static_rotation_rad".to_string(),
        Value::from(
            obj.and_then(|v| v.get("static_rotation_rad"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.static_rotation_rad),
        ),
    );
    out.insert(
        "normal_offset_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("normal_offset_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.normal_offset_px),
        ),
    );
    out.insert(
        "letter_spacing_mul".to_string(),
        Value::from(
            obj.and_then(|v| v.get("letter_spacing_mul"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.letter_spacing_mul)
                .clamp(0.0, 8.0),
        ),
    );
    out.insert(
        "letter_spacing_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("letter_spacing_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.letter_spacing_px)
                .clamp(-10_000.0, 10_000.0),
        ),
    );
    out.insert(
        "color_tolerance".to_string(),
        Value::from(
            obj.and_then(|v| v.get("color_tolerance"))
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok())
                .unwrap_or(defaults.color_tolerance),
        ),
    );
    out.insert(
        "continuation_alpha".to_string(),
        Value::from(
            obj.and_then(|v| v.get("continuation_alpha"))
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok())
                .unwrap_or(defaults.continuation_alpha),
        ),
    );
    out.insert(
        "start_alpha".to_string(),
        Value::from(
            obj.and_then(|v| v.get("start_alpha"))
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok())
                .unwrap_or(defaults.start_alpha),
        ),
    );
    out
}

fn normalize_vector_lines_layout_object(
    obj: Option<&serde_json::Map<String, Value>>,
) -> serde_json::Map<String, Value> {
    let defaults = TextVectorLinesLayoutParams::default();
    let mut out = serde_json::Map::<String, Value>::new();
    out.insert(
        "width_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("width_px"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(defaults.width_px)
                .max(1),
        ),
    );
    out.insert(
        "height_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("height_px"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(defaults.height_px)
                .max(1),
        ),
    );
    out.insert(
        "use_tangent_rotation".to_string(),
        Value::from(
            obj.and_then(|v| v.get("use_tangent_rotation"))
                .and_then(Value::as_bool)
                .unwrap_or(defaults.use_tangent_rotation),
        ),
    );
    out.insert(
        "static_rotation_rad".to_string(),
        Value::from(
            obj.and_then(|v| v.get("static_rotation_rad"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.static_rotation_rad),
        ),
    );
    out.insert(
        "normal_offset_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("normal_offset_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.normal_offset_px),
        ),
    );
    out.insert(
        "letter_spacing_mul".to_string(),
        Value::from(
            obj.and_then(|v| v.get("letter_spacing_mul"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.letter_spacing_mul)
                .clamp(0.0, 8.0),
        ),
    );
    out.insert(
        "letter_spacing_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("letter_spacing_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.letter_spacing_px)
                .clamp(-10_000.0, 10_000.0),
        ),
    );
    let lines = obj
        .and_then(|v| v.get("lines"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(normalize_vector_line_value)
                .collect()
        })
        .unwrap_or_default();
    out.insert("lines".to_string(), Value::Array(lines));
    out
}

fn normalize_vector_line_value(value: &Value) -> Option<Value> {
    let obj = value.as_object()?;
    let points = obj
        .get("points")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(normalize_vector_point_value)
        .collect::<Vec<_>>();
    Some(json!({
        "points": points,
        "corner_smoothing_px": obj
            .get("corner_smoothing_px")
            .and_then(value_as_f32)
            .unwrap_or(0.0)
            .clamp(0.0, 256.0),
        "text_direction": vector_line_text_direction_to_str(vector_line_text_direction_from_value(
            obj.get("text_direction"),
        )),
        "distance_mode": vector_line_distance_mode_to_str(vector_line_distance_mode_from_value(
            obj.get("distance_mode"),
        )),
        "flip_text": obj
            .get("flip_text")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }))
}

fn normalize_vector_point_value(value: &Value) -> Option<Value> {
    let obj = value.as_object()?;
    Some(json!({
        "x": obj.get("x").and_then(value_as_f32)?,
        "y": obj.get("y").and_then(value_as_f32)?,
    }))
}

fn normalize_formula_vars_array(
    vars: Option<&Vec<Value>>,
    defaults: [f32; TEXT_FORMULA_USER_VAR_COUNT],
) -> Vec<Value> {
    let mut out = Vec::<Value>::with_capacity(TEXT_FORMULA_USER_VAR_COUNT);
    for (idx, &default_val) in defaults.iter().enumerate() {
        let value = vars
            .and_then(|arr| arr.get(idx))
            .and_then(value_as_f32)
            .unwrap_or(default_val);
        out.push(Value::from(value));
    }
    out
}

fn normalized_to_static(value: &str) -> &'static str {
    match value {
        "left" => "left",
        "center" => "center",
        "right" => "right",
        "justify" => "justify",
        "free" => "free",
        "rectangle" => "rectangle",
        "oval" => "oval",
        "hexagon" => "hexagon",
        "soft_peak" => "soft_peak",
        "horizontal" => "horizontal",
        "vertical" => "vertical",
        "normal" => "normal",
        "formula" => "formula",
        "shape" => "shape",
        "custom_raster_lines" => "custom_raster_lines",
        "custom_vector_lines" => "custom_vector_lines",
        _ => "",
    }
}

fn parse_transform_uv(obj: &serde_json::Map<String, Value>) -> Option<[[f32; 2]; 4]> {
    let raw_quad = obj.get("transform_uv")?.as_array()?;
    if raw_quad.len() != 4 {
        return None;
    }

    let mut out = [[0.0f32; 2]; 4];
    for (idx, point) in raw_quad.iter().enumerate() {
        let coords = point.as_array()?;
        if coords.len() != 2 {
            return None;
        }
        let u = coords[0].as_f64()? as f32;
        let v = coords[1].as_f64()? as f32;
        out[idx] = [clamp_overlay_uv_coord(u), clamp_overlay_uv_coord(v)];
    }
    Some(out)
}

fn parse_deform_mesh(
    obj: &serde_json::Map<String, Value>,
    page_size: [usize; 2],
) -> Option<TypingOverlayDeformMesh> {
    let mesh_obj = obj.get("deform_mesh")?.as_object()?;
    let cols = mesh_obj
        .get("cols")?
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())?;
    let rows = mesh_obj
        .get("rows")?
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())?;
    let raw_points = mesh_obj
        .get("points_px")
        .or_else(|| mesh_obj.get("points_uv"))?
        .as_array()?;
    let use_page_px = mesh_obj.get("points_px").is_some();
    let mut points_px = Vec::with_capacity(raw_points.len());
    for point in raw_points {
        let coords = point.as_array()?;
        if coords.len() != 2 {
            return None;
        }
        let x = coords[0].as_f64()? as f32;
        let y = coords[1].as_f64()? as f32;
        points_px.push(if use_page_px {
            clamp_page_point([x, y], page_size)
        } else {
            uv_to_page_px(clamp_uv_point([x, y]), page_size)
        });
    }
    TypingOverlayDeformMesh::new(cols, rows, points_px, page_size)
        .map(|mesh| normalize_deform_mesh_resolution(&mesh, page_size))
}

fn legacy_fallback_width_px(obj: &serde_json::Map<String, Value>) -> u32 {
    obj.get("width_px")
        .and_then(value_as_f32)
        .or_else(|| {
            obj.get("render_params")
                .and_then(Value::as_object)
                .and_then(|rp| rp.get("width_px"))
                .and_then(value_as_f32)
        })
        .or_else(|| {
            obj.get("render_data")
                .and_then(Value::as_object)
                .and_then(|rd| rd.get("text_params"))
                .and_then(Value::as_object)
                .and_then(|tp| tp.get("width_px"))
                .and_then(value_as_f32)
        })
        .map(|w| w.round().max(1.0) as u32)
        .unwrap_or(TEXT_RENDER_DATA_FALLBACK_WIDTH_PX)
}

fn default_render_data_for_text(text: &str, width_px: u32) -> Value {
    json!({
        "schema_version": 2,
        "text_params": {
            "text": text,
            "text_color": [0, 0, 0, 255],
            "font_path": Value::Null,
            "font_label": Value::Null,
            "font_size_px": 24.0,
            "line_spacing_px": 4.0,
            "line_spacing_percent": 50.0,
            "width_px": width_px.max(1),
            "align": "center",
            "text_line_mode": "horizontal",
            "text_layout_mode": "normal",
            "formula_layout": normalize_formula_layout_object(None),
            "drawn_lines_layout": normalize_drawn_lines_layout_object(None),
            "vector_lines_layout": normalize_vector_lines_layout_object(None),
            "selected_face_index": 0,
            "force_bold": false,
            "force_italic": false,
            "uppercase_text": false,
            "enable_inline_style_tags": false,
            "text_wrap_mode": "aggressive",
            "allow_moderate_trees": false,
            "text_shape": "rectangle",
            "shape_min_width_percent": 50.0,
            "shape_variant": 5
        },
        "effects": [],
    })
}

fn overlay_render_data_width_hint(render_data: Option<&Value>, fallback_width_px: u32) -> u32 {
    render_data
        .and_then(Value::as_object)
        .and_then(|rd| rd.get("text_params"))
        .and_then(Value::as_object)
        .and_then(|tp| tp.get("width_px"))
        .and_then(value_as_f32)
        .map(|width| width.round().max(1.0) as u32)
        .unwrap_or_else(|| fallback_width_px.max(1))
}

fn parse_overlay_kind(obj: &serde_json::Map<String, Value>) -> TypingOverlayKind {
    match obj
        .get("overlay_type")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("image") => TypingOverlayKind::Image,
        _ => TypingOverlayKind::Text,
    }
}

fn overlay_center_page_px_from_storage(
    obj: &serde_json::Map<String, Value>,
    page_size: [usize; 2],
) -> [f32; 2] {
    if let (Some(x_px), Some(y_px)) = (
        obj.get("img_x_px").and_then(value_as_f32),
        obj.get("img_y_px").and_then(value_as_f32),
    ) {
        return clamp_page_point([x_px, y_px], page_size);
    }
    let u = obj
        .get("img_u")
        .or_else(|| obj.get("u"))
        .and_then(value_as_f32)
        .unwrap_or(0.5)
        .clamp(overlay_uv_min(), overlay_uv_max());
    let v = obj
        .get("img_v")
        .or_else(|| obj.get("v"))
        .and_then(value_as_f32)
        .unwrap_or(0.5)
        .clamp(overlay_uv_min(), overlay_uv_max());
    uv_to_page_px([u, v], page_size)
}

fn normalize_overlay_storage_entry(
    obj: &serde_json::Map<String, Value>,
    page_size: [usize; 2],
) -> Option<Value> {
    let kind = parse_overlay_kind(obj);
    let page_idx = obj
        .get("img_idx")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())?;
    let file_raw = obj
        .get("file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let file_name = Path::new(file_raw)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())?;
    let center_page_px = overlay_center_page_px_from_storage(obj, page_size);
    let rotation_deg = obj
        .get("rotation_deg")
        .or_else(|| obj.get("angle"))
        .and_then(value_as_f32)
        .unwrap_or(0.0);
    let mask_clip_enabled = obj
        .get("mask_clip_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let scale = obj
        .get("scale")
        .or_else(|| obj.get("user_scale"))
        .and_then(value_as_f32)
        .unwrap_or(1.0)
        .max(0.01);
    let deform_mesh = parse_deform_mesh(obj, page_size).or_else(|| {
        parse_transform_uv(obj).map(|quad| {
            deform_mesh_from_quad(
                quad,
                TEXT_OVERLAY_DEFORM_SURFACE_COLS,
                TEXT_OVERLAY_DEFORM_SURFACE_ROWS,
                page_size,
            )
        })
    });
    let render_data = if kind == TypingOverlayKind::Text {
        let fallback_width_px = legacy_fallback_width_px(obj);
        Some(
            parse_overlay_render_data_json(obj, fallback_width_px).unwrap_or_else(|| {
                default_render_data_for_text(
                    obj.get("text").and_then(Value::as_str).unwrap_or_default(),
                    fallback_width_px,
                )
            }),
        )
    } else {
        None
    };

    Some(build_storage_overlay_entry(
        kind,
        page_idx,
        file_name.as_str(),
        center_page_px,
        mask_clip_enabled,
        rotation_deg,
        scale,
        deform_mesh,
        render_data,
    ))
}

fn decode_overlay_from_storage_entry(
    text_images_dir: &Path,
    obj: &serde_json::Map<String, Value>,
    page_size: [usize; 2],
) -> Option<TypingOverlayDecoded> {
    let kind = parse_overlay_kind(obj);
    let page_idx = obj
        .get("img_idx")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())?;
    let file_raw = obj
        .get("file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let file_name = Path::new(file_raw)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())?;
    let image_path = text_images_dir.join(&file_name);
    let decoded = image::open(&image_path).ok()?.to_rgba8();
    let (w, h) = decoded.dimensions();
    if w == 0 || h == 0 {
        return None;
    }

    let center_page_px = overlay_center_page_px_from_storage(obj, page_size);
    let user_scale = obj
        .get("scale")
        .or_else(|| obj.get("user_scale"))
        .and_then(value_as_f32)
        .unwrap_or(1.0)
        .max(0.01);
    let mask_clip_enabled = obj
        .get("mask_clip_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let angle_deg = obj
        .get("rotation_deg")
        .or_else(|| obj.get("angle"))
        .and_then(value_as_f32)
        .unwrap_or(0.0);
    let deform_mesh = parse_deform_mesh(obj, page_size).or_else(|| {
        parse_transform_uv(obj).map(|quad| {
            deform_mesh_from_quad(
                quad,
                TEXT_OVERLAY_DEFORM_SURFACE_COLS,
                TEXT_OVERLAY_DEFORM_SURFACE_ROWS,
                page_size,
            )
        })
    });
    let render_data_json = if kind == TypingOverlayKind::Text {
        let fallback_width_px = legacy_fallback_width_px(obj);
        parse_overlay_render_data_json(obj, fallback_width_px)
    } else {
        None
    };

    Some(TypingOverlayDecoded {
        kind,
        page_idx,
        center_page_px,
        mask_clip_enabled,
        user_scale,
        angle_deg,
        deform_mesh,
        file_name,
        render_data_json,
        size_px: [w as usize, h as usize],
        rgba: decoded.into_raw(),
        warnings: Vec::new(),
    })
}

fn load_typing_page_sizes(page_paths: &[(usize, PathBuf)]) -> HashMap<usize, [usize; 2]> {
    let mut out = HashMap::with_capacity(page_paths.len());
    for (page_idx, path) in page_paths {
        let size = image::image_dimensions(path)
            .ok()
            .map(|(w, h)| [w as usize, h as usize])
            .unwrap_or([1, 1]);
        out.insert(*page_idx, size);
    }
    out
}

fn load_typing_overlays_from_dir(
    text_images_dir: &Path,
    fallback_dir: Option<&Path>,
    page_sizes: &HashMap<usize, [usize; 2]>,
) -> Result<Vec<TypingOverlayDecoded>, String> {
    let text_info_path = text_images_dir.join(TEXT_INFO_FILE_NAME);
    if !text_info_path.is_file() {
        return Ok(Vec::new());
    }

    let raw = fs::read_to_string(&text_info_path)
        .map_err(|err| format!("Не удалось прочитать {}: {err}", text_info_path.display()))?;
    let parsed: Value = serde_json::from_str(&raw)
        .map_err(|err| format!("Не удалось распарсить {}: {err}", text_info_path.display()))?;
    let Some(items) = parsed.as_array() else {
        return Err(format!(
            "Файл {} должен содержать JSON-массив оверлеев.",
            text_info_path.display()
        ));
    };

    let mut decoded_out = Vec::new();
    let mut normalized_items = Vec::<Value>::with_capacity(items.len());
    let mut needs_rewrite = false;

    for item in items {
        let page_idx = item
            .as_object()
            .and_then(|obj| obj.get("img_idx"))
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0);
        let page_size = page_sizes.get(&page_idx).copied().unwrap_or([1, 1]);
        let normalized = item
            .as_object()
            .and_then(|obj| normalize_overlay_storage_entry(obj, page_size))
            .unwrap_or_else(|| item.clone());
        needs_rewrite |= normalized != *item;

        if let Some(decoded) = normalized.as_object().and_then(|obj| {
            // Try the primary dir first; fall back to the fallback dir for PNGs that
            // were not modified in the current unsaved session.
            decode_overlay_from_storage_entry(text_images_dir, obj, page_size).or_else(|| {
                fallback_dir.and_then(|d| decode_overlay_from_storage_entry(d, obj, page_size))
            })
        }) {
            decoded_out.push(decoded);
        }
        normalized_items.push(normalized);
    }

    if needs_rewrite {
        let normalized_raw = serde_json::to_string_pretty(&normalized_items).map_err(|err| {
            format!(
                "Не удалось сериализовать нормализованный {}: {err}",
                text_info_path.display()
            )
        })?;
        fs::write(&text_info_path, normalized_raw).map_err(|err| {
            format!(
                "Не удалось записать нормализованный {}: {err}",
                text_info_path.display()
            )
        })?;
    }

    Ok(decoded_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape_variant_test_params(text_shape: TextShape) -> TextRenderParams {
        TextRenderParams {
            text: "Просто без елок".to_string(),
            text_color: [0, 0, 0, 255],
            font_path: std::path::PathBuf::from("font.ttf"),
            available_inline_fonts: Vec::new(),
            font_size_px: 24.0,
            line_spacing_px: 4.0,
            line_spacing_percent: 50.0,
            kerning_mode: KerningMode::Metric,
            kerning_px: 0.0,
            kerning_percent: 0.0,
            glyph_height_percent: 100.0,
            glyph_width_percent: 100.0,
            width_px: 120,
            align: HorizontalAlign::Center,
            selected_face_index: 0,
            force_bold: false,
            force_italic: false,
            uppercase_text: false,
            trim_extra_spaces: false,
            hanging_punctuation: false,
            new_line_after_sentence: false,
            enable_inline_style_tags: false,
            text_wrap_mode: TextWrapMode::Moderate,
            text_shape,
            shape_min_width_percent: 50.0,
            shape_variant: 5,
            compare_shape_with: None,
            allow_moderate_trees: false,
            text_line_mode: TextLineMode::Horizontal,
            vertical_line_direction: VerticalLineDirection::RightToLeft,
            text_layout_mode: TextLayoutMode::Normal,
            formula_layout: TextFormulaLayoutParams::default(),
            drawn_lines_layout: TextDrawnLinesLayoutParams::default(),
            vector_lines_layout: TextVectorLinesLayoutParams::default(),
            effects_json: String::new(),
        }
    }

    #[test]
    fn soft_peak_shape_menu_pairs_variants_with_wrap_strength() {
        let params = shape_variant_test_params(TextShape::SoftPeak);
        let variants = build_shape_variant_grid(&params);

        assert_eq!(variants.len(), 9);
        for (row, expected_variant) in [3, 9, 6].into_iter().enumerate() {
            let row_variants = variants
                .iter()
                .filter(|variant| variant.row == row)
                .collect::<Vec<_>>();
            assert_eq!(row_variants.len(), 3);
            assert!(
                row_variants
                    .iter()
                    .all(|variant| variant.width_px == params.width_px)
            );
            assert!(
                row_variants.iter().all(
                    |variant| variant.shape_min_width_percent == params.shape_min_width_percent
                )
            );
            assert!(
                row_variants
                    .iter()
                    .all(|variant| variant.shape_variant == expected_variant)
            );
            assert_eq!(row_variants[0].text_wrap_mode, TextWrapMode::Minimal);
            assert_eq!(row_variants[1].text_wrap_mode, TextWrapMode::Moderate);
            assert_eq!(row_variants[2].text_wrap_mode, TextWrapMode::Aggressive);
        }
    }

    #[test]
    fn shape_variant_preview_does_not_depend_on_current_wrap_strength() {
        let mut params = shape_variant_test_params(TextShape::SoftPeak);
        params.text_wrap_mode = TextWrapMode::WholeWords;

        assert!(shape_variant_preview_available(TypingOverlayKind::Text));
        let variants = build_shape_variant_grid(&params);

        assert_eq!(variants.len(), 9);
        assert_eq!(variants[0].text_wrap_mode, TextWrapMode::Minimal);
        assert_eq!(variants[1].text_wrap_mode, TextWrapMode::Moderate);
        assert_eq!(variants[2].text_wrap_mode, TextWrapMode::Aggressive);
    }

    #[test]
    fn canceled_shape_variant_preview_does_not_start_tiles() {
        let params = shape_variant_test_params(TextShape::SoftPeak);
        let variants = build_shape_variant_grid(&params);
        let cancel_render = Arc::new(AtomicBool::new(true));

        let tiles = render_shape_variant_preview_tiles(params, variants, &cancel_render);

        assert!(tiles.is_empty());
    }

    #[test]
    fn storage_normalization_preserves_soft_peak_shape() {
        let raw = json!({
            "schema_version": 2,
            "text_params": {
                "text": "Просто без елок",
                "font_path": "/tmp/font.ttf",
                "width_px": 120,
                "text_shape": "soft_peak",
                "shape_variant": 9
            },
            "effects": []
        });

        let Some(normalized) = normalize_render_data_value(&raw, 500) else {
            panic!("render data should normalize");
        };
        let Some(text_params) = normalized.get("text_params").and_then(Value::as_object) else {
            panic!("normalized render data should contain text params");
        };

        assert_eq!(
            text_params.get("text_shape").and_then(Value::as_str),
            Some("soft_peak")
        );
        assert_eq!(
            text_params.get("shape_variant").and_then(Value::as_u64),
            Some(9)
        );
    }
}
