/*
FILE HEADER (tabs/typing/tab.rs)
- Назначение: состояние вкладки `Текст` на основе `CanvasView` с read-only оверлеями и
  интерактивной деформацией поверх общей high-res surface + созданием новых текстовых оверлеев
  + бинарной маской обрезки страниц.
- Структура: этот файл — КОРЕНЬ модуля вкладки. Он держит модель данных (все `struct`/`enum`,
  включая `TypingTabState`, `TypingTextOverlayLayer`, `TypingOverlayRuntime`, `TypingRasterLayer`),
  публичный фасад `TypingTabState` + `Default`, реализацию `impl CanvasHooks for TypingHooks` и
  объявления подмодулей. Логика (методы и свободные функции) вынесена в дочерние подмодули
  `tab/` (см. `MODULE_README.md` → «Files and submodules» для карты). Дочерние модули —
  потомки `tab`, поэтому читают приватные поля модели напрямую; вынесенные методы/функции —
  `pub(super)` (или `pub(in crate::tabs::typing)`, если их зовёт typing-сосед вроде `panel.rs`).
  Ниже описан общий контракт вкладки; конкретные реализации ищите в подмодулях каталога `tab/`.
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
    с consume wheel-события до `CanvasView`, чтобы не скроллить холст; когда курсор
    поверх любой панели (Foreground-слой над холстом по z-order), обработчик уступает
    событие виджету панели и шрифт холста не меняется),
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
use super::render_next::{FontContentSet, FontProvider};
use super::render_next::types::{
    AntiAliasingMode, HorizontalAlign, KerningMode, PxOrPercent, TEXT_FORMULA_USER_VAR_COUNT,
    TextDrawnLinesLayoutParams,
    TextFormulaLayoutParams, TextLayoutMode, TextLineMode, TextRenderParams,
    TextRenderShapeCompareParams, TextShape, TextVectorLine, TextVectorLineDistanceMode,
    TextVectorLineTextDirection, TextVectorLinesLayoutParams, TextVectorPoint, TextWrapMode,
    VectorMeshWarp, VerticalLineDirection,
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
// Re-exported to `tab`'s child modules (e.g. `panels`, `layout_editor`) via `use super::*`.
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
use ms_thread as thread;
use web_time::{SystemTime, UNIX_EPOCH};

mod geometry;
use geometry::{lerp, normalize_angle_deg, normalize_angle_rad};
mod export;
pub(super) use export::*;
mod codec;
use codec::*;
// Re-export the vector-mesh-warp decoder at the tab level so the sibling `panel`
// module can decode a carried `raster_transform` for its live preview render.
pub(in crate::tabs::typing) use codec::decode_vector_mesh_warp;
mod mesh_geometry;
use mesh_geometry::*;
mod render_store;
use render_store::*;
mod create_upload;
mod doc_layers;
mod panels;
mod persist;
mod render_jobs;
mod selection_rasters;
mod autotype;
mod draw_page;
mod vector_transform;
mod layout_editor;
use layout_editor::*;
mod helpers;
use helpers::*;
// `text_preview_label` moved into `mesh_geometry` but is re-exported by the
// parent typing module (`mod.rs`) as `tab::text_preview_label`; a glob import
// only re-imports it privately, so re-export it explicitly at `pub(crate)`.
pub(crate) use mesh_geometry::text_preview_label;

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
            Self::Perspective => t!("typing.deform.mode_perspective"),
            Self::Bend => t!("typing.deform.mode_bend"),
            Self::Frame => t!("typing.deform.mode_frame"),
            Self::Grid => t!("typing.deform.mode_grid"),
            Self::Bulge => t!("typing.deform.mode_bulge"),
            Self::Pinch => t!("typing.deform.mode_pucker"),
            Self::Push => t!("typing.deform.mode_shift"),
            Self::Twirl => t!("typing.deform.mode_twirl"),
            Self::Restore => t!("typing.deform.mode_restore"),
            Self::Smooth => t!("typing.deform.mode_smooth"),
            Self::Stretch => t!("typing.deform.mode_stretch"),
            Self::Fold => t!("typing.deform.mode_fold"),
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

/// Which flavour of on-canvas transform mode the selected overlay is in.
///
/// `transform_mode_overlay_idx` says WHICH overlay is in transform mode; this says whether the drag
/// edits the RASTER post-process `deform_mesh` (baked on top of the PNG, unchanged legacy path) or the
/// VECTOR mesh warp (`render_data.text_params.raster_transform`, baked into the PNG by re-rendering).
/// The two are independent and compose. Defaults to `Raster`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
enum TypingTransformModeKind {
    #[default]
    Raster,
    Vector,
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
        needs_repaint |= self.text_overlays.poll_vector_transform_base_render(ctx);
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

        let pointer_over_area = crate::input_util::pointer_over_floating_area(ctx);
        let popup_open = ctx.any_popup_open();
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

    /// True when the pointer sits over a panel/popup drawn above the canvas, so the
    /// canvas Shift+wheel font handler must defer to that widget instead of firing.
    ///
    /// The bare canvas is [`egui::Order::Background`]; the Shift+drag selection-capture
    /// overlay ([`egui::Order::Middle`]) counts as bare canvas. Any other floating layer
    /// (a Foreground panel, popup or tooltip) means the wheel belongs to that widget.
    fn pointer_over_panel_over_canvas(ctx: &egui::Context, pos: egui::Pos2) -> bool {
        ctx.layer_id_at(pos).is_some_and(|layer| {
            layer.order != egui::Order::Background
                && layer != create_upload::shift_drag_capture_layer_id()
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
        let (shift_down, raw_wheel_delta, primary_down, hover_pos, interact_pos) =
            ctx.input(|input| {
                (
                    input.modifiers.shift,
                    crate::input_util::raw_wheel_delta(input),
                    input.pointer.primary_down(),
                    input.pointer.hover_pos(),
                    input.pointer.interact_pos(),
                )
            });
        if !shift_down || primary_down {
            return false;
        }

        let Some(pointer_pos) = interact_pos
            .or(hover_pos)
            .filter(|pos| canvas_rect.contains(*pos))
        else {
            return false;
        };
        // Fire only over bare canvas (or the Shift-drag selection overlay). A panel/popup
        // above the canvas owns the wheel — its own Wheel widget applies the 5x Shift step.
        if Self::pointer_over_panel_over_canvas(ctx, pointer_pos) {
            return false;
        }

        // Use the one-frame raw wheel delta, not the smoothed inertia (which ramps over
        // ~a dozen frames and would apply one step per ramp frame): one physical notch =
        // one discrete step.
        let mut wheel_delta = raw_wheel_delta.y;
        if wheel_delta.abs() <= f32::EPSILON {
            // Some backends convert Shift+wheel into horizontal scroll.
            wheel_delta = raw_wheel_delta.x;
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
        let (shift_down, raw_wheel_delta, primary_down, hover_pos, interact_pos) =
            ctx.input(|input| {
                (
                    input.modifiers.shift,
                    crate::input_util::raw_wheel_delta(input),
                    input.pointer.primary_down(),
                    input.pointer.hover_pos(),
                    input.pointer.interact_pos(),
                )
            });
        if !shift_down || primary_down {
            return false;
        }

        let Some(pointer_pos) = interact_pos
            .or(hover_pos)
            .filter(|pos| canvas_rect.contains(*pos))
        else {
            return false;
        };
        // Fire only over bare canvas (or the Shift-drag selection overlay); defer to a
        // panel/popup above the canvas so its Wheel widget handles the Shift+wheel.
        if Self::pointer_over_panel_over_canvas(ctx, pointer_pos) {
            return false;
        }

        // One-frame raw wheel delta (not smoothed inertia): one notch = one step.
        let mut wheel_delta = raw_wheel_delta.y;
        if wheel_delta.abs() <= f32::EPSILON {
            wheel_delta = raw_wheel_delta.x;
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
        // Keep the tab-side render font source in sync with the panel's current font
        // list so background renders resolve fonts by name (and pick up reloads).
        self.text_overlays
            .set_font_provider(self.top_panel.font_provider());
        self.text_overlays.flush_edit_save_on_selection_change();
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
        // Skip the shift-drag create UI while the layout editor is Editing: that mode
        // reuses the canvas for frame/line editing and must not spawn new overlays.
        if !self.top_panel.is_mask_panel_open()
            && !self.text_overlays.layout_editor_editing_active()
        {
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
        // Read the layout-editor-active flag first (immutable) so it does not alias the mutable
        // `&mut self.text_overlays` passed into `draw`; the panel uses it to avoid sitting under the
        // top-left layout-editor panel.
        let layout_editor_active = self.text_overlays.layout_editor_active();
        self.top_panel.draw(
            ctx,
            canvas_rect,
            self.text_overlays,
            canvas.current_page_idx(),
            layout_editor_active,
        );
        // Draws the merged mode+params+opacity panel in Editing and the plain mode
        // switch in Preview; the params section self-gates on Editing mode.
        if self.text_overlays.layout_editor_active() {
            self.text_overlays
                .draw_layout_editor_panels(ctx, canvas_rect);
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
        if ui.small_button(t!("typing.canvas.create_text_button")).clicked() {
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
                    t!("typing.canvas.create_text_button").to_owned(),
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
    /// Font source captured at dispatch time so the worker resolves fonts by name.
    font_provider: Arc<dyn FontProvider>,
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
    /// Font source captured at dispatch time so the worker resolves fonts by name.
    font_provider: Arc<dyn FontProvider>,
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

/// Cached UN-WARPED base image for the live VECTOR-transform GPU preview (Phase 3b).
///
/// Held only while a vector-transform session is active. The base is the overlay rendered WITHOUT
/// its `raster_transform`, so warping it onto the working mesh maps un-warped → warped exactly once
/// (texturing the already-warped baked PNG would double-warp). It is a reconstructable GPU cache:
/// `rgba` is kept resident so `texture` can be re-uploaded after a memory-pressure eviction, and the
/// whole base can be re-rendered if lost.
struct TypingVectorTransformBaseTexture {
    /// Overlay this base was rendered for; used to reject a stale base after the transform target
    /// changes.
    overlay_idx: usize,
    size_px: [usize; 2],
    /// Straight (un-premultiplied) RGBA of the un-warped render (`width * height * 4`). Kept resident
    /// so the texture can be re-uploaded without a re-render.
    rgba: Vec<u8>,
    /// GPU texture uploaded lazily on the GUI thread; `None` ⇒ needs (re)upload from `rgba`.
    texture: Option<egui::TextureHandle>,
}

/// Result of the one-off un-warped base render for the vector-transform preview.
struct TypingVectorBaseRenderResult {
    /// Cancellation token this render was issued with; compared against the latest token on poll so a
    /// superseded render is dropped.
    token: u64,
    overlay_idx: usize,
    size_px: [usize; 2],
    rgba: Vec<u8>,
}

/// Worker request for the one-off un-warped base render (see `TypingVectorTransformBaseTexture`).
///
/// No output path is carried: this render is a transient GPU-cache preview and is never written to
/// disk, and vector transform is only offered for layouts that need no adjacent layout-image file.
struct TypingVectorBaseRenderRequest {
    token: u64,
    latest_token: Arc<AtomicU64>,
    overlay_idx: usize,
    /// Render params built from the overlay's `render_data` with `raster_transform` already CLEARED.
    render_params: TextRenderParams,
    /// Font source captured at dispatch time so the worker resolves fonts by name.
    font_provider: Arc<dyn FontProvider>,
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

impl TypingOverlayRuntime {
    /// Builds the shared-doc affine placement (`TransformRec`) from this runtime's live
    /// center/rotation/scale. `angle_deg` is stored in degrees on the runtime and converted to
    /// radians for the doc. Single source of truth for the runtime→doc transform mapping, shared by
    /// the placement autosave and the text edit-render doc route.
    fn transform_rec(&self) -> crate::models::layer_model::manifest::TransformRec {
        crate::models::layer_model::manifest::TransformRec {
            cx: self.center_page_px[0],
            cy: self.center_page_px[1],
            rotation: self.angle_deg.to_radians(),
            scale: self.user_scale,
        }
    }
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
    /// layout-layer preview opacity in [0,1] for the on-canvas dimmed text under
    /// the frame; Editing sub-mode only; ephemeral (not persisted).
    preview_opacity: f32,
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

/// Active drag while editing the VECTOR transform working mesh (Phase 3a). Separate from
/// `TypingOverlayDragState` (which drives the overlay's placement / raster deform mesh) so the two
/// transform kinds never share drag state. `start_mesh` is the working mesh snapshot at drag start
/// (page px); `mode` reuses the deform drag modes (only the handle / brush / whole-mesh-move variants
/// are produced for a vector edit — no page transitions or rotate handle).
#[derive(Debug, Clone)]
struct TypingVectorTransformDragState {
    overlay_idx: usize,
    page_idx: usize,
    pointer_start_scene: Pos2,
    mode: TypingOverlayDragMode,
    start_mesh: TypingOverlayDeformMesh,
    has_changes: bool,
}

type TypingOverlayLoadResponse = (PathBuf, Result<Vec<TypingOverlayDecoded>, String>);

/// Eager-migration request payload captured at chapter open:
/// `(committed_layers_dir, legacy_text_images_dir, unsaved_layers_dir, page_paths)`,
/// where `page_paths` is a list of `(page_idx, page_path)`.
type PendingMigrationRequest = (PathBuf, PathBuf, PathBuf, Vec<(usize, PathBuf)>);

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
    /// Font source handed to every tab-side render worker. Refreshed each frame from
    /// the top panel's current font list (`refresh_font_provider`) and captured into
    /// each render request so background threads resolve fonts by name.
    font_provider: Arc<dyn FontProvider>,
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
    /// Whether the overlay in `transform_mode_overlay_idx` edits the RASTER post-process mesh or the
    /// VECTOR mesh warp (Phase 3a). Meaningful only while `transform_mode_overlay_idx.is_some()`; reset
    /// to `Raster` whenever transform mode is left.
    transform_mode_kind: TypingTransformModeKind,
    /// Transient working mesh (13x13 page px) for a VECTOR transform edit. Held only while a vector
    /// transform session is active; NOT the runtime `deform_mesh` (that stays the raster post-process
    /// mesh). Reused across frames so a drag can snapshot it; converted to normalized `points_norm` and
    /// baked via re-render on settle.
    vector_transform_mesh: Option<TypingOverlayDeformMesh>,
    /// Source-rect size in CONTENT px used to normalize the vector working mesh (captured on enter:
    /// the stored `raster_transform` src dims if valid, else the un-warped baked PNG `size_px`). Both
    /// the seed and settle conversions normalize over these dims so the UI and renderer (Design B)
    /// agree.
    vector_transform_src_px: [f32; 2],
    /// Active vector-transform drag, if any.
    vector_transform_drag: Option<TypingVectorTransformDragState>,
    /// Un-warped base texture for the live vector-transform GPU preview (Phase 3b). Rendered once on
    /// ENTER (or reused from the overlay's current un-warped `source_rgba` when it has no stored warp),
    /// then warped onto the working mesh during a drag. `None` until ready; a reconstructable GPU cache.
    vector_transform_base: Option<TypingVectorTransformBaseTexture>,
    /// In-flight one-off render of the un-warped base for the vector preview. Present only while the
    /// base render is running; polled by `poll_vector_transform_base_render`.
    vector_transform_base_rx: Option<Receiver<Result<Option<TypingVectorBaseRenderResult>, String>>>,
    /// Monotonic cancellation token for the un-warped base render: every request bumps it, so a
    /// superseded worker (re-enter / target change) sees its token is stale and returns nothing.
    vector_base_render_token: Arc<AtomicU64>,
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
    pending_migration: Option<PendingMigrationRequest>,
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
            // Empty provider until the first frame refreshes it from the top panel.
            font_provider: Arc::new(FontContentSet::default()),
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
            transform_mode_kind: TypingTransformModeKind::default(),
            vector_transform_mesh: None,
            vector_transform_src_px: [1.0, 1.0],
            vector_transform_drag: None,
            vector_transform_base: None,
            vector_transform_base_rx: None,
            vector_base_render_token: Arc::new(AtomicU64::new(0)),
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

#[cfg(test)]
mod tests;
