/*
FILE HEADER (tabs/typing/mask.rs)
- Назначение: бинарная маска обрезки (`mask_page_{idx}.png`) для вкладки `Текст`.
- Ключевые сущности:
  - `TypingMaskLayer`: загрузка/хранение/редактирование/сохранение масок по страницам,
    отрисовка жёлтого mask-overlay (tiled, без переполнения max texture size),
    LRU snapshots/eviction for reconstructable mask overlay GPU tiles,
    панель инструментов маски, режим кисти и режим заливки по цвету.
  - `TypingPageMask`: runtime-состояние маски страницы (0/255, tile-texture cache, dirty-tiles).
- Ключевые методы:
  - `ensure_loader_started` + `poll_loader`: фоновая загрузка всех `mask_page_*.png`.
  - `set_panel_open` + `draw_panel`: открытие/закрытие панели маски, UI кнопок,
    переключение режима заливки и слайдер допуска цвета.
  - `draw_page_mask_overlay_and_handle_input`: рисование маски поверх страницы и кисть
    (`ЛКМ` рисует, `ПКМ`/`Shift+ЛКМ` стирает) без блокировки GUI; размер кисти
    меняется общей `MaskBrush` из `src/tools/mask_brush.rs` (`Shift+колесо`, `-`/`=`/`+`),
    а курсор рисуется кольцом с корректным scene-радиусом. Метод возвращает `true`
    только при реальном изменении битовой маски страницы.
    В режиме заливки — запускает flood fill в worker-потоке по composited-цвету
    (`страница + clean overlay`) из shared cache.
  - `clip_overlay_rgba_if_needed`: бинарное обрезание RGBA-текста по маске страницы.
  - `export_masks_snapshot`: отдаёт снимок масок (по страницам) для фонового экспорта
    финальных изображений без доступа к внутреннему mutable-состоянию слоя.
*/
use crate::trace::cat;
use crate::memory_manager::{
    CacheEvictionReport, CacheEvictionRequest, CacheReloadCost, CacheResourceInfo,
    CacheResourceKind, select_eviction_candidates,
};
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::project::ProjectData;
use crate::tools::MaskBrush;
use crate::widgets::WheelSlider;
use eframe::egui;
use egui::{Color32, CursorIcon, Id, PointerButton, Pos2, Rect, Sense};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;

const MASK_FILE_PREFIX: &str = "mask_page_";
const MASK_FILE_SUFFIX: &str = ".png";
const MASK_PANEL_AREA_ID: &str = "typing_mask_editor_panel";
const MASK_PANEL_TOP_MARGIN_PX: f32 = 220.0;
const MASK_PANEL_WIDTH_PX: f32 = 280.0;
const MASK_PANEL_RIGHT_MARGIN_PX: f32 = 16.0;
const MASK_BRUSH_SLIDER_MIN_RADIUS_PX: usize = 1;
const MASK_BRUSH_SLIDER_MAX_RADIUS_PX: usize = 200;
const MASK_STATUS_ERROR_SECONDS: f64 = 4.0;
const MASK_OVERLAY_RGBA: [u8; 4] = [245, 210, 60, 120];
const MASK_TILE_SIDE_PX: usize = 1024;
const MASK_FILL_TOLERANCE_DEFAULT: u8 = 18;
const MASK_FILL_TOLERANCE_MIN: u8 = 0;
const MASK_FILL_TOLERANCE_MAX: u8 = 255;

type TypingMaskLoadResponse = (PathBuf, Result<Vec<TypingMaskLoadedPage>, String>);

struct TypingMaskLoadedPage {
    page_idx: usize,
    width: usize,
    height: usize,
    data: Vec<u8>,
}

struct TypingMaskSavePage {
    page_idx: usize,
    width: usize,
    height: usize,
    data: Vec<u8>,
}

struct TypingMaskFillJobResult {
    page_idx: usize,
    width: usize,
    height: usize,
    data: Vec<u8>,
    dirty_rect: Option<(usize, usize, usize, usize)>,
}

#[derive(Clone)]
pub(crate) struct TypingMaskExportPage {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy)]
struct TypingMaskStrokeState {
    page_idx: usize,
    erase: bool,
    last_scene_pos: Pos2,
}

struct TypingPageMask {
    width: usize,
    height: usize,
    data: Vec<u8>,
    tile_textures: HashMap<usize, egui::TextureHandle>,
    dirty_tiles: HashSet<usize>,
    last_texture_used_frame: u64,
}

impl TypingPageMask {
    fn has_active_pixels(&self) -> bool {
        self.data.iter().any(|v| *v > 0)
    }
}

pub struct TypingMaskLayer {
    panel_open: bool,
    fill_mode: bool,
    fill_tolerance: u8,
    loaded_project_dir: Option<PathBuf>,
    loaded_text_images_dir: Option<PathBuf>,
    loading_project_dir: Option<PathBuf>,
    loading_rx: Option<Receiver<TypingMaskLoadResponse>>,
    save_rx: Option<Receiver<Result<(), String>>>,
    fill_job_rx: Option<Receiver<Result<TypingMaskFillJobResult, String>>>,
    save_requested_while_busy: bool,
    overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    masks: HashMap<usize, TypingPageMask>,
    changed_pages: HashSet<usize>,
    active_stroke: Option<TypingMaskStrokeState>,
    mask_brush: MaskBrush,
    status_error: Option<(String, f64)>,
}

impl Default for TypingMaskLayer {
    fn default() -> Self {
        Self {
            panel_open: false,
            fill_mode: false,
            fill_tolerance: MASK_FILL_TOLERANCE_DEFAULT,
            loaded_project_dir: None,
            loaded_text_images_dir: None,
            loading_project_dir: None,
            loading_rx: None,
            save_rx: None,
            fill_job_rx: None,
            save_requested_while_busy: false,
            overlays_model: None,
            masks: HashMap::new(),
            changed_pages: HashSet::new(),
            active_stroke: None,
            mask_brush: MaskBrush::default(),
            status_error: None,
        }
    }
}

impl TypingMaskLayer {
    pub fn set_overlays_model(&mut self, model: Arc<Mutex<CleanOverlaysModel>>) {
        self.overlays_model = Some(model);
    }

    pub fn is_panel_open(&self) -> bool {
        self.panel_open
    }

