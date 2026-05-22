/*
File: src/canvas/overlay_runtime.rs

Purpose:
Runtime clean-overlay subsystem для canvas: хранение overlay-изображений и texture tiles,
подготовка данных в фоне, дозированная загрузка в GPU и локальные операции редактирования.

Main responsibilities:
- держать overlay runtime state отдельно от общего `CanvasView`;
- обслуживать background prepare worker и throttled texture upload;
- выполнять replace/clear/commit операции над clean overlay;
- рисовать tiled overlay поверх страницы без блокировки GUI.
- report and evict reconstructable clean-overlay GPU tile caches under memory pressure.

Key structures:
- OverlayRuntimeState

Key functions:
- OverlayRuntimeState::ensure_overlay_for_page_size()
- OverlayRuntimeState::ensure_editable_overlay_for_page()
- OverlayRuntimeState::draw_overlay_on_page()
- OverlayRuntimeState::replace_overlay_region()
- OverlayRuntimeState::replace_overlay_region_px()
Notes:
- Публичный API для остальных вкладок по-прежнему проходит через `CanvasView`;
  этот модуль обслуживает только внутренний runtime bucket overlay-подсистемы.
*/

use super::helpers::{blit_scaled_chunk, build_overlay_tile_image};
use super::types::{
    OverlayPrepareRequest, OverlayPrepareResult, OverlayPreparedPage, OverlayRectPx,
    OverlayTexturePage, OverlayTextureTile,
};
use super::workers::spawn_overlay_prepare_thread;
use super::{OVERLAY_FULL_REBUILD_DIRTY_TILES_THRESHOLD, OVERLAY_TILE_SIDE};
use crate::memory_manager::{
    CacheEvictionReport, CacheEvictionRequest, CacheReloadCost, CacheResourceInfo,
    CacheResourceKind, select_eviction_candidates,
};
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::runtime_log;
use eframe::egui;
use egui::{Color32, Pos2, Rect};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

pub(super) struct OverlayRuntimeState {
    pub(super) overlays_visible: bool,
    pub(super) local_visibility_override: Option<bool>,
    pub(super) overlay_images: HashMap<usize, Arc<egui::ColorImage>>,
    pub(super) overlay_textures: HashMap<usize, OverlayTexturePage>,
    pub(super) overlay_texture_last_used_frame: HashMap<usize, u64>,
    pub(super) overlay_dirty_tiles: HashMap<usize, HashSet<usize>>,
    pub(super) overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    pub(super) synced_overlays_revision: u64,
    pub(super) overlay_upload_min_interval_s: f64,
    pub(super) overlay_last_upload_s: HashMap<usize, f64>,
    pub(super) overlay_prepare_tx: Sender<Option<OverlayPrepareRequest>>,
    pub(super) overlay_prepare_rx: Receiver<OverlayPrepareResult>,
    pub(super) overlay_prepare_thread: Option<JoinHandle<()>>,
    pub(super) overlay_prepare_inflight: HashMap<usize, u64>,
    pub(super) overlay_prepared_pages: HashMap<usize, OverlayPreparedPage>,
    pub(super) overlay_prepare_next_job_id: u64,
    pub(super) overlay_render_suppressed: bool,
}

impl Default for OverlayRuntimeState {
    fn default() -> Self {
        let (overlay_prepare_tx, overlay_prepare_rx, overlay_prepare_thread) =
            spawn_overlay_prepare_thread();
        Self {
            overlays_visible: false,
            local_visibility_override: None,
            overlay_images: HashMap::new(),
            overlay_textures: HashMap::new(),
            overlay_texture_last_used_frame: HashMap::new(),
            overlay_dirty_tiles: HashMap::new(),
            overlays_model: None,
            synced_overlays_revision: 0,
            overlay_upload_min_interval_s: 0.0,
            overlay_last_upload_s: HashMap::new(),
            overlay_prepare_tx,
            overlay_prepare_rx,
            overlay_prepare_thread: Some(overlay_prepare_thread),
            overlay_prepare_inflight: HashMap::new(),
            overlay_prepared_pages: HashMap::new(),
            overlay_prepare_next_job_id: 1,
            overlay_render_suppressed: false,
        }
    }
}

