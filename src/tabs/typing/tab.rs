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
    TypingCreateImageRequest, TypingEditTarget, TypingEditorFontSpec, TypingExportUiStatus,
    TypingOverlayEditRequest, TypingOverlayKind, TypingPanelLayout, TypingSelectedOverlayForEdit,
};
use super::render_next::{apply_effects_to_image, render_text_to_image};
use super::render_next::types::{
    AntiAliasingMode, HorizontalAlign, KerningMode, PxOrPercent, TEXT_FORMULA_USER_VAR_COUNT,
    TextDrawnLinesLayoutParams,
    TextFormulaLayoutParams, TextLayoutMode, TextLineMode, TextRenderParams,
    TextRenderShapeCompareParams, TextShape, TextVectorLine, TextVectorLineDistanceMode,
    TextVectorLineTextDirection, TextVectorLinesLayoutParams, TextVectorPoint, TextWrapMode,
    VerticalLineDirection,
};
use crate::app::{PageImageInfo, PageTexture};
use crate::trace::cat;
use crate::canvas::{
    CanvasDrawParams, CanvasHooks, CanvasUiStatus, CanvasView, CanvasViewportSnapshot, RectCoords,
    SourceTextureUploadBudget, parse_image_text_areas,
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

// "Слои страницы" panel sizing.
/// Minimum text-preview characters a text row shows (the narrowest panel). The panel cannot shrink below
/// the width that fits exactly this many chars.
const LAYERS_PANEL_MIN_PREVIEW_CHARS: usize = 5;
/// Default panel width (px) — roughly the old fixed 260, enough for ~5+ preview chars.
const LAYERS_PANEL_DEFAULT_WIDTH: f32 = 260.0;
/// Default visible height of the layer list, in ROWS, before the inner scroll kicks in.
const LAYERS_PANEL_DEFAULT_ROWS: usize = 8;
/// Fixed horizontal overhead (px) of a text row that is NOT preview text: the ⬆/⬇ buttons + item
/// spacing + the `Текст (` / `)` wrapper + frame padding + scrollbar. Used to derive both the min panel
/// width and the per-width char budget so they stay consistent.
const LAYERS_PANEL_ROW_OVERHEAD_PX: f32 = 116.0;
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
const TEXT_OVERLAY_WIDTH_GUIDE_GAP_PX: f32 = 10.0;
const TEXT_OVERLAY_WIDTH_GUIDE_TICK_HALF_PX: f32 = 5.0;
const TEXT_OVERLAY_WIDTH_GUIDE_LABEL_GAP_PX: f32 = 4.0;
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
pub(super) struct TypingOverlayDeformMesh {
    pub(super) cols: usize,
    pub(super) rows: usize,
    points_px: Vec<[f32; 2]>,
}

impl TypingOverlayDeformMesh {
    pub(super) fn new(
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

    /// Builds the runtime mesh from a canonical `DeformRec` (the shared codec's output), clamping its
    /// page-pixel points to the page. The runtime struct adds rendering helpers (`point`, `translate`,
    /// sampling); parsing/validation of `deform_mesh`/`transform_uv`/`points_uv` lives in the shared
    /// `text_payload` codec, not here.
    fn from_deform_rec(
        rec: &crate::models::layer_model::manifest::DeformRec,
        page_size: [usize; 2],
    ) -> Option<Self> {
        Self::new(rec.cols, rec.rows, rec.points_px.clone(), page_size)
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
    /// Shared unified layer document (app-owned): the source of truth for per-page layer MODEL state,
    /// shared with the PS tab. `None` until `set_layer_doc` is called by app.rs.
    layer_doc: Option<std::sync::Arc<std::sync::Mutex<crate::models::layer_model::layer_doc::LayerDoc>>>,
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
            layer_doc: None,
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

    /// Wires the app-owned shared unified layer document (see `layer_doc`). Propagates to the inner
    /// overlay layer, which owns the per-page load path that populates the doc.
    pub fn set_layer_doc(
        &mut self,
        doc: std::sync::Arc<std::sync::Mutex<crate::models::layer_model::layer_doc::LayerDoc>>,
    ) {
        self.text_overlays.set_layer_doc(Arc::clone(&doc));
        self.layer_doc = Some(doc);
    }

    pub fn set_panel_layout(&mut self, layout: TypingPanelLayout) {
        self.top_panel.set_panel_layout(layout);
    }

    /// Flushes the typing tab's text overlays (inline v3 payload) into the staging `layers.json` so a
    /// legacy chapter that was only viewed still migrates its text on save-to-project. Returns the set
    /// of OWNED text pages (doc-resident this session) for the save-to-project merge to treat as
    /// authoritative; pages NOT in it keep their committed text. Delegates to the overlay layer.
    pub fn flush_text_layers(&mut self) -> std::collections::HashSet<usize> {
        self.text_overlays.flush_text_layers()
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

    pub fn current_page_local_view_center(&self) -> Option<(usize, Vec2)> {
        self.canvas.current_page_local_view_center()
    }

    pub fn focus_page(&mut self, page_idx: usize, center_px: Option<Vec2>, zoom: f32) {
        self.canvas.focus_page(page_idx, center_px, zoom);
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
        let _frame_span = crate::trace_scope!(cat::FRAME, "typing.draw page={}", self.canvas.current_page_idx());
        let canvas_rect = ui.max_rect();
        self.text_overlays.set_page_count(project.pages.len());
        // Cross-tab sync: if the shared LayerDoc changed (version advanced) since we last projected,
        // re-project the current page from it (in-memory; no disk reload).
        self.text_overlays
            .maybe_reproject_from_doc_version(self.canvas.current_page_idx());
        self.text_overlays.ensure_loader_started(project);
        self.mask_layer.ensure_loader_started(project);
        let mut needs_repaint = false;
        needs_repaint |= self.text_overlays.poll_loader();
        needs_repaint |= self.text_overlays.poll_migration();
        needs_repaint |= self.text_overlays.poll_create_overlay_jobs(ctx);
        needs_repaint |= self.text_overlays.poll_create_raster_jobs(ctx);
        needs_repaint |= self.text_overlays.poll_raster_effects_jobs(ctx);
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
            let selected = self
                .text_overlays
                .selected_item_for_edit(canvas.current_page_idx());
            self.top_panel.sync_selected_overlay_for_edit(selected);
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
        // The combined Actions/Layers panel: the «Слои» tab body is rendered by `text_overlays` (which
        // owns the layer/overlay state), routed through the Actions panel's tab UI on `top_panel`.
        self.top_panel.draw(
            ctx,
            canvas_rect,
            &mut self.text_overlays,
            canvas.current_page_idx(),
        );
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
        if let Some((export_dir, export_format)) = self.top_panel.take_export_to_folder_request() {
            let mask_snapshot = self.mask_layer.export_masks_snapshot();
            self.text_overlays.request_export_to_folder(
                ctx,
                project,
                mask_snapshot,
                export_dir,
                export_format,
            );
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

/// In-flight job creating a raster layer from an external image (the new image-add path).
struct TypingCreateRasterState {
    rx: Receiver<Result<TypingCreatedRaster, String>>,
}

/// Worker request to load an external image and persist it as a raster node in `layers.json`.
struct TypingCreateRasterRequest {
    layers_dir: PathBuf,
    /// Committed `layers/` dir; the new staged page is seeded from it so a typeset page keeps its
    /// committed TEXT (data-safety — see `persist::add_page_raster`).
    fallback_dir: Option<PathBuf>,
    page_idx: usize,
    center_page_px: [f32; 2],
    source: TypingCreateImageSource,
}

/// Worker result: the new raster layer was written to disk; the tab reloads the page's raster cache
/// from disk (authoritative) and selects this uid.
struct TypingCreatedRaster {
    page_idx: usize,
    uid: String,
}

/// Worker result for a non-destructive raster effects render: the display image to show (the
/// rendered result, or the untouched base when the chain is empty) plus the chain to persist.
struct TypingRasterEffectsResult {
    page_idx: usize,
    uid: String,
    /// What to show: the post-effects render, or the original base when `effects` is empty.
    display_image: ColorImage,
    /// The effects chain that produced `display_image`.
    effects: Vec<Value>,
}

/// Drag of a raster layer on the typing canvas (parity with overlay drag).
#[derive(Clone)]
struct TypingRasterDragState {
    page_idx: usize,
    raster_idx: usize,
    mode: TypingRasterDragMode,
    pointer_start_scene: Pos2,
    start_transform: crate::models::layer_model::manifest::TransformRec,
    /// Pointer angle (rad) about the raster center at drag start (rotate mode).
    start_pointer_angle_rad: f32,
    /// Deform mesh at drag start (perspective-handle mesh edit). Empty for move/rotate.
    start_mesh: Option<TypingOverlayDeformMesh>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TypingRasterDragMode {
    Move,
    Rotate,
    /// Dragging one of the deform mesh's 4 corner handles (perspective transform mode).
    PerspectiveHandle(usize),
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
    // Только для image-эффектов: новое имя исходной картинки (None — эффекты убраны, исходник = `file_name`).
    image_original_file_name: Option<String>,
    // Истина, когда результат пришёл из image-effects worker: применяется по своей ветке (allow rename).
    is_image_effects: bool,
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
    /// Stable cross-session id; mirrored as a node in `layers.json` and as the `uid` key in
    /// `text_info.json`. Generated on creation or on first load of a pre-uid overlay.
    uid: String,
    kind: TypingOverlayKind,
    page_idx: usize,
    center_page_px: [f32; 2],
    mask_clip_enabled: bool,
    /// Индекс слоя текста, в который сгруппирован оверлей (по умолчанию 0).
    layer_idx: usize,
    user_scale: f32,
    angle_deg: f32,
    deform_mesh: Option<TypingOverlayDeformMesh>,
    file_name: String,
    // Для image-оверлеев — имя файла исходной (до эффектов) картинки, если эффекты применялись.
    original_file_name: Option<String>,
    #[allow(dead_code)]
    render_data_json: Option<Value>,
    size_px: [usize; 2],
    rgba: Vec<u8>,
    warnings: Vec<String>,
}

/// A read-only PS-editor raster layer cached for display under the text overlays in the typing tab.
/// Loaded via `crate::models::layer_model::persist::load_page_rasters` for the current page.
struct TypingRasterLayer {
    uid: String,
    name: String,
    visible: bool,
    opacity: f32,
    /// Center cx/cy in page px, rotation in radians, uniform scale (see `TransformRec`).
    transform: crate::models::layer_model::manifest::TransformRec,
    /// The DISPLAY image (post-effects render when `effects` is non-empty, else the base).
    image: ColorImage,
    /// Base (pre-effects) PNG name, so the effects worker can re-render from the original.
    base_file: String,
    /// Non-destructive effects chain (`[{...}]`). Empty = no effects.
    effects: Vec<Value>,
    /// Optional mesh-deform grid (cols×rows control points, absolute page px, row-major). When
    /// present the raster is rendered through this mesh (like a deformed overlay) instead of its
    /// affine `transform`. `None` = plain affine raster.
    deform: Option<crate::models::layer_model::manifest::DeformRec>,
    /// Whether the raster is clipped to the page mask (typing tab). Rasters DEFAULT OFF (text differs).
    /// Projected from the doc node's `NodeBody::Raster.mask_clip` (`Some(true)` ⇒ on).
    mask_clip_enabled: bool,
    /// Cached mask-clipped DISPLAY image, rebuilt when the doc node `generation` (which the mask-clip
    /// toggle bumps) changes. `None` until first computed / when `mask_clip_enabled` is false.
    clipped_image: Option<ColorImage>,
    /// Lazily uploaded on first draw.
    texture: Option<egui::TextureHandle>,
}

struct TypingOverlayRuntime {
    /// Stable cross-session id (see `TypingOverlayDecoded::uid`).
    uid: String,
    kind: TypingOverlayKind,
    page_idx: usize,
    center_page_px: [f32; 2],
    mask_clip_enabled: bool,
    /// Индекс слоя текста, в который сгруппирован оверлей (по умолчанию 0).
    layer_idx: usize,
    user_scale: f32,
    angle_deg: f32,
    deform_mesh: Option<TypingOverlayDeformMesh>,
    file_name: String,
    // Для image-оверлеев — имя файла исходной (до эффектов) картинки, если эффекты применялись.
    original_file_name: Option<String>,
    #[allow(dead_code)]
    render_data_json: Option<Value>,
    size_px: [usize; 2],
    source_rgba: Vec<u8>,
    texture: Option<egui::TextureHandle>,
    display_texture_stale: bool,
    last_texture_used_frame: u64,
}

#[derive(Clone)]
pub(super) struct TypingExportOverlaySnapshot {
    pub(super) page_idx: usize,
    pub(super) center_page_px: [f32; 2],
    pub(super) mask_clip_enabled: bool,
    /// Индекс слоя текста, в который сгруппирован оверлей (по умолчанию 0).
    pub(super) layer_idx: usize,
    pub(super) user_scale: f32,
    pub(super) angle_deg: f32,
    pub(super) deform_mesh: Option<TypingOverlayDeformMesh>,
    pub(super) size_px: [usize; 2],
    pub(super) source_rgba: Vec<u8>,
    pub(super) render_data_json: Option<serde_json::Value>,
    pub(super) uid: String,
    /// Unified band-Z captured from the SAME in-memory `bands_by_page`/doc-flattened order the raster
    /// snapshot uses, so text and rasters interleave consistently in the export (no disk-vs-memory
    /// divergence). The flatten falls back to a disk band lookup only when no snapshot is provided.
    pub(super) band_z: u32,
}

/// A snapshot of one on-screen PS raster layer for export, taken from the doc-projected
/// `raster_layers_by_page` (the SAME source the live canvas draws) with its unified band-Z. Carrying
/// this in the export job makes the composite use exactly what the user sees — including in-session
/// transforms, deform, and effects renders — instead of re-reading `layers.json` from disk, which can
/// diverge (unflushed edits, a missing `_fx.png` rendered file, or a stale staging manifest) and silently
/// DROP the raster from the bake.
#[derive(Clone)]
pub(super) struct TypingExportRasterSnapshot {
    pub(super) visible: bool,
    pub(super) opacity: f32,
    pub(super) transform: crate::models::layer_model::manifest::TransformRec,
    pub(super) deform: Option<crate::models::layer_model::manifest::DeformRec>,
    /// Straight (un-premultiplied) RGBA of the DISPLAY image (post-effects), row-major.
    pub(super) rgba: Vec<u8>,
    pub(super) size_px: [usize; 2],
    /// Unified band-Z, bottom-to-top, for interleaving with text overlays exactly as on-screen.
    pub(super) band_z: u32,
    /// Whether the raster is clipped to the page mask (matches the on-screen `clipped_image` path). When
    /// set, the export composite masks the raster via `export_clip_overlay_rgba_if_needed`, so a
    /// mask-clipped raster exports clipped (not with pixels outside the mask).
    pub(super) mask_clip_enabled: bool,
}

/// Output format chosen in the typing tab "export to folder" flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum TypingExportFormat {
    #[default]
    Png,
    Psd,
}

pub(super) struct TypingExportPageJob {
    pub(super) page_idx: usize,
    pub(super) page_path: PathBuf,
    pub(super) output_path: PathBuf,
    pub(super) clean_overlay_path: Option<PathBuf>,
    pub(super) clean_overlay_rgba: Option<Arc<image::RgbaImage>>,
    pub(super) overlays: Vec<TypingExportOverlaySnapshot>,
    /// On-screen PS raster layers snapshotted from the doc projection. When present, the composite uses
    /// THESE (matching the canvas) instead of re-reading rasters from `layers_primary_dir`. An empty vec
    /// falls back to the disk read (back-compat).
    pub(super) rasters: Vec<TypingExportRasterSnapshot>,
    pub(super) mask: Option<TypingMaskExportPage>,
    pub(super) export_format: TypingExportFormat,
    pub(super) layers_primary_dir: Option<PathBuf>,
    pub(super) layers_fallback_dir: Option<PathBuf>,
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

pub(super) struct TypingTextOverlayLayer {
    loaded_project_dir: Option<PathBuf>,
    loaded_text_images_dir: Option<PathBuf>,
    /// Directory where new/edited overlays are written (always the unsaved staging dir).
    text_images_save_dir: Option<PathBuf>,
    /// Saved (main) text_images dir, used as a read fallback for source PNGs not yet in staging.
    text_images_fallback_dir: Option<PathBuf>,
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
    /// Raster analogue of `transform_mode_overlay_idx`: the selected raster (index into
    /// `raster_layers_by_page[page]`) currently in deform/perspective transform mode, if any. Mutually
    /// exclusive with overlay transform mode.
    transform_mode_raster_idx: Option<usize>,
    layout_editor: Option<TypingLayoutEditorState>,
    deform_mode: TypingDeformMode,
    frame_handle_side_points: usize,
    pull_neighbor_handles: bool,
    deform_tool_settings: TypingDeformToolSettings,
    drag_state: Option<TypingOverlayDragState>,
    drag_has_changes: bool,
    primary_pointer_targets_overlay_this_frame: bool,
    page_count: usize,
    /// Page image path per page index (captured at project load), so the page's pixel size can be
    /// resolved lazily for legacy-overlay uv→px decoding when handing a page to the shared doc.
    page_image_paths: HashMap<usize, PathBuf>,
    /// Lazily-cached page pixel sizes `[w, h]` keyed by page index (header-only `image_dimensions`).
    page_sizes_px: HashMap<usize, [usize; 2]>,
    clean_overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    auto_typing_next_token: u64,
    auto_typing_job: Option<TypingAutoTypingJobState>,
    auto_typing_debug_visual: Option<TypingAutoTypingDebugVisual>,
    /// Committed (`layers/`) and unsaved (`layers_unsaved/`) dirs for reading PS raster layers,
    /// captured when a project loads. Used to (re)load `raster_layers` for the current page.
    layers_primary_dir: Option<PathBuf>,
    layers_fallback_dir: Option<PathBuf>,
    /// Read-only PS raster layers per page (bottom-to-top), cached lazily so multi-page scenes do
    /// not thrash the loader. Cleared on project (re)load and cross-tab reload.
    raster_layers_by_page: HashMap<usize, Vec<TypingRasterLayer>>,
    /// Unified per-page Z bands (bottom-to-top), cached lazily alongside `raster_layers_by_page` and
    /// used to interleave rasters and text/image overlays in one ordered draw pass. Cleared in the
    /// same places as `raster_layers_by_page`.
    bands_by_page: HashMap<usize, Vec<crate::models::layer_model::ordering::Band>>,
    /// Last `LayerDoc::version` this tab projected. Each frame, if the live doc version differs, the
    /// tab re-projects its current page from the shared doc — the in-memory cross-tab sync. Initialized
    /// to 0 (a fresh doc) and reconciled by every `sync_from_doc`.
    last_doc_version: u64,
    /// In-flight "create external image as a raster layer" job (replaces the old image-overlay path).
    create_raster_state: Option<TypingCreateRasterState>,
    /// In-flight "bake effects into the selected raster" job.
    raster_effects_state: Option<Receiver<Result<TypingRasterEffectsResult, String>>>,
    /// A raster effects edit that arrived while a render was already in flight. Only the latest is
    /// kept (newer edits supersede); it is reapplied when the current render completes so the last
    /// requested effects are never silently dropped (e.g. effecting a second raster right after a
    /// first). `(page_idx, uid, render_data_json, user_scale, rotation_deg)`.
    pending_raster_effects: Option<(usize, String, Value, f32, f32)>,
    /// After a raster is created, select it once the page's raster cache reloads: (page, uid).
    pending_select_raster_uid: Option<(usize, String)>,
    /// The selected raster on the current page (index into `raster_layers_by_page[page]`), mutually
    /// exclusive with `selected_overlay_idx`.
    selected_raster_idx: Option<usize>,
    /// Active raster move/rotate/mesh drag, if any.
    raster_drag_state: Option<TypingRasterDragState>,
    /// True while a raster drag has produced an unsaved transform change.
    raster_drag_has_changes: bool,
    /// Shared unified layer document (app-owned). The single source of truth for per-page layer
    /// MODEL state; the per-page projections (`raster_layers_by_page`, `overlays`, `bands_by_page`)
    /// are rebuilt from it by `sync_from_doc`. `None` until `set_layer_doc` is called.
    layer_doc: Option<std::sync::Arc<std::sync::Mutex<crate::models::layer_model::layer_doc::LayerDoc>>>,
    /// Per (page, raster uid) cache of the doc node `generation` the projected `TypingRasterLayer`'s
    /// GPU texture was uploaded from. `sync_from_doc` preserves the texture across rebuilds when the
    /// generation is unchanged, and forces a re-upload (texture = None) when it changed.
    raster_texture_generations: HashMap<(usize, String), u64>,
    /// In-flight EAGER chapter migration job (legacy `text_info.json` → inline v3 `layers.json`), run
    /// once in the background on chapter open. Carries the migration report; on completion the migrated
    /// doc pages are evicted so both tabs re-project the v3 data. `None` when no migration is running.
    migration_rx: Option<Receiver<Result<crate::models::layer_model::migrate::MigrationReport, String>>>,
    /// Pending eager-migration request captured at chapter open; the worker is only STARTED once the
    /// initial overlay load completes, so it does not race the loader on the overlay PNGs it renames.
    /// `(committed_layers_dir, legacy_text_images_dir, unsaved_layers_dir, page_paths)`.
    pending_migration: Option<(PathBuf, PathBuf, PathBuf, Vec<(usize, PathBuf)>)>,
    /// User-chosen WIDTH (px) of the floating "Слои страницы" panel, persisted across frames/pages.
    /// Clamped to `>= LAYERS_PANEL_MIN_WIDTH` (the width at which a text preview shows exactly 5 chars).
    /// Wider → text rows show more preview chars before the trailing dots (min 5).
    layers_panel_width: f32,
}

impl Default for TypingTextOverlayLayer {
    fn default() -> Self {
        Self {
            loaded_project_dir: None,
            loaded_text_images_dir: None,
            text_images_fallback_dir: None,
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
            transform_mode_raster_idx: None,
            layout_editor: None,
            deform_mode: TypingDeformMode::Perspective,
            frame_handle_side_points: TEXT_OVERLAY_FRAME_HANDLE_SIDE_POINTS_DEFAULT,
            pull_neighbor_handles: true,
            deform_tool_settings: TypingDeformToolSettings::default(),
            drag_state: None,
            drag_has_changes: false,
            primary_pointer_targets_overlay_this_frame: false,
            page_count: 0,
            page_image_paths: HashMap::new(),
            page_sizes_px: HashMap::new(),
            clean_overlays_model: None,
            auto_typing_next_token: 0,
            auto_typing_job: None,
            auto_typing_debug_visual: None,
            layers_primary_dir: None,
            layers_fallback_dir: None,
            raster_layers_by_page: HashMap::new(),
            bands_by_page: HashMap::new(),
            last_doc_version: 0,
            create_raster_state: None,
            raster_effects_state: None,
            pending_raster_effects: None,
            pending_select_raster_uid: None,
            selected_raster_idx: None,
            raster_drag_state: None,
            raster_drag_has_changes: false,
            layer_doc: None,
            raster_texture_generations: HashMap::new(),
            migration_rx: None,
            pending_migration: None,
            layers_panel_width: LAYERS_PANEL_DEFAULT_WIDTH,
        }
    }
}

impl TypingTextOverlayLayer {
    /// Stores the app-owned shared unified layer document (see `layer_doc`).
    fn set_layer_doc(
        &mut self,
        doc: std::sync::Arc<std::sync::Mutex<crate::models::layer_model::layer_doc::LayerDoc>>,
    ) {
        self.layer_doc = Some(doc);
    }

    /// Flattens the page's unified bands (from `self.bands_by_page`) into one `BandRef` per node,
    /// bottom-to-top, expanding each `TextGroup` band into its member text overlays as `PinnedText`
    /// refs sub-ordered by ascending page-Y (lower on the page = lower in the stack), mirroring
    /// `draw_composite`'s tiebreak and the PS unified order. Used to move a SINGLE text within (or out
    /// of) its text group: once flattened, every text owns its own pinned band so it can be reordered
    /// independently. (This is the per-page on-demand pinning the guardrail allows; the `layer_idx`
    /// grouping axis is untouched for other pages.)
    fn flatten_page_bands_to_refs(
        &self,
        page_idx: usize,
    ) -> Vec<crate::models::layer_model::persist::BandRef> {
        use crate::models::layer_model::ordering::Band;
        use crate::models::layer_model::persist;
        let Some(bands) = self.bands_by_page.get(&page_idx) else {
            return Vec::new();
        };
        let mut sorted: Vec<&Band> = bands.iter().collect();
        sorted.sort_by_key(|b| b.z());
        let mut order: Vec<persist::BandRef> = Vec::new();
        for band in sorted {
            match band {
                Band::Raster { uid, .. } => order.push(persist::BandRef::Raster(uid.clone())),
                Band::PinnedText { uid, .. } => {
                    order.push(persist::BandRef::PinnedText(uid.clone()));
                }
                Band::TextGroup { member_uids, .. } => {
                    let mut members = member_uids.clone();
                    members.sort_by(|a, b| {
                        let ya = self.overlay_page_y(a);
                        let yb = self.overlay_page_y(b);
                        ya.partial_cmp(&yb).unwrap_or(std::cmp::Ordering::Equal)
                    });
                    for uid in members {
                        order.push(persist::BandRef::PinnedText(uid));
                    }
                }
            }
        }
        order
    }

    /// Page-Y (vertical center) of an overlay by uid, for the page-Y sub-order of a text group.
    fn overlay_page_y(&self, uid: &str) -> f32 {
        self.overlays
            .iter()
            .find(|o| o.uid == uid)
            .map_or(0.0, |o| o.center_page_px[1])
    }

    /// Moves an INDIVIDUAL text/image overlay one step in the page's UNIFIED band-Z order (text +
    /// raster interleaved), shared with the PS editor. `up` raises it toward the top, `down` lowers it.
    ///
    /// Routed exactly like the PS editor's band move: the page's bands are flattened so the target owns
    /// its own pinned band (a text inside a group is pinned OUT of the group's page-Y auto-order for
    /// this page only), the target is swapped one step, the new order is persisted via
    /// `persist::save_page_band_order` (the disk authority for pin + Z — which a later `flush_page_text`
    /// then PRESERVES via `merge_preserved_text_fields`, so the reorder is never clobbered), and the
    /// SAME order is mirrored into the shared doc via `set_z_order` so both tabs re-project in step.
    fn move_overlay_in_unified_z(&mut self, page_idx: usize, overlay_idx: usize, up: bool) {
        let Some(uid) = self.overlays.get(overlay_idx).map(|o| o.uid.clone()) else {
            return;
        };
        self.move_node_in_unified_z(page_idx, &uid, up);
    }

    /// Moves a RASTER one step in the page's unified band-Z order (text + raster interleaved). Resolves
    /// the raster's uid from `raster_layers_by_page[page][raster_idx]` and reuses the shared band-Z core.
    fn move_raster_in_unified_z(&mut self, page_idx: usize, raster_idx: usize, up: bool) {
        let resolved = self
            .raster_layers_by_page
            .get(&page_idx)
            .and_then(|v| v.get(raster_idx))
            .map(|l| l.uid.clone());
        crate::trace_log!(
            cat::TYPING,
            "move_raster_in_unified_z page={} idx={} up={} uid={:?}",
            page_idx,
            raster_idx,
            up,
            resolved
        );
        let Some(uid) = resolved else {
            return;
        };
        self.move_node_in_unified_z(page_idx, &uid, up);
    }

    /// Uid-based core: moves the node `uid` (a raster or a text/image overlay) one step in the page's
    /// unified band-Z order. Flattens the page's bands to per-node refs, swaps the target one step with
    /// its neighbour, persists the new band order via `save_page_band_order` (the disk authority both
    /// tabs read back), and mirrors the SAME order into the shared doc via `set_z_order`. Shared by the
    /// overlay and raster reorder entry points.
    fn move_node_in_unified_z(&mut self, page_idx: usize, uid: &str, up: bool) {
        use crate::models::layer_model::persist;
        let Some(primary) = self.layers_primary_dir.clone() else {
            return;
        };

        // Ensure the page's rasters have on-disk manifest nodes BEFORE `save_page_band_order`:
        // `apply_band_order` silently SKIPS a `BandRef::Raster` whose node is not yet in the manifest,
        // and the typing tab otherwise only flushes TEXT — so a raster's new Z would never reach disk
        // (the doc move below would show it moved, then it would revert on the next reload). Mirrors
        // the PS editor's pre-reorder flush; `persist_current_page_rasters` uses the SYNCHRONOUS
        // `doc.flush_page`, so the raster is on disk before the band-order write reassigns its Z.
        self.persist_current_page_rasters(page_idx);

        // Flatten to per-node bands, then swap the target one step with its neighbour.
        let mut order = self.flatten_page_bands_to_refs(page_idx);
        let n_raster_bands = order
            .iter()
            .filter(|b| matches!(b, persist::BandRef::Raster(_)))
            .count();
        let target_pos = order.iter().position(|b| matches!(
            b,
            persist::BandRef::PinnedText(u) | persist::BandRef::Raster(u) if u == uid
        ));
        crate::trace_log!(
            cat::TYPING,
            "move_node_in_unified_z uid={} up={} order_len={} raster_bands={} target_pos={:?}",
            uid,
            up,
            order.len(),
            n_raster_bands,
            target_pos
        );
        let Some(i) = target_pos else {
            return;
        };
        let j = if up { i + 1 } else { i.wrapping_sub(1) };
        if (up && j >= order.len()) || (!up && i == 0) {
            crate::trace_log!(
                cat::TYPING,
                "move_node_in_unified_z uid={} at-end i={} len={} -> no-op",
                uid,
                i,
                order.len()
            );
            return; // already at the requested end
        }
        order.swap(i, j);

        // Persist the new band order (pin + Z) to disk — the authority both tabs read back.
        match persist::save_page_band_order(&primary, page_idx, &order) {
            Ok(()) => {
                // Drop the cached bands so the next projection reloads the new pinned-band order.
                self.bands_by_page.remove(&page_idx);
                // Mirror the SAME order into the shared doc so it (and, via its version bump, the PS
                // tab) re-projects without a disk round-trip.
                let node_order: Vec<String> = order
                    .iter()
                    .filter_map(|b| match b {
                        persist::BandRef::Raster(u) | persist::BandRef::PinnedText(u) => {
                            Some(u.clone())
                        }
                        persist::BandRef::TextGroup(_) => None,
                    })
                    .collect();
                let routed = self.route_to_doc(page_idx, |doc| {
                    doc.set_z_order(page_idx, &node_order);
                });
                crate::trace_log!(
                    cat::TYPING,
                    "move_node_in_unified_z persisted+routed uid={} node_order_len={} routed={}",
                    uid,
                    node_order.len(),
                    routed
                );
                if !routed {
                    // No doc wired / page not resident: drop the raster cache too so it reloads.
                    self.raster_layers_by_page.remove(&page_idx);
                }
            }
            Err(e) => crate::runtime_log::log_warn(&format!(
                "не удалось изменить порядок слоя в общем Z: {e}"
            )),
        }
    }

    /// Once-per-frame check: if the shared `LayerDoc` changed since we last projected (its `version`
    /// advanced), and we are idle (not loading/saving), re-project the current page from the doc.
    ///
    /// The doc is the in-memory source of truth shared with the PS tab, so any edit there (or our own
    /// that routed through the doc) bumps `version`; we just `sync_from_doc(current_page)` to rebuild
    /// this tab's projections. This is the in-memory cross-tab sync (no disk reload, no revision Arc).
    fn maybe_reproject_from_doc_version(&mut self, current_page: usize) {
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        // Don't fight in-flight work; we'll pick the change up on a later frame.
        if self.loading_rx.is_some()
            || self.save_rx.is_some()
            || self.create_render_state.is_some()
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
            || self.edit_render_rx.is_some()
        {
            return;
        }
        let Ok(guard) = doc.lock() else {
            return;
        };
        if guard.version() == self.last_doc_version {
            return;
        }
        if guard.page(current_page).is_some() {
            crate::trace_log!(
                cat::SYNC,
                "reproject_from_doc page={} old_version={} new_version={} resident=true",
                current_page,
                self.last_doc_version,
                guard.version()
            );
            self.sync_from_doc(current_page, &guard);
        } else {
            // The current page is not resident (e.g. just evicted by a self-write that will reload it
            // shortly). Adopt the version so we don't spin; the page-load path re-projects on arrival.
            crate::trace_log!(
                cat::SYNC,
                "reproject_from_doc page={} old_version={} new_version={} resident=false adopt_only",
                current_page,
                self.last_doc_version,
                guard.version()
            );
            self.last_doc_version = guard.version();
        }
    }

    /// Page pixel size `[w, h]` for `page_idx`, resolved lazily from the cached page image path
    /// (header-only `image_dimensions`) and memoized. Used for legacy-overlay uv→px decoding when the
    /// page is handed to the shared doc. Falls back to `[1, 1]` when unknown.
    fn page_size_px(&mut self, page_idx: usize) -> [usize; 2] {
        if let Some(size) = self.page_sizes_px.get(&page_idx) {
            return *size;
        }
        let size = self
            .page_image_paths
            .get(&page_idx)
            .and_then(|path| image::image_dimensions(path).ok())
            .map(|(w, h)| [w as usize, h as usize])
            .unwrap_or([1, 1]);
        self.page_sizes_px.insert(page_idx, size);
        size
    }

    /// Pixel sizes for EVERY page of the chapter (memoized via [`Self::page_size_px`]). The shared doc
    /// needs the full map — not just the loaded page — because the legacy absolute-ribbon migration
    /// recovers a chapter-wide ribbon scale from every page's aspect ratio.
    fn page_sizes_map(&mut self) -> HashMap<usize, [usize; 2]> {
        let pages: Vec<usize> = self.page_image_paths.keys().copied().collect();
        let mut out = HashMap::with_capacity(pages.len());
        for idx in pages {
            out.insert(idx, self.page_size_px(idx));
        }
        out
    }

    /// (Re)loads the read-only PS raster layers for `page_idx` if not already cached for it.
    fn ensure_raster_layers_for_page(&mut self, page_idx: usize) {
        if self.raster_layers_by_page.contains_key(&page_idx) {
            return;
        }
        let Some(primary) = self.layers_primary_dir.clone() else {
            self.raster_layers_by_page.insert(page_idx, Vec::new());
            self.bands_by_page.insert(page_idx, Vec::new());
            return;
        };
        let fallback = self.layers_fallback_dir.clone();
        // Unified per-page Z bands, used to interleave rasters with overlays in one ordered pass.
        let bands = crate::models::layer_model::persist::load_page_bands(
            &primary,
            fallback.as_deref(),
            page_idx,
        );
        self.bands_by_page.insert(page_idx, bands);
        let loaded = crate::models::layer_model::persist::load_page_rasters(
            &primary,
            fallback.as_deref(),
            page_idx,
        );
        let layers = match loaded {
            Ok(page) => page
                .layers
                .into_iter()
                .map(|l| TypingRasterLayer {
                    uid: l.uid,
                    name: l.name,
                    visible: l.visible,
                    opacity: l.opacity,
                    transform: l.transform,
                    image: l.image,
                    base_file: l.base_file,
                    effects: l.effects,
                    deform: l.deform,
                    mask_clip_enabled: l.mask_clip.unwrap_or(false),
                    clipped_image: None,
                    texture: None,
                })
                .collect(),
            Err(err) => {
                crate::runtime_log::log_warn(format!(
                    "[typing] load PS raster layers for page {page_idx} failed: {err}"
                ));
                Vec::new()
            }
        };
        // A just-created raster asked to be selected once its page reloaded — resolve by uid now.
        if let Some((pending_page, uid)) = self.pending_select_raster_uid.clone()
            && pending_page == page_idx
            && let Some(idx) = layers.iter().position(|l| l.uid == uid)
        {
            self.selected_raster_idx = Some(idx);
            self.selected_overlay_idx = None;
            self.pending_select_raster_uid = None;
        }
        self.raster_layers_by_page.insert(page_idx, layers);
        // The shared unified layer document is the source of truth for layer MODEL state. Ensure the
        // page is resident, then rebuild the per-page projections (rasters / overlays / bands) from
        // it, overriding the disk-loaded caches above so both tabs read one model.
        // Full chapter page sizes: the legacy ribbon migration in the doc needs every page's aspect.
        let page_sizes = self.page_sizes_map();
        if let Some(doc) = self.layer_doc.clone()
            && let Ok(mut doc_guard) = doc.lock()
        {
            let _ = doc_guard.ensure_page_loaded(page_idx, &primary, fallback.as_deref(), &page_sizes);
            self.sync_from_doc(page_idx, &doc_guard);
        }
    }

    /// Rebuilds the per-page projections (`raster_layers_by_page`, `overlays`, `bands_by_page`) for
    /// `page_idx` from the resident `LayerDoc` page, which is the source of truth for MODEL state
    /// (transform, deform, effects, display pixels, render_data, z, visibility, opacity, group).
    ///
    /// Runtime/GPU/UI state is kept LOCAL and matched to nodes by `uid`:
    /// - Rasters: a fresh `TypingRasterLayer` per doc Raster node; the GPU texture is preserved
    ///   across rebuilds via `raster_texture_generations` and only dropped (forcing re-upload) when
    ///   the node's `generation` changed.
    /// - Overlays: each doc Text node is reconciled onto the existing `TypingOverlayRuntime` with the
    ///   same uid — its MODEL fields are updated from the node while runtime fields (texture, upload
    ///   state, payload tracking) are preserved; the GPU texture is re-uploaded only on a generation
    ///   change. Runtime REMOVAL stays owned by `remove_overlay` / the disk loader, so the projected
    ///   overlay indices are stable across a sync.
    /// - Bands: one `Raster`/`PinnedText` band per node, with `z` taken directly from the node.
    fn sync_from_doc(
        &mut self,
        page_idx: usize,
        doc: &crate::models::layer_model::layer_doc::LayerDoc,
    ) {
        use crate::models::layer_model::layer_doc::NodeBody;
        use crate::models::layer_model::ordering::Band;
        let _sync_span = crate::trace_scope!(
            cat::SYNC,
            "sync_from_doc page={} doc_version={}",
            page_idx,
            doc.version()
        );
        let Some(page) = doc.page(page_idx) else {
            return;
        };

        // --- Rasters: one projected layer per doc Raster node, texture preserved by generation. ---
        // Capture the OLD positional → uid mapping before the rebuild. `selected_raster_idx`,
        // `transform_mode_raster_idx`, and `raster_drag_state.raster_idx` are positions into THIS page's
        // raster list, which `sync_from_doc` rebuilds in z-order every reproject. After a raster reorder
        // (⬆/⬇, or a PS reorder that reprojects), a positional index would point at a DIFFERENT raster, so
        // a transform/delete would hit the wrong layer. We resolve each tracked index to its uid here and
        // remap to the uid's NEW position after the rebuild (clearing it if the raster is gone).
        let prev_raster_uids: Vec<String> = self
            .raster_layers_by_page
            .get(&page_idx)
            .map(|layers| layers.iter().map(|l| l.uid.clone()).collect())
            .unwrap_or_default();
        let selected_raster_uid = self
            .selected_raster_idx
            .and_then(|i| prev_raster_uids.get(i).cloned());
        let transform_raster_uid = self
            .transform_mode_raster_idx
            .and_then(|i| prev_raster_uids.get(i).cloned());
        let drag_raster_uid = self
            .raster_drag_state
            .as_ref()
            .and_then(|d| prev_raster_uids.get(d.raster_idx).cloned());

        let mut prev_rasters: HashMap<String, egui::TextureHandle> = self
            .raster_layers_by_page
            .remove(&page_idx)
            .map(|layers| {
                layers
                    .into_iter()
                    .filter_map(|l| l.texture.map(|t| (l.uid, t)))
                    .collect()
            })
            .unwrap_or_default();

        let mut rasters: Vec<TypingRasterLayer> = Vec::new();
        for node in &page.nodes {
            let NodeBody::Raster {
                display_image,
                effects,
                base_file,
                mask_clip,
                ..
            } = &node.body
            else {
                continue;
            };
            // Preserve the GPU texture when the generation the texture was built from is unchanged. The
            // mask-clip toggle bumps the node generation, so this invalidates the texture (and the
            // cached clipped image below) → re-clip + re-upload.
            let cache_key = (page_idx, node.uid.clone());
            let gen_unchanged = self.raster_texture_generations.get(&cache_key).copied()
                == Some(node.generation);
            let texture = if gen_unchanged {
                prev_rasters.remove(&node.uid)
            } else {
                self.raster_texture_generations
                    .insert(cache_key, node.generation);
                None
            };
            rasters.push(TypingRasterLayer {
                uid: node.uid.clone(),
                name: node.name.clone(),
                visible: node.visible,
                opacity: node.opacity,
                transform: node.transform,
                image: display_image.clone(),
                base_file: base_file.clone(),
                effects: effects.clone(),
                deform: node.deform.clone(),
                mask_clip_enabled: mask_clip.unwrap_or(false),
                // A generation change (e.g. a mask-clip toggle) invalidates the cached clipped image.
                clipped_image: None,
                texture,
            });
        }
        // Any textures left in `prev_rasters` belonged to nodes whose generation changed (or which
        // are gone); they are dropped here, freeing their GPU handles.
        drop(prev_rasters);

        // Remap the tracked raster indices to their uid's NEW position in the rebuilt z-ordered list, so
        // a reorder doesn't silently retarget selection / transform / drag to a different raster. Only
        // touch a field when we resolved a uid for THIS page above (so a selection on another page, or a
        // freshly-set index, is left alone). A uid that's gone (deleted) clears the field.
        if let Some(uid) = &selected_raster_uid {
            self.selected_raster_idx = rasters.iter().position(|l| &l.uid == uid);
        }
        if let Some(uid) = &transform_raster_uid {
            self.transform_mode_raster_idx = rasters.iter().position(|l| &l.uid == uid);
        }
        if let Some(uid) = &drag_raster_uid {
            match rasters.iter().position(|l| &l.uid == uid) {
                Some(new_idx) => {
                    if let Some(drag) = self.raster_drag_state.as_mut() {
                        drag.raster_idx = new_idx;
                    }
                }
                None => self.raster_drag_state = None,
            }
        }

        self.raster_layers_by_page.insert(page_idx, rasters);

        // --- Overlays: reconcile-OR-CREATE doc Text nodes onto the local runtimes by uid (this page). ---
        // The doc is the source of truth for text. For a runtime that already exists (in-session-created,
        // already-projected, or loaded from legacy `text_info.json`) we reconcile its MODEL fields. For a
        // doc Text node with NO local runtime we MATERIALIZE one from the node (mirrors PS's
        // `sync_view_from_doc`). Without this, a MIGRATED chapter — whose `text_info.json` is retired to
        // `.bak`, so the legacy disk loader populates no `self.overlays` — would show no text in the
        // typing tab even though PS and the doc carry it. The runtime's deterministic rendered-PNG name
        // (`text_image_file_name`) is the same the doc's text flush writes, so a later placement-save
        // round-trips.
        let mut to_requeue: Vec<usize> = Vec::new();
        for node in &page.nodes {
            let NodeBody::Text { render_data, image, mask_clip, .. } = &node.body else {
                continue;
            };
            let center = [node.transform.cx, node.transform.cy];
            let angle_deg = node.transform.rotation.to_degrees();
            let user_scale = node.transform.scale;
            let size_px = image.size;
            let deform_mesh = node.deform.as_ref().and_then(|d| {
                TypingOverlayDeformMesh::new(d.cols, d.rows, d.points_px.clone(), size_px)
            });
            let render_data_json = if render_data.is_null() {
                None
            } else {
                Some(render_data.clone())
            };

            let cache_key = (page_idx, node.uid.clone());
            let existing_idx = self
                .overlays
                .iter()
                .position(|o| o.uid == node.uid && o.page_idx == page_idx);

            match existing_idx {
                Some(idx) => {
                    // Reconcile MODEL fields; preserve runtime/payload-tracking fields.
                    let pixels_changed = self.raster_texture_generations.get(&cache_key).copied()
                        != Some(node.generation);
                    let rt = &mut self.overlays[idx];
                    rt.center_page_px = center;
                    rt.angle_deg = angle_deg;
                    rt.user_scale = user_scale;
                    rt.deform_mesh = deform_mesh;
                    rt.render_data_json = render_data_json;
                    if pixels_changed {
                        rt.size_px = size_px;
                        rt.source_rgba = color_image_to_rgba(image);
                        rt.display_texture_stale = true;
                        self.raster_texture_generations
                            .insert(cache_key, node.generation);
                        to_requeue.push(idx);
                    }
                }
                None => {
                    // CREATE: materialize a runtime from the doc node (migrated-chapter case).
                    let runtime = text_runtime_from_doc_node(
                        &node.uid,
                        page_idx,
                        center,
                        user_scale,
                        angle_deg,
                        deform_mesh,
                        mask_clip.unwrap_or(false),
                        node.text_layer_idx.unwrap_or(0) as usize,
                        render_data_json,
                        size_px,
                        color_image_to_rgba(image),
                    );
                    self.overlays.push(runtime);
                    let idx = self.overlays.len() - 1;
                    // Mark the texture generation as projected so a subsequent sync doesn't needlessly
                    // re-upload, and queue this frame's upload so it renders immediately.
                    self.raster_texture_generations
                        .insert(cache_key, node.generation);
                    to_requeue.push(idx);
                }
            }
        }
        for idx in to_requeue {
            self.queue_overlay_texture_upload(idx);
        }
        // Note: runtime REMOVAL is owned by `remove_overlay` (which also fixes the positional upload
        // queue + selection indices) and by the disk loader on a full reload; `sync_from_doc` does
        // not drop runtimes, so the projected overlay indices stay stable across a sync.

        // --- Bands: derive unified Z directly from the doc node z. ---
        let mut bands: Vec<Band> = Vec::with_capacity(page.nodes.len());
        for node in &page.nodes {
            match node.kind {
                crate::models::layer_model::layer_doc::NodeKind::Raster => {
                    bands.push(Band::Raster {
                        uid: node.uid.clone(),
                        z: node.z,
                    });
                }
                crate::models::layer_model::layer_doc::NodeKind::Text => {
                    bands.push(Band::PinnedText {
                        uid: node.uid.clone(),
                        z: node.z,
                    });
                }
            }
        }
        self.bands_by_page.insert(page_idx, bands);

        // A just-created raster asked to be selected once its page synced — resolve by uid now.
        if let Some((pending_page, uid)) = self.pending_select_raster_uid.clone()
            && pending_page == page_idx
            && let Some(idx) = self
                .raster_layers_by_page
                .get(&page_idx)
                .and_then(|ls| ls.iter().position(|l| l.uid == uid))
        {
            self.selected_raster_idx = Some(idx);
            self.selected_overlay_idx = None;
            self.pending_select_raster_uid = None;
        }

        // Record the doc version we just projected so the per-frame `maybe_reproject_from_doc_version`
        // check does not redundantly re-project until the doc changes again.
        self.last_doc_version = doc.version();
    }

    /// Routes an edit to the shared `LayerDoc`: locks it, runs `edit` against the resident page (it
    /// must already be loaded via `ensure_raster_layers_for_page`), then rebuilds the per-page
    /// projections from the doc with `sync_from_doc`. No-op (returns false) if no doc is wired; the
    /// caller then keeps its legacy local-cache + disk path. Returns true when the doc handled it.
    fn route_to_doc<F>(&mut self, page_idx: usize, edit: F) -> bool
    where
        F: FnOnce(&mut crate::models::layer_model::layer_doc::LayerDoc),
    {
        let Some(doc) = self.layer_doc.clone() else {
            return false;
        };
        let Ok(mut guard) = doc.lock() else {
            return false;
        };
        if guard.page(page_idx).is_none() {
            // The page is not resident in the doc; let the caller fall back to its legacy path.
            return false;
        }
        edit(&mut guard);
        // Guarantee a cross-tab notification even if `edit` mutated node fields directly via
        // `node_mut` (which does not bump the version). Idempotent if `edit` already bumped.
        guard.mark_changed();
        self.sync_from_doc(page_idx, &guard);
        true
    }

    /// Draws a single cached read-only PS raster layer (by page + index) into `painter`, lazily
    /// uploading its texture via `ctx`. Uses the same page-px -> scene mapping (`scene_from_page_px`)
    /// as the text overlays. Visibility/opacity handling matches `draw_page_raster_layers`.
    fn draw_one_raster_layer(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        page_idx: usize,
        raster_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) {
        let Some(layer) = self
            .raster_layers_by_page
            .get_mut(&page_idx)
            .and_then(|layers| layers.get_mut(raster_idx))
        else {
            return;
        };
        if !layer.visible || layer.opacity <= 0.0 {
            return;
        }
        let [w, h] = layer.image.size;
        if w == 0 || h == 0 {
            return;
        }
        // Use the mask-clipped image when mask-clip is on (precomputed in `prepare_raster_mask_clips`),
        // else the plain display image.
        let upload_image = layer
            .clipped_image
            .as_ref()
            .filter(|_| layer.mask_clip_enabled)
            .unwrap_or(&layer.image)
            .clone();
        let texture = layer.texture.get_or_insert_with(|| {
            ctx.load_texture("typing_ps_raster_layer", upload_image, TextureOptions::LINEAR)
        });
        let texture_id = texture.id();
        // Deformed raster: positioned by its cols×rows mesh (absolute page px), exactly like a
        // deformed text overlay. The affine transform does not apply while deformed.
        if let Some(grid) = &layer.deform {
            if grid.cols >= 2 && grid.rows >= 2 && grid.points_px.len() == grid.cols * grid.rows {
                let mesh_scene: Vec<Pos2> = grid
                    .points_px
                    .iter()
                    .map(|p| scene_from_page_px(image_rect, zoom, *p))
                    .collect();
                draw_textured_deform_mesh(painter, texture_id, &mesh_scene, grid.cols, grid.rows);
                return;
            }
        }
        // Transform: center in page px, uniform scale, rotation (radians). Corners are the
        // image quad centered on (cx, cy), scaled and rotated, then mapped page-px -> scene.
        let cx = layer.transform.cx;
        let cy = layer.transform.cy;
        let scale = layer.transform.scale;
        let (sin_a, cos_a) = layer.transform.rotation.sin_cos();
        let hw = w as f32 * 0.5 * scale;
        let hh = h as f32 * 0.5 * scale;
        // Local corner offsets (top-left, top-right, bottom-right, bottom-left).
        let corners = [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)];
        let mut quad = [Pos2::ZERO; 4];
        for (i, (dx, dy)) in corners.iter().enumerate() {
            let rx = dx * cos_a - dy * sin_a;
            let ry = dx * sin_a + dy * cos_a;
            quad[i] = scene_from_page_px(image_rect, zoom, [cx + rx, cy + ry]);
        }
        let tint = Color32::from_white_alpha((layer.opacity.clamp(0.0, 1.0) * 255.0) as u8);
        let mut mesh = Mesh::with_texture(texture.id());
        let uvs = [
            Pos2::new(0.0, 0.0),
            Pos2::new(1.0, 0.0),
            Pos2::new(1.0, 1.0),
            Pos2::new(0.0, 1.0),
        ];
        for i in 0..4 {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: quad[i],
                uv: uvs[i],
                color: tint,
            });
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        painter.add(egui::Shape::mesh(mesh));
    }

    /// Unified band Z for a raster (by uid) on `page_idx`: the Z of the matching `Raster` band, or a
    /// top-of-stack key (`bands.len()`) for an unsaved raster not yet in the manifest.
    fn raster_band_z(&self, page_idx: usize, uid: &str) -> u32 {
        let Some(bands) = self.bands_by_page.get(&page_idx) else {
            return 0;
        };
        for band in bands {
            if let crate::models::layer_model::ordering::Band::Raster { uid: u, z } = band
                && u == uid
            {
                return *z;
            }
        }
        bands.len() as u32
    }

    /// Unified band Z for an overlay on `page_idx`: if a `PinnedText` band with `uid` exists, its Z;
    /// else the Z of the `TextGroup` band whose `layer_idx == layer_idx`; else a top-of-stack key
    /// (`bands.len()`) for an item not yet in the manifest.
    fn overlay_band_z(&self, page_idx: usize, uid: &str, layer_idx: usize) -> u32 {
        use crate::models::layer_model::ordering::Band;
        let Some(bands) = self.bands_by_page.get(&page_idx) else {
            return 0;
        };
        for band in bands {
            if let Band::PinnedText { uid: u, z } = band
                && u == uid
            {
                return *z;
            }
        }
        let layer_idx_u32 = u32::try_from(layer_idx).unwrap_or(u32::MAX);
        for band in bands {
            if let Band::TextGroup {
                layer_idx: li, z, ..
            } = band
                && *li == layer_idx_u32
            {
                return *z;
            }
        }
        bands.len() as u32
    }

    /// The TOPMOST text/image overlay whose scene quad contains `pointer` on `page_idx`, as
    /// `(overlay_idx, unified band-Z)`, or `None` if no overlay is under the pointer. Used by the unified
    /// click hit-test so a raster cannot steal a click that lands on a higher-Z overlay (and vice-versa
    /// once text can sit below a raster). Mirrors `merged_fills`' overlay band-Z lookup.
    fn topmost_overlay_at(
        &self,
        page_idx: usize,
        pointer: Option<Pos2>,
        image_rect: Rect,
        zoom: f32,
    ) -> Option<(usize, u32)> {
        let p = pointer?;
        let mut best: Option<(usize, u32)> = None;
        for (idx, overlay) in self.overlays.iter().enumerate() {
            if overlay.page_idx != page_idx || overlay.texture.is_none() {
                continue;
            }
            let quad = overlay_quad_scene(overlay, image_rect, zoom);
            if !point_in_quad(p, &quad) {
                continue;
            }
            let z = self.overlay_band_z(page_idx, &overlay.uid, overlay.layer_idx);
            if best.is_none_or(|(_, bz)| z >= bz) {
                best = Some((idx, z));
            }
        }
        best
    }

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

    /// Task C: compact, collapsible layers list for the current page. Shows the read-only PS raster
    /// rows (name + visibility) followed by this tab's text/image overlays, which can be reordered
    /// (up/down) within the page. Reordering rewrites overlay array order, hence persisted z.
    /// Renders the «Слои» tab BODY (the unified interleaved layer list with per-row ⬆/⬇ move,
    /// text-preview names, the width-resize, and the 8-row scroll) into the supplied `ui`. The outer
    /// Area/Frame and the tab header/collapse are provided by the combined Actions/Layers panel (drawn
    /// from `TypingTopPanelState`). The WIDTH is still user-resizable here and persisted in
    /// `layers_panel_width`, driving the per-width `max_chars` preview budget. No-op while the layout
    /// editor is active.
    /// The current persisted «Слои» list width — lets the combined panel size its Frame so the list's
    /// inner width-resize can actually widen the panel.
    pub(super) fn layers_panel_width(&self) -> f32 {
        self.layers_panel_width
    }

    pub(super) fn draw_layers_tab_body(&mut self, ui: &mut egui::Ui, page_idx: usize) {
        if self.layout_editor.is_some() {
            return;
        }
        self.ensure_raster_layers_for_page(page_idx);

        // Indices into `self.overlays` for this page, in array order (== persisted z order).
        let page_overlay_indices: Vec<usize> = self
            .overlays
            .iter()
            .enumerate()
            .filter(|(_, o)| o.page_idx == page_idx)
            .map(|(i, _)| i)
            .collect();

        let raster_count = self
            .raster_layers_by_page
            .get(&page_idx)
            .map_or(0, Vec::len);

        // Build ONE unified, interleaved row list (text + image overlays + rasters) ordered by unified
        // band-Z DESCENDING (top of the stack first). Overlay above raster at equal Z (the canvas/hit-test
        // tie-break). This uses the SAME Z the canvas/hit-test use, so the panel matches what's drawn.
        let mut row_inputs: Vec<(TypingLayerRow, u32, bool)> = Vec::new();
        for &ov_idx in &page_overlay_indices {
            if let Some(o) = self.overlays.get(ov_idx) {
                let z = self.overlay_band_z(page_idx, &o.uid, o.layer_idx);
                row_inputs.push((TypingLayerRow::Overlay(ov_idx), z, false));
            }
        }
        for raster_idx in 0..raster_count {
            if let Some(uid) = self
                .raster_layers_by_page
                .get(&page_idx)
                .and_then(|v| v.get(raster_idx))
                .map(|l| l.uid.clone())
            {
                let z = self.raster_band_z(page_idx, &uid);
                row_inputs.push((TypingLayerRow::Raster(raster_idx), z, true));
            }
        }
        let ordered_rows = order_unified_layer_rows(row_inputs);

        // A single move per frame across BOTH kinds; the row identity carries the kind.
        let mut move_row: Option<(TypingLayerRow, bool)> = None;
        let mut select_overlay: Option<usize> = None;
        let mut select_raster: Option<usize> = None;

        // Representative glyph width + row height from the current font/spacing (not magic numbers).
        // egui 0.33's `Fonts*::glyph_width`/`row_height` need a &mut view (only `Painter`/`Ui` text
        // measuring gives it), so measure a 10-glyph run via a galley and divide.
        let font_id = egui::TextStyle::Body.resolve(&ui.ctx().style());
        let probe = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap("оооооооооо".to_string(), font_id.clone(), Color32::WHITE)
        });
        let char_px = (probe.rect.width() / 10.0).max(1.0);
        let line_height = probe.rect.height().max(1.0);
        // A row is a line plus the vertical item spacing between rows.
        let row_height = (line_height + ui.ctx().style().spacing.item_spacing.y).max(1.0);
        let list_height = row_height * LAYERS_PANEL_DEFAULT_ROWS as f32;

        // MIN width = overhead + exactly `LAYERS_PANEL_MIN_PREVIEW_CHARS` chars of preview, so at the
        // narrowest the preview shows 5 chars and the panel can't shrink further. Clamp the persisted
        // width up to it.
        let min_width =
            LAYERS_PANEL_ROW_OVERHEAD_PX + LAYERS_PANEL_MIN_PREVIEW_CHARS as f32 * char_px;
        if self.layers_panel_width < min_width {
            self.layers_panel_width = min_width;
        }
        let panel_width = self.layers_panel_width;
        // Preview char budget from the CURRENT width: how many chars fit after the fixed overhead.
        let max_chars = preview_char_budget(panel_width - LAYERS_PANEL_ROW_OVERHEAD_PX, char_px);

        let mut new_width = panel_width;
        // Width-only resize for the list; HEIGHT follows content, capped at ~8 rows by the ScrollArea
        // (`auto_shrink` lets a short list hug). The combined panel's Frame + the «Слои» tab supply the
        // surrounding chrome.
        egui::Resize::default()
            .id_salt("typing_layers_panel_resize")
            .resizable([true, false])
            .default_size(egui::vec2(panel_width, 0.0))
            .min_size(egui::vec2(min_width, 0.0))
            .show(ui, |ui| {
                new_width = ui.available_width().max(min_width);
                egui::ScrollArea::vertical()
                    .max_height(list_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if ordered_rows.is_empty() {
                            ui.weak("Нет слоёв на этой странице.");
                        }
                        for row in &ordered_rows {
                            match *row {
                                TypingLayerRow::Overlay(ov_idx) => {
                                    let Some(overlay) = self.overlays.get(ov_idx) else {
                                        continue;
                                    };
                                    let label = match overlay.kind {
                                        TypingOverlayKind::Text => {
                                            let text = overlay
                                                .render_data_json
                                                .as_ref()
                                                .and_then(|rd| rd.get("text_params"))
                                                .and_then(|tp| tp.get("text"))
                                                .and_then(Value::as_str)
                                                .unwrap_or("");
                                            let preview = text_preview_label(text, max_chars);
                                            if preview.is_empty() {
                                                "Текст".to_string()
                                            } else {
                                                format!("Текст ({preview})")
                                            }
                                        }
                                        TypingOverlayKind::Image => "Картинка".to_string(),
                                    };
                                    let selected = self.selected_overlay_idx == Some(ov_idx);
                                    ui.horizontal(|ui| {
                                        if ui.button("⬆").clicked() {
                                            move_row = Some((*row, true));
                                        }
                                        if ui.button("⬇").clicked() {
                                            move_row = Some((*row, false));
                                        }
                                        if ui.selectable_label(selected, label).clicked() {
                                            select_overlay = Some(ov_idx);
                                        }
                                    });
                                }
                                TypingLayerRow::Raster(raster_idx) => {
                                    let Some(layer) = self
                                        .raster_layers_by_page
                                        .get(&page_idx)
                                        .and_then(|v| v.get(raster_idx))
                                    else {
                                        continue;
                                    };
                                    let selected = self.selected_raster_idx == Some(raster_idx);
                                    let label = format!("🖼 {}", layer.name);
                                    ui.horizontal(|ui| {
                                        if ui.button("⬆").clicked() {
                                            move_row = Some((*row, true));
                                        }
                                        if ui.button("⬇").clicked() {
                                            move_row = Some((*row, false));
                                        }
                                        if ui.selectable_label(selected, label).clicked() {
                                            select_raster = Some(raster_idx);
                                        }
                                    });
                                }
                            }
                        }
                    });
            });
        // Persist the (clamped) user-chosen width for next frame.
        self.layers_panel_width = new_width.max(min_width);

        if let Some(idx) = select_overlay {
            self.selected_overlay_idx = Some(idx);
            self.selected_raster_idx = None;
            self.transform_mode_raster_idx = None;
        }
        if let Some(idx) = select_raster {
            self.select_raster(idx);
        }
        // Apply at most one Z change per frame, routing by row kind. Both move helpers route through the
        // shared doc band reorder, so text and rasters interleave correctly. ⬆ raises one step, ⬇ lowers.
        if let Some((row, up)) = move_row {
            match row {
                TypingLayerRow::Overlay(idx) => {
                    self.move_overlay_in_unified_z(page_idx, idx, up)
                }
                TypingLayerRow::Raster(idx) => {
                    self.move_raster_in_unified_z(page_idx, idx, up)
                }
            }
        }
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
        self.migration_rx = None;
        self.pending_migration = None;
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

        // Text overlays now live in the chapter's `layers/` folder. Saves go to the unsaved
        // staging `layers/` dir; reads prefer it, then the committed `layers/` dir. Chapters that
        // predate this move still keep their overlays under the legacy `text_images/` folder, so
        // that is used as the committed read source until the next save migrates them into
        // `layers/`. Page masks are a separate store and stay under `text_images/`.
        let unsaved_layers_dir = project.paths.unsaved_layers_dir.clone();
        let main_layers_dir = project.paths.layers_dir.clone();
        let legacy_text_images_dir = project.paths.text_images_dir.clone();

        // Capture the dirs used to read read-only PS raster layers (Task B) and force a reload of
        // the raster cache for the current page on this project (re)load.
        self.layers_primary_dir = Some(unsaved_layers_dir.clone());
        self.layers_fallback_dir = Some(main_layers_dir.clone());
        self.raster_layers_by_page.clear();
        self.bands_by_page.clear();

        // Committed (non-staging) read source: migrated chapters have `text_info.json` under
        // `layers/`; older ones only under the legacy `text_images/` dir. Used as the save-time
        // fallback for locating original image PNGs.
        let committed_read_dir = if main_layers_dir.join(TEXT_INFO_FILE_NAME).is_file() {
            main_layers_dir.clone()
        } else if legacy_text_images_dir.join(TEXT_INFO_FILE_NAME).is_file() {
            legacy_text_images_dir.clone()
        } else {
            main_layers_dir.clone()
        };

        // Saves always go to the unsaved staging dir.
        self.text_images_save_dir = Some(unsaved_layers_dir.clone());
        // The committed dir is a read fallback: original image PNGs may still live only there
        // (including a legacy `text_images/` for not-yet-migrated chapters).
        self.text_images_fallback_dir = Some(committed_read_dir.clone());

        // Loading reads `text_info.json` from the first candidate that has it, and resolves each
        // overlay PNG from that dir then every later one. The order — unsaved staging, committed
        // `layers/`, legacy `text_images/` — means an old chapter's PNGs are still found after its
        // metadata has migrated into `layers/`.
        let candidate_dirs = [unsaved_layers_dir, main_layers_dir, legacy_text_images_dir];
        let primary_idx = candidate_dirs
            .iter()
            .position(|d| d.join(TEXT_INFO_FILE_NAME).is_file())
            .unwrap_or(0);
        let primary_load_dir = candidate_dirs[primary_idx].clone();
        let fallback_load_dirs: Vec<PathBuf> = candidate_dirs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != primary_idx)
            .map(|(_, d)| d.clone())
            .collect();

        let page_paths = project
            .pages
            .iter()
            .map(|page| (page.idx, page.path.clone()))
            .collect::<Vec<_>>();
        // Cache page image paths for lazy page-pixel-size resolution (legacy overlay uv→px), and drop
        // any stale sizes from a previous project.
        self.page_image_paths = page_paths.iter().cloned().collect();
        self.page_sizes_px.clear();
        let (tx, rx) = mpsc::channel::<TypingOverlayLoadResponse>();
        let project_dir_for_thread = project_dir.clone();
        let primary_load_dir_for_thread = primary_load_dir.clone();
        thread::spawn(move || {
            let page_sizes = load_typing_page_sizes(&page_paths);
            let fallback_refs: Vec<&Path> = fallback_load_dirs.iter().map(PathBuf::as_path).collect();
            let result = load_typing_overlays_from_dir(
                &primary_load_dir_for_thread,
                &fallback_refs,
                &page_sizes,
            );
            let _ = tx.send((project_dir_for_thread, result));
        });
        self.loading_project_dir = Some(project_dir);
        self.loading_text_images_dir = Some(primary_load_dir);
        self.loading_rx = Some(rx);

        // EAGER one-shot migration: if this is a legacy chapter (a `text_info.json` not yet fully
        // inlined into v3 `layers.json`), convert the WHOLE chapter to v3 on disk once, in the
        // background. Pixels are preserved by renaming the overlay PNGs; `text_info.json` becomes
        // `.bak` LAST. RECORD the request now (cheap detection); it is STARTED only after the initial
        // overlay load completes (`poll_loader`), so the migration never races the loader on the
        // overlay PNGs it renames.
        use crate::models::layer_model::migrate;
        self.pending_migration = None;
        let committed_layers = project.paths.layers_dir.clone();
        let legacy_text_images = project.paths.text_images_dir.clone();
        if migrate::chapter_needs_migration(&committed_layers, &legacy_text_images).is_some() {
            let page_paths = project
                .pages
                .iter()
                .map(|page| (page.idx, page.path.clone()))
                .collect::<Vec<_>>();
            self.pending_migration = Some((
                committed_layers,
                legacy_text_images,
                project.paths.unsaved_layers_dir.clone(),
                page_paths,
            ));
        }
    }

    /// Spawns the eager chapter-migration worker for the `pending_migration` request captured at open.
    /// Called once the initial overlay load has completed, so the migration (which renames overlay
    /// PNGs) does not race the loader. The result is polled by `poll_migration`.
    fn start_pending_migration(&mut self) {
        use crate::models::layer_model::migrate;
        if self.migration_rx.is_some() {
            return;
        }
        let Some((committed_layers, legacy_text_images, unsaved_layers, page_paths)) =
            self.pending_migration.take()
        else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let page_sizes = load_typing_page_sizes(&page_paths);
            let result = migrate::migrate_chapter_to_v3(
                &committed_layers,
                &legacy_text_images,
                Some(&unsaved_layers),
                &page_sizes,
            );
            let _ = tx.send(result);
        });
        self.migration_rx = Some(rx);
    }

    /// Polls the eager migration worker. On success, evicts the migrated doc pages so both tabs
    /// re-project the v3 data; drops the page caches so they reload. Returns true when it completed.
    fn poll_migration(&mut self) -> bool {
        let Some(rx) = self.migration_rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.migration_rx = None;
                match result {
                    Ok(report) => {
                        if !report.migrated_pages.is_empty() {
                            crate::runtime_log::log_info(format!(
                                "[migrate] chapter migrated to v3: {} overlays, {} PNGs renamed, {} missing, {} pages; backup {:?}",
                                report.migrated_overlays,
                                report.renamed_pngs,
                                report.missing_pngs,
                                report.migrated_pages.len(),
                                report.backup_path
                            ));
                            // Evict the migrated pages from the shared doc so both tabs re-project the
                            // v3 inline data (the doc version bump drives their per-frame reproject).
                            if let Some(doc) = self.layer_doc.clone()
                                && let Ok(mut guard) = doc.lock()
                            {
                                for &page in &report.migrated_pages {
                                    guard.evict_page(page);
                                }
                            }
                            // Drop the local per-page caches so they reload the migrated bands/text.
                            for &page in &report.migrated_pages {
                                self.bands_by_page.remove(&page);
                                self.raster_layers_by_page.remove(&page);
                            }
                        }
                    }
                    Err(err) => crate::runtime_log::log_warn(format!(
                        "[migrate] chapter migration failed (will retry on next open, text_info.json intact): {err}"
                    )),
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.migration_rx = None;
                crate::runtime_log::log_warn("[migrate] migration worker disconnected".to_string());
                true
            }
        }
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
                        // MERGE by (uid, page) instead of wholesale-replace, so doc-created runtimes
                        // (materialized by an early `sync_from_doc` on a MIGRATED chapter, whose loader
                        // returns an empty set) are NOT wiped on loader completion. See
                        // `merge_loaded_overlays`. Only the merged-in entries are (re)queued for upload;
                        // doc-created runtimes keep whatever upload state `sync_from_doc` gave them.
                        let touched = merge_loaded_overlays(&mut self.overlays, decoded);
                        for idx in touched {
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
                        // Do NOT wholesale-clear `overlays` (same class as the merge fix): on a CORRUPT
                        // / unreadable `text_info.json` the doc-created runtimes are authoritative and
                        // must survive — clearing would wipe text the user is editing. Just record the
                        // error and log it; keep the existing runtimes + their upload queue intact.
                        self.loading_text_images_dir = None;
                        self.loaded_text_images_dir = None;
                        crate::runtime_log::log_warn(format!(
                            "[typing] overlay load failed (keeping doc-created runtimes): {err}"
                        ));
                        self.export_rx = None;
                        self.export_status = TypingExportUiStatus::Hidden;
                        self.last_load_error = Some(err);
                        self.cancel_active_edit_overlay_render();
                        self.edit_render_data_dirty = false;
                    }
                }
                // The initial overlay load is done reading `text_info.json` + the overlay PNGs, so it is
                // now safe to start the eager migration (which renames those PNGs) without a race.
                self.start_pending_migration();
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
                crate::trace_log!(
                    cat::SYNC,
                    "create_overlay_render result=ok uid={} kind={:?} size={}x{} warnings={}",
                    decoded.uid,
                    decoded.kind,
                    decoded.size_px[0],
                    decoded.size_px[1],
                    decoded.warnings.len()
                );
                if !decoded.warnings.is_empty() {
                    self.set_create_warning(ctx, decoded.warnings.join("; "));
                }
                self.insert_runtime_overlay(decoded);
                self.request_overlay_placement_save();
                true
            }
            Ok(Err(err)) | Err(err) => {
                crate::trace_log!(cat::SYNC, "create_overlay_render result=err err={}", err);
                self.set_create_error(ctx, err);
                true
            }
        }
    }

    /// Drops the cached raster layers + bands for `page_idx` so they reload from disk (authoritative)
    /// on the next `ensure_raster_layers_for_page`.
    fn invalidate_raster_cache_for_page(&mut self, page_idx: usize) {
        self.raster_layers_by_page.remove(&page_idx);
        self.bands_by_page.remove(&page_idx);
        // Evict the page from the shared doc too, so the next `ensure_raster_layers_for_page`
        // reloads it from disk (where a worker just wrote a new raster) and re-projects.
        if let Some(doc) = &self.layer_doc
            && let Ok(mut guard) = doc.lock()
        {
            guard.evict_page(page_idx);
        }
        // Drop this page's raster texture-generation cache so re-projected nodes re-upload cleanly.
        self.raster_texture_generations
            .retain(|(p, _), _| *p != page_idx);
    }

    /// Polls the "create raster from external image" worker. On success the page's raster cache is
    /// reloaded from disk and the new raster is selected; the cross-tab revision is bumped (PS picks
    /// it up). Mirrors `poll_create_overlay_jobs`.
    fn poll_create_raster_jobs(&mut self, ctx: &egui::Context) -> bool {
        let recv_result = {
            let Some(state) = self.create_raster_state.as_ref() else {
                return false;
            };
            match state.rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    Some(Err("Создание растрового слоя прервано (ошибка канала).".to_string()))
                }
            }
        };
        let Some(recv_result) = recv_result else {
            return false;
        };
        self.create_raster_state = None;
        match recv_result {
            Ok(created) => {
                self.invalidate_raster_cache_for_page(created.page_idx);
                self.pending_select_raster_uid = Some((created.page_idx, created.uid));
                true
            }
            Err(err) => {
                self.set_create_error(ctx, err);
                true
            }
        }
    }

    /// Applies a scale/rotation edit from the image panel to a raster layer (by uid), persisting the
    /// transform. Rotation arrives in degrees (panel space) and is converted to radians.
    fn apply_raster_transform_edit(
        &mut self,
        page_idx: usize,
        uid: &str,
        user_scale: f32,
        rotation_deg: f32,
    ) {
        let Some(layer) = self
            .raster_layers_by_page
            .get_mut(&page_idx)
            .and_then(|ls| ls.iter_mut().find(|l| l.uid == uid))
        else {
            return;
        };
        layer.transform.scale = user_scale.clamp(0.05, 20.0);
        layer.transform.rotation = rotation_deg.to_radians();
        let transform = layer.transform;
        self.persist_raster_transform(page_idx, uid, transform);
    }

    /// Applies an effects edit (non-destructive) from the image panel to a raster: updates the
    /// transform, then spawns a worker that renders the effects chain from the ORIGINAL base image.
    /// `poll_raster_effects_jobs` writes the rendered PNG (or clears it) and persists the chain via
    /// `update_raster_effects`, leaving the base untouched so the effects stay reversible.
    fn apply_raster_effects_edit(
        &mut self,
        page_idx: usize,
        uid: &str,
        render_data_json: &Value,
        user_scale: f32,
        rotation_deg: f32,
    ) {
        self.apply_raster_transform_edit(page_idx, uid, user_scale, rotation_deg);
        if self.raster_effects_state.is_some() {
            // A render is already in flight: stash the latest request (superseding any older
            // pending one) so `poll_raster_effects_jobs` reapplies it once the current render
            // finishes. Otherwise this edit would be silently lost — e.g. effecting a second
            // raster right after a first, leaving the second without its effects on save.
            self.pending_raster_effects = Some((
                page_idx,
                uid.to_string(),
                render_data_json.clone(),
                user_scale,
                rotation_deg,
            ));
            return;
        }
        let effects: Vec<Value> = render_data_json
            .get("effects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(layer) = self
            .raster_layers_by_page
            .get(&page_idx)
            .and_then(|ls| ls.iter().find(|l| l.uid == uid))
        else {
            return;
        };
        let base_file = layer.base_file.clone();
        let uid = uid.to_string();
        let primary = self.layers_primary_dir.clone();
        let fallback = self.layers_fallback_dir.clone();
        // Prefer the resident doc's in-memory base pixels: a freshly created raster (e.g. a cut)
        // may not be flushed to disk yet, so the disk-only load would fail. Clone the pixels and
        // drop the guard BEFORE spawning so the lock is never held across the worker thread.
        let base_in_memory: Option<ColorImage> = self
            .layer_doc
            .clone()
            .and_then(|doc| doc.lock().ok().and_then(|g| g.raster_base_image(page_idx, &uid)));
        let (tx, rx) = mpsc::channel::<Result<TypingRasterEffectsResult, String>>();
        thread::spawn(move || {
            let _ = tx.send(render_raster_effects(
                page_idx,
                uid,
                base_file,
                primary,
                fallback,
                effects,
                base_in_memory,
            ));
        });
        self.raster_effects_state = Some(rx);
    }

    /// Polls the non-destructive raster-effects worker: swaps the cached display image, persists the
    /// chain (`update_raster_effects` — base untouched), and bumps the cross-tab revision.
    fn poll_raster_effects_jobs(&mut self, ctx: &egui::Context) -> bool {
        let recv = {
            let Some(rx) = self.raster_effects_state.as_ref() else {
                return false;
            };
            match rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    Some(Err("Эффекты растра прерваны (ошибка канала).".to_string()))
                }
            }
        };
        let Some(recv) = recv else {
            return false;
        };
        self.raster_effects_state = None;
        let result = match recv {
            Ok(r) => r,
            Err(err) => {
                self.set_create_error(ctx, err);
                return true;
            }
        };

        // Route the rendered display image + effects chain to the shared doc (the source of truth),
        // then re-project. `set_effects` bumps the node generation, so `sync_from_doc` re-uploads
        // the raster texture. Falls back to mutating the local cache directly if no doc is wired.
        let routed = {
            let page = result.page_idx;
            let uid = result.uid.clone();
            let effects = result.effects.clone();
            let display = result.display_image.clone();
            self.route_to_doc(page, move |doc| doc.set_effects(page, &uid, effects, display))
        };
        if !routed
            && let Some(layer) = self
                .raster_layers_by_page
                .get_mut(&result.page_idx)
                .and_then(|ls| ls.iter_mut().find(|l| l.uid == result.uid))
        {
            layer.image = result.display_image.clone();
            layer.effects = result.effects.clone();
            layer.texture = None; // size may have changed → re-upload on next draw
        }
        if let Some(dir) = self.layers_primary_dir.clone() {
            let rendered = if result.effects.is_empty() {
                None
            } else {
                Some(&result.display_image)
            };
            let fallback = self.layers_fallback_dir.clone();
            // ASYNC: route the effects persist through the doc's effects-only saver path (targeted
            // single-raster RMW, PNG encode off-thread). Falls back to a direct synchronous
            // `update_raster_effects` when no doc/saver is wired. The save-to-project / app-close
            // barriers guarantee the enqueued effects land before merge/exit.
            let effects_persist = self
                .layer_doc
                .as_ref()
                .and_then(|doc| {
                    doc.lock().ok().map(|guard| {
                        guard.enqueue_raster_effects(
                            result.page_idx,
                            &dir,
                            fallback.as_deref(),
                            &result.uid,
                            &result.effects,
                            rendered,
                        )
                    })
                })
                .unwrap_or_else(|| {
                    crate::models::layer_model::persist::update_raster_effects(
                        &dir,
                        result.page_idx,
                        &result.uid,
                        &result.effects,
                        rendered,
                        fallback.as_deref(),
                    )
                });
            if let Err(err) = effects_persist {
                crate::runtime_log::log_warn(format!("[typing] persist raster effects: {err}"));
            }
        }
        // Reapply an edit that arrived while this render was in flight, so the last requested
        // effects (e.g. on a second raster) are not lost. `raster_effects_state` is now `None`,
        // so this spawns a fresh render instead of re-stashing.
        if let Some((page_idx, uid, render_data_json, user_scale, rotation_deg)) =
            self.pending_raster_effects.take()
        {
            self.apply_raster_effects_edit(
                page_idx,
                &uid,
                &render_data_json,
                user_scale,
                rotation_deg,
            );
        }
        true
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
                crate::trace_log!(
                    cat::SYNC,
                    "edit_overlay_render result=ok token={} overlay_idx={} image_effects={} size={}x{} warnings={}",
                    result.token,
                    result.overlay_idx,
                    result.is_image_effects,
                    result.size_px[0],
                    result.size_px[1],
                    result.warnings.len()
                );
                if !result.warnings.is_empty() {
                    self.set_create_warning(ctx, result.warnings.join("; "));
                }
                repainted |= self.apply_edit_overlay_render_result(result);
            }
            Ok(Ok(None)) => {
                crate::trace_log!(cat::SYNC, "edit_overlay_render result=none (skipped/cancelled)");
            }
            Ok(Err(err)) | Err(err) => {
                crate::trace_log!(cat::SYNC, "edit_overlay_render result=err err={}", err);
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
            if result.is_image_effects {
                // У image-эффектов имя показываемого файла может смениться (исходник <-> `_fx`),
                // поэтому идентичность оверлея проверяем по виду, а не по имени файла.
                if overlay.kind != TypingOverlayKind::Image {
                    return false;
                }
                overlay.file_name = result.file_name;
                overlay.original_file_name = result.image_original_file_name;
            } else if overlay.file_name != result.file_name {
                return false;
            }

            overlay.user_scale = result.user_scale.clamp(0.05, 20.0);
            overlay.angle_deg = normalize_angle_deg(result.rotation_deg);
            overlay.render_data_json = Some(result.render_data_json.clone());
            overlay.size_px = result.size_px;
            overlay.source_rgba = result.rgba.clone();
        }
        // Route a TEXT overlay's re-render (render_data + rendered image) to the shared doc, the
        // source of truth for text MODEL state, then re-project. Image-effect overlays are not doc
        // nodes (they are local image overlays), so they keep only the local runtime mutation above.
        if !result.is_image_effects
            && let Some(overlay) = self.overlays.get(result.overlay_idx)
            && overlay.kind == TypingOverlayKind::Text
            && overlay.size_px[0] > 0
            && overlay.size_px[1] > 0
            && result.rgba.len() == result.size_px[0] * result.size_px[1] * 4
        {
            let page_idx = overlay.page_idx;
            let uid = overlay.uid.clone();
            let render_data = result.render_data_json.clone();
            let image =
                ColorImage::from_rgba_unmultiplied(result.size_px, result.rgba.as_slice());
            self.route_to_doc(page_idx, move |doc| {
                doc.set_text_render(page_idx, &uid, render_data, image);
            });
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
        match request {
            TypingOverlayEditRequest::ImageTransform {
                target,
                user_scale,
                rotation_deg,
            } => match target {
                TypingEditTarget::Raster { page_idx, uid } => {
                    self.apply_raster_transform_edit(page_idx, &uid, user_scale, rotation_deg);
                }
                TypingEditTarget::Overlay(overlay_idx) => {
                    if self.selected_overlay_idx != Some(overlay_idx) {
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
            },
            TypingOverlayEditRequest::ImageEffects {
                target,
                render_data_json,
                user_scale,
                rotation_deg,
            } => match target {
                TypingEditTarget::Raster { page_idx, uid } => {
                    self.apply_raster_effects_edit(
                        page_idx,
                        &uid,
                        &render_data_json,
                        user_scale,
                        rotation_deg,
                    );
                }
                TypingEditTarget::Overlay(overlay_idx) => {
                    // Re-render пишет в неподтверждённую staging-папку.
                    let Some(text_images_dir) = self.text_images_save_dir.clone() else {
                        self.set_create_error(
                            ctx,
                            "Не найдена папка text_images для редактирования картинки.",
                        );
                        return;
                    };
                    if self.selected_overlay_idx != Some(overlay_idx) {
                        return;
                    }
                    let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
                        return;
                    };
                    if overlay.kind != TypingOverlayKind::Image {
                        return;
                    }
                    overlay.user_scale = user_scale.clamp(0.05, 20.0);
                    overlay.angle_deg = normalize_angle_deg(rotation_deg);

                    let edit_request = TypingEditImageEffectsRequest {
                        token: 0,
                        latest_token: Arc::clone(&self.edit_render_latest_token),
                        overlay_idx,
                        file_name: overlay.file_name.clone(),
                        original_file_name: overlay.original_file_name.clone(),
                        text_images_dir,
                        fallback_text_images_dir: self.text_images_fallback_dir.clone(),
                        user_scale: overlay.user_scale,
                        rotation_deg: overlay.angle_deg,
                        render_data_json,
                    };

                    self.start_edit_image_effects_render_job(edit_request);
                }
            },
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
                if self.selected_overlay_idx != Some(overlay_idx) {
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
        let preempted = self.edit_render_rx.is_some();
        crate::trace_log!(
            cat::SYNC,
            "edit_overlay_render dispatch kind=text token={} overlay_idx={} scale={:.3} rot={:.1} preempted_prev={}",
            request.token,
            request.overlay_idx,
            request.user_scale,
            request.rotation_deg,
            preempted
        );
        let (tx, rx) = mpsc::channel::<Result<Option<TypingEditOverlayResult>, String>>();
        thread::spawn(move || {
            let result = render_and_store_edited_overlay(request);
            let _ = tx.send(result);
        });
        self.edit_render_rx = Some(rx);
    }

    fn start_edit_image_effects_render_job(&mut self, mut request: TypingEditImageEffectsRequest) {
        request.token = self.next_edit_render_token();
        let preempted = self.edit_render_rx.is_some();
        crate::trace_log!(
            cat::SYNC,
            "edit_overlay_render dispatch kind=image_effects token={} overlay_idx={} scale={:.3} rot={:.1} preempted_prev={}",
            request.token,
            request.overlay_idx,
            request.user_scale,
            request.rotation_deg,
            preempted
        );
        let (tx, rx) = mpsc::channel::<Result<Option<TypingEditOverlayResult>, String>>();
        thread::spawn(move || {
            let result = render_and_store_image_effects_overlay(request);
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
            Ok(Ok(())) => {
                crate::trace_log!(cat::PERSIST, "overlay_placement_save result=ok");
                // Our own overlay write to `layers.json` / PNGs completed. The MODEL change already
                // routed through the shared doc (bumping its version, so the PS tab re-projects); this
                // job only persisted it to disk, so there is nothing more to signal cross-tab.
            }
            Ok(Err(err)) | Err(err) => {
                crate::trace_log!(cat::PERSIST, "overlay_placement_save result=err err={}", err);
                self.set_create_error(ctx, err);
            }
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
                            crate::trace_log!(
                                cat::PERSIST,
                                "export result=ok exported={} total={}",
                                result.exported,
                                result.total
                            );
                            self.create_status_error = None;
                            self.export_status = TypingExportUiStatus::Success {
                                done: result.exported,
                                total: result.total,
                            };
                            let _ = result.output_dir;
                        }
                        Err(err) => {
                            crate::trace_log!(cat::PERSIST, "export result=err err={}", err);
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
        export_format: TypingExportFormat,
    ) {
        if self.export_rx.is_some() {
            self.set_create_error(ctx, "Экспорт уже выполняется.");
            return;
        }
        if project.pages.is_empty() {
            self.set_create_error(ctx, "В проекте нет страниц для экспорта.");
            return;
        }
        crate::trace_log!(
            cat::PERSIST,
            "export dispatch pages={} format={:?} output_dir={}",
            project.pages.len(),
            export_format,
            output_dir.display()
        );
        let clean_overlays_model = self.clean_overlays_model.clone();

        let mut overlays_by_page = HashMap::<usize, Vec<TypingExportOverlaySnapshot>>::new();
        for overlay in &self.overlays {
            if overlay.size_px[0] == 0 || overlay.size_px[1] == 0 {
                continue;
            }
            if overlay.source_rgba.len() != overlay.size_px[0] * overlay.size_px[1] * 4 {
                continue;
            }
            let band_z = self.overlay_band_z(overlay.page_idx, &overlay.uid, overlay.layer_idx);
            overlays_by_page.entry(overlay.page_idx).or_default().push(
                TypingExportOverlaySnapshot {
                    page_idx: overlay.page_idx,
                    center_page_px: overlay.center_page_px,
                    mask_clip_enabled: overlay.mask_clip_enabled,
                    layer_idx: overlay.layer_idx,
                    user_scale: overlay.user_scale,
                    angle_deg: overlay.angle_deg,
                    deform_mesh: overlay.deform_mesh.clone(),
                    size_px: overlay.size_px,
                    source_rgba: overlay.source_rgba.clone(),
                    render_data_json: overlay.render_data_json.clone(),
                    uid: overlay.uid.clone(),
                    band_z,
                },
            );
        }

        // Bottom-to-top by the UNIFIED manual band-Z (same as the on-screen draw order), so the export
        // stacks text exactly as shown. (Was the old layer_idx + page-Y auto-order.)
        for (page, overlays) in overlays_by_page.iter_mut() {
            overlays.sort_by_key(|o| self.overlay_band_z(*page, &o.uid, o.layer_idx));
        }

        // Snapshot the on-screen PS raster layers PER PAGE from the doc projection, so the export
        // composites EXACTLY what the canvas shows (post-effects display image, in-session transform /
        // deform, band-Z) rather than re-reading `layers.json` from disk — which silently dropped rasters
        // for the user (missing `_fx.png`, unflushed staging, etc.). `ensure_raster_layers_for_page` is
        // lazy (only visited pages are projected), so project every export page first.
        // Projecting every page (`ensure_raster_layers_for_page`) resolves `pending_select_raster_uid`
        // and would mutate the user's current selection. Triggering an export must NOT change selection,
        // so snapshot and restore it around the projection loop.
        let saved_selected_raster = self.selected_raster_idx;
        let saved_selected_overlay = self.selected_overlay_idx;
        let saved_pending_select = self.pending_select_raster_uid.clone();

        let mut rasters_by_page = HashMap::<usize, Vec<TypingExportRasterSnapshot>>::new();
        for page in &project.pages {
            self.ensure_raster_layers_for_page(page.idx);
            let Some(layers) = self.raster_layers_by_page.get(&page.idx) else {
                continue;
            };
            if layers.is_empty() {
                continue;
            }
            let snaps: Vec<TypingExportRasterSnapshot> = layers
                .iter()
                .map(|l| TypingExportRasterSnapshot {
                    visible: l.visible,
                    opacity: l.opacity,
                    transform: l.transform,
                    deform: l.deform.clone(),
                    rgba: color_image_to_rgba(&l.image),
                    size_px: l.image.size,
                    band_z: self.raster_band_z(page.idx, &l.uid),
                    mask_clip_enabled: l.mask_clip_enabled,
                })
                .collect();
            rasters_by_page.insert(page.idx, snaps);
        }

        // Restore the selection the projection loop may have changed (export is side-effect-free).
        self.selected_raster_idx = saved_selected_raster;
        self.selected_overlay_idx = saved_selected_overlay;
        self.pending_select_raster_uid = saved_pending_select;

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
                let out_ext = match export_format {
                    TypingExportFormat::Png => "png",
                    TypingExportFormat::Psd => "psd",
                };
                let out_name = format!("{stem}.{out_ext}");
                TypingExportPageJob {
                    page_idx: page.idx,
                    page_path: page.path.clone(),
                    output_path: output_dir.join(out_name),
                    clean_overlay_path: clean_overlay_path.is_file().then_some(clean_overlay_path),
                    clean_overlay_rgba: None,
                    overlays: overlays_by_page.remove(&page.idx).unwrap_or_default(),
                    rasters: rasters_by_page.remove(&page.idx).unwrap_or_default(),
                    mask: masks_snapshot.get(&page.idx).cloned(),
                    export_format,
                    layers_primary_dir: self.layers_primary_dir.clone(),
                    layers_fallback_dir: self.layers_fallback_dir.clone(),
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
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
            || self.edit_render_rx.is_some()
        {
            self.save_requested_while_busy = true;
            return;
        }
        self.spawn_overlay_placement_save();
    }

    /// Syncs every text overlay's full MODEL state (geometry + deform + grouping + mask_clip) from the
    /// local runtimes into its shared-doc Text node, grouped per page so each resident page is synced
    /// once. Returns the set of pages that carry text (callers flush exactly those). The render image +
    /// `render_data` are pushed into the doc by `set_text_render` at render time, so this only needs to
    /// reconcile the placement/grouping fields that drag/group edits change.
    fn sync_overlay_state_into_doc(&mut self) -> std::collections::BTreeSet<usize> {
        let mut pages_with_text: std::collections::BTreeSet<usize> =
            std::collections::BTreeSet::new();
        #[allow(clippy::type_complexity)]
        let mut state_by_page: HashMap<
            usize,
            Vec<(
                String,
                crate::models::layer_model::manifest::TransformRec,
                Option<crate::models::layer_model::manifest::DeformRec>,
                Option<u32>,
                Option<bool>,
            )>,
        > = HashMap::new();
        for overlay in &self.overlays {
            if overlay.kind != TypingOverlayKind::Text {
                continue;
            }
            pages_with_text.insert(overlay.page_idx);
            let transform = crate::models::layer_model::manifest::TransformRec {
                cx: overlay.center_page_px[0],
                cy: overlay.center_page_px[1],
                rotation: overlay.angle_deg.to_radians(),
                scale: overlay.user_scale,
            };
            let deform = overlay.deform_mesh.as_ref().map(|m| {
                crate::models::layer_model::manifest::DeformRec {
                    cols: m.cols,
                    rows: m.rows,
                    points_px: m.points_px.clone(),
                }
            });
            state_by_page.entry(overlay.page_idx).or_default().push((
                overlay.uid.clone(),
                transform,
                deform,
                u32::try_from(overlay.layer_idx).ok(),
                Some(overlay.mask_clip_enabled),
            ));
        }
        for (page_idx, states) in state_by_page {
            self.route_to_doc(page_idx, |doc| {
                for (uid, transform, deform, layer_idx, mask_clip) in &states {
                    doc.set_transform(page_idx, uid, *transform);
                    if let Some(node) = doc.node_mut(page_idx, uid) {
                        node.deform = deform.clone();
                        node.text_layer_idx = *layer_idx;
                        if let crate::models::layer_model::layer_doc::NodeBody::Text {
                            mask_clip: mc,
                            ..
                        } = &mut node.body
                        {
                            *mc = *mask_clip;
                        }
                    }
                }
            });
        }
        pages_with_text
    }

    /// Synchronously flushes ONE page's CURRENT doc text to the staging `layers/` dir. Used right before
    /// creating a raster on `page_idx` so the staged page reflects the doc (including a deleted-last-text
    /// page → present-but-empty), preventing `add_page_raster`/`ensure_page_staged` from re-seeding stale
    /// committed text. A no-op if no doc/dir is wired or the page is not resident. The page is flushed
    /// even when it has zero text — `write_page_text_payload` keeps a previously-existing page
    /// present-but-empty, making the deletion durable. (`flush_page_text` for an empty page does no PNG
    /// IO, so this is cheap on the UI thread.)
    fn flush_target_page_text_to_staging(&mut self, page_idx: usize) {
        let Some(layers_dir) = self.layers_primary_dir.clone() else {
            return;
        };
        let fallback_dir = self.layers_fallback_dir.clone();
        // Reconcile the local overlay placement into the doc first (so the flush writes current state),
        // then flush only the target page.
        self.sync_overlay_state_into_doc();
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        let Ok(mut guard) = doc.lock() else {
            return;
        };
        // INTENTIONALLY SYNCHRONOUS (not enqueued): the caller spawns a worker that immediately reads
        // this page's on-disk staging `layers.json` via `add_page_raster`. The anti-resurrection
        // contract requires the page to be PRESENT (possibly empty) on disk BEFORE that read. An async
        // enqueue would race the worker (it could read stale committed text before the enqueued write
        // lands), resurrecting a deleted-last-text overlay. We cannot barrier on the GUI thread, and
        // the empty-page case does no PNG IO, so a direct synchronous flush is both correct and cheap.
        if let Err(err) = guard.flush_page_text(page_idx, &layers_dir, fallback_dir.as_deref()) {
            crate::runtime_log::log_warn(format!(
                "[typing] flush target page {page_idx} text before raster create: {err}"
            ));
        }
    }

    fn spawn_overlay_placement_save(&mut self) {
        // Text persistence is now owned by the shared doc: route each text overlay's MODEL state into
        // the doc, then flush the doc's INLINE v3 text payload to `layers.json` (staging `layers/`
        // dir). Nothing writes `text_info.json` anymore — the doc is the sole text writer, mirroring
        // how rasters persist.
        let Some(layers_dir) = self.layers_primary_dir.clone() else {
            return;
        };
        let fallback_dir = self.layers_fallback_dir.clone();
        let pages_with_text = self.sync_overlay_state_into_doc();

        // Flush the doc's text payload on a worker thread (PNG re-encode is off the UI thread). The
        // doc lock is shared via the Arc; `flush_page_text` writes only text nodes, leaving rasters
        // untouched on disk.
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        let pages: Vec<usize> = pages_with_text.into_iter().collect();
        crate::trace_log!(
            cat::PERSIST,
            "spawn_overlay_placement_save pages={:?}",
            pages
        );
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        thread::spawn(move || {
            let result = (|| {
                let mut guard = doc.lock().map_err(|_| "doc lock poisoned".to_string())?;
                for page_idx in pages {
                    // ASYNC: enqueue to the coalescing saver (falls back to sync flush when no saver).
                    // This is the placement autosave; no reader depends on it landing synchronously,
                    // and the save-to-project / app-close barriers guarantee durability.
                    guard.enqueue_page_text_save(page_idx, &layers_dir, fallback_dir.as_deref())?;
                }
                Ok(())
            })();
            let _ = tx.send(result);
        });
        self.save_rx = Some(rx);
    }

    /// Synchronously flushes text into the staging `layers/` dir for EVERY page the shared doc has
    /// resident (not just pages with typing-tab overlays), so staging is text-complete for every page
    /// the session loaded — including deletions and pages only PS visited (which load text into the doc
    /// too). Returns the set of OWNED text pages (the doc-resident pages flushed): the save-to-project
    /// merge replaces those pages wholesale (authoritative, incl. deletions) and PRESERVES committed
    /// text for pages NOT in this set (never loaded this session → the session doesn't own their text,
    /// so a raster-only PS edit must not drop their committed text). Mirrors the PS editor's
    /// `flush_layers`; best-effort per page.
    pub fn flush_text_layers(&mut self) -> std::collections::HashSet<usize> {
        let _persist_span = crate::trace_scope!(cat::PERSIST, "flush_text_layers");
        let mut owned: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let Some(layers_dir) = self.layers_primary_dir.clone() else {
            return owned;
        };
        let fallback_dir = self.layers_fallback_dir.clone();
        // Push the live overlay MODEL state into the doc first (geometry/group/mask-clip edits), then
        // flush EVERY resident doc page's text — not only pages with overlays loaded in this tab.
        self.sync_overlay_state_into_doc();
        let Some(doc) = self.layer_doc.clone() else {
            return owned;
        };
        let Ok(mut guard) = doc.lock() else {
            return owned;
        };
        for page_idx in guard.resident_pages() {
            // ASYNC: enqueue each resident page's text to the coalescing saver (PNG encode off the GUI
            // thread). The save-to-project merge worker barriers the saver BEFORE reading the staging
            // `layers.json`, so every enqueued page is on disk before the merge — the FIFO channel +
            // barrier give the same ordering the old synchronous flush did. A page is marked OWNED on a
            // successful ENQUEUE (the barrier guarantees the write); an enqueue failure leaves it
            // unowned (fail-safe), so the merge preserves committed text rather than dropping it. With
            // no saver, `enqueue_page_text_save` falls back to a synchronous flush, also correct.
            match guard.enqueue_page_text_save(page_idx, &layers_dir, fallback_dir.as_deref()) {
                Ok(()) => {
                    owned.insert(page_idx);
                }
                Err(err) => crate::runtime_log::log_warn(format!(
                    "[typing] flush text page {page_idx} to layers.json failed: {err}"
                )),
            }
        }
        crate::trace_log!(cat::PERSIST, "flush_text_layers owned_pages={}", owned.len());
        owned
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
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
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
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
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
        let seed_text =
            pick_bubble_text_for_selection(&project.bubbles, page_idx, scene_rect, page_rect)
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
            text_images_dir: project.paths.unsaved_layers_dir.clone(),
            page_idx: editor.page_idx,
            center_page_px: editor.center_page_px,
            render_params,
            render_data_json,
        };
        crate::trace_log!(
            cat::SYNC,
            "create_overlay_render dispatch page={} center=({:.1},{:.1}) width_px={}",
            editor.page_idx,
            editor.center_page_px[0],
            editor.center_page_px[1],
            editor.width_px
        );
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
        if self.loading_rx.is_some()
            || self.create_render_state.is_some()
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
            || self.create_raster_state.is_some()
        {
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
        // DATA-SAFETY (anti-resurrection): the worker's `add_page_raster` seeds an unstaged page from the
        // COMMITTED manifest (so a typeset page keeps its text — the drop fix). But committed is STALE
        // w.r.t. an in-session deletion: when the user deleted the page's LAST text, the placement-save
        // skipped the now-empty page (`pages_with_text` no longer lists it), so the deletion lived only
        // in the doc. Seeding committed would RESURRECT it. Fix: flush the target page's CURRENT doc text
        // to staging NOW (main thread, has the doc) — for a deleted-last-text page this writes it
        // PRESENT-but-EMPTY, so `ensure_page_staged` sees the page present and does NOT seed stale text;
        // for a typeset page it writes the current text, which the new raster is then added on top of.
        self.flush_target_page_text_to_staging(target_page_idx);

        // External images now become RASTER layers (in layers.json), not text/image overlays, so
        // they are first-class in both the typing and PS editor tabs.
        let create_request = TypingCreateRasterRequest {
            layers_dir: project.paths.unsaved_layers_dir.clone(),
            fallback_dir: Some(project.paths.layers_dir.clone()),
            page_idx: target_page_idx,
            center_page_px,
            source,
        };
        let (tx, rx) = mpsc::channel::<Result<TypingCreatedRaster, String>>();
        thread::spawn(move || {
            let _ = tx.send(render_and_store_created_raster(create_request));
        });
        self.create_raster_state = Some(TypingCreateRasterState { rx });
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
        // Build the doc Text node for a TEXT overlay (the doc is the source of truth, so it joins the
        // unified Z stack and re-projects like the rest). Image overlays remain local-only → no node.
        //
        // CRITICAL ordering: build the node here, but ADD it to the doc only AFTER the runtime is pushed
        // into `self.overlays` (below). `route_to_doc` reprojects via `sync_from_doc`, whose CREATE/None
        // branch MATERIALIZES a runtime for any doc Text node that has no matching local runtime yet. If
        // we added the node before pushing the runtime, that branch would create a SECOND runtime for the
        // same uid — a duplicate text layer (one doc-backed, one orphaned). The duplicate is invisible at
        // create time (both render the same image, perfectly overlapping) but becomes visible on the
        // first advanced-form apply: `sync_from_doc` reconciles only the FIRST uid match, leaving the
        // other stuck on the pre-form render.
        let pending_text_node = if decoded.kind == TypingOverlayKind::Text
            && decoded.size_px[0] > 0
            && decoded.size_px[1] > 0
            && decoded.rgba.len() == decoded.size_px[0] * decoded.size_px[1] * 4
        {
            use crate::models::layer_model::layer_doc::{LayerNode, NodeBody, NodeKind};
            let page_idx = decoded.page_idx;
            let uid = decoded.uid.clone();
            let name = decoded
                .render_data_json
                .as_ref()
                .and_then(|v| v.get("text"))
                .and_then(Value::as_str)
                .map(|s| s.chars().take(40).collect::<String>())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "Текст".to_string());
            let transform = crate::models::layer_model::manifest::TransformRec {
                cx: decoded.center_page_px[0],
                cy: decoded.center_page_px[1],
                rotation: decoded.angle_deg.to_radians(),
                scale: decoded.user_scale,
            };
            let deform = decoded.deform_mesh.as_ref().map(|m| {
                crate::models::layer_model::manifest::DeformRec {
                    cols: m.cols,
                    rows: m.rows,
                    points_px: m.points_px.clone(),
                }
            });
            let image =
                ColorImage::from_rgba_unmultiplied(decoded.size_px, decoded.rgba.as_slice());
            let render_data = decoded.render_data_json.clone().unwrap_or(Value::Null);
            let node = LayerNode {
                uid: uid.clone(),
                name,
                kind: NodeKind::Text,
                z: 0, // set on top by add_node
                visible: true,
                opacity: 1.0,
                group_uid: None,
                // The typing tab's «Группа текста N» axis — carried so the doc flush persists it.
                text_layer_idx: u32::try_from(decoded.layer_idx).ok(),
                transform,
                deform,
                generation: 0,
                // A freshly rendered overlay: mark dirty so the doc flush writes its rendered PNG.
                pixels_dirty: true,
                body: NodeBody::Text {
                    render_data,
                    image,
                    payload_uid: uid,
                    // Carry the overlay's mask-clip flag so the v3 inline payload persists it.
                    mask_clip: Some(decoded.mask_clip_enabled),
                },
            };
            Some((page_idx, node))
        } else {
            None
        };
        self.overlays.push(TypingOverlayRuntime {
            uid: decoded.uid,
            kind: decoded.kind,
            page_idx: decoded.page_idx,
            center_page_px: decoded.center_page_px,
            mask_clip_enabled: decoded.mask_clip_enabled,
            layer_idx: decoded.layer_idx,
            user_scale: decoded.user_scale,
            angle_deg: decoded.angle_deg,
            deform_mesh: decoded.deform_mesh,
            file_name: decoded.file_name,
            original_file_name: decoded.original_file_name,
            render_data_json: decoded.render_data_json,
            size_px: decoded.size_px,
            source_rgba: decoded.rgba,
            texture: None,
            display_texture_stale: true,
            last_texture_used_frame: 0,
        });
        // Now that the runtime is in `self.overlays`, add the doc node. `route_to_doc`'s reproject finds
        // the runtime by uid and RECONCILES it (no duplicate materialized). See the ordering note above.
        if let Some((page_idx, node)) = pending_text_node {
            self.route_to_doc(page_idx, move |doc| {
                doc.add_node(page_idx, node);
            });
        }
        self.queue_overlay_texture_upload(idx);
        self.selected_overlay_idx = Some(idx);
        self.transform_mode_overlay_idx = None;
        self.drag_state = None;
    }

    /// Computes the mask-clipped DISPLAY image for every mask-clip-enabled raster whose clipped image
    /// is not yet cached, and drops its GPU texture so `draw_one_raster_layer` re-uploads the clipped
    /// version. Runs before the overlay upload (which already has the mask layer). Mirrors the overlay
    /// clip path (`clip_overlay_rgba_if_needed` with the layer's deform mesh as page-relative UV; an
    /// affine raster uses an identity quad mesh derived from its transform).
    fn prepare_raster_mask_clips(&mut self, mask_layer: &TypingMaskLayer) {
        let pages: Vec<usize> = self.raster_layers_by_page.keys().copied().collect();
        for page_idx in pages {
            let Some(page_size) = mask_layer.page_mask_size(page_idx) else {
                continue;
            };
            let Some(layers) = self.raster_layers_by_page.get_mut(&page_idx) else {
                continue;
            };
            for layer in layers.iter_mut() {
                if !layer.mask_clip_enabled {
                    layer.clipped_image = None;
                    continue;
                }
                if layer.clipped_image.is_some() {
                    continue; // already computed for this generation
                }
                let [w, h] = layer.image.size;
                if w == 0 || h == 0 {
                    continue;
                }
                // Deform mesh in page-relative UV (the raster's mesh, or an identity quad for affine).
                let mesh = match &layer.deform {
                    Some(rec) => TypingOverlayDeformMesh::from_deform_rec(rec, page_size),
                    None => Some(default_deform_mesh_for_page(
                        [layer.transform.cx, layer.transform.cy],
                        layer.image.size,
                        layer.transform.scale,
                        layer.transform.rotation.to_degrees(),
                        page_size,
                    )),
                };
                let Some(mesh) = mesh else { continue };
                let points_uv: Vec<[f32; 2]> = mesh
                    .points_px
                    .iter()
                    .map(|&p| page_px_to_uv(p, page_size))
                    .collect();
                let src_rgba = color_image_to_rgba(&layer.image);
                if let Some(clipped) = mask_layer.clip_overlay_rgba_if_needed(
                    page_idx,
                    [w, h],
                    &src_rgba,
                    mesh.cols,
                    mesh.rows,
                    &points_uv,
                ) {
                    layer.clipped_image =
                        Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &clipped));
                    // Force re-upload with the clipped pixels.
                    layer.texture = None;
                }
            }
        }
    }

    fn upload_pending_textures(
        &mut self,
        ctx: &egui::Context,
        mask_layer: &TypingMaskLayer,
    ) -> bool {
        self.prepare_raster_mask_clips(mask_layer);
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

        if uploaded_any {
            crate::trace_log!(
                cat::RENDER,
                "upload_overlay_textures count={} bytes={} pending_remaining={}",
                uploaded_textures,
                uploaded_bytes,
                self.pending_upload_indices.len()
            );
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
        if crate::trace::trace_enabled()
            && (self.selected_overlay_idx.is_some() || self.selected_raster_idx.is_some())
        {
            crate::trace_log!(
                cat::TYPING,
                "clear_selection overlay_idx={:?} raster_idx={:?}",
                self.selected_overlay_idx,
                self.selected_raster_idx
            );
        }
        self.selected_overlay_idx = None;
        self.transform_mode_overlay_idx = None;
        self.drag_state = None;
        self.drag_has_changes = false;
        self.shape_variant_preview = None;
        self.selected_raster_idx = None;
        self.transform_mode_raster_idx = None;
        self.raster_drag_state = None;
        self.raster_drag_has_changes = false;
    }

    /// Selects a raster layer for the current page, clearing any overlay selection (one selection at
    /// a time across the two layer kinds). Selecting a DIFFERENT raster exits raster transform mode.
    fn select_raster(&mut self, raster_idx: usize) {
        if self.selected_raster_idx != Some(raster_idx) {
            crate::trace_log!(cat::TYPING, "select_raster raster_idx={}", raster_idx);
        }
        if self.transform_mode_raster_idx != Some(raster_idx) {
            self.transform_mode_raster_idx = None;
        }
        self.selected_raster_idx = Some(raster_idx);
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
            target: TypingEditTarget::Overlay(overlay_idx),
        })
    }

    /// The edit-panel payload for the current selection: a text/image overlay, or — when a raster is
    /// selected — the raster, shown with the same image UI (scale + rotation + effects, no text).
    fn selected_item_for_edit(&self, page_idx: usize) -> Option<TypingSelectedOverlayForEdit> {
        if self.selected_overlay_idx.is_some() {
            return self.selected_overlay_for_edit();
        }
        let raster_idx = self.selected_raster_idx?;
        let raster = self.raster_layers_by_page.get(&page_idx)?.get(raster_idx)?;
        Some(TypingSelectedOverlayForEdit {
            overlay_idx: 0, // unused for a raster target
            overlay_kind: TypingOverlayKind::Image,
            render_data_json: Some(serde_json::json!({ "effects": raster.effects.clone() })),
            width_px_hint: raster.image.size[0] as u32,
            user_scale: raster.transform.scale,
            rotation_deg: raster.transform.rotation.to_degrees(),
            target: TypingEditTarget::Raster {
                page_idx,
                uid: raster.uid.clone(),
            },
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
        // Capture the doc-node identity (TEXT overlays only) before removing the runtime, so the
        // matching node can be dropped from the shared doc afterward.
        let doc_node = self
            .overlays
            .get(overlay_idx)
            .filter(|o| o.kind == TypingOverlayKind::Text)
            .map(|o| (o.page_idx, o.uid.clone()));
        if crate::trace::trace_enabled() {
            if let Some(o) = self.overlays.get(overlay_idx) {
                crate::trace_log!(
                    cat::TYPING,
                    "remove_overlay idx={} uid={} kind={:?} page={}",
                    overlay_idx,
                    o.uid,
                    o.kind,
                    o.page_idx
                );
            }
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
        // Drop the matching node from the shared doc (the source of truth), then re-project bands.
        if let Some((page_idx, uid)) = doc_node {
            self.route_to_doc(page_idx, move |doc| {
                doc.remove_node(page_idx, &uid);
            });
        }
        self.request_overlay_placement_save();
    }

    /// Removes a raster layer from the current page: drops the doc node (the source of truth), removes
    /// the cached projection, fixes `selected_raster_idx` / `transform_mode_raster_idx` / drag state,
    /// frees its texture, and persists. Mirrors `remove_overlay`.
    fn remove_raster(&mut self, page_idx: usize, raster_idx: usize) {
        let Some(uid) = self
            .raster_layers_by_page
            .get(&page_idx)
            .and_then(|v| v.get(raster_idx))
            .map(|l| l.uid.clone())
        else {
            return;
        };
        crate::trace_log!(
            cat::TYPING,
            "remove_raster page={} raster_idx={} uid={}",
            page_idx,
            raster_idx,
            uid
        );
        // Drop the node from the shared doc (its texture goes with the cached layer below).
        self.route_to_doc(page_idx, |doc| {
            doc.remove_node(page_idx, &uid);
        });
        // Remove the cached projection (its `texture` handle is freed on drop).
        if let Some(layers) = self.raster_layers_by_page.get_mut(&page_idx) {
            if raster_idx < layers.len() {
                layers.remove(raster_idx);
            }
        }
        self.raster_texture_generations
            .retain(|(p, u), _| !(*p == page_idx && *u == uid));
        // Fix the selection / transform-mode / drag indices (shift down past the removed one).
        shift_index_after_remove(&mut self.selected_raster_idx, raster_idx);
        shift_index_after_remove(&mut self.transform_mode_raster_idx, raster_idx);
        if let Some(mut state) = self.raster_drag_state.take() {
            if state.page_idx == page_idx && state.raster_idx == raster_idx {
                self.raster_drag_state = None;
                self.raster_drag_has_changes = false;
            } else {
                if state.page_idx == page_idx && state.raster_idx > raster_idx {
                    state.raster_idx -= 1;
                }
                self.raster_drag_state = Some(state);
            }
        }
        // Persist: flush the page, explicitly DROPPING the removed raster from the manifest (otherwise
        // `save_page_rasters` would preserve it as another tab's, and it would resurrect on disk).
        if let Some(primary) = self.layers_primary_dir.clone() {
            let fallback = self.layers_fallback_dir.clone();
            if let Some(doc) = self.layer_doc.clone()
                && let Ok(mut guard) = doc.lock()
                && let Err(err) =
                    guard.flush_page_dropping_raster(page_idx, &primary, fallback.as_deref(), &uid)
            {
                crate::runtime_log::log_warn(format!("[typing] persist raster delete: {err}"));
            }
        }
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

    /// Scale the selected raster with the `-` / `=` / `0` keys (parity with the overlay shortcut).
    fn try_scale_selected_raster_by_shortcuts(&mut self, ui: &mut egui::Ui, page_idx: usize) {
        if ui.ctx().wants_keyboard_input() {
            return;
        }
        let Some(idx) = self.selected_raster_idx else {
            return;
        };
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
        let Some(layer) = self
            .raster_layers_by_page
            .get_mut(&page_idx)
            .and_then(|v| v.get_mut(idx))
        else {
            return;
        };
        let prev = layer.transform.scale;
        if reset {
            layer.transform.scale = 1.0;
        } else if increase {
            layer.transform.scale = (layer.transform.scale * 1.1).clamp(0.05, 20.0);
        } else if decrease {
            layer.transform.scale = (layer.transform.scale / 1.1).clamp(0.05, 20.0);
        }
        if (layer.transform.scale - prev).abs() <= 1e-6 {
            return;
        }
        let (uid, transform) = (layer.uid.clone(), layer.transform);
        self.persist_raster_transform(page_idx, &uid, transform);
        ui.ctx().request_repaint();
    }

    /// Routes one raster's transform to the shared doc (the cross-tab source of truth) and persists
    /// it to the unsaved layers dir so it survives reloads / save-to-project.
    /// Ensures the raster at `raster_idx` has a deform mesh (seeding an identity grid from its current
    /// affine transform when it has none), so entering perspective transform mode has handles to drag.
    /// Returns the resulting mesh (resolution-normalized), or `None` if the raster is absent. Mirrors
    /// `ensure_overlay_deform_mesh`. Pure in-memory on the cached layer; persisted on drag-end.
    fn ensure_raster_deform_mesh(
        &mut self,
        page_idx: usize,
        raster_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) -> Option<TypingOverlayDeformMesh> {
        let page_size = page_size_from_image_rect(image_rect, zoom);
        let layer = self
            .raster_layers_by_page
            .get_mut(&page_idx)
            .and_then(|v| v.get_mut(raster_idx))?;
        let mesh = match &layer.deform {
            Some(rec) => {
                let m = TypingOverlayDeformMesh::from_deform_rec(rec, page_size)?;
                normalize_deform_mesh_resolution(&m, page_size)
            }
            None => {
                // Seed an identity grid covering the raster's current affine quad.
                let m = default_deform_mesh_for_page(
                    [layer.transform.cx, layer.transform.cy],
                    layer.image.size,
                    layer.transform.scale,
                    layer.transform.rotation.to_degrees(),
                    page_size,
                );
                layer.deform = Some(crate::models::layer_model::manifest::DeformRec {
                    cols: m.cols,
                    rows: m.rows,
                    points_px: m.points_px.clone(),
                });
                m
            }
        };
        Some(mesh)
    }

    fn persist_raster_transform(
        &mut self,
        page_idx: usize,
        uid: &str,
        transform: crate::models::layer_model::manifest::TransformRec,
    ) {
        let Some(dir) = self.layers_primary_dir.clone() else {
            return;
        };
        let fallback = self.layers_fallback_dir.clone();
        // Route the MODEL change to the shared doc: it bumps the doc version (so the PS tab
        // re-projects) and re-projects this tab's page.
        let uid_owned = uid.to_string();
        self.route_to_doc(page_idx, |doc| doc.set_transform(page_idx, &uid_owned, transform));
        // Persist to disk so the transform survives a reload / save-to-project.
        if let Err(err) = crate::models::layer_model::persist::update_raster_transform(
            &dir,
            page_idx,
            uid,
            transform,
            fallback.as_deref(),
        ) {
            crate::runtime_log::log_warn(format!("[typing] persist raster transform: {err}"));
        }
    }

    /// Flushes the doc page's RASTER nodes to disk (whole-page `save_page_rasters`), used after a
    /// raster mask-clip toggle (routed through the doc) so the flag survives a reload / save-to-project.
    /// `save_page_rasters` carries each raster's `mask_clip`. No-op if the doc/page is not resident.
    fn persist_current_page_rasters(&mut self, page_idx: usize) {
        let Some(primary) = self.layers_primary_dir.clone() else {
            return;
        };
        let fallback = self.layers_fallback_dir.clone();
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        let Ok(mut guard) = doc.lock() else {
            return;
        };
        if let Err(err) = guard.flush_page(page_idx, &primary, fallback.as_deref()) {
            crate::runtime_log::log_warn(format!("[typing] persist raster mask-clip: {err}"));
        }
    }

    /// Routes a raster's deform mesh (+ its affine transform) to the shared doc and persists both to
    /// disk. Used by the raster perspective transform mode and by "Сбросить трансформацию" (deform =
    /// None). The doc is the source of truth, so the PS tab re-projects via its version watch.
    fn persist_raster_deform(
        &mut self,
        page_idx: usize,
        uid: &str,
        transform: crate::models::layer_model::manifest::TransformRec,
        deform: Option<crate::models::layer_model::manifest::DeformRec>,
    ) {
        let Some(dir) = self.layers_primary_dir.clone() else {
            return;
        };
        let fallback = self.layers_fallback_dir.clone();
        let uid_owned = uid.to_string();
        let deform_for_doc = deform.clone();
        self.route_to_doc(page_idx, |doc| {
            doc.set_transform(page_idx, &uid_owned, transform);
            doc.set_deform(page_idx, &uid_owned, deform_for_doc);
        });
        if let Err(err) = crate::models::layer_model::persist::update_raster_geometry(
            &dir,
            page_idx,
            uid,
            transform,
            deform,
            fallback.as_deref(),
        ) {
            crate::runtime_log::log_warn(format!("[typing] persist raster deform: {err}"));
        }
    }

    /// Canvas select + move/rotate drag for raster layers (parity with overlays). Runs after the
    /// overlay interaction so overlays win pointer ties; draws the selection decoration. The raster
    /// pixels themselves are drawn in the unified merged-fill pass.
    fn interact_page_rasters(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        painter: &egui::Painter,
    ) {
        let count = self
            .raster_layers_by_page
            .get(&page_idx)
            .map_or(0, |v| v.len());
        if self.selected_raster_idx.is_some_and(|i| i >= count) {
            self.selected_raster_idx = None;
        }
        if self.transform_mode_raster_idx.is_some_and(|i| i >= count) {
            self.transform_mode_raster_idx = None;
        }
        if self
            .raster_drag_state
            .as_ref()
            .is_some_and(|s| s.page_idx != page_idx || s.raster_idx >= count)
        {
            self.raster_drag_state = None;
            self.raster_drag_has_changes = false;
        }

        // Drag-end: persist the final geometry (transform, and the mesh for a perspective edit).
        let primary_down = ui.input(|i| i.pointer.primary_down());
        if !primary_down
            && let Some(state) = self.raster_drag_state.take()
        {
            if self.raster_drag_has_changes
                && let Some(layer) = self
                    .raster_layers_by_page
                    .get(&state.page_idx)
                    .and_then(|v| v.get(state.raster_idx))
            {
                let (uid, transform, deform) =
                    (layer.uid.clone(), layer.transform, layer.deform.clone());
                if matches!(state.mode, TypingRasterDragMode::PerspectiveHandle(_)) {
                    self.persist_raster_deform(state.page_idx, &uid, transform, deform);
                } else {
                    self.persist_raster_transform(state.page_idx, &uid, transform);
                }
            }
            self.raster_drag_has_changes = false;
        }
        if count == 0 {
            return;
        }

        // Deferred menu actions (set inside the menu closure, applied after this method).
        let mut menu_enter_transform: Option<usize> = None;
        let mut menu_exit_transform = false;
        let mut menu_reset_transform: Option<usize> = None;
        let mut menu_toggle_mask_clip: Option<usize> = None;
        let mut menu_move_z: Option<(usize, bool)> = None;
        let mut menu_delete: Option<usize> = None;

        // === Perspective transform mode: edit the selected raster's deform mesh corners. ===
        if let Some(sel) = self.transform_mode_raster_idx {
            let mesh = self.ensure_raster_deform_mesh(page_idx, sel, image_rect, zoom);
            let deform = self
                .raster_layers_by_page
                .get(&page_idx)
                .and_then(|v| v.get(sel))
                .and_then(|l| l.deform.clone());
            if let (Some(_), Some(deform)) = (mesh, deform)
                && let Some(corners) = deform_mesh_corners_scene(&deform, image_rect, zoom)
            {
                let pointer = ui.ctx().pointer_latest_pos();
                let interact_rect = egui::Rect::from_points(&corners).expand(
                    TEXT_OVERLAY_TRANSFORM_HANDLE_RADIUS_PX * 2.0 + 2.0,
                );
                let resp = ui.interact(
                    interact_rect,
                    egui::Id::new(("typing_raster_xform", page_idx, sel)),
                    egui::Sense::click_and_drag(),
                );
                // Start a corner-handle drag.
                if self.raster_drag_state.is_none()
                    && resp.drag_started()
                    && let Some(p) = pointer
                    && let Some(handle_idx) = hit_test_transform_handle(p, &corners)
                {
                    let page_size = page_size_from_image_rect(image_rect, zoom);
                    let start_mesh =
                        TypingOverlayDeformMesh::from_deform_rec(&deform, page_size);
                    let start_transform = self
                        .raster_layers_by_page
                        .get(&page_idx)
                        .and_then(|v| v.get(sel))
                        .map(|l| l.transform)
                        .unwrap_or(crate::models::layer_model::manifest::TransformRec {
                            cx: 0.0,
                            cy: 0.0,
                            rotation: 0.0,
                            scale: 1.0,
                        });
                    self.raster_drag_state = Some(TypingRasterDragState {
                        page_idx,
                        raster_idx: sel,
                        mode: TypingRasterDragMode::PerspectiveHandle(handle_idx),
                        pointer_start_scene: p,
                        start_transform,
                        start_pointer_angle_rad: 0.0,
                        start_mesh,
                    });
                    self.raster_drag_has_changes = false;
                    self.primary_pointer_targets_overlay_this_frame = true;
                }
                // Continue the corner drag.
                if let Some(state) = self.raster_drag_state.clone()
                    && state.raster_idx == sel
                    && matches!(state.mode, TypingRasterDragMode::PerspectiveHandle(_))
                    && (resp.dragged() || primary_down)
                    && let Some(p) = pointer
                {
                    self.apply_raster_drag(&state, p, image_rect, zoom);
                    self.primary_pointer_targets_overlay_this_frame = true;
                }
                self.raster_context_menu(
                    &resp,
                    page_idx,
                    sel,
                    true,
                    &mut menu_enter_transform,
                    &mut menu_exit_transform,
                    &mut menu_reset_transform,
                    &mut menu_toggle_mask_clip,
                    &mut menu_move_z,
                    &mut menu_delete,
                );

                // Decoration: deformed mesh wireframe outline + corner handles.
                let scene_pts = deform_mesh_scene_points(&deform, image_rect, zoom);
                draw_textured_deform_mesh_wire(painter, &scene_pts, deform.cols, deform.rows);
                draw_perspective_handles(painter, &corners);
            }
            self.apply_raster_menu_actions(
                page_idx,
                image_rect,
                zoom,
                menu_enter_transform,
                menu_exit_transform,
                menu_reset_transform,
                menu_toggle_mask_clip,
                menu_move_z,
                menu_delete,
            );
            return;
        }

        // === Normal mode: move / rotate drag + selection + context menu. ===
        // Scene quads + centers for this page's rasters.
        let entries: Vec<(usize, [Pos2; 4], Pos2)> = (0..count)
            .filter_map(|i| {
                let l = self.raster_layers_by_page.get(&page_idx)?.get(i)?;
                let quad = raster_quad_scene(&l.transform, l.image.size, image_rect, zoom);
                let center = scene_from_page_px(image_rect, zoom, [l.transform.cx, l.transform.cy]);
                Some((i, quad, center))
            })
            .collect();
        let pointer = ui.ctx().pointer_latest_pos();

        // === Unified topmost-at-pointer gate (text vs raster) ===
        // The raster interaction runs AFTER the overlay pass, and egui gives the LATER-registered widget
        // the click — so without this a raster would steal a click that lands on a higher-Z text overlay.
        // Decide the winner by UNIFIED band-Z (same axis as the draw order): if a TEXT overlay is on top
        // at the pointer, claim the click for overlays (`primary_pointer_targets_overlay_this_frame`) so
        // the raster pass below gates out. If a RASTER is on top (text now allowed BELOW a raster), do
        // NOT set the overlay gate, so the raster pass can take it. Skipped during an active drag (the
        // drag owns the pointer) and when an overlay already claimed the click this frame.
        if self.raster_drag_state.is_none() && !self.primary_pointer_targets_overlay_this_frame {
            let topmost_raster_z = topmost_raster_target(&entries, pointer, image_rect, None)
                .and_then(|(idx, _, _, _)| {
                    self.raster_layers_by_page
                        .get(&page_idx)
                        .and_then(|v| v.get(idx))
                        .map(|l| self.raster_band_z(page_idx, &l.uid))
                });
            let topmost_overlay = self.topmost_overlay_at(page_idx, pointer, image_rect, zoom);
            if unified_topmost_pointer_target(topmost_overlay.map(|(_, z)| z), topmost_raster_z)
                == TypingPointerTarget::Overlay
            {
                // A higher-or-equal-Z overlay is on top. Gate the raster pass so it can't steal the
                // click. egui awarded the click to the later-registered raster widget (so the overlay
                // pass's `.clicked()` did NOT fire) — so on a primary click here, SELECT the winning
                // overlay directly, matching the visual top. (Click already routed to the raster by egui,
                // so this is the only place the overlay can claim it.)
                self.primary_pointer_targets_overlay_this_frame = true;
                if let Some((overlay_idx, _)) = topmost_overlay {
                    let primary_clicked = ui.input(|i| i.pointer.primary_clicked());
                    if primary_clicked && self.selected_overlay_idx != Some(overlay_idx) {
                        self.selected_overlay_idx = Some(overlay_idx);
                        self.selected_raster_idx = None;
                        self.transform_mode_raster_idx = None;
                    }
                }
            }
        }

        if let Some(state) = self.raster_drag_state.clone() {
            // Continue an active drag (same Id keeps egui's drag association). This owns the selected
            // raster's `("typing_raster", page_idx, raster_idx)` Id for the frame, so the branches below
            // must NOT also create a resp for it.
            if let Some((_, quad, _)) = entries.iter().find(|(i, _, _)| *i == state.raster_idx) {
                let resp = ui.interact(
                    egui::Rect::from_points(quad),
                    egui::Id::new(("typing_raster", page_idx, state.raster_idx)),
                    egui::Sense::click_and_drag(),
                );
                if (resp.dragged() || primary_down)
                    && let Some(p) = pointer
                {
                    self.apply_raster_drag(&state, p, image_rect, zoom);
                    self.primary_pointer_targets_overlay_this_frame = true;
                }
                // Keep the menu attached to the selected raster's resp even mid-drag, so it persists.
                self.raster_context_menu(
                    &resp,
                    page_idx,
                    state.raster_idx,
                    false,
                    &mut menu_enter_transform,
                    &mut menu_exit_transform,
                    &mut menu_reset_transform,
                    &mut menu_toggle_mask_clip,
                    &mut menu_move_z,
                    &mut menu_delete,
                );
            }
        } else {
            // No active drag. Two independent responses are created (distinct Ids):
            //   (1) the SELECTED raster's resp UNCONDITIONALLY every frame — so its context menu stays
            //       open regardless of pointer position (mirrors transform-mode and text overlays); and
            //   (2) the topmost NON-selected raster under the pointer (a hit-test), so a first
            //       right/left click selects it and opens its menu immediately.
            // Tie gating with overlays is preserved: when an overlay claimed the pointer this frame
            // (`primary_pointer_targets_overlay_this_frame`), we still CREATE the selected raster's resp
            // and attach the menu (so it persists), but we DON'T run its click/drag handling.
            let gated = self.primary_pointer_targets_overlay_this_frame;

            // (1) Selected raster: unconditional resp + menu.
            if let Some(sel) = self.selected_raster_idx
                && let Some((_, sel_quad, sel_center)) =
                    entries.iter().find(|(i, _, _)| *i == sel).copied()
            {
                let resp = ui.interact(
                    egui::Rect::from_points(&sel_quad),
                    egui::Id::new(("typing_raster", page_idx, sel)),
                    egui::Sense::click_and_drag(),
                );
                if !gated {
                    let on_rotate = pointer.is_some_and(|p| {
                        let (_, handle) = rotation_handle_scene_with_corner(&sel_quad, image_rect);
                        p.distance(handle) <= TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX * 2.0
                    });
                    let over = pointer
                        .is_some_and(|p| point_in_quad(p, &sel_quad) || on_rotate);
                    if over && (resp.clicked() || resp.secondary_clicked()) {
                        // Already selected; just claim the click so the deselect-on-empty doesn't fire.
                        self.primary_pointer_targets_overlay_this_frame = true;
                    }
                    if over
                        && resp.drag_started()
                        && let Some(p) = pointer
                        && let Some(start_transform) = self
                            .raster_layers_by_page
                            .get(&page_idx)
                            .and_then(|v| v.get(sel))
                            .map(|l| l.transform)
                    {
                        crate::trace_log!(
                            cat::INPUT,
                            "raster_drag_begin owner=selected idx={} selected_was={:?} reason=selected_under_pointer",
                            sel,
                            self.selected_raster_idx
                        );
                        self.raster_drag_state = Some(TypingRasterDragState {
                            page_idx,
                            raster_idx: sel,
                            mode: if on_rotate {
                                TypingRasterDragMode::Rotate
                            } else {
                                TypingRasterDragMode::Move
                            },
                            pointer_start_scene: p,
                            start_transform,
                            start_pointer_angle_rad: pointer_angle_rad(sel_center, p),
                            start_mesh: None,
                        });
                        self.raster_drag_has_changes = false;
                        self.primary_pointer_targets_overlay_this_frame = true;
                    }
                }
                self.raster_context_menu(
                    &resp,
                    page_idx,
                    sel,
                    false,
                    &mut menu_enter_transform,
                    &mut menu_exit_transform,
                    &mut menu_reset_transform,
                    &mut menu_toggle_mask_clip,
                    &mut menu_move_z,
                    &mut menu_delete,
                );
            }

            // (2) Non-selected rasters: topmost hit-test (skips the selected idx → no duplicate Id).
            if !self.primary_pointer_targets_overlay_this_frame {
                let target = topmost_raster_target(
                    &entries,
                    pointer,
                    image_rect,
                    self.selected_raster_idx,
                );
                if let Some((idx, quad, center, on_rotate)) = target {
                    // Sticky-focus on DRAG: if the pointer is ALSO over the currently-selected raster's
                    // quad, this non-selected widget must NOT capture the drag — egui awards both
                    // `hits.click` and `hits.drag` to the last-registered widget at the pixel (this one),
                    // which would steal the drag from the selected raster (branch 1). So when the selected
                    // raster is under the pointer, register THIS widget as click-only: `hits.drag` then
                    // falls back to branch (1)'s click_and_drag widget (the selected raster), so a drag
                    // moves the SELECTED layer. A click (press-release) still lands here → reselect.
                    let pointer_over_selected = pointer.is_some_and(|p| {
                        self.selected_raster_idx
                            .and_then(|sel| entries.iter().find(|(i, _, _)| *i == sel))
                            .is_some_and(|(_, sel_quad, _)| point_in_quad(p, sel_quad))
                    });
                    let sense = if pointer_over_selected {
                        egui::Sense::click()
                    } else {
                        egui::Sense::click_and_drag()
                    };
                    let resp = ui.interact(
                        egui::Rect::from_points(&quad),
                        egui::Id::new(("typing_raster", page_idx, idx)),
                        sense,
                    );
                    if resp.clicked() {
                        self.select_raster(idx);
                        self.primary_pointer_targets_overlay_this_frame = true;
                    }
                    // Right-click selects the raster (mirror the overlay menu), then opens the menu.
                    if resp.secondary_clicked() {
                        self.select_raster(idx);
                        self.primary_pointer_targets_overlay_this_frame = true;
                    }
                    if resp.drag_started()
                        && let Some(p) = pointer
                        && let Some(start_transform) = self
                            .raster_layers_by_page
                            .get(&page_idx)
                            .and_then(|v| v.get(idx))
                            .map(|l| l.transform)
                    {
                        crate::trace_log!(
                            cat::INPUT,
                            "raster_drag_begin owner=reselect idx={} selected_was={:?} reason=no_selected_under_pointer",
                            idx,
                            self.selected_raster_idx
                        );
                        self.select_raster(idx);
                        self.raster_drag_state = Some(TypingRasterDragState {
                            page_idx,
                            raster_idx: idx,
                            mode: if on_rotate {
                                TypingRasterDragMode::Rotate
                            } else {
                                TypingRasterDragMode::Move
                            },
                            pointer_start_scene: p,
                            start_transform,
                            start_pointer_angle_rad: pointer_angle_rad(center, p),
                            start_mesh: None,
                        });
                        self.raster_drag_has_changes = false;
                        self.primary_pointer_targets_overlay_this_frame = true;
                    }
                    self.raster_context_menu(
                        &resp,
                        page_idx,
                        idx,
                        false,
                        &mut menu_enter_transform,
                        &mut menu_exit_transform,
                        &mut menu_reset_transform,
                        &mut menu_toggle_mask_clip,
                        &mut menu_move_z,
                        &mut menu_delete,
                    );
                }
            }
        }

        // Deselect when clicking empty image area (no raster and no overlay targeted this frame).
        if self.selected_raster_idx.is_some()
            && self.raster_drag_state.is_none()
            && !self.primary_pointer_targets_overlay_this_frame
        {
            let clicked_empty = ui.input(|i| {
                i.pointer.primary_clicked()
                    && i.pointer
                        .interact_pos()
                        .is_some_and(|p| image_rect.contains(p))
            }) && !ui.ctx().is_pointer_over_area();
            if clicked_empty {
                self.selected_raster_idx = None;
                self.transform_mode_raster_idx = None;
            }
        }

        // Selection decoration (dashed boundary + rotate handle).
        if let Some(sel) = self.selected_raster_idx
            && let Some((_, quad, _)) = entries.iter().find(|(i, _, _)| *i == sel)
        {
            let path = [quad[0], quad[1], quad[2], quad[3], quad[0]];
            draw_dashed_selection_path(painter, &path);
            draw_rotation_handle(painter, quad, image_rect);
        }

        self.apply_raster_menu_actions(
            page_idx,
            image_rect,
            zoom,
            menu_enter_transform,
            menu_exit_transform,
            menu_reset_transform,
            menu_toggle_mask_clip,
            menu_move_z,
            menu_delete,
        );
    }

    /// Attaches the raster context menu to `resp`, recording chosen actions into the deferred `out_*`
    /// slots (applied after the closure by `apply_raster_menu_actions`, avoiding mid-closure mutation).
    /// `is_transform_mode` toggles the enter/exit/reset items. Mirrors the text-overlay canvas menu.
    #[allow(clippy::too_many_arguments)]
    fn raster_context_menu(
        &self,
        resp: &egui::Response,
        _page_idx: usize,
        idx: usize,
        is_transform_mode: bool,
        out_enter_transform: &mut Option<usize>,
        out_exit_transform: &mut bool,
        out_reset_transform: &mut Option<usize>,
        out_toggle_mask_clip: &mut Option<usize>,
        out_move_z: &mut Option<(usize, bool)>,
        out_delete: &mut Option<usize>,
    ) {
        let mask_clip_on = self
            .raster_layers_by_page
            .get(&_page_idx)
            .and_then(|v| v.get(idx))
            .map(|l| l.mask_clip_enabled)
            .unwrap_or(false);
        resp.context_menu(|menu_ui| {
            if self.selected_raster_idx != Some(idx) {
                menu_ui.label("Выделите слой ЛКМ.");
                return;
            }
            if !is_transform_mode {
                if menu_ui.button("Войти в режим трансформации").clicked() {
                    *out_enter_transform = Some(idx);
                    menu_ui.close();
                }
            } else {
                if menu_ui.button("Выйти из режима трансформации").clicked() {
                    *out_exit_transform = true;
                    menu_ui.close();
                }
                if menu_ui.button("Сбросить трансформацию").clicked() {
                    *out_reset_transform = Some(idx);
                    menu_ui.close();
                }
            }
            menu_ui.separator();
            let toggle_label = if mask_clip_on {
                "Выключить обрезание маской"
            } else {
                "Включить обрезание маской"
            };
            if menu_ui.button(toggle_label).clicked() {
                *out_toggle_mask_clip = Some(idx);
                menu_ui.close();
            }
            menu_ui.separator();
            menu_ui.horizontal(|row| {
                row.label("Порядок");
                if row.button("▲").clicked() {
                    *out_move_z = Some((idx, true));
                }
                if row.button("▼").clicked() {
                    *out_move_z = Some((idx, false));
                }
            });
            menu_ui.separator();
            if menu_ui.button("Удалить слой").clicked() {
                *out_delete = Some(idx);
                menu_ui.close();
            }
        });
    }

    /// Applies the deferred raster context-menu actions captured by `raster_context_menu`.
    #[allow(clippy::too_many_arguments)]
    fn apply_raster_menu_actions(
        &mut self,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        enter_transform: Option<usize>,
        exit_transform: bool,
        reset_transform: Option<usize>,
        toggle_mask_clip: Option<usize>,
        move_z: Option<(usize, bool)>,
        delete: Option<usize>,
    ) {
        if let Some(idx) = enter_transform {
            // Seed the mesh (if absent) and enter perspective transform mode.
            if self.ensure_raster_deform_mesh(page_idx, idx, image_rect, zoom).is_some() {
                self.transform_mode_raster_idx = Some(idx);
                self.deform_mode = TypingDeformMode::Perspective;
                self.raster_drag_state = None;
                self.raster_drag_has_changes = false;
                // Persist the seeded mesh so it survives without a drag.
                if let Some(layer) = self
                    .raster_layers_by_page
                    .get(&page_idx)
                    .and_then(|v| v.get(idx))
                {
                    let (uid, transform, deform) =
                        (layer.uid.clone(), layer.transform, layer.deform.clone());
                    self.persist_raster_deform(page_idx, &uid, transform, deform);
                }
            }
        }
        if exit_transform {
            self.transform_mode_raster_idx = None;
            self.raster_drag_state = None;
            self.raster_drag_has_changes = false;
        }
        if let Some(idx) = reset_transform {
            // Clear the deform (back to plain affine), persist, exit transform mode.
            if let Some(layer) = self
                .raster_layers_by_page
                .get_mut(&page_idx)
                .and_then(|v| v.get_mut(idx))
            {
                layer.deform = None;
            }
            if let Some(layer) = self
                .raster_layers_by_page
                .get(&page_idx)
                .and_then(|v| v.get(idx))
            {
                let (uid, transform) = (layer.uid.clone(), layer.transform);
                self.persist_raster_deform(page_idx, &uid, transform, None);
            }
            self.transform_mode_raster_idx = None;
            self.raster_drag_state = None;
            self.raster_drag_has_changes = false;
        }
        if let Some(idx) = toggle_mask_clip {
            if let Some(layer) = self
                .raster_layers_by_page
                .get(&page_idx)
                .and_then(|v| v.get(idx))
            {
                let uid = layer.uid.clone();
                let new_val = !layer.mask_clip_enabled;
                // Route through the doc (source of truth): bumps generation → re-clip + re-upload, and
                // bumps the doc version → the PS tab re-projects.
                self.route_to_doc(page_idx, |doc| {
                    doc.set_raster_mask_clip(page_idx, &uid, Some(new_val));
                });
                // Persist so it survives a reload / save-to-project (whole-page raster save preserves it).
                self.persist_current_page_rasters(page_idx);
            }
        }
        if let Some((idx, up)) = move_z {
            self.move_raster_in_unified_z(page_idx, idx, up);
        }
        if let Some(idx) = delete {
            self.remove_raster(page_idx, idx);
        }
    }

    /// Applies an in-progress raster drag (move or rotate) to the cached transform.
    fn apply_raster_drag(
        &mut self,
        state: &TypingRasterDragState,
        pointer: Pos2,
        image_rect: Rect,
        zoom: f32,
    ) {
        let Some(layer) = self
            .raster_layers_by_page
            .get_mut(&state.page_idx)
            .and_then(|v| v.get_mut(state.raster_idx))
        else {
            return;
        };
        match state.mode {
            TypingRasterDragMode::Move => {
                let z = zoom.max(f32::EPSILON);
                layer.transform.cx =
                    state.start_transform.cx + (pointer.x - state.pointer_start_scene.x) / z;
                layer.transform.cy =
                    state.start_transform.cy + (pointer.y - state.pointer_start_scene.y) / z;
            }
            TypingRasterDragMode::Rotate => {
                let center = scene_from_page_px(
                    image_rect,
                    zoom,
                    [state.start_transform.cx, state.start_transform.cy],
                );
                let cur = pointer_angle_rad(center, pointer);
                layer.transform.rotation =
                    state.start_transform.rotation + (cur - state.start_pointer_angle_rad);
            }
            TypingRasterDragMode::PerspectiveHandle(handle_idx) => {
                let Some(start_mesh) = &state.start_mesh else {
                    return;
                };
                let page_size = page_size_from_image_rect(image_rect, zoom);
                let z = zoom.max(f32::EPSILON);
                // Pointer delta in page px (scene → page).
                let delta_page_px = [
                    (pointer.x - state.pointer_start_scene.x) / z,
                    (pointer.y - state.pointer_start_scene.y) / z,
                ];
                let mesh = apply_perspective_corner_drag(
                    start_mesh,
                    handle_idx,
                    delta_page_px,
                    page_size,
                );
                layer.deform = Some(crate::models::layer_model::manifest::DeformRec {
                    cols: mesh.cols,
                    rows: mesh.rows,
                    points_px: mesh.points_px.clone(),
                });
            }
        }
        self.raster_drag_has_changes = true;
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

    /// Nudges the selected RASTER layer by whole page pixels with the arrow keys (parity with the
    /// overlay nudge `try_move_selected_overlay_by_arrow_shortcuts`). SHIFT moves by 5 px. Mirrors the
    /// raster mouse-drag Move path: a perspective-deformed raster translates its mesh, otherwise the
    /// affine `transform.cx/cy` move (clamped to the page, snapped to whole pixels when
    /// `strict_pixel_movement`). The change is routed to the shared doc and persisted to disk.
    ///
    /// Gated on `selected_raster_idx`, which is mutually exclusive with `selected_overlay_idx`, so this
    /// only consumes the arrow keys when a raster is selected (the overlay nudge, called first, returns
    /// before consuming keys when no overlay is selected).
    fn try_move_selected_raster_by_arrow_shortcuts(
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

        let Some(selected_idx) = self.selected_raster_idx else {
            return;
        };
        let has_layer = self
            .raster_layers_by_page
            .get(&page_idx)
            .is_some_and(|v| selected_idx < v.len());
        if !has_layer {
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
        let Some(layer) = self
            .raster_layers_by_page
            .get_mut(&page_idx)
            .and_then(|v| v.get_mut(selected_idx))
        else {
            return;
        };

        // A perspective-deformed raster (mesh present) renders from its mesh points, so translate the
        // mesh; the plain affine raster moves its center. Matches the mouse-drag Move path.
        if let Some(rec) = layer.deform.as_ref() {
            let Some(mut mesh) = TypingOverlayDeformMesh::from_deform_rec(rec, page_size) else {
                return;
            };
            mesh.translate(page_delta[0], page_delta[1], page_size);
            layer.deform = Some(crate::models::layer_model::manifest::DeformRec {
                cols: mesh.cols,
                rows: mesh.rows,
                points_px: mesh.points_px.clone(),
            });
            let (uid, transform, deform) =
                (layer.uid.clone(), layer.transform, layer.deform.clone());
            self.persist_raster_deform(page_idx, &uid, transform, deform);
        } else {
            let mut center = clamp_page_point(
                [
                    layer.transform.cx + page_delta[0],
                    layer.transform.cy + page_delta[1],
                ],
                page_size,
            );
            if strict_pixel_movement {
                center = clamp_page_point([center[0].round(), center[1].round()], page_size);
            }
            layer.transform.cx = center[0];
            layer.transform.cy = center[1];
            let (uid, transform) = (layer.uid.clone(), layer.transform);
            self.persist_raster_transform(page_idx, &uid, transform);
        }
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
        crate::trace_log!(
            cat::SYNC,
            "auto_typing dispatch token={} overlay_idx={} page={} click_uv=({:.3},{:.3})",
            token,
            selected_idx,
            page_idx,
            click_uv[0],
            click_uv[1]
        );
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
            Ok(Ok(result)) => {
                crate::trace_log!(
                    cat::SYNC,
                    "auto_typing result=ok token={} page={}",
                    result.token,
                    result.page_idx
                );
                self.apply_auto_typing_result(ctx, job_state, result)
            }
            Ok(Err(err)) | Err(err) => {
                crate::trace_log!(cat::SYNC, "auto_typing result=err err={}", err);
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
        // One selection at a time across the two layer kinds: an overlay selection wins (overlay
        // interaction runs before the raster pass below; `select_raster` clears overlays directly).
        if self.selected_overlay_idx.is_some() {
            self.selected_raster_idx = None;
            self.transform_mode_raster_idx = None;
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
        // Ensure the read-only PS raster layers and unified Z bands for this page are loaded; the
        // actual raster quads are now drawn interleaved with the text overlays (one ordered pass
        // below) so a raster moved above a text group in the PS editor renders on top.
        self.ensure_raster_layers_for_page(page_idx);
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
            self.try_scale_selected_raster_by_shortcuts(ui, page_idx);
            self.try_move_selected_overlay_by_arrow_shortcuts(
                ui,
                page_idx,
                image_rect,
                zoom,
                panel_text_input_focused,
                strict_pixel_movement,
            );
            self.try_move_selected_raster_by_arrow_shortcuts(
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
            render_width_px: Option<u32>,
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
            let render_width_px = if overlay.kind == TypingOverlayKind::Text {
                overlay.render_data_json.as_ref().map(|render_data| {
                    overlay_render_data_width_hint(
                        Some(render_data),
                        u32::try_from(overlay.size_px[0]).unwrap_or(u32::MAX),
                    )
                })
            } else {
                None
            };
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
                render_width_px,
            });
        }

        // Bottom-to-top by the UNIFIED manual band-Z (retire the old layer_idx + page-Y auto-order):
        // the top overlay draws last (on top) AND registers its egui interaction last, so on an overlap
        // the topmost-by-Z overlay wins the click — the same Z the raster/text unified hit-test and the
        // `merged_fills` draw order use, so draw order == manual order == click order.
        draw_entries.sort_by(|a, b| {
            let z = |idx: usize| {
                self.overlays
                    .get(idx)
                    .map(|o| self.overlay_band_z(page_idx, &o.uid, o.layer_idx))
                    .unwrap_or(0)
            };
            z(a.idx).cmp(&z(b.idx))
        });

        if !draw_entries.is_empty() && !mask_panel_open && !layout_editor_active {
            let mut clicked_overlay_idx: Option<usize> = None;
            let mut pending_delete_overlay_idx: Option<usize> = None;
            let mut pending_enter_layout_editor_idx: Option<usize> = None;
            let popup_open_before = ui.ctx().is_popup_open();
            // Sticky-фокус: если клик пришёлся внутрь рамки уже выделенного оверлея,
            // фокус остаётся на нём, даже если сверху лежит перекрывающий оверлей или
            // растровый слой. Считаем это один раз по позиции клика и по grab-мешу
            // выделенного оверлея (та же область, что и `pointer_inside_grab_area`).
            let click_in_selected_frame = ui
                .input(|i| i.pointer.primary_clicked())
                .then(|| ui.input(|i| i.pointer.interact_pos()))
                .flatten()
                .zip(self.selected_overlay_idx)
                .is_some_and(|(pos, selected_idx)| {
                    draw_entries.iter().any(|entry| {
                        entry.idx == selected_idx
                            && deform_mesh_contains_point(
                                &entry.selection_mesh_scene,
                                entry.mesh_cols,
                                entry.mesh_rows,
                                pos,
                            )
                    })
                });
            // Sticky-фокус на ПЕРЕТАСКИВАНИИ (по позиции курсора, без клика): курсор находится
            // внутри grab-рамки уже выделенного оверлея. Тогда перекрывающий НЕвыделенный оверлей
            // регистрируется как click-only (см. ниже), и egui отдаёт drag выделенному оверлею.
            let pointer_in_selected_overlay_frame = ui
                .input(|i| i.pointer.latest_pos())
                .zip(self.selected_overlay_idx)
                .is_some_and(|(pos, selected_idx)| {
                    draw_entries.iter().any(|entry| {
                        entry.idx == selected_idx
                            && deform_mesh_contains_point(
                                &entry.selection_mesh_scene,
                                entry.mesh_cols,
                                entry.mesh_rows,
                                pos,
                            )
                    })
                });
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
                // Если курсор внутри рамки уже выделенного оверлея, перекрывающий НЕвыделенный
                // оверлей не должен перехватывать DRAG: регистрируем его click-only, чтобы egui
                // отдал drag выделенному оверлею (его виджет sense'ит click_and_drag). Клик
                // (нажал-отпустил) по-прежнему попадает сюда и переселектит — см. блок
                // sticky-фокуса по `click_in_selected_frame`.
                let sense = if pointer_in_selected_overlay_frame
                    && self.selected_overlay_idx != Some(entry.idx)
                {
                    Sense::click()
                } else {
                    Sense::click_and_drag()
                };
                let response = ui.interact(
                    interact_rect,
                    Id::new(("typing_text_overlay", entry.idx)),
                    sense,
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
                    // Не перехватываем фокус перекрывающим оверлеем, если клик попал
                    // в рамку уже выделенного (нижнего) оверлея — фокус удержит
                    // блок sticky-фокуса после цикла.
                    if !(click_in_selected_frame && self.selected_overlay_idx != Some(entry.idx)) {
                        clicked_overlay_idx = Some(entry.idx);
                        self.selected_overlay_idx = Some(entry.idx);
                        self.primary_pointer_targets_overlay_this_frame = true;
                    }
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
                                crate::trace_log!(
                                    cat::TYPING,
                                    "overlay_transform_mode enter idx={}",
                                    entry.idx
                                );
                                self.transform_mode_overlay_idx = Some(entry.idx);
                                self.deform_mode = TypingDeformMode::Perspective;
                                self.drag_state = None;
                            }
                            menu_ui.close();
                        }
                    } else {
                        if menu_ui.button("Выйти из режима трансформации").clicked()
                        {
                            crate::trace_log!(
                                cat::TYPING,
                                "overlay_transform_mode exit idx={}",
                                entry.idx
                            );
                            if self.transform_mode_overlay_idx == Some(entry.idx) {
                                self.transform_mode_overlay_idx = None;
                            }
                            self.drag_state = None;
                            self.drag_has_changes = false;
                            menu_ui.close();
                        }
                        if menu_ui.button("Сбросить трансформацию").clicked() {
                            crate::trace_log!(
                                cat::TYPING,
                                "overlay_transform_reset idx={}",
                                entry.idx
                            );
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
                            let mut new_state = false;
                            if let Some(overlay) = self.overlays.get_mut(entry.idx) {
                                overlay.mask_clip_enabled = !overlay.mask_clip_enabled;
                                new_state = overlay.mask_clip_enabled;
                            }
                            crate::trace_log!(
                                cat::TYPING,
                                "overlay_mask_clip_toggle idx={} enabled={}",
                                entry.idx,
                                new_state
                            );
                            self.mark_overlay_pixels_dirty(entry.idx);
                            self.request_overlay_placement_save();
                            menu_ui.close();
                        }
                    }
                    menu_ui.separator();
                    {
                        // ▲ / ▼ move the overlay one step in the unified Z order (text + raster
                        // interleaved, shared with the PS editor). No more per-overlay text-group
                        // number — order is the shared layer stack.
                        let mut move_z_up: Option<bool> = None;
                        menu_ui.horizontal(|row| {
                            row.label("Порядок");
                            if row.button("▲").clicked() {
                                move_z_up = Some(true);
                            }
                            if row.button("▼").clicked() {
                                move_z_up = Some(false);
                            }
                        });
                        if let Some(up) = move_z_up {
                            self.move_overlay_in_unified_z(page_idx, entry.idx, up);
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

                        crate::trace_log!(
                            cat::INPUT,
                            "overlay_drag_begin owner={} idx={} selected_was={:?} reason=drag_started",
                            if self.selected_overlay_idx == Some(entry.idx) {
                                "selected"
                            } else {
                                "reselect"
                            },
                            entry.idx,
                            self.selected_overlay_idx
                        );
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
                                crate::trace_log!(
                                    cat::INPUT,
                                    "overlay_drag_begin transform=true idx={} page={} mode={:?} deform_mode={:?}",
                                    entry.idx,
                                    page_idx,
                                    mode,
                                    self.deform_mode
                                );
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
                        crate::trace_log!(
                            cat::INPUT,
                            "overlay_drag_begin transform=false idx={} page={} mode={:?}",
                            entry.idx,
                            page_idx,
                            mode
                        );
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
                    if crate::trace::trace_enabled() {
                        let (center, angle) = self
                            .overlays
                            .get(entry.idx)
                            .map(|o| (o.center_page_px, o.angle_deg))
                            .unwrap_or(([0.0, 0.0], 0.0));
                        crate::trace_log!(
                            cat::INPUT,
                            "overlay_drag_end idx={} committed={} center=({:.1},{:.1}) angle={:.1}",
                            entry.idx,
                            self.drag_has_changes,
                            center[0],
                            center[1],
                            angle
                        );
                    }
                    if self.drag_has_changes {
                        self.flush_overlay_texture_if_stale(entry.idx);
                        self.request_overlay_placement_save();
                    }
                    self.drag_state = None;
                    self.drag_has_changes = false;
                }
            }

            // Клик внутри рамки выделенного оверлея считаем нацеленным на него:
            // помечаем кадр как «попал в оверлей» (чтобы растровый слой выше не
            // перехватил фокус, см. `interact_page_rasters`) и подставляем
            // выделенный индекс в `clicked_overlay_idx`, чтобы не сработал сброс
            // выделения при клике по «пустому» месту.
            if click_in_selected_frame {
                self.primary_pointer_targets_overlay_this_frame = true;
                if clicked_overlay_idx.is_none() {
                    clicked_overlay_idx = self.selected_overlay_idx;
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

        // Unified-Z fill pass: interleave the read-only PS raster quads with the text/image overlay
        // textured meshes in one pass ordered bottom-to-top by band Z. (Selection decorations and
        // editing handles are drawn afterwards so they always sit on top.)
        enum MergedFillItem {
            /// Index into the page's cached `raster_layers_by_page` vector.
            Raster(usize),
            /// Index into `draw_entries`.
            Overlay(usize),
        }
        let mut merged_fills: Vec<(u32, u32, MergedFillItem)> = Vec::new();
        // Rasters: band Z from the matching `Raster` band (else top). Tiebreak `0` keeps the cached
        // bottom-to-top raster order via the raster index in the third tuple slot's stable sort.
        if let Some(rasters) = self.raster_layers_by_page.get(&page_idx) {
            for (raster_idx, raster) in rasters.iter().enumerate() {
                let band_z = self.raster_band_z(page_idx, &raster.uid);
                merged_fills.push((band_z, 0, MergedFillItem::Raster(raster_idx)));
            }
        }
        // Overlays: band Z from the overlay's text group / pinned-text band (else top). Tiebreak `1`
        // so that, within the same band Z, overlays draw above rasters; `draw_entries` is already in
        // the desired within-group order, preserved by the stable sort.
        for (entry_pos, entry) in draw_entries.iter().enumerate() {
            let band_z = self
                .overlays
                .get(entry.idx)
                .map(|overlay| self.overlay_band_z(page_idx, &overlay.uid, overlay.layer_idx))
                .unwrap_or_else(|| {
                    self.bands_by_page
                        .get(&page_idx)
                        .map(|b| b.len() as u32)
                        .unwrap_or(0)
                });
            merged_fills.push((band_z, 1, MergedFillItem::Overlay(entry_pos)));
        }
        // Stable sort: primary band Z, then raster-below-overlay tiebreak; existing raster order and
        // within-group overlay order are preserved as the stable tiebreak.
        merged_fills.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for (_, _, item) in &merged_fills {
            match item {
                MergedFillItem::Raster(raster_idx) => {
                    self.draw_one_raster_layer(
                        ui.ctx(),
                        &painter,
                        page_idx,
                        *raster_idx,
                        image_rect,
                        zoom,
                    );
                }
                MergedFillItem::Overlay(entry_pos) => {
                    let entry = &draw_entries[*entry_pos];
                    draw_textured_deform_mesh(
                        &painter,
                        entry.texture.id(),
                        &entry.mesh_scene,
                        entry.mesh_cols,
                        entry.mesh_rows,
                    );
                }
            }
        }

        for entry in &draw_entries {
            if !mask_panel_open && self.selected_overlay_idx == Some(entry.idx) {
                let selection_path = mesh_boundary_path(
                    &entry.selection_mesh_scene,
                    entry.mesh_cols,
                    entry.mesh_rows,
                );
                draw_dashed_selection_path(&painter, &selection_path);
                if let Some(render_width_px) = entry.render_width_px {
                    draw_text_overlay_width_guide(
                        &painter,
                        entry.selection_bounds_rect,
                        render_width_px,
                        entry.bounds_rect.width(),
                        self.overlays
                            .get(entry.idx)
                            .map(|overlay| overlay.size_px[0])
                            .unwrap_or_default(),
                    );
                }
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
        if !mask_panel_open && !layout_editor_active {
            self.interact_page_rasters(ui, page_idx, image_rect, zoom, &painter);
        }
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
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
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

fn draw_text_overlay_width_guide(
    painter: &egui::Painter,
    selection_bounds_rect: Rect,
    render_width_px: u32,
    overlay_screen_width_px: f32,
    overlay_source_width_px: usize,
) {
    let source_width = overlay_source_width_px.max(1) as f32;
    let guide_width =
        (render_width_px.max(1) as f32 / source_width) * overlay_screen_width_px.max(1.0);
    let half_width = guide_width.max(1.0) * 0.5;
    let center_x = selection_bounds_rect.center().x;
    let line_y = selection_bounds_rect.top() - TEXT_OVERLAY_WIDTH_GUIDE_GAP_PX;
    let left = Pos2::new(center_x - half_width, line_y);
    let right = Pos2::new(center_x + half_width, line_y);
    let tick_top_y = line_y - TEXT_OVERLAY_WIDTH_GUIDE_TICK_HALF_PX;
    let tick_bottom_y = line_y + TEXT_OVERLAY_WIDTH_GUIDE_TICK_HALF_PX;

    draw_dashed_selection_path(
        painter,
        &[
            Pos2::new(left.x, tick_top_y),
            Pos2::new(left.x, tick_bottom_y),
        ],
    );
    draw_dashed_selection_path(painter, &[left, right]);
    draw_dashed_selection_path(
        painter,
        &[
            Pos2::new(right.x, tick_top_y),
            Pos2::new(right.x, tick_bottom_y),
        ],
    );

    let label = format!("{} px", render_width_px.max(1));
    let label_pos = Pos2::new(center_x, tick_top_y - TEXT_OVERLAY_WIDTH_GUIDE_LABEL_GAP_PX);
    let font_id = egui::FontId::proportional(13.0);
    painter.text(
        label_pos + Vec2::new(1.0, 1.0),
        egui::Align2::CENTER_BOTTOM,
        label.as_str(),
        font_id.clone(),
        Color32::BLACK,
    );
    painter.text(
        label_pos,
        egui::Align2::CENTER_BOTTOM,
        label,
        font_id,
        Color32::WHITE,
    );
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

/// The four scene-space corners of a raster layer's image quad (top-left, top-right, bottom-right,
/// bottom-left), from its `TransformRec` (center page px, uniform scale, rotation radians). Mirrors
/// the corner math in `draw_one_raster_layer`.
fn raster_quad_scene(
    transform: &crate::models::layer_model::manifest::TransformRec,
    size: [usize; 2],
    image_rect: Rect,
    zoom: f32,
) -> [Pos2; 4] {
    let (sin_a, cos_a) = transform.rotation.sin_cos();
    let hw = size[0] as f32 * 0.5 * transform.scale;
    let hh = size[1] as f32 * 0.5 * transform.scale;
    let corners = [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)];
    let mut quad = [Pos2::ZERO; 4];
    for (i, (dx, dy)) in corners.iter().enumerate() {
        let rx = dx * cos_a - dy * sin_a;
        let ry = dx * sin_a + dy * cos_a;
        quad[i] = scene_from_page_px(image_rect, zoom, [transform.cx + rx, transform.cy + ry]);
    }
    quad
}

/// The 4 corner scene points of a deform mesh grid (TL, TR, BR, BL), for perspective-handle drag.
fn deform_mesh_corners_scene(
    deform: &crate::models::layer_model::manifest::DeformRec,
    image_rect: Rect,
    zoom: f32,
) -> Option<[Pos2; 4]> {
    let (c, r) = (deform.cols, deform.rows);
    if c < 2 || r < 2 || deform.points_px.len() != c * r {
        return None;
    }
    let at = |col: usize, row: usize| {
        scene_from_page_px(image_rect, zoom, deform.points_px[row * c + col])
    };
    Some([at(0, 0), at(c - 1, 0), at(c - 1, r - 1), at(0, r - 1)])
}

/// All scene points of a deform mesh grid (row-major), for drawing the wireframe.
fn deform_mesh_scene_points(
    deform: &crate::models::layer_model::manifest::DeformRec,
    image_rect: Rect,
    zoom: f32,
) -> Vec<Pos2> {
    deform
        .points_px
        .iter()
        .map(|p| scene_from_page_px(image_rect, zoom, *p))
        .collect()
}

/// Draws a deform mesh's grid lines (row + column segments) — the wireframe shown while a raster is in
/// perspective transform mode.
fn draw_textured_deform_mesh_wire(painter: &egui::Painter, mesh_scene: &[Pos2], cols: usize, rows: usize) {
    if cols < 2 || rows < 2 || mesh_scene.len() != cols * rows {
        return;
    }
    let stroke = Stroke::new(1.0, Color32::from_rgba_unmultiplied(90, 185, 255, 170));
    let at = |c: usize, r: usize| mesh_scene[r * cols + c];
    for r in 0..rows {
        for c in 0..cols {
            if c + 1 < cols {
                painter.line_segment([at(c, r), at(c + 1, r)], stroke);
            }
            if r + 1 < rows {
                painter.line_segment([at(c, r), at(c, r + 1)], stroke);
            }
        }
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

/// Finds the TOPMOST raster (last in `entries`, which are bottom-to-top) under `pointer`, SKIPPING the
/// currently-selected idx so the normal-mode interaction never creates a second response for the
/// selected raster (egui duplicate-Id). A raster is "under" the pointer if the point is inside its quad
/// OR within the rotate-handle radius. Returns `(idx, quad, center, on_rotate)`. Pure (geometry only),
/// so it is unit-testable. `excluded` (the selected idx) is skipped; pass `None` to consider every entry.
fn topmost_raster_target(
    entries: &[(usize, [Pos2; 4], Pos2)],
    pointer: Option<Pos2>,
    image_rect: Rect,
    excluded: Option<usize>,
) -> Option<(usize, [Pos2; 4], Pos2, bool)> {
    let p = pointer?;
    entries.iter().rev().find_map(|(idx, quad, center)| {
        if excluded == Some(*idx) {
            return None;
        }
        let (_, handle) = rotation_handle_scene_with_corner(quad, image_rect);
        let on_rotate = p.distance(handle) <= TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX * 2.0;
        if point_in_quad(p, quad) || on_rotate {
            Some((*idx, *quad, *center, on_rotate))
        } else {
            None
        }
    })
}

/// Which kind of layer the pointer should interact with when a text overlay and a raster overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypingPointerTarget {
    Overlay,
    Raster,
    None,
}

/// Picks the TOPMOST item (text overlay vs raster) under the pointer by UNIFIED band-Z, so the click
/// goes to whatever is drawn on top — matching the canvas draw order exactly. `overlay_z` / `raster_z`
/// are the topmost overlay's / raster's band-Z *if one is under the pointer* (else `None`). Ties go to
/// the OVERLAY (text draws above a raster at the same band-Z, mirroring `merged_fills`' `(z, kind)`
/// tiebreak where raster=0 < overlay=1). Pure, so it is unit-testable.
fn unified_topmost_pointer_target(
    overlay_z: Option<u32>,
    raster_z: Option<u32>,
) -> TypingPointerTarget {
    match (overlay_z, raster_z) {
        (Some(oz), Some(rz)) => {
            // Equal band-Z → overlay wins (overlay draws above raster at the same band).
            if oz >= rz {
                TypingPointerTarget::Overlay
            } else {
                TypingPointerTarget::Raster
            }
        }
        (Some(_), None) => TypingPointerTarget::Overlay,
        (None, Some(_)) => TypingPointerTarget::Raster,
        (None, None) => TypingPointerTarget::None,
    }
}

/// How many text-preview characters fit in a text row's available label width, with a floor of
/// `LAYERS_PANEL_MIN_PREVIEW_CHARS`. `available_px` is the row width left for the preview text (panel
/// content width minus the fixed row overhead — buttons, `Текст (…)` wrapper, spacing); `char_px` is a
/// representative glyph width. Wider panel → more chars before the dots; never below the min. Pure.
fn preview_char_budget(available_px: f32, char_px: f32) -> usize {
    if char_px <= 0.0 || !available_px.is_finite() {
        return LAYERS_PANEL_MIN_PREVIEW_CHARS;
    }
    let fits = (available_px / char_px).floor();
    let fits = if fits.is_finite() && fits > 0.0 { fits as usize } else { 0 };
    fits.max(LAYERS_PANEL_MIN_PREVIEW_CHARS)
}

/// Builds the `{preview}` shown inside a text row's `Текст ({preview})` label.
///
/// - Takes the first `max_chars` CHARACTERS (Unicode `chars()`, NOT bytes — text is Cyrillic) of `text`
///   after trimming leading whitespace. `max_chars` grows with the panel width (min 5).
/// - Ensures the run of trailing "dot-equivalents" is AT LEAST 3, accounting for dots already present:
///   a regular dot `.` counts 1, the single ellipsis char `…` (U+2026) counts 3. Trailing dots are
///   counted from the end of the prefix until the first non-dot char; then `max(0, 3 - count)` regular
///   dots are appended.
/// - Empty (after trim) → `""` (the caller then shows just `Текст`, no parentheses).
///
/// Crate-visible so other tabs (e.g. the PS editor layers panel) reuse the SAME preview logic.
pub(crate) fn text_preview_label(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut prefix: String = trimmed.chars().take(max_chars).collect();
    // Count trailing dot-equivalents (regular dot = 1, ellipsis = 3), stopping at the first non-dot.
    let mut existing = 0u32;
    for ch in prefix.chars().rev() {
        match ch {
            '.' => existing += 1,
            '…' => existing += 3,
            _ => break,
        }
    }
    let needed = 3u32.saturating_sub(existing);
    for _ in 0..needed {
        prefix.push('.');
    }
    prefix
}

/// One row in the unified "Слои страницы" list: a text/image overlay (index into `self.overlays`) or a
/// raster (index into `raster_layers_by_page[page]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypingLayerRow {
    Overlay(usize),
    Raster(usize),
}

/// Orders the page's layer rows for the panel: by unified band-Z DESCENDING (top of the stack first),
/// interleaving overlays and rasters. Tie-break at equal Z: OVERLAY above RASTER (matches the canvas
/// draw/hit-test tie-break where raster=0 < overlay=1). The input is `(row, band_z, raster_below_overlay)`
/// where the bool is `true` for a raster (sorts below an overlay at the same Z). Pure → unit-testable.
fn order_unified_layer_rows(mut rows: Vec<(TypingLayerRow, u32, bool)>) -> Vec<TypingLayerRow> {
    // Sort TOP-first: higher Z first; at equal Z, overlay (raster_below=false) before raster (true).
    rows.sort_by(|a, b| {
        b.1.cmp(&a.1) // band-Z descending
            .then_with(|| a.2.cmp(&b.2)) // false (overlay) before true (raster) at equal Z
    });
    rows.into_iter().map(|(row, _, _)| row).collect()
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

/// Picks the seed text for a freshly drawn typing selection from the bubble anchor closest to the
/// selection center whose anchor falls inside the selection rectangle.
///
/// A multi-area `ImageBubble` is a single `Bubble` in the data model but splits into one read-only
/// aside per text area, each with its own anchor. To match what the user sees, every image text
/// area is treated as an independent anchor candidate here; a plain text bubble contributes its one
/// `img_u`/`img_v` anchor. Returns `None` when no eligible anchor with non-empty text overlaps.
fn pick_bubble_text_for_selection(
    bubbles: &[Bubble],
    page_idx: usize,
    scene_rect: Rect,
    page_rect: Rect,
) -> Option<String> {
    let selection_center = scene_rect.center();
    let mut best: Option<(f32, String)> = None;

    let mut consider = |anchor_uv: (f32, f32), text: String| {
        if text.is_empty() {
            return;
        }
        let anchor_pos = scene_from_uv(page_rect, anchor_uv.0, anchor_uv.1);
        if !scene_rect.contains(anchor_pos) {
            return;
        }
        let dist_sq = selection_center.distance_sq(anchor_pos);
        let should_replace = best
            .as_ref()
            .is_none_or(|(best_dist, _)| dist_sq < *best_dist);
        if should_replace {
            best = Some((dist_sq, text));
        }
    };

    for bubble in bubbles.iter().filter(|bubble| bubble.img_idx == page_idx) {
        // Image bubbles expose one anchor per text area (matching the split read-only asides); text
        // bubbles expose a single anchor at `img_u`/`img_v`.
        let areas = parse_image_text_areas(bubble);
        if areas.is_empty() {
            consider(
                (bubble.img_u, bubble.img_v),
                preferred_bubble_seed_text(bubble),
            );
        } else {
            for area in &areas {
                consider(
                    (area.anchor.x, area.anchor.y),
                    preferred_area_seed_text(area),
                );
            }
        }
    }

    best.map(|(_, text)| text)
}

/// Seed text for a plain text bubble: the translation when present, otherwise the original.
fn preferred_bubble_seed_text(bubble: &crate::project::Bubble) -> String {
    let translated = bubble.text.trim();
    if !translated.is_empty() {
        return translated.to_string();
    }
    bubble.original_text.trim().to_string()
}

/// Seed text for one image text area: the translation when present, otherwise the original. The
/// description is intentionally excluded so a selection never seeds editor text with a note.
fn preferred_area_seed_text(area: &crate::canvas::ImageTextArea) -> String {
    let translated = area.translation.trim();
    if !translated.is_empty() {
        return translated.to_string();
    }
    area.original.trim().to_string()
}

/// Flattens a `ColorImage` into a row-major STRAIGHT (un-premultiplied) RGBA byte buffer (4 bytes/pixel),
/// the `source_rgba` layout every consumer expects. egui `Color32` stores PREMULTIPLIED bytes, so we use
/// `to_srgba_unmultiplied()` (NOT `to_array()`, which would return premultiplied). Every consumer of
/// `source_rgba` treats it as straight alpha — the display upload and effects/mask-clip paths feed it
/// back through `ColorImage::from_rgba_unmultiplied`, and the export composite blends it as straight — so
/// emitting premultiplied here would premultiply the text TWICE, darkening semi-transparent (antialiased
/// stroke) edges to gray.
fn color_image_to_rgba(image: &ColorImage) -> Vec<u8> {
    let mut out = Vec::with_capacity(image.pixels.len() * 4);
    for px in &image.pixels {
        out.extend_from_slice(&px.to_srgba_unmultiplied());
    }
    out
}

/// Materializes a typing TEXT overlay runtime from a doc Text node's projected fields. Used by
/// `sync_from_doc` when a doc Text node has no local runtime (the migrated-chapter case, where the
/// legacy `text_info.json` loader populated nothing). The rendered-PNG `file_name` is reconstructed
/// deterministically from `page_idx`+`uid` via [`persist::text_image_file_name`] — the SAME name the
/// doc's text flush (`write_text_image`) writes — so a later placement-save/flush round-trips. Pure
/// (no egui), so it is unit-testable. The new runtime starts with no GPU texture and is stale, so the
/// caller queues it for upload.
#[allow(clippy::too_many_arguments)]
fn text_runtime_from_doc_node(
    uid: &str,
    page_idx: usize,
    center_page_px: [f32; 2],
    user_scale: f32,
    angle_deg: f32,
    deform_mesh: Option<TypingOverlayDeformMesh>,
    mask_clip_enabled: bool,
    layer_idx: usize,
    render_data_json: Option<Value>,
    size_px: [usize; 2],
    source_rgba: Vec<u8>,
) -> TypingOverlayRuntime {
    TypingOverlayRuntime {
        uid: uid.to_string(),
        kind: TypingOverlayKind::Text,
        page_idx,
        center_page_px,
        mask_clip_enabled,
        layer_idx,
        user_scale,
        angle_deg,
        deform_mesh,
        file_name: crate::models::layer_model::persist::text_image_file_name(page_idx, uid),
        original_file_name: None,
        render_data_json,
        size_px,
        source_rgba,
        texture: None,
        display_texture_stale: true,
        last_texture_used_frame: 0,
    }
}

/// Builds an overlay runtime from a freshly-decoded legacy `text_info.json` entry. Fresh runtimes
/// start with no GPU texture and are stale, so the caller queues them for upload.
fn runtime_from_decoded(entry: TypingOverlayDecoded) -> TypingOverlayRuntime {
    TypingOverlayRuntime {
        uid: entry.uid,
        kind: entry.kind,
        page_idx: entry.page_idx,
        center_page_px: entry.center_page_px,
        mask_clip_enabled: entry.mask_clip_enabled,
        layer_idx: entry.layer_idx,
        user_scale: entry.user_scale,
        angle_deg: entry.angle_deg,
        deform_mesh: entry.deform_mesh,
        file_name: entry.file_name,
        original_file_name: entry.original_file_name,
        render_data_json: entry.render_data_json,
        size_px: entry.size_px,
        source_rgba: entry.rgba,
        texture: None,
        display_texture_stale: true,
        last_texture_used_frame: 0,
    }
}

/// MERGES freshly-loaded legacy overlays (`decoded`) INTO `existing` by `(uid, page_idx)` instead of
/// wholesale-replacing. CRITICAL for migrated chapters: their `text_info.json` is retired, so the loader
/// returns an EMPTY set; `sync_from_doc` may have already MATERIALIZED text runtimes from the doc on an
/// earlier frame (that path is not gated on `loading_rx`). A wholesale `self.overlays = decoded` would
/// then WIPE those doc-created runtimes the instant the loader completes → the user's intermittent
/// "text shows then vanishes" symptom. Merge semantics: a loaded entry whose (uid, page) already exists
/// REPLACES that entry (legacy data is authoritative for a legacy chapter); a new one is APPENDED; an
/// existing runtime whose uid is ABSENT from the loaded set is KEPT (doc-created on a migrated chapter).
/// Cross-chapter reset is handled separately by `ensure_loader_started`, which clears `overlays` at the
/// START of a chapter open — so a stale chapter's overlays never linger; this merge only governs the
/// COMPLETION within one open. Returns the indices of entries that need a texture upload (replaced or
/// appended), so the caller can queue exactly those.
fn merge_loaded_overlays(
    existing: &mut Vec<TypingOverlayRuntime>,
    decoded: Vec<TypingOverlayDecoded>,
) -> Vec<usize> {
    let mut touched: Vec<usize> = Vec::with_capacity(decoded.len());
    for entry in decoded {
        let runtime = runtime_from_decoded(entry);
        let idx = existing
            .iter()
            .position(|o| o.uid == runtime.uid && o.page_idx == runtime.page_idx);
        match idx {
            Some(i) => {
                existing[i] = runtime;
                touched.push(i);
            }
            None => {
                existing.push(runtime);
                touched.push(existing.len() - 1);
            }
        }
    }
    touched
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
    let overlay_uid = uuid::Uuid::new_v4().to_string();
    // Persistence is owned by the shared doc: the caller adds this overlay as a doc Text node and the
    // following placement save flushes the INLINE v3 payload to `layers.json`. The create path no
    // longer writes `text_info.json` (the doc is the sole text writer). The rendered PNG above is kept
    // on disk only as the create-job artifact; the doc flush writes its own uid-keyed `_text.png`.
    let _ = &layout_image_path;

    Ok(TypingOverlayDecoded {
        uid: overlay_uid,
        kind: TypingOverlayKind::Text,
        page_idx: request.page_idx,
        center_page_px: request.center_page_px,
        mask_clip_enabled: true,
        layer_idx: 0,
        user_scale,
        angle_deg: 0.0,
        deform_mesh: None,
        file_name,
        original_file_name: None,
        render_data_json: Some(request.render_data_json),
        size_px: [rendered.width as usize, rendered.height as usize],
        rgba: rendered.rgba,
        warnings: rendered.warnings,
    })
}

// Superseded by `render_and_store_created_raster` (external images are now raster layers). Kept for
// reference / potential "insert image as overlay" path.
#[allow(dead_code)]
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

    let render_data_json = default_render_data_for_image();
    let overlay_uid = uuid::Uuid::new_v4().to_string();
    // (Superseded path.) Persistence is owned by the shared doc; no `text_info.json` write here.
    let _ = &image_path;

    Ok(TypingOverlayDecoded {
        uid: overlay_uid,
        kind: TypingOverlayKind::Image,
        page_idx: request.page_idx,
        center_page_px: request.center_page_px,
        mask_clip_enabled: true,
        layer_idx: 0,
        user_scale: 1.0,
        angle_deg: 0.0,
        deform_mesh: None,
        file_name,
        original_file_name: None,
        render_data_json: Some(render_data_json),
        size_px: [width, height],
        rgba,
        warnings: Vec::new(),
    })
}

/// Стартовые render-data для image-оверлея: только пустой список эффектов.
/// Эффекты к сторонним картинкам применяются тем же pipeline, что и к растрированному тексту.
#[allow(dead_code)]
fn default_render_data_for_image() -> Value {
    json!({ "effects": [] })
}

/// Worker: loads an external image (clipboard/file) and persists it as a NEW raster layer node in
/// `layers.json` (via `persist::add_page_raster`), centered at `center_page_px`. Returns the page +
/// uid so the tab reloads its raster cache from disk and selects it. No text/image overlay is made.
fn render_and_store_created_raster(
    request: TypingCreateRasterRequest,
) -> Result<TypingCreatedRaster, String> {
    let (rgba, width, height) = match &request.source {
        TypingCreateImageSource::Clipboard => read_image_rgba_from_clipboard()?,
        TypingCreateImageSource::File(path) => read_image_rgba_from_file(path.as_path())?,
    };
    if width == 0 || height == 0 {
        return Err("Изображение нулевого размера.".to_string());
    }
    if rgba.len() != width.saturating_mul(height).saturating_mul(4) {
        return Err("Некорректный буфер RGBA изображения.".to_string());
    }
    let image = ColorImage::from_rgba_unmultiplied([width, height], &rgba);
    let name = match &request.source {
        TypingCreateImageSource::File(path) => path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Картинка".to_string()),
        TypingCreateImageSource::Clipboard => "Картинка".to_string(),
    };
    let uid = uuid::Uuid::new_v4().to_string();
    let transform = crate::models::layer_model::manifest::TransformRec {
        cx: request.center_page_px[0],
        cy: request.center_page_px[1],
        rotation: 0.0,
        scale: 1.0,
    };
    crate::models::layer_model::persist::add_page_raster(
        &request.layers_dir,
        request.fallback_dir.as_deref(),
        request.page_idx,
        &uid,
        &name,
        true,
        1.0,
        transform,
        &image,
    )?;
    Ok(TypingCreatedRaster {
        page_idx: request.page_idx,
        uid,
    })
}

/// Worker: renders a raster's effects chain from its ORIGINAL base PNG (non-destructive). Returns the
/// display image to show (the rendered result, or the base unchanged when the chain is empty) plus
/// the chain. The base is never modified, so effects stay reversible.
fn render_raster_effects(
    page_idx: usize,
    uid: String,
    base_file: String,
    primary: Option<PathBuf>,
    fallback: Option<PathBuf>,
    effects: Vec<Value>,
    base_in_memory: Option<ColorImage>,
) -> Result<TypingRasterEffectsResult, String> {
    // Prefer the resident doc's in-memory base; fall back to the on-disk base PNG when absent.
    let (base, source) = match base_in_memory {
        Some(img) => (img, "memory"),
        None => {
            let img = load_raster_base_png(&base_file, primary.as_deref(), fallback.as_deref())
                .ok_or_else(|| format!("Не найден исходный PNG растра «{base_file}»."))?;
            (img, "disk")
        }
    };
    crate::trace_log!(
        crate::trace::cat::RENDER,
        "render_raster_effects base source={} uid={} base_file={}",
        source,
        uid,
        base_file
    );
    if effects.is_empty() {
        return Ok(TypingRasterEffectsResult {
            page_idx,
            uid,
            display_image: base,
            effects,
        });
    }
    let effects_json = serde_json::to_string(&Value::Array(effects.clone()))
        .map_err(|e| format!("Эффекты растра: {e}"))?;
    let (rendered, _origin) =
        crate::models::layer_model::effects::apply_effects_to_color_image(&base, &effects_json)
            .map_err(|e| format!("Эффекты растра: {e}"))?;
    Ok(TypingRasterEffectsResult {
        page_idx,
        uid,
        display_image: rendered,
        effects,
    })
}

/// Loads a raster's base PNG by name, trying the unsaved dir then the committed fallback.
fn load_raster_base_png(file: &str, primary: Option<&Path>, fallback: Option<&Path>) -> Option<ColorImage> {
    for dir in [primary, fallback].into_iter().flatten() {
        let path = dir.join(file);
        if path.is_file()
            && let Ok(decoded) = image::open(&path)
        {
            let rgba = decoded.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            return Some(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()));
        }
    }
    None
}

/// Извлекает `effects_json` (как массив) из render-data оверлея для подачи в `apply_effects_to_image`.
fn effects_json_from_render_data(render_data: &Value) -> String {
    render_data
        .as_object()
        .and_then(|obj| obj.get("effects"))
        .and_then(Value::as_array)
        .map(|effects| Value::Array(effects.clone()))
        .and_then(|effects| serde_json::to_string(&effects).ok())
        .unwrap_or_default()
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
        image_original_file_name: None,
        is_image_effects: false,
        user_scale: request.user_scale.max(0.05),
        rotation_deg: request.rotation_deg,
        render_data_json: request.render_data_json,
        size_px: [rendered.width as usize, rendered.height as usize],
        rgba: rendered.rgba,
        warnings: rendered.warnings,
    }))
}

struct TypingEditImageEffectsRequest {
    token: u64,
    latest_token: Arc<AtomicU64>,
    overlay_idx: usize,
    // Текущий показываемый файл (исходник либо предыдущий `_fx`).
    file_name: String,
    // Исходник до эффектов, если он уже отделён от `file_name`.
    original_file_name: Option<String>,
    text_images_dir: PathBuf,
    // Read-fallback (сохранённая main-папка), если исходник ещё не скопирован в staging.
    fallback_text_images_dir: Option<PathBuf>,
    user_scale: f32,
    rotation_deg: f32,
    // render-data вида `{ "effects": [...] }`.
    render_data_json: Value,
}

/// Re-рендер image-оверлея: грузит исходник, применяет post-effects тем же pipeline, что и текст,
/// и сохраняет результат отдельным `_fx`-файлом, сохраняя исходную картинку нетронутой.
fn render_and_store_image_effects_overlay(
    request: TypingEditImageEffectsRequest,
) -> Result<Option<TypingEditOverlayResult>, String> {
    if request.latest_token.load(Ordering::Acquire) != request.token {
        return Ok(None);
    }

    // Исходник: отдельный original-файл, если он есть; иначе текущий показываемый файл является
    // исходным (эффекты ещё не применялись).
    let source_name = request
        .original_file_name
        .clone()
        .unwrap_or_else(|| request.file_name.clone());
    let primary_source_path = request.text_images_dir.join(&source_name);
    // Исходник предпочтительно из staging; если его там ещё нет — из сохранённой main-папки.
    let source_path = if primary_source_path.is_file() {
        primary_source_path
    } else if let Some(fallback) = request
        .fallback_text_images_dir
        .as_ref()
        .map(|dir| dir.join(&source_name))
        .filter(|path| path.is_file())
    {
        fallback
    } else {
        primary_source_path
    };
    let decoded = image::open(&source_path)
        .map_err(|err| {
            format!(
                "Не удалось открыть исходную картинку {}: {err}",
                source_path.display()
            )
        })?
        .to_rgba8();
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 {
        return Err("Исходная картинка нулевого размера.".to_string());
    }

    let effects_json = effects_json_from_render_data(&request.render_data_json);
    let has_effects = !effects_json_array_is_empty(&effects_json);

    let rendered = match apply_effects_to_image(
        decoded.into_raw(),
        width,
        height,
        effects_json.as_str(),
        Some((&request.latest_token, request.token)),
    ) {
        Ok(rendered) => rendered,
        Err(err) if err == "render_next render cancelled" => return Ok(None),
        Err(err) => return Err(err),
    };
    if rendered.width == 0 || rendered.height == 0 {
        return Err("Рендер эффектов вернул изображение нулевого размера.".to_string());
    }
    if request.latest_token.load(Ordering::Acquire) != request.token {
        return Ok(None);
    }

    // Когда эффекты есть — пишем отдельный `_fx`-файл, исходник остаётся как original-файл.
    // Когда эффектов нет — показываем исходник напрямую и подчищаем устаревший `_fx`-файл.
    let (display_file_name, new_original_file_name) = if has_effects {
        let fx_name = image_effects_fx_file_name(&source_name);
        let fx_path = request.text_images_dir.join(&fx_name);
        image::save_buffer(
            &fx_path,
            rendered.rgba.as_slice(),
            rendered.width,
            rendered.height,
            image::ColorType::Rgba8,
        )
        .map_err(|err| format!("Не удалось сохранить {}: {err}", fx_path.display()))?;
        (fx_name, Some(source_name))
    } else {
        // Если раньше был отдельный `_fx`-файл — удаляем его, возвращаясь к исходнику.
        if request.original_file_name.is_some() && request.file_name != source_name {
            let _ = fs::remove_file(request.text_images_dir.join(&request.file_name));
        }
        (source_name, None)
    };

    Ok(Some(TypingEditOverlayResult {
        token: request.token,
        overlay_idx: request.overlay_idx,
        file_name: display_file_name,
        image_original_file_name: new_original_file_name,
        is_image_effects: true,
        user_scale: request.user_scale.max(0.05),
        rotation_deg: request.rotation_deg,
        render_data_json: request.render_data_json,
        size_px: [rendered.width as usize, rendered.height as usize],
        rgba: rendered.rgba,
        warnings: rendered.warnings,
    }))
}

/// Имя `_fx`-файла, производное от имени исходной картинки (`name.png` -> `name_fx.png`).
fn image_effects_fx_file_name(source_name: &str) -> String {
    let path = Path::new(source_name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("png");
    format!("{stem}_fx.{ext}")
}

/// Истина, когда сериализованный массив эффектов пуст или отсутствует.
fn effects_json_array_is_empty(effects_json: &str) -> bool {
    let trimmed = effects_json.trim();
    if trimmed.is_empty() {
        return true;
    }
    serde_json::from_str::<Value>(trimmed)
        .ok()
        .and_then(|value| value.as_array().map(|arr| arr.is_empty()))
        .unwrap_or(true)
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

    // Сформированный текст (если задан) идёт в рендер вместо исходного, без
    // повторного авто-переноса.
    let formed_text = text_params
        .get("formed_text")
        .and_then(Value::as_str)
        .filter(|formed| !formed.trim().is_empty());
    let source_text = text_params
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let uses_formed = formed_text.is_some();
    let render_text = formed_text.unwrap_or(source_text).to_string();

    let font_size_px = text_params
        .get("font_size_px")
        .and_then(value_as_f32)
        .unwrap_or(24.0)
        .max(1.0);
    // Единое представление `px-или-%`: новый строковый ключ либо устаревшая пара.
    let line_spacing = read_render_param_px_or_percent(
        text_params,
        "line_spacing",
        "line_spacing_px",
        "line_spacing_percent",
        PxOrPercent::percent(50.0),
    );
    let kerning = read_render_param_px_or_percent(
        text_params,
        "kerning",
        "kerning_px",
        "kerning_percent",
        PxOrPercent::percent(0.0),
    );
    let glyph_height = read_render_param_px_or_percent(
        text_params,
        "glyph_height",
        "",
        "glyph_height_percent",
        PxOrPercent::percent(100.0),
    );
    let glyph_width = read_render_param_px_or_percent(
        text_params,
        "glyph_width",
        "",
        "glyph_width_percent",
        PxOrPercent::percent(100.0),
    );

    Some(TextRenderParams {
        text: render_text,
        text_color: text_params
            .get("text_color")
            .and_then(parse_rgba_value)
            .unwrap_or([0, 0, 0, 255]),
        font_path,
        available_inline_fonts: Vec::new(),
        font_size_px,
        line_spacing_px: line_spacing.as_px_percent().0,
        line_spacing_percent: line_spacing.as_px_percent().1,
        kerning_mode: text_params
            .get("kerning_mode")
            .and_then(Value::as_str)
            .and_then(parse_kerning_mode_config_str)
            .unwrap_or(KerningMode::Auto),
        kerning_px: kerning.as_px_percent().0,
        kerning_percent: kerning.as_px_percent().1,
        glyph_height_percent: glyph_height.as_percent_of(font_size_px),
        glyph_width_percent: glyph_width.as_percent_of(font_size_px),
        width_px: text_params
            .get("width_px")
            .and_then(value_as_f32)
            .map(|value| value.round().max(1.0) as u32)
            .unwrap_or(TEXT_RENDER_DATA_FALLBACK_WIDTH_PX),
        align: HorizontalAlign::from_config(
            text_params.get("align").and_then(Value::as_str),
            text_params.get("align_bias").and_then(value_as_f32),
        ),
        // Global vector rotation of the whole block; absent in old projects -> 0.
        global_rotation_deg: text_params
            .get("global_rotation_deg")
            .and_then(value_as_f32)
            .unwrap_or(0.0),
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
        text_wrap_mode: if uses_formed {
            TextWrapMode::None
        } else {
            text_params
                .get("text_wrap_mode")
                .and_then(Value::as_str)
                .and_then(parse_text_wrap_mode_config_str)
                .unwrap_or(TextWrapMode::Aggressive)
        },
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
        anti_aliasing: text_params
            .get("anti_aliasing")
            .and_then(Value::as_str)
            .and_then(parse_anti_aliasing_config_str)
            .unwrap_or(AntiAliasingMode::Strong),
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


/// Parse a serialized kerning-mode config string. Accepts the current tokens
/// (`"fixed"`/`"auto"`/`"optical"`) and the legacy `"metric"` token (font-pair
/// kerning), which maps to [`KerningMode::Auto`] so old projects render
/// identically. Returns `None` for unknown/missing values.
fn parse_kerning_mode_config_str(raw: &str) -> Option<KerningMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "fixed" => Some(KerningMode::Fixed),
        "auto" | "metric" => Some(KerningMode::Auto),
        "optical" => Some(KerningMode::Optical),
        _ => None,
    }
}

/// Прочитать параметр `px-или-%`: сначала новый строковый ключ-токен, затем
/// устаревшие отдельные ключи `*_px`/`*_percent` (с приоритетом пикселей).
fn read_render_param_px_or_percent(
    obj: &serde_json::Map<String, Value>,
    token_key: &str,
    legacy_px_key: &str,
    legacy_percent_key: &str,
    default: PxOrPercent,
) -> PxOrPercent {
    if let Some(value) = obj.get(token_key) {
        if let Some(text) = value.as_str() {
            if let Some(parsed) = PxOrPercent::parse(text) {
                return parsed;
            }
        } else if let Some(number) = value_as_f32(value) {
            // Голое число в ключе-токене встречается лишь в легаси `line_spacing`,
            // где оно означало пиксели.
            return PxOrPercent::px(number);
        }
    }
    let legacy_px = obj.get(legacy_px_key).and_then(value_as_f32);
    let legacy_percent = obj.get(legacy_percent_key).and_then(value_as_f32);
    if legacy_px.is_some() || legacy_percent.is_some() {
        return PxOrPercent::from_legacy_pair(
            legacy_px.unwrap_or(0.0),
            legacy_percent.unwrap_or(0.0),
        );
    }
    default
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

/// Parse a persisted anti-aliasing token; `None` for unknown text.
fn parse_anti_aliasing_config_str(raw: &str) -> Option<AntiAliasingMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "none" => Some(AntiAliasingMode::None),
        "sharp" => Some(AntiAliasingMode::Sharp),
        "crisp" => Some(AntiAliasingMode::Crisp),
        "strong" => Some(AntiAliasingMode::Strong),
        "smooth" => Some(AntiAliasingMode::Smooth),
        _ => None,
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
    match job.export_format {
        TypingExportFormat::Png => {
            let (base_rgba, base_w, base_h) = flatten_typing_export_page_rgba(&job)?;
            image::save_buffer(
                &job.output_path,
                &base_rgba,
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
        TypingExportFormat::Psd => {
            let bytes = super::psd_export::export_typing_single_page_psd(&job)?;
            fs::write(&job.output_path, &bytes).map_err(|err| {
                format!(
                    "Не удалось сохранить страницу {}: {err}",
                    job.output_path.display()
                )
            })
        }
    }
}

/// Загружает страницу-источник, накладывает клин и все оверлеи (так же, как делает
/// PNG-экспорт) и возвращает финальный плоский RGBA8 буфер + размеры страницы.
/// Используется и PNG-веткой, и PSD-веткой (для composite image_data).
/// Сравнение оверлеев по порядку наложения (от низа стопки к верху).
/// Приоритет: меньший `layer_idx` ниже; внутри одного слоя — чем ниже на
/// картинке (больший `center_y`), тем выше в стопке. Используется и для отрисовки
/// в редакторе, и для композиции при экспорте, чтобы UI и PNG/PSD совпадали.
// `overlay_stack_cmp` (the old layer_idx + page-Y auto-order) was retired: text is now ordered by the
// unified manual band-Z everywhere (draw, interaction, export), like rasters.

pub(super) fn flatten_typing_export_page_rgba(
    job: &TypingExportPageJob,
) -> Result<(Vec<u8>, usize, usize), String> {
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

    // PS raster layers to composite, normalized to a common shape (straight RGBA + band-Z). PREFER the
    // on-screen snapshot taken from the doc projection (`job.rasters`, matching the canvas exactly);
    // FALL BACK to a disk read of `layers.json` only when no snapshot was provided (back-compat). Then
    // interleave rasters with text/image overlays in the SAME band-Z order the live canvas uses.
    use crate::models::layer_model::ordering::Band;
    use crate::models::layer_model::persist;
    struct RasterDraw {
        visible: bool,
        opacity: f32,
        transform: crate::models::layer_model::manifest::TransformRec,
        deform: Option<crate::models::layer_model::manifest::DeformRec>,
        rgba: Vec<u8>,
        size_px: [usize; 2],
        band_z: u32,
        mask_clip_enabled: bool,
    }

    // On-disk page bands: needed for OVERLAY (text) band-Z in both paths, and for raster band-Z in the
    // disk-fallback path (the snapshot carries raster band-Z directly).
    let disk_bands = match job.layers_primary_dir.as_deref() {
        Some(primary) => {
            persist::load_page_bands(primary, job.layers_fallback_dir.as_deref(), job.page_idx)
        }
        None => Vec::new(),
    };

    let raster_draws: Vec<RasterDraw> = if !job.rasters.is_empty() {
        job.rasters
            .iter()
            .map(|r| RasterDraw {
                visible: r.visible,
                opacity: r.opacity,
                transform: r.transform,
                deform: r.deform.clone(),
                rgba: r.rgba.clone(),
                size_px: r.size_px,
                band_z: r.band_z,
                mask_clip_enabled: r.mask_clip_enabled,
            })
            .collect()
    } else if let Some(primary) = job.layers_primary_dir.as_deref() {
        let fb = job.layers_fallback_dir.as_deref();
        let loaded = persist::load_page_rasters(primary, fb, job.page_idx)
            .unwrap_or_else(|err| {
                eprintln!(
                    "WARN typing::flatten_export_failed_to_load_rasters page={} err={err}",
                    job.page_idx
                );
                persist::PageRasters {
                    groups: Vec::new(),
                    layers: Vec::new(),
                }
            })
            .layers;
        let raster_band_z = |uid: &str| -> u32 {
            for band in &disk_bands {
                if let Band::Raster { uid: u, z } = band
                    && u == uid
                {
                    return *z;
                }
            }
            disk_bands.len() as u32
        };
        loaded
            .into_iter()
            .map(|l| {
                let rgba: Vec<u8> = l
                    .image
                    .pixels
                    .iter()
                    .flat_map(|p| p.to_srgba_unmultiplied())
                    .collect();
                let band_z = raster_band_z(&l.uid);
                RasterDraw {
                    visible: l.visible,
                    opacity: l.opacity,
                    transform: l.transform,
                    deform: l.deform,
                    size_px: l.image.size,
                    rgba,
                    band_z,
                    mask_clip_enabled: l.mask_clip.unwrap_or(false),
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    let overlay_z = |uid: &str, layer_idx: usize| -> u32 {
        for band in &disk_bands {
            if let Band::PinnedText { uid: u, z } = band
                && u == uid
            {
                return *z;
            }
        }
        let layer_idx_u32 = u32::try_from(layer_idx).unwrap_or(u32::MAX);
        for band in &disk_bands {
            if let Band::TextGroup {
                layer_idx: li, z, ..
            } = band
                && *li == layer_idx_u32
            {
                return *z;
            }
        }
        disk_bands.len() as u32
    };

    enum Item {
        Raster(usize),
        Overlay(usize),
    }
    // Source BOTH raster and overlay band-Z from the SAME place to avoid divergence: when the in-memory
    // raster snapshot is present, the overlay snapshot's `band_z` (captured from the same `bands_by_page`)
    // is authoritative; otherwise fall back to the disk band lookup. Tie-break keeps raster=0 below
    // overlay=1 at the same Z (text on top of a same-Z raster).
    let use_snapshot_z = !job.rasters.is_empty();
    let mut items: Vec<(u32, u32, Item)> = Vec::new();
    for (i, r) in raster_draws.iter().enumerate() {
        items.push((r.band_z, 0, Item::Raster(i)));
    }
    for (i, ov) in job.overlays.iter().enumerate() {
        if ov.page_idx != job.page_idx {
            continue;
        }
        let z = if use_snapshot_z { ov.band_z } else { overlay_z(&ov.uid, ov.layer_idx) };
        items.push((z, 1, Item::Overlay(i)));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    for (_, _, item) in &items {
        match item {
            Item::Overlay(i) => {
                let overlay = &job.overlays[*i];
                let deform_mesh = export_overlay_deform_mesh_for_page(overlay, [base_w, base_h]);
                let clipped_rgba = export_overlay_clipped_rgba(job, overlay, &deform_mesh);
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
            Item::Raster(i) => {
                let r = &raster_draws[*i];
                if !r.visible {
                    continue;
                }
                let [w, h] = r.size_px;
                if w == 0 || h == 0 || r.rgba.len() != w * h * 4 {
                    continue;
                }
                // Honor the deform mesh when present (matching the canvas), else build the affine quad.
                let mesh = if let Some(d) = &r.deform {
                    TypingOverlayDeformMesh {
                        cols: d.cols,
                        rows: d.rows,
                        points_px: d.points_px.clone(),
                    }
                } else {
                    let (s, c) = r.transform.rotation.sin_cos();
                    let hw = w as f32 * 0.5 * r.transform.scale;
                    let hh = h as f32 * 0.5 * r.transform.scale;
                    let rot = |dx: f32, dy: f32| {
                        [
                            r.transform.cx + dx * c - dy * s,
                            r.transform.cy + dx * s + dy * c,
                        ]
                    };
                    // Row-major TL, TR, BL, BR. Construct the mesh directly (not via
                    // `TypingOverlayDeformMesh::new`) to skip its page clamping — a raster may extend
                    // off-page.
                    let points_px = vec![rot(-hw, -hh), rot(hw, -hh), rot(-hw, hh), rot(hw, hh)];
                    TypingOverlayDeformMesh {
                        cols: 2,
                        rows: 2,
                        points_px,
                    }
                };
                // Mask-clip ON → clip the raster to the page mask through its mesh (same as on-screen
                // `clipped_image` and the text-overlay export clip), so it exports WITHOUT pixels outside
                // the mask. Falls back to unclipped only if there is no mask snapshot.
                let mut rgba = if r.mask_clip_enabled {
                    job.mask
                        .as_ref()
                        .and_then(|mask| {
                            export_clip_overlay_rgba_if_needed(mask, [w, h], r.rgba.as_slice(), &mesh)
                        })
                        .unwrap_or_else(|| r.rgba.clone())
                } else {
                    r.rgba.clone()
                };
                if r.opacity < 1.0 {
                    for px in rgba.chunks_exact_mut(4) {
                        px[3] = (px[3] as f32 * r.opacity).round().clamp(0.0, 255.0) as u8;
                    }
                }
                composite_overlay_mesh_over_page(base_rgba, [base_w, base_h], &rgba, [w, h], &mesh);
            }
        }
    }

    Ok((base.into_raw(), base_w, base_h))
}

/// Применяет маску обрезки к оверлею, если она включена и доступна; иначе
/// возвращает исходный RGBA. Общая логика для PNG- и PSD-экспорта.
pub(super) fn export_overlay_clipped_rgba(
    job: &TypingExportPageJob,
    overlay: &TypingExportOverlaySnapshot,
    deform_mesh: &TypingOverlayDeformMesh,
) -> Vec<u8> {
    if overlay.mask_clip_enabled {
        job.mask
            .as_ref()
            .and_then(|mask| {
                export_clip_overlay_rgba_if_needed(
                    mask,
                    overlay.size_px,
                    overlay.source_rgba.as_slice(),
                    deform_mesh,
                )
            })
            .unwrap_or_else(|| overlay.source_rgba.clone())
    } else {
        overlay.source_rgba.clone()
    }
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

pub(super) fn composite_overlay_full_image_over(
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

pub(super) fn composite_overlay_at_page_position_over(
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

pub(super) fn composite_overlay_mesh_over_page(
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
            // Raw page-pixel corners (NO clamping to the page rect): the triangle rasterizer below
            // already clips pixel iteration to the page bounds, so clamping the vertices would only
            // distort geometry that extends off-page — e.g. a scaled-up raster, making its scale
            // appear ignored. Off-page parts are correctly clipped by the rasterizer's bbox.
            let p00 = deform_mesh.point(col, row);
            let p10 = deform_mesh.point(col + 1, row);
            let p01 = deform_mesh.point(col, row + 1);
            let p11 = deform_mesh.point(col + 1, row + 1);

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

pub(super) fn direct_overlay_blit_top_left_px(overlay: &TypingExportOverlaySnapshot) -> Option<[i32; 2]> {
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

pub(super) fn export_overlay_deform_mesh_for_page(
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
fn build_storage_overlay_entry(
    uid: &str,
    kind: TypingOverlayKind,
    page_idx: usize,
    file_name: &str,
    original_file_name: Option<&str>,
    center_page_px: [f32; 2],
    mask_clip_enabled: bool,
    layer_idx: usize,
    rotation_deg: f32,
    scale: f32,
    deform_mesh: Option<TypingOverlayDeformMesh>,
    render_data: Option<Value>,
) -> Value {
    let mut out = serde_json::Map::<String, Value>::new();
    out.insert("uid".to_string(), Value::String(uid.to_string()));
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
    // Для image-оверлеев `file` хранит картинку ПОСЛЕ эффектов (она же идёт в показ/экспорт),
    // а `image_original_file` — исходную импортированную картинку, чтобы эффекты можно было
    // переприменять и отменять без потери качества.
    if let Some(original) = original_file_name.filter(|name| !name.is_empty() && *name != file_name)
    {
        out.insert(
            "image_original_file".to_string(),
            Value::String(original.to_string()),
        );
    }
    // Serialize position/rotation/scale through the shared encoder (single encode point: center →
    // img_x/y, rad → rotation_deg, scale). The caller supplies rotation in DEGREES, so convert to the
    // canonical radians `TransformRec` the encoder consumes.
    crate::models::layer_model::text_payload::encode_transform_fields(
        &crate::models::layer_model::manifest::TransformRec {
            cx: center_page_px[0],
            cy: center_page_px[1],
            rotation: rotation_deg.to_radians(),
            scale: scale.max(0.01),
        },
        &mut out,
    );
    out.insert(
        "mask_clip_enabled".to_string(),
        Value::from(mask_clip_enabled),
    );
    out.insert("layer_idx".to_string(), Value::from(layer_idx as u64));
    if let Some(mesh) = deform_mesh {
        // Serialize the deform mesh through the shared encoder (single encode point), converting the
        // runtime mesh to the canonical `DeformRec` first.
        let rec = crate::models::layer_model::manifest::DeformRec {
            cols: mesh.cols,
            rows: mesh.rows,
            points_px: mesh.points_px.clone(),
        };
        out.insert(
            "deform_mesh".to_string(),
            crate::models::layer_model::text_payload::encode_deform_mesh(&rec),
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

    let mut params = json!({
        "text": text,
        "text_color": text_color,
        "font_path": font_path,
        "font_label": font_label,
        "font_size_px": obj.get("font_size_px").and_then(value_as_f32).or_else(|| obj.get("font_size").and_then(value_as_f32)).or_else(|| obj.get("size").and_then(value_as_f32)).unwrap_or(24.0).max(1.0),
        "line_spacing": read_render_param_px_or_percent(obj, "line_spacing", "line_spacing_px", "line_spacing_percent", PxOrPercent::percent(50.0)).to_token(),
        "kerning": read_render_param_px_or_percent(obj, "kerning", "kerning_px", "kerning_percent", PxOrPercent::percent(0.0)).to_token(),
        "glyph_height": read_render_param_px_or_percent(obj, "glyph_height", "", "glyph_height_percent", PxOrPercent::percent(100.0)).to_token(),
        "glyph_width": read_render_param_px_or_percent(obj, "glyph_width", "", "glyph_width_percent", PxOrPercent::percent(100.0)).to_token(),
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
    });

    // Современные поля панели, которых не было в легаси-схеме. Нормализатор строит
    // `text_params` по белому списку, поэтому без явного проброса они терялись при
    // загрузке проекта (напр. `formed_text` — сформированный текст «продвинутой
    // формы»). Сохраняем как есть, если присутствуют; иначе панель подставит свои
    // дефолты при чтении.
    if let Some(map) = params.as_object_mut() {
        for key in [
            "formed_text",
            "kerning_mode",
            "hanging_punctuation",
            "new_line_after_sentence",
            "trim_extra_spaces",
            "vertical_line_direction",
            // Точное смещение выравнивания (слайдер лево↔право). Легаси-строка
            // `align` сохраняется отдельно для совместимости/PSD-экспорта, но
            // непрерывное значение живёт только здесь.
            "align_bias",
        ] {
            if let Some(value) = obj.get(key) {
                map.insert(key.to_string(), value.clone());
            }
        }
    }
    params
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
    // В легаси-схеме `line_spacing` — пиксели, `line_spacing_percent` — проценты.
    let line_spacing = PxOrPercent::from_legacy_pair(
        overlay_param_f32(style, obj, "line_spacing").unwrap_or(4.0),
        overlay_param_f32(style, obj, "line_spacing_percent").unwrap_or(50.0),
    );
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
            "line_spacing": line_spacing.to_token(),
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

// Legacy per-entry geometry decoding (`transform_uv` quad, `deform_mesh`, `img_u`/`img_v`/`u`/`v`
// position, `angle`/`user_scale` aliases) now lives in the shared `text_payload` codec
// (`decode_overlay_placement` / `decode_deform_mesh`) — the single source of truth so the typing tab
// and the doc resolve old chapters identically. The former `parse_transform_uv` / `parse_deform_mesh`
// / `overlay_center_page_px_from_storage` here were removed.

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
            "line_spacing": "50%",
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
    // Geometry decode through the SINGLE shared codec (center, rotation, scale, deform).
    let placement =
        crate::models::layer_model::text_payload::decode_overlay_placement(obj, page_size);
    let center_page_px = [placement.transform.cx, placement.transform.cy];
    let rotation_deg = placement.transform.rotation.to_degrees();
    let scale = placement.transform.scale;
    let deform_mesh = placement
        .deform
        .as_ref()
        .and_then(|rec| TypingOverlayDeformMesh::from_deform_rec(rec, page_size))
        .map(|mesh| normalize_deform_mesh_resolution(&mesh, page_size));
    let mask_clip_enabled = obj
        .get("mask_clip_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let layer_idx = obj
        .get("layer_idx")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0);
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
        Some(parse_image_overlay_render_data(obj))
    };
    let original_file_name = if kind == TypingOverlayKind::Image {
        parse_overlay_original_file_name(obj)
    } else {
        None
    };

    // Preserve an existing stable id, or mint one so pre-uid overlays acquire it on this rewrite.
    let uid = obj
        .get("uid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    Some(build_storage_overlay_entry(
        &uid,
        kind,
        page_idx,
        file_name.as_str(),
        original_file_name.as_deref(),
        center_page_px,
        mask_clip_enabled,
        layer_idx,
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

    // Geometry decode (center, rotation, scale, deform incl. transform_uv) goes through the SINGLE
    // shared codec so the typing tab and the doc resolve legacy formats identically.
    let placement =
        crate::models::layer_model::text_payload::decode_overlay_placement(obj, page_size);
    let center_page_px = [placement.transform.cx, placement.transform.cy];
    let user_scale = placement.transform.scale;
    let angle_deg = placement.transform.rotation.to_degrees();
    let deform_mesh = placement
        .deform
        .as_ref()
        .and_then(|rec| TypingOverlayDeformMesh::from_deform_rec(rec, page_size))
        .map(|mesh| normalize_deform_mesh_resolution(&mesh, page_size));
    let mask_clip_enabled = obj
        .get("mask_clip_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let layer_idx = obj
        .get("layer_idx")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0);
    let render_data_json = if kind == TypingOverlayKind::Text {
        let fallback_width_px = legacy_fallback_width_px(obj);
        parse_overlay_render_data_json(obj, fallback_width_px)
    } else {
        Some(parse_image_overlay_render_data(obj))
    };
    let original_file_name = if kind == TypingOverlayKind::Image {
        parse_overlay_original_file_name(obj)
    } else {
        None
    };

    let uid = obj
        .get("uid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    Some(TypingOverlayDecoded {
        uid,
        kind,
        page_idx,
        center_page_px,
        mask_clip_enabled,
        layer_idx,
        user_scale,
        angle_deg,
        deform_mesh,
        file_name,
        original_file_name,
        render_data_json,
        size_px: [w as usize, h as usize],
        rgba: decoded.into_raw(),
        warnings: Vec::new(),
    })
}

/// Парсит имя файла исходной картинки image-оверлея (`image_original_file`), очищая путь до имени.
fn parse_overlay_original_file_name(obj: &serde_json::Map<String, Value>) -> Option<String> {
    obj.get("image_original_file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|file| Path::new(file).file_name().and_then(|name| name.to_str()))
        .map(|name| name.to_string())
}

/// Парсит render-data image-оверлея (только список эффектов). Отсутствие/мусор → пустые эффекты.
fn parse_image_overlay_render_data(obj: &serde_json::Map<String, Value>) -> Value {
    let effects = obj
        .get("render_data")
        .and_then(Value::as_object)
        .and_then(|render_data| render_data.get("effects"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    json!({ "effects": effects })
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

// The cross-entry legacy migration (absolute ribbon `x`/`y`+`region_w`/`region_h`, top-left
// `u`/`v`) and its helpers now live in the shared `text_payload::migrate_overlay_entries` codec,
// so the typing loader and the doc loader normalize old chapters identically before per-entry
// decode. The former `overlay_entry_is_modern` / `legacy_overlay_page_*` / `legacy_overlay_png_size`
// / `migrate_legacy_text_overlays` here were removed.

fn load_typing_overlays_from_dir(
    text_images_dir: &Path,
    fallback_dirs: &[&Path],
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

    // Migrate the cross-entry legacy placement families (absolute ribbon x/y, top-left u/v) up front
    // via the SHARED codec so the per-entry decode below — and the doc loader — see modern
    // center-anchored `img_idx`/`img_u`/`img_v`. The PNG footprint (top-left case) is resolved from the
    // text dirs (the model codec owns no image IO).
    let fallback_png_dir = fallback_dirs.first().copied();
    let migrated_items = crate::models::layer_model::text_payload::migrate_overlay_entries(
        items,
        page_sizes,
        |obj| {
            obj.get("file")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .and_then(|file| Path::new(file).file_name().and_then(|n| n.to_str()))
                .map(|name| {
                    let dims = image::image_dimensions(text_images_dir.join(name))
                        .ok()
                        .or_else(|| {
                            fallback_png_dir
                                .and_then(|d| image::image_dimensions(d.join(name)).ok())
                        });
                    match dims {
                        Some((w, h)) => (w as f32, h as f32),
                        None => (0.0, 0.0),
                    }
                })
                .unwrap_or((0.0, 0.0))
        },
    );

    let mut decoded_out = Vec::new();

    for item in migrated_items.iter() {
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

        if let Some(decoded) = normalized.as_object().and_then(|obj| {
            // Try the primary dir first, then each fallback in order — covering PNGs left in the
            // committed `layers/` dir or the legacy `text_images/` dir after a metadata migration.
            decode_overlay_from_storage_entry(text_images_dir, obj, page_size).or_else(|| {
                fallback_dirs
                    .iter()
                    .find_map(|d| decode_overlay_from_storage_entry(d, obj, page_size))
            })
        }) {
            decoded_out.push(decoded);
        }
    }

    // NOTE: `text_info.json` is now READ-ONLY legacy. The in-memory normalization above feeds the
    // session; it is NOT written back. The doc persists the overlays inline into `layers.json` on the
    // next flush, after which this legacy file is no longer read (the doc loads from the inline payload).
    Ok(decoded_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_composites_raster_from_disk_fallback() {
        // Disk-fallback path (no snapshot in the job): rasters are read from `layers.json`, including the
        // migrated layout (committed-only page reached via the per-page fallback).
        use crate::models::layer_model::persist;
        let dir = std::env::temp_dir().join(format!("typ_flat_disk_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layers = dir.join("layers");
        std::fs::create_dir_all(&layers).unwrap();
        let base = dir.join("page.png");
        image::save_buffer(&base, &vec![0u8; 20 * 20 * 4], 20, 20, image::ColorType::Rgba8).unwrap();
        let red = ColorImage::filled([10, 10], Color32::from_rgba_unmultiplied(255, 0, 0, 255));
        persist::add_page_raster(
            &layers, None, 0, "r0", "R", true, 1.0,
            crate::models::layer_model::manifest::TransformRec { cx: 10.0, cy: 10.0, rotation: 0.0, scale: 1.0 },
            &red,
        ).unwrap();
        let job = TypingExportPageJob {
            page_idx: 0,
            page_path: base,
            output_path: dir.join("out.png"),
            clean_overlay_path: None,
            clean_overlay_rgba: None,
            overlays: Vec::new(),
            rasters: Vec::new(), // force the disk-read path
            mask: None,
            export_format: TypingExportFormat::Png,
            layers_primary_dir: Some(layers.clone()),
            layers_fallback_dir: None,
        };
        let (rgba, w, h) = flatten_typing_export_page_rgba(&job).unwrap();
        assert_eq!([w, h], [20, 20]);
        let center = (10 * 20 + 10) * 4;
        assert_eq!(&rgba[center..center + 4], &[255, 0, 0, 255], "disk raster composited at center");

        // Migrated layout: primary=unsaved (manifest exists, lacks page 0), raster on committed page 0.
        let committed = dir.join("committed");
        let unsaved = dir.join("unsaved");
        std::fs::create_dir_all(&committed).unwrap();
        std::fs::create_dir_all(&unsaved).unwrap();
        persist::add_page_raster(
            &committed, None, 0, "rc", "R", true, 1.0,
            crate::models::layer_model::manifest::TransformRec { cx: 10.0, cy: 10.0, rotation: 0.0, scale: 1.0 },
            &red,
        ).unwrap();
        persist::add_page_raster(
            &unsaved, None, 5, "rs", "R", true, 1.0,
            crate::models::layer_model::manifest::TransformRec { cx: 10.0, cy: 10.0, rotation: 0.0, scale: 1.0 },
            &red,
        ).unwrap();
        let base2 = dir.join("page2.png");
        image::save_buffer(&base2, &vec![0u8; 20 * 20 * 4], 20, 20, image::ColorType::Rgba8).unwrap();
        let job2 = TypingExportPageJob {
            page_idx: 0,
            page_path: base2,
            output_path: dir.join("out2.png"),
            clean_overlay_path: None,
            clean_overlay_rgba: None,
            overlays: Vec::new(),
            rasters: Vec::new(),
            mask: None,
            export_format: TypingExportFormat::Png,
            layers_primary_dir: Some(unsaved.clone()),
            layers_fallback_dir: Some(committed.clone()),
        };
        let (rgba2, _, _) = flatten_typing_export_page_rgba(&job2).unwrap();
        assert_eq!(&rgba2[center..center + 4], &[255, 0, 0, 255], "committed-only raster composited (migrated)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flatten_composites_raster_from_on_screen_snapshot() {
        // PRIMARY Bug B fix: the export composites the ON-SCREEN raster snapshot (`job.rasters`) even when
        // the disk dirs would yield NOTHING (no `layers.json` at all) — proving the bake no longer depends
        // on a disk re-read that can silently drop the raster.
        let dir = std::env::temp_dir().join(format!("typ_flat_snap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("page.png");
        image::save_buffer(&base, &vec![0u8; 20 * 20 * 4], 20, 20, image::ColorType::Rgba8).unwrap();
        // A 10x10 RED straight-alpha snapshot centered at (10,10), no disk dirs.
        let snap = TypingExportRasterSnapshot {
            visible: true,
            opacity: 1.0,
            transform: crate::models::layer_model::manifest::TransformRec { cx: 10.0, cy: 10.0, rotation: 0.0, scale: 1.0 },
            deform: None,
            rgba: vec![255, 0, 0, 255].repeat(10 * 10),
            size_px: [10, 10],
            band_z: 0,
            mask_clip_enabled: false,
        };
        let job = TypingExportPageJob {
            page_idx: 0,
            page_path: base,
            output_path: dir.join("out.png"),
            clean_overlay_path: None,
            clean_overlay_rgba: None,
            overlays: Vec::new(),
            rasters: vec![snap],
            mask: None,
            export_format: TypingExportFormat::Png,
            layers_primary_dir: None, // no disk source at all
            layers_fallback_dir: None,
        };
        let (rgba, w, h) = flatten_typing_export_page_rgba(&job).unwrap();
        assert_eq!([w, h], [20, 20]);
        let center = (10 * 20 + 10) * 4;
        assert_eq!(&rgba[center..center + 4], &[255, 0, 0, 255], "on-screen snapshot raster composited");
        // A hidden snapshot is skipped.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flatten_clips_mask_clip_enabled_raster_in_export() {
        // ITEM B: a mask-clip-ENABLED raster must export CLIPPED — pixels over an inactive page mask are
        // absent (transparent), matching the on-screen `clipped_image`. An unclipped raster is unchanged.
        use crate::tabs::typing::mask::TypingMaskExportPage;
        let dir = std::env::temp_dir().join(format!("typ_flat_maskclip_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("page.png");
        // 20x20 OPAQUE black base (alpha 255), so a clipped raster reveals the base, not transparency.
        let base_px: Vec<u8> = (0..20 * 20).flat_map(|_| [0u8, 0, 0, 255]).collect();
        image::save_buffer(&base, &base_px, 20, 20, image::ColorType::Rgba8).unwrap();

        // A 10x10 RED raster centered at (10,10) → covers page px [5..15]x[5..15].
        let make_snap = |mask_clip: bool| TypingExportRasterSnapshot {
            visible: true,
            opacity: 1.0,
            transform: crate::models::layer_model::manifest::TransformRec { cx: 10.0, cy: 10.0, rotation: 0.0, scale: 1.0 },
            deform: None,
            rgba: vec![255, 0, 0, 255].repeat(10 * 10),
            size_px: [10, 10],
            band_z: 0,
            mask_clip_enabled: mask_clip,
        };
        // Page mask ACTIVE only on the LEFT half (x < 10) of the 20x20 page.
        let mask = TypingMaskExportPage {
            width: 20,
            height: 20,
            data: (0..20 * 20).map(|i| if (i % 20) < 10 { 255 } else { 0 }).collect(),
        };
        let make_job = |snap: TypingExportRasterSnapshot, mask: Option<TypingMaskExportPage>| TypingExportPageJob {
            page_idx: 0,
            page_path: base.clone(),
            output_path: dir.join("out.png"),
            clean_overlay_path: None,
            clean_overlay_rgba: None,
            overlays: Vec::new(),
            rasters: vec![snap],
            mask,
            export_format: TypingExportFormat::Png,
            layers_primary_dir: None,
            layers_fallback_dir: None,
        };

        // CLIPPED export: left-half page pixels keep the raster (red); right-half are clipped → base (black).
        let (rgba, _, _) = flatten_typing_export_page_rgba(&make_job(make_snap(true), Some(mask.clone()))).unwrap();
        let px = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 20 + x) * 4;
            [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
        };
        assert_eq!(px(7, 10), [255, 0, 0, 255], "raster kept where mask is active (left half)");
        assert_eq!(px(13, 10), [0, 0, 0, 255], "raster CLIPPED where mask is inactive (right half)");

        // UNCLIPPED (mask_clip OFF): the same right-half pixel keeps the raster.
        let (rgba2, _, _) = flatten_typing_export_page_rgba(&make_job(make_snap(false), Some(mask))).unwrap();
        let i = (10 * 20 + 13) * 4;
        assert_eq!(&rgba2[i..i + 4], &[255, 0, 0, 255], "unclipped raster unchanged on the right half");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_char_budget_floors_at_min_and_grows_with_width() {
        let cp = 8.0; // representative char width px
        // At/below the min available width (5 chars fit) → exactly the min (5).
        assert_eq!(preview_char_budget(5.0 * cp, cp), 5, "5 chars fit → 5");
        assert_eq!(preview_char_budget(0.0, cp), 5, "no room → still min 5");
        assert_eq!(preview_char_budget(-50.0, cp), 5, "negative (overhead > width) → min 5");
        assert_eq!(preview_char_budget(3.0 * cp, cp), 5, "only 3 fit but floor is 5");
        // Grows by 1 per ~char_px wider.
        assert_eq!(preview_char_budget(6.0 * cp, cp), 6, "6 chars wide → 6");
        assert_eq!(preview_char_budget(6.0 * cp + cp / 2.0, cp), 6, "partial char floors down");
        assert_eq!(preview_char_budget(12.0 * cp, cp), 12, "12 chars wide → 12");
        // Degenerate inputs → min (helper guards non-finite available + non-positive char_px).
        assert_eq!(preview_char_budget(1000.0, 0.0), 5, "zero char width → min 5");
        assert_eq!(preview_char_budget(f32::INFINITY, cp), 5, "non-finite available → min 5");
        assert_eq!(preview_char_budget(f32::NAN, cp), 5, "NaN available → min 5");
    }

    #[test]
    fn text_preview_label_appends_dots_to_three_accounting_for_existing() {
        // First `max_chars` CHARACTERS (Unicode), trailing dot-equivalents brought to >= 3 (regular dot
        // = 1, ellipsis '…' = 3), accounting for what's already there. These use max_chars = 5 (the min).
        assert_eq!(text_preview_label("Привет мир", 5), "Приве...", "no trailing dots → append 3");
        assert_eq!(text_preview_label("Да.", 5), "Да...", "1 existing dot → append 2");
        assert_eq!(text_preview_label("Эй..", 5), "Эй...", "2 existing dots → append 1");
        // "Стоп..." → first5 = "Стоп." (С,т,о,п,.), 1 trailing dot → append 2.
        assert_eq!(text_preview_label("Стоп...", 5), "Стоп...", "first-5 truncation keeps one dot → append 2");
        // Ellipsis char counts as 3 → append none.
        assert_eq!(text_preview_label("Всё…", 5), "Всё…", "ellipsis = 3 → append none");
        // "Хм….." → first5 = Х,м,…,.,. → trailing .,. then … = 1+1+3 = 5 → append none.
        assert_eq!(text_preview_label("Хм…..", 5), "Хм…..", "ellipsis + 2 dots → already >= 3");
        // Short text (< 5 chars), not truncated, still gets dots.
        assert_eq!(text_preview_label("Да", 5), "Да...");
        // Empty (after trim) → empty preview (caller shows just "Текст").
        assert_eq!(text_preview_label("", 5), "");
        assert_eq!(text_preview_label("   ", 5), "", "whitespace-only trims to empty");
        // Leading whitespace is trimmed before taking the first 5 chars.
        assert_eq!(text_preview_label("  Привет", 5), "Приве...");
        // Cyrillic char-boundary safety: exactly 5 chars taken, no byte-panic on multibyte text.
        let long = "Текстовая строка";
        assert_eq!(long.chars().count() > 5, true);
        assert_eq!(text_preview_label(long, 5), "Текст...");
        // A 5-char prefix that is ALL dots stays as-is (>= 3).
        assert_eq!(text_preview_label(".....", 5), ".....");

        // Larger max_chars → more preview chars before the dots (wider panel). "Длинноеслово" has no
        // space in the first 10, so the prefix is exactly its first 10 chars.
        assert_eq!(text_preview_label("Длинноеслово", 10), "Длинноесло...", "first 10 chars + dots");
        // A text SHORTER than max_chars still gets the dots.
        assert_eq!(text_preview_label("Привет", 10), "Привет...", "short-than-max still gets dots");
        // Dot accounting still applies with a larger budget.
        assert_eq!(text_preview_label("Конец..", 10), "Конец...", "2 trailing dots → append 1");
    }

    #[test]
    fn order_unified_layer_rows_interleaves_by_z_overlay_above_raster_on_ties() {
        use TypingLayerRow::*;
        // Rows with band-Z; bool = raster_below_overlay (true for rasters).
        // overlay@5, raster@5 (tie → overlay above), raster@3, overlay@1.
        let rows = vec![
            (Overlay(0), 5, false),
            (Raster(0), 5, true),
            (Raster(1), 3, true),
            (Overlay(1), 1, false),
        ];
        // TOP-first (Z desc): overlay@5, raster@5 (overlay wins the tie → listed first), raster@3, overlay@1.
        assert_eq!(
            order_unified_layer_rows(rows),
            vec![Overlay(0), Raster(0), Raster(1), Overlay(1)]
        );

        // A raster strictly ABOVE a text (text can sit below a raster now): raster@7 first.
        let rows2 = vec![(Overlay(2), 2, false), (Raster(2), 7, true)];
        assert_eq!(order_unified_layer_rows(rows2), vec![Raster(2), Overlay(2)]);

        // Empty input → empty output.
        assert!(order_unified_layer_rows(Vec::new()).is_empty());
    }

    #[test]
    fn unified_topmost_pointer_target_picks_by_z_overlay_wins_ties() {
        let t = TypingPointerTarget::Overlay;
        let r = TypingPointerTarget::Raster;
        let n = TypingPointerTarget::None;
        // Text above raster → text wins.
        assert_eq!(unified_topmost_pointer_target(Some(5), Some(2)), t);
        // Raster above text → raster wins (text can now sit BELOW a raster).
        assert_eq!(unified_topmost_pointer_target(Some(2), Some(5)), r);
        // Equal band-Z → overlay wins (text draws above a raster at the same band).
        assert_eq!(unified_topmost_pointer_target(Some(3), Some(3)), t);
        // Only one present → that one.
        assert_eq!(unified_topmost_pointer_target(Some(0), None), t);
        assert_eq!(unified_topmost_pointer_target(None, Some(0)), r);
        // Neither under the pointer → None.
        assert_eq!(unified_topmost_pointer_target(None, None), n);
    }

    #[test]
    fn topmost_raster_target_skips_selected_and_picks_topmost() {
        // The normal-mode raster interaction creates the SELECTED raster's response unconditionally, so
        // the hit-test for the OTHER rasters must skip the selected idx (else egui gets a duplicate Id).
        // It must also pick the TOPMOST (last in bottom-to-top `entries`) when quads overlap.
        let image_rect = Rect::from_min_size(Pos2::new(0.0, 0.0), egui::vec2(1000.0, 1000.0));
        let quad = |cx: f32, cy: f32| -> [Pos2; 4] {
            [
                Pos2::new(cx - 20.0, cy - 20.0),
                Pos2::new(cx + 20.0, cy - 20.0),
                Pos2::new(cx + 20.0, cy + 20.0),
                Pos2::new(cx - 20.0, cy + 20.0),
            ]
        };
        // Two overlapping rasters at the same center: idx 0 (bottom), idx 1 (top).
        let entries = vec![
            (0usize, quad(100.0, 100.0), Pos2::new(100.0, 100.0)),
            (1usize, quad(100.0, 100.0), Pos2::new(100.0, 100.0)),
        ];
        let p = Some(Pos2::new(100.0, 100.0));

        // No exclusion → topmost (idx 1) wins.
        let t = topmost_raster_target(&entries, p, image_rect, None).expect("hit");
        assert_eq!(t.0, 1, "topmost (last) raster wins on overlap");

        // Exclude the selected top raster → the hit-test falls through to idx 0 (no duplicate Id).
        let t = topmost_raster_target(&entries, p, image_rect, Some(1)).expect("hit");
        assert_eq!(t.0, 0, "selected idx skipped, next raster targeted");

        // Pointer far outside every quad → no target.
        assert!(topmost_raster_target(&entries, Some(Pos2::new(900.0, 900.0)), image_rect, None).is_none());

        // No pointer → no target.
        assert!(topmost_raster_target(&entries, None, image_rect, None).is_none());

        // Excluding the only raster under the pointer → no target.
        let single = vec![(5usize, quad(100.0, 100.0), Pos2::new(100.0, 100.0))];
        assert!(topmost_raster_target(&single, p, image_rect, Some(5)).is_none());
    }

    #[test]
    fn color_image_to_rgba_round_trips_straight_alpha() {
        // BUG A: `color_image_to_rgba` must return STRAIGHT (un-premultiplied) alpha so it round-trips
        // through `ColorImage::from_rgba_unmultiplied`. With the old `to_array()` (premultiplied), white
        // (255,255,255,128) came back as (128,128,128,128) — graying antialiased stroke edges.
        let straight: Vec<u8> = vec![255, 255, 255, 128, 200, 100, 50, 64, 10, 20, 30, 255, 0, 0, 0, 0];
        let image = ColorImage::from_rgba_unmultiplied([4, 1], &straight);
        let out = color_image_to_rgba(&image);
        assert_eq!(out.len(), straight.len());
        // Alpha round-trips exactly; RGB is recovered within the unavoidable premultiply→u8→unpremultiply
        // quantization (≈255/alpha), which the OLD `to_array()` (premultiplied) would blow past entirely.
        for px in 0..4 {
            let a = straight[px * 4 + 3] as i32;
            assert_eq!(out[px * 4 + 3], straight[px * 4 + 3], "alpha exact at pixel {px}");
            // Worst-case round-trip error ≈ ceil(255 / (2*alpha)).
            let tol = if a == 0 { 0 } else { ((255 + 2 * a - 1) / (2 * a)).max(1) };
            for ch in 0..3 {
                let (g, o) = (out[px * 4 + ch] as i32, straight[px * 4 + ch] as i32);
                // A fully-transparent pixel's RGB is undefined post-premult; skip it.
                if a == 0 {
                    continue;
                }
                assert!(
                    (g - o).abs() <= tol,
                    "pixel {px} ch {ch}: round-tripped {g} != original {o} (±{tol}, alpha {a})"
                );
            }
        }
        // The CRITICAL guard: un-premultiplied white (255,255,255,128) must NOT come back grayed to ~128
        // (the old `to_array()` premultiplied bug). With the fix it stays white.
        assert!(out[0] >= 254 && out[1] >= 254 && out[2] >= 254, "white stays white, not premultiplied gray");
    }

    #[test]
    fn image_effects_fx_file_name_appends_fx_suffix() {
        assert_eq!(image_effects_fx_file_name("image_p0_1.png"), "image_p0_1_fx.png");
        assert_eq!(image_effects_fx_file_name("photo.jpeg"), "photo_fx.jpeg");
        // Без расширения — по умолчанию png.
        assert_eq!(image_effects_fx_file_name("noext"), "noext_fx.png");
    }

    #[test]
    fn raster_identity_deform_seed_is_a_valid_grid_over_the_affine_quad() {
        // Entering raster transform mode seeds an identity deform from the affine transform via
        // `default_deform_mesh_for_page` (the same fn `ensure_raster_deform_mesh` uses for a raster
        // with no deform). It must produce a valid cols×rows grid whose corners equal the affine quad.
        let page_size = [200, 100];
        let center = [100.0_f32, 50.0];
        let size = [40usize, 20];
        let mesh = default_deform_mesh_for_page(center, size, 1.0, 0.0, page_size);
        assert_eq!(mesh.cols, TEXT_OVERLAY_DEFORM_SURFACE_COLS);
        assert_eq!(mesh.rows, TEXT_OVERLAY_DEFORM_SURFACE_ROWS);
        assert_eq!(mesh.points_px.len(), mesh.cols * mesh.rows);
        // The 4 grid corners are the affine image quad corners (centered, unrotated, unit scale).
        let tl = mesh.point(0, 0);
        let br = mesh.point(mesh.cols - 1, mesh.rows - 1);
        assert!((tl[0] - (center[0] - size[0] as f32 * 0.5)).abs() < 1e-2, "TL x = cx - w/2");
        assert!((tl[1] - (center[1] - size[1] as f32 * 0.5)).abs() < 1e-2, "TL y = cy - h/2");
        assert!((br[0] - (center[0] + size[0] as f32 * 0.5)).abs() < 1e-2, "BR x = cx + w/2");
        assert!((br[1] - (center[1] + size[1] as f32 * 0.5)).abs() < 1e-2, "BR y = cy + h/2");
    }

    #[test]
    fn perspective_corner_drag_moves_the_dragged_corner_fully() {
        // The raster perspective transform mode drags a mesh corner via `apply_perspective_corner_drag`
        // (shared with overlays): the dragged corner moves by the full delta; the opposite corner is
        // untouched.
        let page_size = [500, 500];
        let mesh = default_deform_mesh_for_page([250.0, 250.0], [100, 100], 1.0, 0.0, page_size);
        let tl_before = mesh.point(0, 0);
        let br_before = mesh.point(mesh.cols - 1, mesh.rows - 1);
        // Drag handle 0 (top-left) by (+10, +20) page px.
        let dragged = apply_perspective_corner_drag(&mesh, 0, [10.0, 20.0], page_size);
        let tl_after = dragged.point(0, 0);
        let br_after = dragged.point(dragged.cols - 1, dragged.rows - 1);
        assert!((tl_after[0] - (tl_before[0] + 10.0)).abs() < 1e-3, "TL fully follows the drag x");
        assert!((tl_after[1] - (tl_before[1] + 20.0)).abs() < 1e-3, "TL fully follows the drag y");
        assert!((br_after[0] - br_before[0]).abs() < 1e-3, "opposite corner unaffected x");
        assert!((br_after[1] - br_before[1]).abs() < 1e-3, "opposite corner unaffected y");
    }

    #[test]
    fn effects_json_array_emptiness_is_detected() {
        assert!(effects_json_array_is_empty(""));
        assert!(effects_json_array_is_empty("   "));
        assert!(effects_json_array_is_empty("[]"));
        assert!(!effects_json_array_is_empty(r#"[{"effect":"stroke"}]"#));
        // Некорректный JSON трактуем как «пусто», чтобы не падать на мусоре.
        assert!(effects_json_array_is_empty("not-json"));
    }

    #[test]
    fn raster_selection_tracks_by_uid_across_a_reorder() {
        // FIX 2 (wrong-layer): `selected_raster_idx` / `transform_mode_raster_idx` /
        // `raster_drag_state.raster_idx` are POSITIONS into `raster_layers_by_page[page]`, which
        // `sync_from_doc` rebuilds in z-order on every reproject. After a raster reorder the SAME position
        // points at a DIFFERENT raster — so transform/delete would hit the wrong one. The remap at the end
        // of `sync_from_doc` must keep these tracking the SAME raster by uid.
        use crate::models::layer_model::layer_doc::LayerDoc;
        use crate::models::layer_model::persist;
        use std::collections::HashMap;

        let dir = std::env::temp_dir().join(format!("typ_rsel_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tf = crate::models::layer_model::manifest::TransformRec {
            cx: 1.0,
            cy: 1.0,
            rotation: 0.0,
            scale: 1.0,
        };
        let pic = ColorImage::filled([2, 2], Color32::WHITE);
        // Add order is bottom-to-top: r0 (bottom), r1 (top).
        persist::add_page_raster(&dir, None, 0, "r0", "Bottom", true, 1.0, tf, &pic).unwrap();
        persist::add_page_raster(&dir, None, 0, "r1", "Top", true, 1.0, tf, &pic).unwrap();

        let mut doc = LayerDoc::new();
        let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
        page_sizes.insert(0, [100, 100]);
        doc.ensure_page_loaded(0, &dir, None, &page_sizes).unwrap();

        let mut layer = TypingTextOverlayLayer::default();
        layer.sync_from_doc(0, &doc);
        let rasters = &layer.raster_layers_by_page[&0];
        assert_eq!(rasters.len(), 2);
        // Projected bottom-to-top: index 0 == r0, index 1 == r1.
        let r0_pos = rasters.iter().position(|l| l.uid == "r0").unwrap();
        let r1_pos = rasters.iter().position(|l| l.uid == "r1").unwrap();
        assert_eq!(r0_pos, 0);

        // Select r0 (bottom), enter transform mode on it, and start a drag tracking it.
        layer.selected_raster_idx = Some(r0_pos);
        layer.transform_mode_raster_idx = Some(r0_pos);
        layer.raster_drag_state = Some(TypingRasterDragState {
            page_idx: 0,
            raster_idx: r0_pos,
            mode: TypingRasterDragMode::Move,
            pointer_start_scene: Pos2::ZERO,
            start_transform: tf,
            start_pointer_angle_rad: 0.0,
            start_mesh: None,
        });

        // Reorder r0 UP past r1 in the doc, then reproject.
        assert!(doc.reorder_node_one(0, "r0", true));
        layer.sync_from_doc(0, &doc);

        let rasters = &layer.raster_layers_by_page[&0];
        let r0_new = rasters.iter().position(|l| l.uid == "r0").unwrap();
        assert_ne!(r0_new, r0_pos, "the reorder actually moved r0 to a new position");
        // All three trackers now point at r0's NEW position (the SAME raster), not the stale index.
        assert_eq!(layer.selected_raster_idx, Some(r0_new), "selection follows r0 by uid");
        assert_eq!(layer.transform_mode_raster_idx, Some(r0_new), "transform mode follows r0 by uid");
        assert_eq!(
            layer.raster_drag_state.as_ref().map(|d| d.raster_idx),
            Some(r0_new),
            "drag state follows r0 by uid"
        );
        // The stale position now holds r1 — proof a positional tracker would have retargeted.
        assert_eq!(rasters[r0_pos].uid, "r1");
        let _ = r1_pos;

        // A deleted raster clears the trackers instead of pointing at a neighbour.
        layer.selected_raster_idx = Some(r0_new);
        assert!(doc.remove_node(0, "r0"));
        layer.sync_from_doc(0, &doc);
        assert_eq!(layer.selected_raster_idx, None, "selection cleared when its raster is gone");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_from_doc_materializes_text_runtimes_for_a_migrated_chapter() {
        // LIVE BUG: after the eager migration `text_info.json` is retired (.bak), so the legacy disk
        // loader populates NO `self.overlays`. `sync_from_doc` must MATERIALIZE a text runtime from each
        // doc Text node that has no local runtime (reconcile-OR-CREATE), else the typing tab shows no
        // text while PS + the doc carry it. A second sync must NOT duplicate them (reconcile path).
        use crate::models::layer_model::layer_doc::LayerDoc;
        use crate::models::layer_model::persist;
        use std::collections::HashMap;

        let dir = std::env::temp_dir().join(format!("typ_migtext_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Seed two inline v3 text nodes on page 0 with real rendered PNGs (no text_info.json — migrated).
        let seed_text = |uid: &str, cx: f32, cy: f32| -> persist::TextPayloadOut {
            let img = ColorImage::filled([4, 3], Color32::GREEN);
            let file = persist::write_text_image(&dir, 0, uid, &img).unwrap();
            persist::TextPayloadOut {
                uid: uid.into(),
                name: uid.into(),
                z: 1,
                layer_idx: 2,
                pinned: false,
                visible: true,
                opacity: 1.0,
                group_uid: None,
                pinned_by_group: false,
                payload_uid: uid.into(),
                render_data: json!({ "text": uid }),
                transform: crate::models::layer_model::manifest::TransformRec {
                    cx,
                    cy,
                    rotation: 0.0,
                    scale: 1.0,
                },
                deform: None,
                rendered_file: Some(file),
                mask_clip: None,
            }
        };
        persist::write_page_text_payload(&dir, None, 0, &[seed_text("ta", 10.0, 20.0), seed_text("tb", 30.0, 40.0)])
            .unwrap();

        let mut doc = LayerDoc::new();
        let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
        page_sizes.insert(0, [100, 100]);
        doc.ensure_page_loaded(0, &dir, None, &page_sizes).unwrap();
        assert_eq!(
            doc.page(0).unwrap().nodes.iter().filter(|n| n.is_text()).count(),
            2,
            "doc loaded both text nodes"
        );

        // Migrated-chapter state: NO local overlay runtimes.
        let mut layer = TypingTextOverlayLayer::default();
        assert!(layer.overlays.is_empty());

        layer.sync_from_doc(0, &doc);

        // Both text nodes materialized as runtimes with correct projected fields.
        assert_eq!(layer.overlays.len(), 2, "sync_from_doc created a runtime per doc text node");
        let ta = layer.overlays.iter().find(|o| o.uid == "ta").expect("ta runtime");
        assert_eq!(ta.kind, TypingOverlayKind::Text);
        assert_eq!(ta.page_idx, 0);
        assert_eq!(ta.center_page_px, [10.0, 20.0]);
        assert!((ta.angle_deg - 0.0).abs() < 1e-6);
        assert!((ta.user_scale - 1.0).abs() < 1e-6);
        assert_eq!(ta.layer_idx, 2, "text-group axis carried from the node");
        assert_eq!(ta.size_px, [4, 3], "doc image projected");
        assert_eq!(ta.source_rgba.len(), 4 * 3 * 4, "rgba populated from the doc image");
        assert_eq!(
            ta.file_name,
            persist::text_image_file_name(0, "ta"),
            "deterministic rendered-PNG name (round-trips with the doc flush)"
        );
        assert!(ta.texture.is_none() && ta.display_texture_stale, "queued for upload this frame");
        // Newly-created runtimes are queued for texture upload.
        assert_eq!(layer.pending_upload_indices.len(), 2, "both runtimes queued for upload");

        // A second sync reconciles (no duplicates).
        layer.sync_from_doc(0, &doc);
        assert_eq!(layer.overlays.len(), 2, "second sync does NOT duplicate runtimes");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_interleave_doc_text_survives_empty_loader_completion() {
        // End-to-end interleave the unit test missed: a migrated chapter materializes text via
        // `sync_from_doc`, THEN the loader completes with an empty set. The doc text must SURVIVE.
        use crate::models::layer_model::layer_doc::LayerDoc;
        use crate::models::layer_model::persist;
        use std::collections::HashMap;

        let dir = std::env::temp_dir().join(format!("typ_interleave_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let img = ColorImage::filled([4, 3], Color32::GREEN);
        let file = persist::write_text_image(&dir, 0, "ta", &img).unwrap();
        let payload = persist::TextPayloadOut {
            uid: "ta".into(),
            name: "ta".into(),
            z: 1,
            layer_idx: 0,
            pinned: false,
            visible: true,
            opacity: 1.0,
            group_uid: None,
            pinned_by_group: false,
            payload_uid: "ta".into(),
            render_data: json!({ "text": "ta" }),
            transform: crate::models::layer_model::manifest::TransformRec {
                cx: 10.0,
                cy: 20.0,
                rotation: 0.0,
                scale: 1.0,
            },
            deform: None,
            rendered_file: Some(file),
            mask_clip: None,
        };
        persist::write_page_text_payload(&dir, None, 0, &[payload]).unwrap();

        let mut doc = LayerDoc::new();
        let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
        page_sizes.insert(0, [100, 100]);
        doc.ensure_page_loaded(0, &dir, None, &page_sizes).unwrap();

        let mut layer = TypingTextOverlayLayer::default();
        // 1) Early frame: doc materializes the text runtime (loader still in flight).
        layer.sync_from_doc(0, &doc);
        assert_eq!(layer.overlays.len(), 1, "doc-created the text runtime");

        // 2) Loader completes with an EMPTY decoded set (migrated chapter) — drive the exact merge step
        //    `poll_loader` runs. The doc-created runtime must NOT be wiped.
        let touched = merge_loaded_overlays(&mut layer.overlays, Vec::new());
        assert!(touched.is_empty());
        assert_eq!(layer.overlays.len(), 1, "doc text SURVIVES the empty loader completion (race fixed)");
        assert_eq!(layer.overlays[0].uid, "ta");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn decoded_text_overlay(uid: &str, page_idx: usize, center: [f32; 2]) -> TypingOverlayDecoded {
        TypingOverlayDecoded {
            uid: uid.into(),
            kind: TypingOverlayKind::Text,
            page_idx,
            center_page_px: center,
            mask_clip_enabled: false,
            layer_idx: 0,
            user_scale: 1.0,
            angle_deg: 0.0,
            deform_mesh: None,
            file_name: crate::models::layer_model::persist::text_image_file_name(page_idx, uid),
            original_file_name: None,
            render_data_json: None,
            size_px: [2, 2],
            rgba: vec![0u8; 2 * 2 * 4],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn loader_completion_merge_does_not_wipe_doc_created_runtimes() {
        // CRITICAL RACE (the intermittent "text shows then vanishes, sometimes half"): on a MIGRATED
        // chapter `sync_from_doc` materializes text runtimes from the doc on an early frame, then the
        // loader thread completes with an EMPTY decoded set (no `text_info.json`). The old wholesale
        // `self.overlays = decoded` WIPED the doc-created runtimes. The merge must leave them intact.
        let mut overlays: Vec<TypingOverlayRuntime> = vec![
            text_runtime_from_doc_node("ta", 0, [10.0, 20.0], 1.0, 0.0, None, false, 1, None, [4, 3], vec![0u8; 4 * 3 * 4]),
            text_runtime_from_doc_node("tb", 0, [30.0, 40.0], 1.0, 0.0, None, false, 1, None, [4, 3], vec![0u8; 4 * 3 * 4]),
        ];

        // Loader completes with an EMPTY set (migrated chapter).
        let touched = merge_loaded_overlays(&mut overlays, Vec::new());
        assert!(touched.is_empty(), "empty load touches nothing");
        assert_eq!(overlays.len(), 2, "doc-created runtimes SURVIVE an empty loader completion");
        assert!(overlays.iter().any(|o| o.uid == "ta"));
        assert!(overlays.iter().any(|o| o.uid == "tb"));
    }

    #[test]
    fn loader_completion_merge_replaces_same_uid_without_duplicating() {
        // LEGACY/dup case: a doc-created runtime with uid "ta" exists (from the race), and the loader
        // returns the SAME uid "ta" (plus a brand-new "tc"). The merge must REPLACE "ta" in place (no
        // duplicate) and APPEND "tc".
        let mut overlays: Vec<TypingOverlayRuntime> = vec![text_runtime_from_doc_node(
            "ta", 0, [10.0, 20.0], 1.0, 0.0, None, false, 0, None, [4, 3], vec![0u8; 4 * 3 * 4],
        )];

        let touched = merge_loaded_overlays(
            &mut overlays,
            vec![decoded_text_overlay("ta", 0, [99.0, 88.0]), decoded_text_overlay("tc", 0, [1.0, 2.0])],
        );

        assert_eq!(overlays.len(), 2, "same-uid REPLACED in place (no dup), new uid APPENDED");
        let ta = overlays.iter().find(|o| o.uid == "ta").unwrap();
        assert_eq!(ta.center_page_px, [99.0, 88.0], "loaded entry replaced the doc-created one");
        assert_eq!(overlays.iter().filter(|o| o.uid == "ta").count(), 1, "no duplicate ta");
        assert!(overlays.iter().any(|o| o.uid == "tc"), "new loaded overlay appended");
        // Both the replaced and the appended entry are flagged for upload.
        assert_eq!(touched.len(), 2);
        // Same uid on a DIFFERENT page is NOT treated as a match (page-scoped key).
        let mut o2 = vec![text_runtime_from_doc_node(
            "ta", 1, [5.0, 6.0], 1.0, 0.0, None, false, 0, None, [4, 3], vec![0u8; 4 * 3 * 4],
        )];
        merge_loaded_overlays(&mut o2, vec![decoded_text_overlay("ta", 0, [7.0, 8.0])]);
        assert_eq!(o2.len(), 2, "same uid on a different page is a distinct runtime");
    }

    #[test]
    fn image_overlay_render_data_round_trips_effects() {
        let effects = json!([{ "effect": "stroke", "width_px": 4 }]);
        let render_data = json!({ "effects": effects.clone() });
        let entry = build_storage_overlay_entry(
            "test-uid",
            TypingOverlayKind::Image,
            0,
            "image_p0_1_fx.png",
            Some("image_p0_1.png"),
            [10.0, 20.0],
            true,
            0,
            0.0,
            1.0,
            None,
            Some(render_data),
        );
        let obj = entry.as_object().expect("entry must be an object");
        assert_eq!(
            obj.get("image_original_file").and_then(Value::as_str),
            Some("image_p0_1.png")
        );
        let parsed = parse_image_overlay_render_data(obj);
        assert_eq!(
            effects_json_from_render_data(&parsed),
            serde_json::to_string(&effects).unwrap()
        );
        assert_eq!(
            parse_overlay_original_file_name(obj).as_deref(),
            Some("image_p0_1.png")
        );
    }

    #[test]
    fn image_overlay_entry_omits_original_when_same_as_file() {
        // Когда исходник совпадает с показываемым файлом, дублирующий ключ не пишем.
        let entry = build_storage_overlay_entry(
            "test-uid",
            TypingOverlayKind::Image,
            0,
            "image_p0_1.png",
            Some("image_p0_1.png"),
            [0.0, 0.0],
            true,
            0,
            0.0,
            1.0,
            None,
            Some(default_render_data_for_image()),
        );
        let obj = entry.as_object().expect("entry must be an object");
        assert!(!obj.contains_key("image_original_file"));
    }

    fn shape_variant_test_params(text_shape: TextShape) -> TextRenderParams {
        TextRenderParams {
            text: "Просто без елок".to_string(),
            text_color: [0, 0, 0, 255],
            font_path: std::path::PathBuf::from("font.ttf"),
            available_inline_fonts: Vec::new(),
            font_size_px: 24.0,
            line_spacing_px: 4.0,
            line_spacing_percent: 50.0,
            kerning_mode: KerningMode::Auto,
            kerning_px: 0.0,
            kerning_percent: 0.0,
            glyph_height_percent: 100.0,
            glyph_width_percent: 100.0,
            width_px: 120,
            align: HorizontalAlign::CENTER,
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
            anti_aliasing: AntiAliasingMode::Strong,
            global_rotation_deg: 0.0,
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

    #[test]
    fn storage_normalization_preserves_formed_text_and_modern_fields() {
        let raw = json!({
            "schema_version": 2,
            "text_params": {
                "text": "Ты станешь выше и сильнее",
                "font_path": "/tmp/font.ttf",
                "width_px": 120,
                "formed_text": "Ты\nстанешь выше\nи сильнее",
                "kerning_px": 3.0,
                "hanging_punctuation": true,
                "new_line_after_sentence": true
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
            text_params.get("formed_text").and_then(Value::as_str),
            Some("Ты\nстанешь выше\nи сильнее"),
            "formed_text must survive normalization on project load"
        );
        // Устаревший `kerning_px` мигрирует в единый строковый ключ `kerning`.
        assert_eq!(
            text_params.get("kerning").and_then(Value::as_str),
            Some("3.00")
        );
        assert_eq!(
            text_params.get("hanging_punctuation").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            text_params
                .get("new_line_after_sentence")
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    fn text_bubble(id: i64, u: f32, v: f32, translation: &str) -> Bubble {
        Bubble {
            id,
            img_idx: 0,
            img_u: u,
            img_v: v,
            side: None,
            bubble_class: None,
            bubble_type: None,
            text: translation.to_string(),
            original_text: String::new(),
            extra: serde_json::Map::new(),
        }
    }

    /// Builds an image bubble whose red rect spans the whole page and whose `text_areas` carry the
    /// given anchors/translations. Area 0 mirrors its text into the legacy `text` field, matching
    /// the persisted contract.
    fn image_bubble_with_areas(id: i64, areas: &[((f32, f32), &str)]) -> Bubble {
        let mut extra = serde_json::Map::new();
        extra.insert("image_source_type".to_string(), Value::from("external"));
        // Red image-area rect spanning the whole page, in the persisted {p1,p2} object form.
        extra.insert(
            "rect_coords".to_string(),
            json!({
                "p1": {"img_u": 0.0, "img_v": 0.0},
                "p2": {"img_u": 1.0, "img_v": 1.0},
            }),
        );
        let items: Vec<Value> = areas
            .iter()
            .map(|((au, av), text)| {
                json!({
                    "rect": [au - 0.02, av - 0.02, au + 0.02, av + 0.02],
                    "anchor": [au, av],
                    "original": "",
                    "description": "",
                    "translation": text,
                })
            })
            .collect();
        extra.insert("text_areas".to_string(), Value::Array(items));
        let primary = areas.first().map(|(_, text)| *text).unwrap_or_default();
        Bubble {
            id,
            img_idx: 0,
            img_u: areas.first().map(|((u, _), _)| *u).unwrap_or(0.5),
            img_v: areas.first().map(|((_, v), _)| *v).unwrap_or(0.5),
            side: None,
            bubble_class: Some("image".to_string()),
            bubble_type: None,
            text: primary.to_string(),
            original_text: String::new(),
            extra,
        }
    }

    #[test]
    fn selection_seeds_text_from_each_image_area_anchor() {
        let page_rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 100.0));
        // One image bubble with three areas at distinct anchors.
        let bubbles = vec![image_bubble_with_areas(
            1,
            &[
                ((0.2, 0.2), "first"),
                ((0.5, 0.5), "second"),
                ((0.8, 0.8), "third"),
            ],
        )];

        // A small selection around the second area's anchor (50,50) must seed the second area's
        // text, not only area 0's. This is the regression: previously only `img_u/img_v` (area 0)
        // was considered, so later areas never matched a selection.
        let around = |u: f32, v: f32| {
            Rect::from_center_size(scene_from_uv(page_rect, u, v), Vec2::splat(6.0))
        };
        assert_eq!(
            pick_bubble_text_for_selection(&bubbles, 0, around(0.2, 0.2), page_rect),
            Some("first".to_string())
        );
        assert_eq!(
            pick_bubble_text_for_selection(&bubbles, 0, around(0.5, 0.5), page_rect),
            Some("second".to_string())
        );
        assert_eq!(
            pick_bubble_text_for_selection(&bubbles, 0, around(0.8, 0.8), page_rect),
            Some("third".to_string())
        );
    }

    #[test]
    fn selection_picks_closest_anchor_and_skips_empty_text() {
        let page_rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 100.0));
        let bubbles = vec![
            text_bubble(1, 0.3, 0.3, "plain"),
            image_bubble_with_areas(2, &[((0.31, 0.31), ""), ((0.6, 0.6), "img-area")]),
        ];

        // Selection covers the plain bubble and the empty image area 0; the empty area is skipped
        // and the closest non-empty anchor (the plain bubble) wins.
        let selection = Rect::from_min_max(
            scene_from_uv(page_rect, 0.25, 0.25),
            scene_from_uv(page_rect, 0.35, 0.35),
        );
        assert_eq!(
            pick_bubble_text_for_selection(&bubbles, 0, selection, page_rect),
            Some("plain".to_string())
        );

        // A selection that contains no anchor returns None.
        let empty = Rect::from_min_max(
            scene_from_uv(page_rect, 0.9, 0.05),
            scene_from_uv(page_rect, 0.98, 0.12),
        );
        assert_eq!(
            pick_bubble_text_for_selection(&bubbles, 0, empty, page_rect),
            None
        );
    }

    // Legacy ribbon/page-index migration tests moved to `models::layer_model::text_payload` together
    // with the `migrate_overlay_entries` logic (the single shared codec).
}