    pub fn gpu_memory_snapshot(&self, pinned_pages: &BTreeSet<usize>) -> Vec<CacheResourceInfo> {
        self.masks
            .iter()
            .filter(|(_, mask)| !mask.tile_textures.is_empty())
            .map(|(page_idx, mask)| CacheResourceInfo {
                id: format!("typing-mask-gpu:{page_idx}"),
                kind: CacheResourceKind::TypingMaskGpu,
                page_idx: Some(*page_idx),
                estimated_bytes: typing_mask_texture_estimated_bytes(mask),
                last_used_frame: mask.last_texture_used_frame,
                reload_cost: CacheReloadCost::RebuildFromModel,
                dirty: false,
                visible: pinned_pages.contains(page_idx),
                reconstructable: true,
            })
            .collect()
    }

    pub fn evict_gpu_cache(&mut self, request: &CacheEvictionRequest) -> CacheEvictionReport {
        let snapshot = self.gpu_memory_snapshot(&request.pinned_pages);
        let candidates = select_eviction_candidates(&snapshot, request);
        let mut evicted = Vec::new();
        let mut freed = 0_u64;
        for resource in candidates.resources {
            let Some(page_idx) = resource.page_idx else {
                continue;
            };
            let Some(mask) = self.masks.get_mut(&page_idx) else {
                continue;
            };
            if mask.tile_textures.is_empty() {
                continue;
            }
            mask.tile_textures.clear();
            mask.dirty_tiles = all_mask_tile_indices(mask.width, mask.height);
            mask.last_texture_used_frame = 0;
            freed = freed.saturating_add(resource.estimated_bytes);
            evicted.push(resource);
        }
        CacheEvictionReport {
            resources: evicted,
            estimated_freed_bytes: freed,
        }
    }

    pub fn set_panel_open(&mut self, ctx: &egui::Context, is_open: bool) {
        if self.panel_open == is_open {
            return;
        }
        self.panel_open = is_open;
        self.active_stroke = None;
        self.fill_mode = false;
        self.fill_job_rx = None;
        if !is_open {
            self.request_save_all();
        }
        if is_open {
            ctx.request_repaint();
        }
    }

    pub fn ensure_loader_started(&mut self, project: &ProjectData) {
        let project_dir = project.project_dir.clone();
        if self.loaded_project_dir.as_ref() == Some(&project_dir) {
            return;
        }
        if self.loading_project_dir.as_ref() == Some(&project_dir) {
            return;
        }

        self.masks.clear();
        self.changed_pages.clear();
        self.active_stroke = None;
        self.status_error = None;
        self.save_rx = None;
        self.fill_job_rx = None;
        self.fill_mode = false;
        self.save_requested_while_busy = false;
        self.loaded_project_dir = None;
        self.loaded_text_images_dir = None;

        let text_images_dir = project.paths.text_images_dir.clone();
        let (tx, rx) = mpsc::channel::<TypingMaskLoadResponse>();
        let project_dir_for_thread = project_dir.clone();
        let text_images_dir_for_thread = text_images_dir.clone();
        thread::spawn(move || {
            let result = load_masks_from_text_images_dir(&text_images_dir_for_thread);
            let _ = tx.send((project_dir_for_thread, result));
        });

        self.loading_project_dir = Some(project_dir);
        self.loading_rx = Some(rx);
        self.loaded_text_images_dir = Some(text_images_dir);
    }