fn clip_overlay_rect_to_size(rect: OverlayRectPx, size: [usize; 2]) -> OverlayRectPx {
    let x0 = rect.x.min(size[0]);
    let y0 = rect.y.min(size[1]);
    let x1 = rect.x.saturating_add(rect.w).min(size[0]);
    let y1 = rect.y.saturating_add(rect.h).min(size[1]);
    OverlayRectPx {
        x: x0,
        y: y0,
        w: x1.saturating_sub(x0),
        h: y1.saturating_sub(y0),
    }
}

impl OverlayRuntimeState {
    pub(super) fn shutdown(&mut self) {
        if self.overlay_prepare_tx.send(None).is_err() {
            runtime_log::log_warn(
                "[canvas::overlay_runtime] failed to signal overlay prepare worker shutdown",
            );
        }
        if let Some(handle) = self.overlay_prepare_thread.take()
            && handle.join().is_err()
        {
            runtime_log::log_warn(
                "[canvas::overlay_runtime] overlay prepare worker panicked during shutdown",
            );
        }
    }

    pub(super) fn set_model(&mut self, model: Arc<Mutex<CleanOverlaysModel>>) {
        let model_visible = match model.lock() {
            Ok(locked) => locked.is_visible(),
            Err(_) => {
                runtime_log::log_warn(
                    "[canvas::overlay_runtime] failed to lock CleanOverlaysModel while attaching canvas model; overlays visibility reset to hidden",
                );
                false
            }
        };
        self.overlays_visible = self.local_visibility_override.unwrap_or(model_visible);
        self.overlays_model = Some(model);
        self.synced_overlays_revision = 0;
        self.overlay_prepare_inflight.clear();
        self.overlay_prepared_pages.clear();
    }

    pub(super) fn model_handle(&self) -> Option<Arc<Mutex<CleanOverlaysModel>>> {
        self.overlays_model.clone()
    }

    pub(super) fn set_render_suppressed(&mut self, suppressed: bool) {
        self.overlay_render_suppressed = suppressed;
    }

    pub(super) fn set_upload_min_interval_s(&mut self, seconds: f64) {
        self.overlay_upload_min_interval_s = seconds.max(0.0);
    }

    pub(super) fn set_clean_overlays_visible(&mut self, visible: bool) {
        let Some(model) = self.overlays_model.as_ref() else {
            self.overlays_visible = false;
            self.local_visibility_override = None;
            return;
        };
        self.local_visibility_override = None;
        self.overlays_visible = visible;
        if let Ok(mut locked) = model.lock() {
            locked.set_visible(visible);
            self.synced_overlays_revision = locked.revision();
        }
    }

    pub(super) fn set_clean_overlays_visible_for_canvas_only(&mut self, visible: bool) {
        if self.overlays_model.is_none() {
            self.overlays_visible = false;
            self.local_visibility_override = None;
            return;
        }
        self.local_visibility_override = Some(visible);
        self.overlays_visible = visible;
    }

    pub(super) fn apply_model_visibility(&mut self, visible: bool) {
        if self.local_visibility_override.is_none() {
            self.overlays_visible = visible;
        }
    }

    pub(super) fn clean_overlays_visible(&self) -> bool {
        self.overlays_model.is_some() && self.overlays_visible
    }