    pub fn poll_loader(&mut self, ctx: &egui::Context) -> bool {
        let Some(rx) = self.loading_rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok((project_dir, result)) => {
                self.loading_rx = None;
                self.loading_project_dir = None;
                self.loaded_project_dir = Some(project_dir);
                match result {
                    Ok(loaded_pages) => {
                        self.masks.clear();
                        self.changed_pages.clear();
                        for page in loaded_pages {
                            self.changed_pages.insert(page.page_idx);
                            self.masks.insert(
                                page.page_idx,
                                TypingPageMask {
                                    width: page.width,
                                    height: page.height,
                                    data: page.data,
                                    tile_textures: HashMap::new(),
                                    dirty_tiles: all_mask_tile_indices(page.width, page.height),
                                    last_texture_used_frame: 0,
                                },
                            );
                        }
                        true
                    }
                    Err(err) => {
                        self.masks.clear();
                        self.changed_pages.clear();
                        self.set_error(ctx, err);
                        true
                    }
                }
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.loading_rx = None;
                self.loading_project_dir = None;
                self.set_error(ctx, "Не удалось получить результат загрузки масок.");
                true
            }
        }
    }

    pub fn poll_save_jobs(&mut self, ctx: &egui::Context) -> bool {
        let recv_result = {
            let Some(rx) = self.save_rx.as_ref() else {
                return false;
            };
            match rx.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    "Фоновое сохранение маски завершилось с ошибкой канала.".to_string(),
                )),
            }
        };
        let Some(recv_result) = recv_result else {
            return false;
        };
        self.save_rx = None;
        match recv_result {
            Ok(Ok(())) => {}
            Ok(Err(err)) | Err(err) => self.set_error(ctx, err),
        }
        if self.save_requested_while_busy {
            self.save_requested_while_busy = false;
            self.request_save_all();
        }
        true
    }

    pub fn poll_fill_jobs(&mut self, ctx: &egui::Context) -> bool {
        let recv_result = {
            let Some(rx) = self.fill_job_rx.as_ref() else {
                return false;
            };
            match rx.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    "Фоновая заливка маски завершилась с ошибкой канала.".to_string(),
                )),
            }
        };
        let Some(recv_result) = recv_result else {
            return false;
        };
        self.fill_job_rx = None;
        match recv_result {
            Ok(Ok(job)) => {
                let Some(mask) = self.masks.get_mut(&job.page_idx) else {
                    return true;
                };
                if mask.width != job.width
                    || mask.height != job.height
                    || mask.data.len() != job.data.len()
                {
                    self.set_error(
                        ctx,
                        "Результат заливки устарел: размер маски страницы изменился.",
                    );
                    return true;
                }
                let changed = job.dirty_rect.is_some();
                crate::trace_log!(
                    cat::TYPING,
                    "mask_fill result=ok page={} changed={}",
                    job.page_idx,
                    changed
                );
                if changed {
                    mask.data = job.data;
                    if let Some((x, y, w, h)) = job.dirty_rect {
                        mark_mask_dirty_rect(mask, x, y, w, h);
                    } else {
                        mark_mask_dirty_full(mask);
                    }
                    self.changed_pages.insert(job.page_idx);
                }
                true
            }
            Ok(Err(err)) | Err(err) => {
                crate::trace_log!(cat::TYPING, "mask_fill result=err err={}", err);
                self.set_error(ctx, err);
                true
            }
        }
    }

    pub fn take_changed_pages(&mut self) -> Vec<usize> {
        self.changed_pages.drain().collect()
    }

    pub fn draw_panel(&mut self, ctx: &egui::Context, canvas_rect: Rect, current_page_idx: usize) {
        let now_s = ctx.input(|i| i.time);
        if self
            .status_error
            .as_ref()
            .is_some_and(|(_, hide_at)| now_s >= *hide_at)
        {
            self.status_error = None;
        }
        if !self.panel_open {
            return;
        }

        let panel_pos = egui::pos2(
            canvas_rect.right() - MASK_PANEL_WIDTH_PX - MASK_PANEL_RIGHT_MARGIN_PX,
            canvas_rect.top() + MASK_PANEL_TOP_MARGIN_PX,
        );
        egui::Area::new(Id::new(MASK_PANEL_AREA_ID))
            .order(egui::Order::Foreground)
            .fixed_pos(panel_pos)
            .show(ctx, |ui| {
                ui.set_width(MASK_PANEL_WIDTH_PX);
                ui.set_min_width(MASK_PANEL_WIDTH_PX);
                ui.set_max_width(MASK_PANEL_WIDTH_PX);
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(egui::RichText::new("Маска обрезки").strong());
                    ui.label(format!("Страница: {}", current_page_idx.saturating_add(1)));
                    if !self.fill_mode {
                        let mut radius = self.mask_brush.radius_px();
                        if ui
                            .add(
                                WheelSlider::new(
                                    &mut radius,
                                    MASK_BRUSH_SLIDER_MIN_RADIUS_PX
                                        ..=MASK_BRUSH_SLIDER_MAX_RADIUS_PX,
                                )
                                .text("Кисть (px)"),
                            )
                            .changed()
                        {
                            self.mask_brush.set_radius_px(radius);
                        }
                    }
                    let mut tolerance = self.fill_tolerance;
                    if ui
                        .add(
                            WheelSlider::new(
                                &mut tolerance,
                                MASK_FILL_TOLERANCE_MIN..=MASK_FILL_TOLERANCE_MAX,
                            )
                            .text("Допуск цвета"),
                        )
                        .changed()
                    {
                        self.fill_tolerance = tolerance;
                    }
                    let fill_button_label = if self.fill_mode {
                        "Отменить заливку"
                    } else {
                        "Заливка маски"
                    };
                    if ui.button(fill_button_label).clicked() {
                        self.fill_mode = !self.fill_mode;
                        self.active_stroke = None;
                        if !self.fill_mode {
                            self.fill_job_rx = None;
                        }
                        ctx.request_repaint();
                    }
                    if self.fill_job_rx.is_some() {
                        ui.label("Заливка: обработка...");
                    }
                    let clear_enabled = self.masks.contains_key(&current_page_idx);
                    if ui
                        .add_enabled(clear_enabled, egui::Button::new("Очистить маску страницы"))
                        .clicked()
                    {
                        self.clear_mask_page(current_page_idx);
                    }
                    ui.separator();
                    if self.fill_mode {
                        ui.label("ЛКМ: залить область по цвету");
                        ui.label("Курсор: крестик");
                    } else {
                        ui.label("ЛКМ: рисовать");
                        ui.label("ПКМ или Shift+ЛКМ: стирать");
                    }
                    if let Some((message, _)) = self.status_error.as_ref() {
                        ui.separator();
                        ui.colored_label(Color32::from_rgb(240, 110, 110), message);
                    }
                });
            });
    }

    pub fn draw_page_mask_overlay_and_handle_input(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) -> bool {
        if !self.panel_open {
            self.active_stroke = None;
            return false;
        }

        let mut mask_changed = false;
        let response = ui.interact(
            image_rect,
            Id::new(("typing_mask_overlay_page", page_idx)),
            Sense::click_and_drag(),
        );
        if response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
        }

        let pointer_pos = response
            .interact_pointer_pos()
            .or_else(|| response.hover_pos());
        let hover_pos = response.hover_pos();
        let (primary_down, secondary_down, shift_down) = ui.ctx().input(|input| {
            (
                input.pointer.primary_down(),
                input.pointer.secondary_down(),
                input.modifiers.shift,
            )
        });
        if self.fill_mode {
            self.active_stroke = None;
            if response.clicked_by(PointerButton::Primary)
                && self.fill_job_rx.is_none()
                && pointer_pos.is_some_and(|pos| image_rect.contains(pos))
                && let Some(pos) = pointer_pos
                && let Err(err) = self.start_fill_job(ui.ctx(), page_idx, image_rect, zoom, pos)
            {
                self.set_error(ui.ctx(), err);
            }
        } else {
            let mode = if secondary_down || (primary_down && shift_down) {
                Some(true)
            } else if primary_down {
                Some(false)
            } else {
                None
            };

            if mode.is_none() {
                self.active_stroke = None;
            }

            if response.hovered() {
                let _ = self.handle_brush_wheel(ui, page_idx, image_rect, zoom);
                let _ = self.handle_brush_size_shortcuts(ui);
            }

            if let (Some(erase), Some(pos)) = (mode, pointer_pos)
                && image_rect.contains(pos)
            {
                let stroke_continues = matches!(
                    self.active_stroke,
                    Some(state) if state.page_idx == page_idx && state.erase == erase
                );
                if !stroke_continues {
                    crate::trace_log!(
                        cat::TYPING,
                        "mask_stroke_begin page={} erase={} radius={}",
                        page_idx,
                        erase,
                        self.mask_brush.radius_px()
                    );
                }
                let start_pos = match self.active_stroke {
                    Some(state) if state.page_idx == page_idx && state.erase == erase => {
                        state.last_scene_pos
                    }
                    _ => pos,
                };
                let mask_brush = self.mask_brush.clone();
                let brush_radius = mask_brush.radius_px();
                let page_mask = self.ensure_mask_for_paint(page_idx, image_rect, zoom);
                if let Some(mask) = page_mask {
                    let dirty_rect =
                        stroke_bounds_mask_rect(mask, image_rect, start_pos, pos, brush_radius);
                    let was_changed = paint_mask_segment(
                        &mask_brush,
                        mask,
                        image_rect,
                        start_pos,
                        pos,
                        brush_radius,
                        erase,
                    );
                    if was_changed {
                        if let Some((x, y, w, h)) = dirty_rect {
                            mark_mask_dirty_rect(mask, x, y, w, h);
                        } else {
                            mark_mask_dirty_full(mask);
                        }
                        mask_changed = true;
                        self.changed_pages.insert(page_idx);
                    }
                    self.active_stroke = Some(TypingMaskStrokeState {
                        page_idx,
                        erase,
                        last_scene_pos: pos,
                    });
                }
            }
        }

        if let Some(mask) = self.masks.get_mut(&page_idx) {
            sync_mask_tile_textures(ui.ctx(), page_idx, mask);
            draw_mask_tiles(ui, image_rect, mask);
        }

        if !self.fill_mode
            && let Some(pointer) = hover_pos
            && image_rect.contains(pointer)
        {
            let [mask_w, mask_h] = self.effective_mask_size_for_page(page_idx, image_rect, zoom);
            self.mask_brush
                .draw_circle_cursor_on_image(ui, image_rect, [mask_w, mask_h], pointer);
        }

        mask_changed
    }

    pub fn clip_overlay_rgba_if_needed(
        &self,
        page_idx: usize,
        overlay_size: [usize; 2],
        overlay_rgba: &[u8],
        deform_mesh_cols: usize,
        deform_mesh_rows: usize,
        deform_mesh_points_uv: &[[f32; 2]],
    ) -> Option<Vec<u8>> {
        let mask = self.masks.get(&page_idx)?;
        if overlay_size[0] == 0 || overlay_size[1] == 0 {
            return None;
        }
        if overlay_rgba.len() != overlay_size[0] * overlay_size[1] * 4 {
            return None;
        }
        if mask.data.is_empty() || mask.width == 0 || mask.height == 0 {
            return None;
        }
        if deform_mesh_cols < 2
            || deform_mesh_rows < 2
            || deform_mesh_points_uv.len() != deform_mesh_cols.saturating_mul(deform_mesh_rows)
        {
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
                let uv = sample_deform_mesh_uv(
                    deform_mesh_cols,
                    deform_mesh_rows,
                    deform_mesh_points_uv,
                    tu,
                    tv,
                );
                let active = sample_mask_active(mask, uv[0], uv[1]);
                if active {
                    touched_active = true;
                } else {
                    out[px_idx + 3] = 0;
                }
            }
        }

        if touched_active { Some(out) } else { None }
    }

    pub fn page_mask_size(&self, page_idx: usize) -> Option<[usize; 2]> {
        self.masks
            .get(&page_idx)
            .map(|mask| [mask.width, mask.height])
    }

    pub fn export_masks_snapshot(&self) -> HashMap<usize, TypingMaskExportPage> {
        self.masks
            .iter()
            .map(|(page_idx, mask)| {
                (
                    *page_idx,
                    TypingMaskExportPage {
                        width: mask.width,
                        height: mask.height,
                        data: mask.data.clone(),
                    },
                )
            })
            .collect()
    }

    fn ensure_mask_for_paint(
        &mut self,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) -> Option<&mut TypingPageMask> {
        if self.masks.contains_key(&page_idx) {
            return self.masks.get_mut(&page_idx);
        }
        let source_w = (image_rect.width() / zoom.max(f32::EPSILON))
            .round()
            .max(1.0) as usize;
        let source_h = (image_rect.height() / zoom.max(f32::EPSILON))
            .round()
            .max(1.0) as usize;
        self.masks.insert(
            page_idx,
            TypingPageMask {
                width: source_w,
                height: source_h,
                data: vec![0u8; source_w * source_h],
                tile_textures: HashMap::new(),
                dirty_tiles: HashSet::new(),
                last_texture_used_frame: 0,
            },
        );
        self.masks.get_mut(&page_idx)
    }

    fn clear_mask_page(&mut self, page_idx: usize) {
        let Some(mask) = self.masks.get_mut(&page_idx) else {
            return;
        };
        if !mask.has_active_pixels() {
            return;
        }
        crate::trace_log!(cat::TYPING, "mask_clear_page page={}", page_idx);
        mask.data.fill(0);
        mask.tile_textures.clear();
        mask.dirty_tiles.clear();
        self.changed_pages.insert(page_idx);
    }

    fn request_save_all(&mut self) {
        if self.save_rx.is_some() {
            self.save_requested_while_busy = true;
            return;
        }
        let Some(text_images_dir) = self.loaded_text_images_dir.clone() else {
            return;
        };
        let pages = self
            .masks
            .iter()
            .map(|(page_idx, mask)| TypingMaskSavePage {
                page_idx: *page_idx,
                width: mask.width,
                height: mask.height,
                data: mask.data.clone(),
            })
            .collect::<Vec<_>>();
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        thread::spawn(move || {
            let result = save_masks_to_text_images_dir(&text_images_dir, &pages);
            let _ = tx.send(result);
        });
        self.save_rx = Some(rx);
    }

    fn set_error(&mut self, ctx: &egui::Context, message: impl Into<String>) {
        let now_s = ctx.input(|i| i.time);
        self.status_error = Some((message.into(), now_s + MASK_STATUS_ERROR_SECONDS));
    }

    fn effective_mask_size_for_page(
        &self,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) -> [usize; 2] {
        if let Some(mask) = self.masks.get(&page_idx) {
            return [mask.width.max(1), mask.height.max(1)];
        }
        let source_w = (image_rect.width() / zoom.max(f32::EPSILON))
            .round()
            .max(1.0) as usize;
        let source_h = (image_rect.height() / zoom.max(f32::EPSILON))
            .round()
            .max(1.0) as usize;
        [source_w.max(1), source_h.max(1)]
    }

    fn handle_brush_wheel(
        &mut self,
        ui: &mut egui::Ui,
        _page_idx: usize,
        _image_rect: Rect,
        _zoom: f32,
    ) -> bool {
        let (mods, smooth_scroll, raw_scroll, primary_down) = ui.ctx().input(|input| {
            (
                input.modifiers,
                input.smooth_scroll_delta,
                input.raw_scroll_delta,
                input.pointer.primary_down(),
            )
        });
        if primary_down {
            return false;
        }
        let mut wheel_delta = smooth_scroll.y;
        if wheel_delta.abs() <= f32::EPSILON {
            wheel_delta = raw_scroll.y;
        }
        if wheel_delta.abs() <= f32::EPSILON {
            wheel_delta = smooth_scroll.x;
        }
        if wheel_delta.abs() <= f32::EPSILON {
            wheel_delta = raw_scroll.x;
        }
        if wheel_delta.abs() <= f32::EPSILON {
            return false;
        }
        let changed = self.mask_brush.handle_wheel(wheel_delta, mods);
        if !mods.shift {
            return false;
        }
        ui.ctx().input_mut(|input| {
            input.smooth_scroll_delta = egui::Vec2::ZERO;
            input.raw_scroll_delta = egui::Vec2::ZERO;
        });
        if changed {
            ui.ctx().request_repaint();
        }
        changed
    }

    fn handle_brush_size_shortcuts(&mut self, ui: &mut egui::Ui) -> bool {
        if ui.ctx().wants_keyboard_input() {
            return false;
        }
        let changed = self.mask_brush.handle_size_shortcuts(ui.ctx());
        if changed {
            ui.ctx().request_repaint();
        }
        changed
    }

    fn start_fill_job(
        &mut self,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        scene_pos: Pos2,
    ) -> Result<(), String> {
        let Some(mask) = self.ensure_mask_for_paint(page_idx, image_rect, zoom) else {
            return Err("Не удалось подготовить маску страницы для заливки.".to_string());
        };
        let Some((seed_xf, seed_yf)) =
            scene_to_mask_px(image_rect, mask.width, mask.height, scene_pos)
        else {
            return Err("Точка заливки вне области страницы.".to_string());
        };
        let seed_x = seed_xf
            .round()
            .clamp(0.0, (mask.width.saturating_sub(1)) as f32) as usize;
        let seed_y = seed_yf
            .round()
            .clamp(0.0, (mask.height.saturating_sub(1)) as f32) as usize;
        let width = mask.width;
        let height = mask.height;
        let mask_data = mask.data.clone();
        let Some(overlays_model) = self.overlays_model.as_ref().cloned() else {
            return Err("Кэш clean overlay недоступен: модель не подключена.".to_string());
        };
        let tolerance = self.fill_tolerance;

        crate::trace_log!(
            cat::TYPING,
            "mask_fill dispatch page={} seed=({},{}) tolerance={}",
            page_idx,
            seed_x,
            seed_y,
            tolerance
        );
        let (tx, rx) = mpsc::channel::<Result<TypingMaskFillJobResult, String>>();
        thread::spawn(move || {
            let result =
                build_fill_source_from_cache_model(&overlays_model, page_idx, [width, height])
                    .and_then(|fill_source| {
                        flood_fill_mask_from_seed(
                            page_idx,
                            width,
                            height,
                            seed_x,
                            seed_y,
                            tolerance,
                            mask_data,
                            fill_source,
                        )
                    });
            let _ = tx.send(result);
        });
        self.fill_job_rx = Some(rx);
        ctx.request_repaint();
        Ok(())
    }
}

fn mask_tiles_x(width: usize) -> usize {
    width.div_ceil(MASK_TILE_SIDE_PX).max(1)
}

fn mask_tiles_y(height: usize) -> usize {
    height.div_ceil(MASK_TILE_SIDE_PX).max(1)
}

fn all_mask_tile_indices(width: usize, height: usize) -> HashSet<usize> {
    let tiles_x = mask_tiles_x(width);
    let tiles_y = mask_tiles_y(height);
    (0..tiles_x * tiles_y).collect::<HashSet<_>>()
}

fn mark_mask_dirty_full(mask: &mut TypingPageMask) {
    mask.dirty_tiles = all_mask_tile_indices(mask.width, mask.height);
}

fn mark_mask_dirty_rect(mask: &mut TypingPageMask, x: usize, y: usize, w: usize, h: usize) {
    if mask.width == 0 || mask.height == 0 || w == 0 || h == 0 {
        return;
    }
    let x0 = x.min(mask.width.saturating_sub(1));
    let y0 = y.min(mask.height.saturating_sub(1));
    let x1 = (x.saturating_add(w).saturating_sub(1)).min(mask.width.saturating_sub(1));
    let y1 = (y.saturating_add(h).saturating_sub(1)).min(mask.height.saturating_sub(1));
    let tiles_x = mask_tiles_x(mask.width);
    let tx0 = x0 / MASK_TILE_SIDE_PX;
    let ty0 = y0 / MASK_TILE_SIDE_PX;
    let tx1 = x1 / MASK_TILE_SIDE_PX;
    let ty1 = y1 / MASK_TILE_SIDE_PX;
    for ty in ty0..=ty1 {
        for tx in tx0..=tx1 {
            mask.dirty_tiles.insert(ty * tiles_x + tx);
        }
    }
}