    pub(super) fn ensure_overlay_for_page_size(
        &mut self,
        page_idx: usize,
        size_px: [usize; 2],
    ) -> bool {
        if self.overlays_model.is_none() {
            return false;
        }
        if self.overlay_images.contains_key(&page_idx) {
            return true;
        }

        let [w, h] = size_px;
        if w == 0 || h == 0 {
            return false;
        }

        let model_snapshot = if let Some(model) = self.overlays_model.clone() {
            match model.lock() {
                Ok(locked) => locked
                    .get(page_idx)
                    .cloned()
                    .map(|image| (image, locked.revision())),
                Err(_) => None,
            }
        } else {
            None
        };
        if let Some((image, revision)) = model_snapshot {
            self.reset_prepare_state(page_idx);
            self.overlay_last_upload_s.remove(&page_idx);
            self.overlay_textures.remove(&page_idx);
            self.overlay_texture_last_used_frame.remove(&page_idx);
            self.overlay_images.insert(page_idx, Arc::new(image));
            self.mark_dirty_full(page_idx);
            self.synced_overlays_revision = revision;
            return true;
        }
        false
    }

    pub(super) fn ensure_editable_overlay_for_page(
        &mut self,
        page_idx: usize,
        size_px: [usize; 2],
    ) -> bool {
        if self.overlay_images.contains_key(&page_idx) {
            return true;
        }

        let [w, h] = size_px;
        if w == 0 || h == 0 {
            return false;
        }

        self.reset_prepare_state(page_idx);
        self.overlay_last_upload_s.remove(&page_idx);
        self.overlay_textures.remove(&page_idx);
        self.overlay_images.insert(
            page_idx,
            Arc::new(egui::ColorImage::filled([w, h], Color32::TRANSPARENT)),
        );
        self.mark_dirty_full(page_idx);
        if let Some(model) = self.overlays_model.as_ref()
            && let Ok(mut locked) = model.lock()
        {
            locked.ensure_overlay(page_idx, [w, h]);
            self.synced_overlays_revision = locked.revision();
        }
        true
    }

    pub(super) fn reset_prepare_state(&mut self, page_idx: usize) {
        self.overlay_prepare_inflight.remove(&page_idx);
        self.overlay_prepared_pages.remove(&page_idx);
    }

    pub(super) fn request_prepare(&mut self, page_idx: usize, image: Arc<egui::ColorImage>) {
        if self.overlay_prepare_inflight.contains_key(&page_idx)
            || self.overlay_prepared_pages.contains_key(&page_idx)
        {
            return;
        }
        let job_id = self.overlay_prepare_next_job_id;
        self.overlay_prepare_next_job_id = self.overlay_prepare_next_job_id.saturating_add(1);
        let request = OverlayPrepareRequest {
            page_idx,
            job_id,
            image,
        };
        if self.overlay_prepare_tx.send(Some(request)).is_ok() {
            self.overlay_prepare_inflight.insert(page_idx, job_id);
        }
    }