fn stroke_bounds_mask_rect(
    mask: &TypingPageMask,
    image_rect: Rect,
    scene_p0: Pos2,
    scene_p1: Pos2,
    radius_px: usize,
) -> Option<(usize, usize, usize, usize)> {
    let (x0, y0) = scene_to_mask_px(image_rect, mask.width, mask.height, scene_p0)?;
    let (x1, y1) = scene_to_mask_px(image_rect, mask.width, mask.height, scene_p1)?;
    let r = radius_px.max(1) as f32;
    let min_x = (x0.min(x1) - r).floor().max(0.0) as usize;
    let min_y = (y0.min(y1) - r).floor().max(0.0) as usize;
    let max_x = (x0.max(x1) + r)
        .ceil()
        .min(mask.width.saturating_sub(1) as f32) as usize;
    let max_y = (y0.max(y1) + r)
        .ceil()
        .min(mask.height.saturating_sub(1) as f32) as usize;
    let w = max_x.saturating_sub(min_x).saturating_add(1);
    let h = max_y.saturating_sub(min_y).saturating_add(1);
    Some((min_x, min_y, w, h))
}

fn sync_mask_tile_textures(ctx: &egui::Context, page_idx: usize, mask: &mut TypingPageMask) {
    if mask.dirty_tiles.is_empty() {
        return;
    }
    let color = Color32::from_rgba_unmultiplied(
        MASK_OVERLAY_RGBA[0],
        MASK_OVERLAY_RGBA[1],
        MASK_OVERLAY_RGBA[2],
        MASK_OVERLAY_RGBA[3],
    );
    let tiles_x = mask_tiles_x(mask.width);
    let dirty_tiles = mask.dirty_tiles.drain().collect::<Vec<_>>();
    for tile_idx in dirty_tiles {
        let tx = tile_idx % tiles_x;
        let ty = tile_idx / tiles_x;
        let Some((tile_size, rgba, has_active)) = build_mask_overlay_tile_rgba(mask, tx, ty, color)
        else {
            mask.tile_textures.remove(&tile_idx);
            continue;
        };
        if !has_active {
            mask.tile_textures.remove(&tile_idx);
            continue;
        }
        let image = egui::ColorImage::from_rgba_unmultiplied(tile_size, &rgba);
        if let Some(texture) = mask.tile_textures.get_mut(&tile_idx) {
            texture.set(image, egui::TextureOptions::LINEAR);
        } else {
            let texture = ctx.load_texture(
                format!("typing-mask-overlay-page-{page_idx}-tile-{tile_idx}"),
                image,
                egui::TextureOptions::LINEAR,
            );
            mask.tile_textures.insert(tile_idx, texture);
        }
    }
}

fn build_mask_overlay_tile_rgba(
    mask: &TypingPageMask,
    tx: usize,
    ty: usize,
    color: Color32,
) -> Option<([usize; 2], Vec<u8>, bool)> {
    let x0 = tx * MASK_TILE_SIDE_PX;
    let y0 = ty * MASK_TILE_SIDE_PX;
    if x0 >= mask.width || y0 >= mask.height {
        return None;
    }
    let x1 = (x0 + MASK_TILE_SIDE_PX).min(mask.width);
    let y1 = (y0 + MASK_TILE_SIDE_PX).min(mask.height);
    let tile_w = x1 - x0;
    let tile_h = y1 - y0;
    let mut rgba = vec![0u8; tile_w * tile_h * 4];
    let mut has_active = false;
    for local_y in 0..tile_h {
        let src_y = y0 + local_y;
        let src_row = src_y * mask.width;
        let dst_row = local_y * tile_w;
        for local_x in 0..tile_w {
            let src_idx = src_row + x0 + local_x;
            let value = mask.data[src_idx];
            if value == 0 {
                continue;
            }
            has_active = true;
            let dst_idx = (dst_row + local_x) * 4;
            rgba[dst_idx] = color.r();
            rgba[dst_idx + 1] = color.g();
            rgba[dst_idx + 2] = color.b();
            rgba[dst_idx + 3] = color.a();
        }
    }
    Some(([tile_w, tile_h], rgba, has_active))
}