    pub(super) fn poll_prepare_results(&mut self) {
        loop {
            match self.overlay_prepare_rx.try_recv() {
                Ok(result) => {
                    let Some(inflight_job_id) =
                        self.overlay_prepare_inflight.get(&result.page_idx).copied()
                    else {
                        continue;
                    };
                    if inflight_job_id != result.job_id {
                        continue;
                    }
                    self.overlay_prepare_inflight.remove(&result.page_idx);
                    self.overlay_prepared_pages.insert(
                        result.page_idx,
                        OverlayPreparedPage {
                            size: result.size,
                            tiles: result.tiles,
                            next_upload_tile: 0,
                        },
                    );
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    pub(super) fn upload_prepared_tiles(
        &mut self,
        ctx: &egui::Context,
        page_idx: usize,
        tile_budget: &mut usize,
        bytes_budget: &mut usize,
        texture_options: egui::TextureOptions,
    ) {
        let done;
        {
            let Some(prepared) = self.overlay_prepared_pages.get_mut(&page_idx) else {
                return;
            };
            let Some(page_tex) = self.overlay_textures.get_mut(&page_idx) else {
                return;
            };
            if prepared.size != page_tex.size {
                done = true;
            } else {
                let mut uploaded_any = false;
                while prepared.next_upload_tile < prepared.tiles.len() && *tile_budget > 0 {
                    let tile = &prepared.tiles[prepared.next_upload_tile];
                    if uploaded_any && tile.rgba.len() > *bytes_budget {
                        break;
                    }
                    let tile_img = egui::ColorImage::from_rgba_premultiplied(
                        [tile.size_px[0], tile.size_px[1]],
                        &tile.rgba,
                    );
                    if tile.tile_idx < page_tex.tiles.len() {
                        page_tex.tiles[tile.tile_idx]
                            .texture
                            .set(tile_img, texture_options);
                    } else if tile.tile_idx == page_tex.tiles.len() {
                        let texture = ctx.load_texture(
                            format!(
                                "overlay-{}-{}-{}-{:?}",
                                page_idx, tile.origin_px[0], tile.origin_px[1], texture_options
                            ),
                            tile_img,
                            texture_options,
                        );
                        page_tex.tiles.push(OverlayTextureTile {
                            texture,
                            origin_px: tile.origin_px,
                            size_px: tile.size_px,
                        });
                    }
                    prepared.next_upload_tile += 1;
                    *tile_budget = tile_budget.saturating_sub(1);
                    *bytes_budget = bytes_budget.saturating_sub(tile.rgba.len());
                    uploaded_any = true;
                }
                done = prepared.next_upload_tile >= prepared.tiles.len();
            }
        }
        if done {
            self.overlay_prepared_pages.remove(&page_idx);
        }
    }

    pub(super) fn has_pending_work(&self) -> bool {
        !self.overlay_prepare_inflight.is_empty()
            || !self.overlay_prepared_pages.is_empty()
            || self
                .overlay_dirty_tiles
                .values()
                .any(|dirty| !dirty.is_empty())
    }

    pub(super) fn memory_usage_snapshot(
        &self,
        pinned_pages: &BTreeSet<usize>,
    ) -> Vec<CacheResourceInfo> {
        self.overlay_textures
            .iter()
            .map(|(page_idx, page_tex)| {
                let estimated_bytes = page_tex
                    .tiles
                    .iter()
                    .map(|tile| {
                        tile.size_px[0]
                            .saturating_mul(tile.size_px[1])
                            .saturating_mul(4)
                    })
                    .fold(0usize, usize::saturating_add);
                CacheResourceInfo {
                    id: format!("clean-overlay-gpu:{page_idx}"),
                    kind: CacheResourceKind::CleanOverlayGpu,
                    page_idx: Some(*page_idx),
                    estimated_bytes: u64::try_from(estimated_bytes).unwrap_or(u64::MAX),
                    last_used_frame: self
                        .overlay_texture_last_used_frame
                        .get(page_idx)
                        .copied()
                        .unwrap_or(0),
                    reload_cost: CacheReloadCost::RebuildFromModel,
                    dirty: false,
                    visible: pinned_pages.contains(page_idx),
                    reconstructable: !self.overlay_prepare_inflight.contains_key(page_idx)
                        && !self.overlay_prepared_pages.contains_key(page_idx),
                }
            })
            .collect()
    }

    pub(super) fn evict_cache(&mut self, request: &CacheEvictionRequest) -> CacheEvictionReport {
        let snapshot = self.memory_usage_snapshot(&request.pinned_pages);
        let candidates = select_eviction_candidates(&snapshot, request);
        let mut evicted = Vec::new();
        let mut freed = 0_u64;
        for resource in candidates.resources {
            let Some(page_idx) = resource.page_idx else {
                continue;
            };
            if self.overlay_prepare_inflight.contains_key(&page_idx)
                || self.overlay_prepared_pages.contains_key(&page_idx)
            {
                continue;
            }
            if self.overlay_textures.remove(&page_idx).is_some() {
                self.overlay_last_upload_s.remove(&page_idx);
                self.overlay_texture_last_used_frame.remove(&page_idx);
                freed = freed.saturating_add(resource.estimated_bytes);
                evicted.push(resource);
            }
        }
        CacheEvictionReport {
            resources: evicted,
            estimated_freed_bytes: freed,
        }
    }

    pub(super) fn overlay_image(&self, page_idx: usize) -> Option<&egui::ColorImage> {
        self.overlay_images.get(&page_idx).map(Arc::as_ref)
    }

    pub(super) fn clear_overlay_index(&mut self, page_idx: usize, size_px: [usize; 2]) {
        if !self.ensure_editable_overlay_for_page(page_idx, size_px) {
            return;
        }
        let mut should_sync_model = false;
        if let Some(ov) = self.overlay_images.get_mut(&page_idx) {
            let overlay = Arc::make_mut(ov);
            for px in &mut overlay.pixels {
                *px = Color32::TRANSPARENT;
            }
            should_sync_model = true;
        }
        if should_sync_model {
            self.mark_dirty_full(page_idx);
            if let Some(model) = self.overlays_model.as_ref()
                && let Some(overlay) = self.overlay_images.get(&page_idx)
                && let Ok(mut locked) = model.lock()
            {
                locked.replace(page_idx, overlay);
                self.synced_overlays_revision = locked.revision();
            }
        }
    }

    pub(super) fn scene_point_to_overlay_xy(
        &self,
        page_idx: usize,
        scene_pt: Pos2,
        page_rect: Rect,
    ) -> Option<(usize, usize)> {
        let ov = self.overlay_images.get(&page_idx)?;
        let w = ov.size[0];
        let h = ov.size[1];
        if w == 0 || h == 0 {
            return None;
        }
        let u = ((scene_pt.x - page_rect.left()) / page_rect.width()).clamp(0.0, 1.0);
        let v = ((scene_pt.y - page_rect.top()) / page_rect.height()).clamp(0.0, 1.0);
        let x = ((u * w as f32).round() as isize).clamp(0, (w.saturating_sub(1)) as isize) as usize;
        let y = ((v * h as f32).round() as isize).clamp(0, (h.saturating_sub(1)) as isize) as usize;
        Some((x, y))
    }

    pub(super) fn scene_rect_to_overlay_rect(
        &self,
        page_idx: usize,
        scene_rect: Rect,
        page_rect: Rect,
    ) -> Option<OverlayRectPx> {
        let ov = self.overlay_images.get(&page_idx)?;
        let clipped = scene_rect.intersect(page_rect);
        if !clipped.is_positive() {
            return None;
        }
        let w = ov.size[0];
        let h = ov.size[1];
        if w == 0 || h == 0 {
            return None;
        }
        let u0 = ((clipped.left() - page_rect.left()) / page_rect.width()).clamp(0.0, 1.0);
        let v0 = ((clipped.top() - page_rect.top()) / page_rect.height()).clamp(0.0, 1.0);
        let u1 = ((clipped.right() - page_rect.left()) / page_rect.width()).clamp(0.0, 1.0);
        let v1 = ((clipped.bottom() - page_rect.top()) / page_rect.height()).clamp(0.0, 1.0);
        let x0 = ((u0 * w as f32).round() as isize).clamp(0, w as isize) as usize;
        let y0 = ((v0 * h as f32).round() as isize).clamp(0, h as isize) as usize;
        let x1 = ((u1 * w as f32).round() as isize).clamp(0, w as isize) as usize;
        let y1 = ((v1 * h as f32).round() as isize).clamp(0, h as isize) as usize;
        let rw = x1.saturating_sub(x0);
        let rh = y1.saturating_sub(y0);
        if rw == 0 || rh == 0 {
            return None;
        }
        Some(OverlayRectPx {
            x: x0,
            y: y0,
            w: rw,
            h: rh,
        })
    }

    pub(super) fn replace_overlay_region(
        &mut self,
        page_idx: usize,
        size_px: [usize; 2],
        scene_rect: Rect,
        page_rect: Rect,
        chunk: &egui::ColorImage,
        sync_model: bool,
    ) -> bool {
        if !self.ensure_editable_overlay_for_page(page_idx, size_px) {
            return false;
        }
        if chunk.size[0] == 0 || chunk.size[1] == 0 {
            return false;
        }
        let Some(target) = self.scene_rect_to_overlay_rect(page_idx, scene_rect, page_rect) else {
            return false;
        };
        let Some(dst) = self.overlay_images.get_mut(&page_idx) else {
            return false;
        };
        blit_scaled_chunk(Arc::make_mut(dst), target, chunk);
        if sync_model
            && let Some(model) = self.overlays_model.as_ref()
            && let Ok(mut locked) = model.lock()
        {
            locked.replace_region(
                page_idx, size_px, target.x, target.y, target.w, target.h, chunk,
            );
            self.synced_overlays_revision = locked.revision();
        }
        self.mark_dirty_rect(page_idx, target.x, target.y, target.w, target.h);
        true
    }

    pub(super) fn replace_overlay_region_px(
        &mut self,
        page_idx: usize,
        size_px: [usize; 2],
        target: OverlayRectPx,
        chunk: &egui::ColorImage,
        sync_model: bool,
    ) -> bool {
        if !self.ensure_editable_overlay_for_page(page_idx, size_px) {
            return false;
        }
        if target.w == 0 || target.h == 0 || chunk.size[0] == 0 || chunk.size[1] == 0 {
            return false;
        }
        let Some(dst) = self.overlay_images.get_mut(&page_idx) else {
            return false;
        };
        let clipped = clip_overlay_rect_to_size(target, dst.size);
        if clipped.w == 0 || clipped.h == 0 {
            return false;
        }
        blit_scaled_chunk(Arc::make_mut(dst), clipped, chunk);
        if sync_model
            && let Some(model) = self.overlays_model.as_ref()
            && let Ok(mut locked) = model.lock()
        {
            locked.replace_region(
                page_idx, size_px, clipped.x, clipped.y, clipped.w, clipped.h, chunk,
            );
            self.synced_overlays_revision = locked.revision();
        }
        self.mark_dirty_rect(page_idx, clipped.x, clipped.y, clipped.w, clipped.h);
        true
    }

    pub(super) fn commit_overlay_page_to_model(&mut self, page_idx: usize) -> bool {
        let Some(model) = self.overlays_model.as_ref() else {
            return false;
        };
        let Some(dst) = self.overlay_images.get(&page_idx) else {
            return false;
        };
        if let Ok(mut locked) = model.lock() {
            locked.replace(page_idx, dst);
            self.synced_overlays_revision = locked.revision();
            return true;
        }
        false
    }

    pub(super) fn mark_dirty_full(&mut self, page_idx: usize) {
        let Some(ov) = self.overlay_images.get(&page_idx) else {
            self.overlay_dirty_tiles.remove(&page_idx);
            return;
        };
        let w = ov.size[0];
        let h = ov.size[1];
        if w == 0 || h == 0 {
            self.overlay_dirty_tiles.remove(&page_idx);
            return;
        }
        let tiles_x = w.div_ceil(OVERLAY_TILE_SIDE);
        let tiles_y = h.div_ceil(OVERLAY_TILE_SIDE);
        let full = self.overlay_dirty_tiles.entry(page_idx).or_default();
        full.clear();
        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                full.insert(ty * tiles_x + tx);
            }
        }
    }

    pub(super) fn mark_dirty_rect(
        &mut self,
        page_idx: usize,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    ) {
        if w == 0 || h == 0 {
            return;
        }
        let Some(ov) = self.overlay_images.get(&page_idx) else {
            return;
        };
        let ov_w = ov.size[0];
        let ov_h = ov.size[1];
        if ov_w == 0 || ov_h == 0 {
            return;
        }
        let x0 = x.min(ov_w);
        let y0 = y.min(ov_h);
        let x1 = x.saturating_add(w).min(ov_w);
        let y1 = y.saturating_add(h).min(ov_h);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let tile_x0 = x0 / OVERLAY_TILE_SIDE;
        let tile_y0 = y0 / OVERLAY_TILE_SIDE;
        let tile_x1 = (x1.saturating_sub(1)) / OVERLAY_TILE_SIDE;
        let tile_y1 = (y1.saturating_sub(1)) / OVERLAY_TILE_SIDE;
        let tiles_x = ov_w.div_ceil(OVERLAY_TILE_SIDE);
        let dirty = self.overlay_dirty_tiles.entry(page_idx).or_default();
        for ty in tile_y0..=tile_y1 {
            for tx in tile_x0..=tile_x1 {
                dirty.insert(ty * tiles_x + tx);
            }
        }
    }

    pub(super) fn draw_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        image_rect: Rect,
        tile_budget: &mut usize,
        bytes_budget: &mut usize,
        pixel_sampling_nearest: bool,
    ) {
        let Some(ov_size) = self.overlay_images.get(&page_idx).map(|ov| ov.size) else {
            return;
        };
        if ov_size[0] == 0 || ov_size[1] == 0 {
            return;
        }
        let texture_options = if pixel_sampling_nearest {
            egui::TextureOptions::NEAREST
        } else {
            egui::TextureOptions::LINEAR
        };

        let dirty_tiles = self.overlay_dirty_tiles.remove(&page_idx);
        let now_s = ui.ctx().input(|i| i.time);
        let needs_reset = self
            .overlay_textures
            .get(&page_idx)
            .is_none_or(|tex| tex.size != ov_size || tex.texture_options != texture_options);
        if needs_reset {
            self.reset_prepare_state(page_idx);
            self.overlay_textures.insert(
                page_idx,
                OverlayTexturePage {
                    size: ov_size,
                    texture_options,
                    tiles: Vec::new(),
                },
            );
            self.overlay_last_upload_s.remove(&page_idx);
        }
        if self
            .overlay_textures
            .get(&page_idx)
            .is_some_and(|tex| tex.tiles.is_empty())
            && let Some(ov) = self.overlay_images.get(&page_idx).cloned()
        {
            self.request_prepare(page_idx, ov);
        }
        self.upload_prepared_tiles(
            ui.ctx(),
            page_idx,
            tile_budget,
            bytes_budget,
            texture_options,
        );

        if let Some(dirty_tiles) = dirty_tiles {
            let can_skip_upload = self.overlay_upload_min_interval_s > 0.0
                && !dirty_tiles.is_empty()
                && self
                    .overlay_last_upload_s
                    .get(&page_idx)
                    .is_some_and(|last| now_s - *last < self.overlay_upload_min_interval_s);
            let prefer_partial_upload =
                self.overlay_textures
                    .get(&page_idx)
                    .is_some_and(|page_tex| {
                        let tiles_x = page_tex.size[0].div_ceil(OVERLAY_TILE_SIDE);
                        let tiles_y = page_tex.size[1].div_ceil(OVERLAY_TILE_SIDE);
                        tiles_x <= 1 || tiles_y <= 1
                    });
            if can_skip_upload {
                self.overlay_dirty_tiles
                    .entry(page_idx)
                    .or_default()
                    .extend(dirty_tiles);
            } else if !prefer_partial_upload
                && dirty_tiles.len() >= OVERLAY_FULL_REBUILD_DIRTY_TILES_THRESHOLD
            {
                self.reset_prepare_state(page_idx);
                if let Some(ov) = self.overlay_images.get(&page_idx).cloned() {
                    self.request_prepare(page_idx, ov);
                }
            } else {
                let mut uploaded_any = false;
                let mut remaining_dirty = HashSet::new();
                let tile_meta: Vec<(usize, [usize; 2], [usize; 2])> = self
                    .overlay_textures
                    .get(&page_idx)
                    .map(|page_tex| {
                        dirty_tiles
                            .iter()
                            .filter_map(|tile_idx| {
                                page_tex
                                    .tiles
                                    .get(*tile_idx)
                                    .map(|tile| (*tile_idx, tile.origin_px, tile.size_px))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for tile_idx in dirty_tiles {
                    if tile_meta.iter().all(|(idx, _, _)| *idx != tile_idx) {
                        remaining_dirty.insert(tile_idx);
                    }
                }
                for (tile_idx, origin_px, size_px) in tile_meta {
                    if *tile_budget == 0 {
                        remaining_dirty.insert(tile_idx);
                        continue;
                    }
                    let tile_bytes = size_px[0].saturating_mul(size_px[1]).saturating_mul(4);
                    if uploaded_any && tile_bytes > *bytes_budget {
                        remaining_dirty.insert(tile_idx);
                        continue;
                    }
                    let Some(ov) = self.overlay_images.get(&page_idx) else {
                        break;
                    };
                    let tile_img = build_overlay_tile_image(
                        ov,
                        origin_px[0],
                        origin_px[1],
                        size_px[0],
                        size_px[1],
                    );
                    if let Some(page_tex) = self.overlay_textures.get_mut(&page_idx)
                        && let Some(tile) = page_tex.tiles.get_mut(tile_idx)
                    {
                        tile.texture.set(tile_img, texture_options);
                        *tile_budget = tile_budget.saturating_sub(1);
                        *bytes_budget = bytes_budget.saturating_sub(tile_bytes);
                        uploaded_any = true;
                    }
                }
                if !remaining_dirty.is_empty() {
                    self.overlay_dirty_tiles
                        .entry(page_idx)
                        .or_default()
                        .extend(remaining_dirty);
                }
                if uploaded_any {
                    self.overlay_last_upload_s.insert(page_idx, now_s);
                }
            }
        }

        let Some(page_tex) = self.overlay_textures.get(&page_idx) else {
            return;
        };
        self.overlay_texture_last_used_frame
            .insert(page_idx, ui.ctx().cumulative_frame_nr());
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
                    image_rect.left() + image_rect.width() * (ox / src_w),
                    image_rect.top() + image_rect.height() * (oy / src_h),
                ),
                egui::vec2(
                    image_rect.width() * (tw / src_w),
                    image_rect.height() * (th / src_h),
                ),
            );
            ui.painter().image(
                tile.texture.id(),
                dst,
                Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_model() -> Arc<Mutex<CleanOverlaysModel>> {
        Arc::new(Mutex::new(CleanOverlaysModel::new_from_pages(&[
            PathBuf::from("001.png"),
        ])))
    }

    fn model_visible(model: &Arc<Mutex<CleanOverlaysModel>>) -> bool {
        match model.lock() {
            Ok(locked) => locked.is_visible(),
            Err(err) => panic!("test failed to lock CleanOverlaysModel: {err}"),
        }
    }

    fn set_model_visible(model: &Arc<Mutex<CleanOverlaysModel>>, visible: bool) {
        match model.lock() {
            Ok(mut locked) => locked.set_visible(visible),
            Err(err) => panic!("test failed to lock CleanOverlaysModel: {err}"),
        }
    }

    #[test]
    fn canvas_only_visibility_does_not_mutate_shared_model() {
        let model = test_model();
        let mut runtime = OverlayRuntimeState::default();
        runtime.set_model(Arc::clone(&model));

        runtime.set_clean_overlays_visible_for_canvas_only(false);

        assert!(!runtime.clean_overlays_visible());
        assert!(model_visible(&model));
        runtime.shutdown();
    }

    #[test]
    fn local_visibility_override_ignores_model_visibility_until_shared_set() {
        let model = test_model();
        let mut runtime = OverlayRuntimeState::default();
        runtime.set_model(Arc::clone(&model));

        runtime.set_clean_overlays_visible_for_canvas_only(false);
        set_model_visible(&model, true);
        runtime.apply_model_visibility(true);
        assert!(!runtime.clean_overlays_visible());

        runtime.set_clean_overlays_visible(true);
        assert!(runtime.clean_overlays_visible());
        set_model_visible(&model, false);
        runtime.apply_model_visibility(false);
        assert!(!runtime.clean_overlays_visible());
        runtime.shutdown();
    }
}