fn draw_mask_tiles(ui: &mut egui::Ui, image_rect: Rect, mask: &mut TypingPageMask) {
    if mask.tile_textures.is_empty() || mask.width == 0 || mask.height == 0 {
        return;
    }
    mask.last_texture_used_frame = ui.ctx().cumulative_frame_nr();
    let painter = ui.painter();
    let tiles_x = mask_tiles_x(mask.width);
    for (tile_idx, texture) in &mask.tile_textures {
        let tx = tile_idx % tiles_x;
        let ty = tile_idx / tiles_x;
        let x0 = tx * MASK_TILE_SIDE_PX;
        let y0 = ty * MASK_TILE_SIDE_PX;
        if x0 >= mask.width || y0 >= mask.height {
            continue;
        }
        let x1 = (x0 + MASK_TILE_SIDE_PX).min(mask.width);
        let y1 = (y0 + MASK_TILE_SIDE_PX).min(mask.height);
        let u0 = x0 as f32 / mask.width as f32;
        let v0 = y0 as f32 / mask.height as f32;
        let u1 = x1 as f32 / mask.width as f32;
        let v1 = y1 as f32 / mask.height as f32;
        let tile_rect = Rect::from_min_max(
            egui::pos2(
                image_rect.left() + u0 * image_rect.width(),
                image_rect.top() + v0 * image_rect.height(),
            ),
            egui::pos2(
                image_rect.left() + u1 * image_rect.width(),
                image_rect.top() + v1 * image_rect.height(),
            ),
        );
        painter.image(
            texture.id(),
            tile_rect,
            Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

fn typing_mask_texture_estimated_bytes(mask: &TypingPageMask) -> u64 {
    let bytes = mask
        .tile_textures
        .keys()
        .map(|tile_idx| {
            let tiles_x = mask_tiles_x(mask.width);
            let tx = tile_idx % tiles_x;
            let ty = tile_idx / tiles_x;
            let x0 = tx * MASK_TILE_SIDE_PX;
            let y0 = ty * MASK_TILE_SIDE_PX;
            if x0 >= mask.width || y0 >= mask.height {
                return 0usize;
            }
            let tile_w = (x0 + MASK_TILE_SIDE_PX).min(mask.width) - x0;
            let tile_h = (y0 + MASK_TILE_SIDE_PX).min(mask.height) - y0;
            tile_w.saturating_mul(tile_h).saturating_mul(4)
        })
        .fold(0usize, usize::saturating_add);
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

fn paint_mask_segment(
    brush: &MaskBrush,
    mask: &mut TypingPageMask,
    image_rect: Rect,
    scene_p0: Pos2,
    scene_p1: Pos2,
    radius_px: usize,
    erase: bool,
) -> bool {
    let Some((x0, y0)) = scene_to_mask_px(image_rect, mask.width, mask.height, scene_p0) else {
        return false;
    };
    let Some((x1, y1)) = scene_to_mask_px(image_rect, mask.width, mask.height, scene_p1) else {
        return false;
    };
    let Some((rx, ry, rw, rh)) =
        stroke_bounds_mask_rect(mask, image_rect, scene_p0, scene_p1, radius_px)
    else {
        return false;
    };
    let target = if erase { 0u8 } else { 255u8 };
    let mut has_change = false;
    let max_x = rx.saturating_add(rw).min(mask.width);
    let max_y = ry.saturating_add(rh).min(mask.height);
    for y in ry..max_y {
        let row = y * mask.width;
        for x in rx..max_x {
            if mask.data[row + x] != target {
                has_change = true;
                break;
            }
        }
        if has_change {
            break;
        }
    }
    if !has_change {
        return false;
    }
    brush.paint_binary_mask_segment(
        &mut mask.data,
        mask.width,
        mask.height,
        x0.round() as i32,
        y0.round() as i32,
        x1.round() as i32,
        y1.round() as i32,
        erase,
    );
    true
}

// Parameters represent distinct required inputs with no natural grouping.
#[allow(clippy::too_many_arguments)]
fn flood_fill_mask_from_seed(
    page_idx: usize,
    width: usize,
    height: usize,
    seed_x: usize,
    seed_y: usize,
    tolerance: u8,
    mut mask_data: Vec<u8>,
    source_rgba: Vec<u8>,
) -> Result<TypingMaskFillJobResult, String> {
    if width == 0 || height == 0 {
        return Err("Нельзя выполнить заливку: размер страницы равен 0.".to_string());
    }
    let expected_mask = width.saturating_mul(height);
    if mask_data.len() != expected_mask {
        return Err(
            "Нельзя выполнить заливку: размер маски не совпадает с размером страницы.".to_string(),
        );
    }
    let expected_rgba = expected_mask.saturating_mul(4);
    if source_rgba.len() != expected_rgba {
        return Err("Нельзя выполнить заливку: размер кэшированного изображения не совпадает с размером маски.".to_string());
    }
    if seed_x >= width || seed_y >= height {
        return Err("Нельзя выполнить заливку: точка вне границ страницы.".to_string());
    }

    let seed_idx = seed_y.saturating_mul(width).saturating_add(seed_x);
    let target_off = seed_idx.saturating_mul(4);
    let target_rgb = [
        source_rgba[target_off],
        source_rgba[target_off + 1],
        source_rgba[target_off + 2],
    ];
    let tolerance = tolerance as i16;

    let mut visited = vec![0u8; expected_mask];
    let mut queue = std::collections::VecDeque::<usize>::new();
    queue.push_back(seed_idx);

    let mut changed = false;
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0usize;
    let mut max_y = 0usize;

    while let Some(idx) = queue.pop_front() {
        if idx >= expected_mask || visited[idx] != 0 {
            continue;
        }
        visited[idx] = 1;

        let off = idx.saturating_mul(4);
        let px_rgb = [source_rgba[off], source_rgba[off + 1], source_rgba[off + 2]];
        if !is_rgb_within_tolerance(px_rgb, target_rgb, tolerance) {
            continue;
        }

        if mask_data[idx] != 255 {
            mask_data[idx] = 255;
            changed = true;
            let x = idx % width;
            let y = idx / width;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }

        let x = idx % width;
        let y = idx / width;
        if x > 0 {
            queue.push_back(idx - 1);
        }
        if x + 1 < width {
            queue.push_back(idx + 1);
        }
        if y > 0 {
            queue.push_back(idx - width);
        }
        if y + 1 < height {
            queue.push_back(idx + width);
        }
    }

    let dirty_rect = if changed {
        Some((
            min_x,
            min_y,
            max_x.saturating_sub(min_x).saturating_add(1),
            max_y.saturating_sub(min_y).saturating_add(1),
        ))
    } else {
        None
    };

    Ok(TypingMaskFillJobResult {
        page_idx,
        width,
        height,
        data: mask_data,
        dirty_rect,
    })
}

fn build_fill_source_from_cache_model(
    model: &Arc<Mutex<CleanOverlaysModel>>,
    page_idx: usize,
    target_size: [usize; 2],
) -> Result<Vec<u8>, String> {
    let mut locked = model
        .lock()
        .map_err(|_| "Не удалось получить доступ к кэшу clean overlay.".to_string())?;

    let Some(page_rgba) = locked.cached_page_rgba(page_idx) else {
        return Err(
            "Страница ещё не готова в кэше. Подождите загрузку и повторите заливку.".to_string(),
        );
    };

    let src_size = [page_rgba.width() as usize, page_rgba.height() as usize];
    let mut out = if src_size == target_size {
        page_rgba.as_raw().clone()
    } else {
        resize_rgba_nearest(page_rgba.as_raw(), src_size, target_size)
    };

    if let Some(overlay) = locked.get(page_idx) {
        let overlay_size = overlay.size;
        let overlay_rgba = color_image_to_rgba_unmultiplied(overlay);
        let overlay_composited = if overlay_size == target_size {
            overlay_rgba
        } else {
            resize_rgba_nearest(overlay_rgba.as_slice(), overlay_size, target_size)
        };
        blend_rgba_source_over(out.as_mut_slice(), overlay_composited.as_slice());
    }

    Ok(out)
}

fn is_rgb_within_tolerance(px: [u8; 3], target: [u8; 3], tolerance: i16) -> bool {
    (px[0] as i16 - target[0] as i16).abs() <= tolerance
        && (px[1] as i16 - target[1] as i16).abs() <= tolerance
        && (px[2] as i16 - target[2] as i16).abs() <= tolerance
}

fn color_image_to_rgba_unmultiplied(image: &egui::ColorImage) -> Vec<u8> {
    let mut raw = Vec::with_capacity(image.pixels.len().saturating_mul(4));
    for px in &image.pixels {
        let [r, g, b, a] = px.to_srgba_unmultiplied();
        raw.extend_from_slice(&[r, g, b, a]);
    }
    raw
}

fn resize_rgba_nearest(src_rgba: &[u8], src_size: [usize; 2], dst_size: [usize; 2]) -> Vec<u8> {
    let [src_w, src_h] = src_size;
    let [dst_w, dst_h] = dst_size;
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return vec![0u8; dst_w.saturating_mul(dst_h).saturating_mul(4)];
    }
    if src_rgba.len() != src_w.saturating_mul(src_h).saturating_mul(4) {
        return vec![0u8; dst_w.saturating_mul(dst_h).saturating_mul(4)];
    }
    let mut out = vec![0u8; dst_w.saturating_mul(dst_h).saturating_mul(4)];
    for y in 0..dst_h {
        let sy = y.saturating_mul(src_h) / dst_h;
        for x in 0..dst_w {
            let sx = x.saturating_mul(src_w) / dst_w;
            let src_idx = (sy.saturating_mul(src_w).saturating_add(sx)).saturating_mul(4);
            let dst_idx = (y.saturating_mul(dst_w).saturating_add(x)).saturating_mul(4);
            out[dst_idx..dst_idx + 4].copy_from_slice(&src_rgba[src_idx..src_idx + 4]);
        }
    }
    out
}

fn blend_rgba_source_over(dst_rgba: &mut [u8], src_rgba: &[u8]) {
    if dst_rgba.len() != src_rgba.len() || !dst_rgba.len().is_multiple_of(4) {
        return;
    }
    for i in (0..dst_rgba.len()).step_by(4) {
        let sr = src_rgba[i] as f32 / 255.0;
        let sg = src_rgba[i + 1] as f32 / 255.0;
        let sb = src_rgba[i + 2] as f32 / 255.0;
        let sa = src_rgba[i + 3] as f32 / 255.0;
        if sa <= f32::EPSILON {
            continue;
        }

        let dr = dst_rgba[i] as f32 / 255.0;
        let dg = dst_rgba[i + 1] as f32 / 255.0;
        let db = dst_rgba[i + 2] as f32 / 255.0;
        let da = dst_rgba[i + 3] as f32 / 255.0;

        let out_a = sa + da * (1.0 - sa);
        let (out_r, out_g, out_b) = if out_a <= f32::EPSILON {
            (0.0, 0.0, 0.0)
        } else {
            (
                (sr * sa + dr * da * (1.0 - sa)) / out_a,
                (sg * sa + dg * da * (1.0 - sa)) / out_a,
                (sb * sa + db * da * (1.0 - sa)) / out_a,
            )
        };

        dst_rgba[i] = (out_r * 255.0).round().clamp(0.0, 255.0) as u8;
        dst_rgba[i + 1] = (out_g * 255.0).round().clamp(0.0, 255.0) as u8;
        dst_rgba[i + 2] = (out_b * 255.0).round().clamp(0.0, 255.0) as u8;
        dst_rgba[i + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
    }
}

fn scene_to_mask_px(
    image_rect: Rect,
    mask_w: usize,
    mask_h: usize,
    scene: Pos2,
) -> Option<(f32, f32)> {
    if !image_rect.contains(scene) || mask_w == 0 || mask_h == 0 {
        return None;
    }
    let u = ((scene.x - image_rect.left()) / image_rect.width().max(f32::EPSILON)).clamp(0.0, 1.0);
    let v = ((scene.y - image_rect.top()) / image_rect.height().max(f32::EPSILON)).clamp(0.0, 1.0);
    Some((
        u * (mask_w.saturating_sub(1)) as f32,
        v * (mask_h.saturating_sub(1)) as f32,
    ))
}

fn bilinear_quad_uv(quad_uv: [[f32; 2]; 4], tu: f32, tv: f32) -> [f32; 2] {
    let t = tu.clamp(0.0, 1.0);
    let v = tv.clamp(0.0, 1.0);
    let top_u = lerp_f32(quad_uv[0][0], quad_uv[1][0], t);
    let top_v = lerp_f32(quad_uv[0][1], quad_uv[1][1], t);
    let bot_u = lerp_f32(quad_uv[3][0], quad_uv[2][0], t);
    let bot_v = lerp_f32(quad_uv[3][1], quad_uv[2][1], t);
    [lerp_f32(top_u, bot_u, v), lerp_f32(top_v, bot_v, v)]
}

fn lerp_f32(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn sample_mask_active(mask: &TypingPageMask, u: f32, v: f32) -> bool {
    if mask.width == 0 || mask.height == 0 {
        return false;
    }
    let clamped_u = u.clamp(0.0, 1.0);
    let clamped_v = v.clamp(0.0, 1.0);
    let x = (clamped_u * (mask.width.saturating_sub(1)) as f32).round() as usize;
    let y = (clamped_v * (mask.height.saturating_sub(1)) as f32).round() as usize;
    mask.data
        .get(y.saturating_mul(mask.width).saturating_add(x))
        .is_some_and(|v| *v > 0)
}

fn sample_deform_mesh_uv(
    cols: usize,
    rows: usize,
    points_uv: &[[f32; 2]],
    tu: f32,
    tv: f32,
) -> [f32; 2] {
    let u = tu.clamp(0.0, 1.0) * (cols - 1) as f32;
    let v = tv.clamp(0.0, 1.0) * (rows - 1) as f32;
    let col0 = u.floor().clamp(0.0, (cols - 2) as f32) as usize;
    let row0 = v.floor().clamp(0.0, (rows - 2) as f32) as usize;
    let col1 = (col0 + 1).min(cols - 1);
    let row1 = (row0 + 1).min(rows - 1);
    let local_u = u - col0 as f32;
    let local_v = v - row0 as f32;
    let idx = |col: usize, row: usize| -> usize { row * cols + col };
    bilinear_quad_uv(
        [
            points_uv[idx(col0, row0)],
            points_uv[idx(col1, row0)],
            points_uv[idx(col1, row1)],
            points_uv[idx(col0, row1)],
        ],
        local_u,
        local_v,
    )
}

fn load_masks_from_text_images_dir(
    text_images_dir: &Path,
) -> Result<Vec<TypingMaskLoadedPage>, String> {
    if !text_images_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut pages = Vec::<TypingMaskLoadedPage>::new();
    for entry_res in fs::read_dir(text_images_dir)
        .map_err(|err| format!("Не удалось прочитать {}: {err}", text_images_dir.display()))?
    {
        let entry = match entry_res {
            Ok(v) => v,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(page_idx) = parse_mask_page_idx(file_name) else {
            continue;
        };
        let image = match image::open(&path) {
            Ok(v) => v.to_luma8(),
            Err(_) => continue,
        };
        let (w, h) = image.dimensions();
        if w == 0 || h == 0 {
            continue;
        }
        let mut data = image.into_raw();
        for px in data.iter_mut() {
            *px = if *px >= 128 { 255 } else { 0 };
        }
        pages.push(TypingMaskLoadedPage {
            page_idx,
            width: w as usize,
            height: h as usize,
            data,
        });
    }
    pages.sort_by_key(|page| page.page_idx);
    Ok(pages)
}

fn save_masks_to_text_images_dir(
    text_images_dir: &Path,
    pages: &[TypingMaskSavePage],
) -> Result<(), String> {
    fs::create_dir_all(text_images_dir).map_err(|err| {
        format!(
            "Не удалось создать папку {}: {err}",
            text_images_dir.display()
        )
    })?;
    for page in pages {
        if page.width == 0 || page.height == 0 {
            continue;
        }
        let mut rgba = vec![0u8; page.width * page.height * 4];
        for (idx, &v) in page.data.iter().enumerate() {
            let base = idx * 4;
            rgba[base] = v;
            rgba[base + 1] = v;
            rgba[base + 2] = v;
            rgba[base + 3] = 255;
        }
        let out_path = text_images_dir.join(mask_file_name_for_page(page.page_idx));
        image::save_buffer(
            &out_path,
            &rgba,
            page.width as u32,
            page.height as u32,
            image::ColorType::Rgba8,
        )
        .map_err(|err| format!("Не удалось сохранить {}: {err}", out_path.display()))?;
    }
    Ok(())
}

fn parse_mask_page_idx(file_name: &str) -> Option<usize> {
    let stem = file_name
        .strip_prefix(MASK_FILE_PREFIX)?
        .strip_suffix(MASK_FILE_SUFFIX)?;
    stem.parse::<usize>().ok()
}

fn mask_file_name_for_page(page_idx: usize) -> String {
    format!("{MASK_FILE_PREFIX}{page_idx}{MASK_FILE_SUFFIX}")
}
