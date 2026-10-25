/*
File: crates/ms-canvas/src/overlay_runtime.rs

Purpose:
Runtime clean-overlay subsystem для canvas: хранение overlay-изображений и texture tiles,
подготовка данных в фоне, дозированная загрузка в GPU и локальные операции редактирования.

Main responsibilities:
- держать overlay runtime state отдельно от общего `CanvasView`;
- обслуживать background prepare worker и throttled texture upload;
- выполнять replace/clear/commit операции над clean overlay;
- рисовать tiled overlay поверх страницы без блокировки GUI.
- report and evict reconstructable clean-overlay GPU tile caches under memory pressure.
- keep pending GPU work from repainting the GUI for pages that are not drawn this frame.

Key structures:
- OverlayRuntimeState

Key functions:
- OverlayRuntimeState::ensure_overlay_for_page_size()
- OverlayRuntimeState::ensure_editable_overlay_for_page()
- OverlayRuntimeState::draw_overlay_on_page()
- OverlayRuntimeState::replace_overlay_region()
- OverlayRuntimeState::replace_overlay_region_px()
- OverlayRuntimeState::sync_from_model() + plan_overlay_sync(): non-destructive per-canvas delta apply
- write_own_change() / synced_revision_after_own_write(): own model writes never skip foreign changes
- begin_draw_tracking() / has_pending_work(): the pending-work repaint rule
- upload_page_work(): the GPU half of draw_overlay_on_page (also run for render-suppressed pages)
- page_settled() / page_generation(): what a tool preview standing in for a page waits on
Notes:
- Render suppression is per page (`overlay_render_suppressed_pages`): a suppressed page in view is
  still uploaded and counted in `drawn_pages`, only its painting is skipped. A tool preview that
  replaces it retires once `page_settled` (textures current) or `page_generation` moved.
- A tile edited while a full prepare is pending is "superseded": the late prepare uploads it from
  the current pixels, and dirty tiles missing from a half-built texture request a full prepare, so
  no pending work is stranded and settled pages really show `overlay_images`.
- Pending work (prepare in flight, prepared tiles, dirty tiles) can only be uploaded while its page
  is drawn, so only pages drawn this frame (`drawn_pages`) count as pending for the repaint request.
  Pending work of an off-screen page is kept (a finished prepare is not thrown away) and resumes
  when the page is drawn again; memory-pressure eviction may free it for any non-pinned page.
- The synced revision is a per-canvas cursor into the shared, non-destructive
  `CleanOverlaysModel::delta_since`; own writes advance it only when the canvas was fully synced,
  and the read-only lazy load (`ensure_overlay_for_page_size`) never advances it. Own writes record
  the page stamp they produced (`own_write_stamps`), so a re-listed own page is not re-fetched.
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
use ms_memory::{
    CacheEvictionReport, CacheEvictionRequest, CacheReloadCost, CacheResourceInfo,
    CacheResourceKind, select_eviction_candidates,
};
use ms_models::clean_overlays_model::{CleanOverlaysModel, RegionPx};
use ms_log::runtime_log;
use eframe::egui;
use egui::{Color32, Pos2, Rect};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use ms_thread::JoinHandle;

pub(super) struct OverlayRuntimeState {
    pub(super) overlays_visible: bool,
    pub(super) local_visibility_override: Option<bool>,
    pub(super) overlay_images: HashMap<usize, Arc<egui::ColorImage>>,
    pub(super) overlay_textures: HashMap<usize, OverlayTexturePage>,
    pub(super) overlay_texture_last_used_frame: HashMap<usize, u64>,
    pub(super) overlay_dirty_tiles: HashMap<usize, HashSet<usize>>,
    pub(super) overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    pub(super) synced_overlays_revision: u64,
    /// Per page, the model change stamp produced by this canvas's OWN last write to it. A cached
    /// page whose model stamp still equals it holds exactly that write, so sync keeps the cached
    /// image (which may carry newer local-only preview pixels) instead of re-fetching it.
    pub(super) own_write_stamps: HashMap<usize, u64>,
    pub(super) overlay_upload_min_interval_s: f64,
    pub(super) overlay_last_upload_s: HashMap<usize, f64>,
    pub(super) overlay_prepare_tx: Sender<Option<OverlayPrepareRequest>>,
    pub(super) overlay_prepare_rx: Receiver<OverlayPrepareResult>,
    pub(super) overlay_prepare_thread: Option<JoinHandle<()>>,
    pub(super) overlay_prepare_inflight: HashMap<usize, u64>,
    pub(super) overlay_prepared_pages: HashMap<usize, OverlayPreparedPage>,
    /// Per page, the tiles whose CPU pixels changed AFTER its pending prepare (in flight or not yet
    /// fully uploaded) took its snapshot. The prepare's copy of such a tile is older than what the
    /// dirty-tile path may already have uploaded, so `upload_prepared_tiles` builds it from the
    /// current `overlay_images` instead: newer pixels always win over a late full prepare.
    pub(super) overlay_prepare_superseded: HashMap<usize, HashSet<usize>>,
    pub(super) overlay_prepare_next_job_id: u64,
    /// Pages whose overlay is NOT painted this frame because a tool preview stands in for it
    /// (`CanvasView::set_overlay_render_suppressed_pages`). Their GPU work still progresses:
    /// `draw_overlay_on_page` uploads and records them in `drawn_pages`, it only skips painting.
    pub(super) overlay_render_suppressed_pages: HashSet<usize>,
    /// Per page, the value of `overlay_generation_clock` at the last change of its
    /// `overlay_images` entry (own edit, model sync, lazy load, drop); see `page_generation`.
    pub(super) overlay_page_generations: HashMap<usize, u64>,
    /// Monotonic source of `overlay_page_generations` values, so a page that is dropped and
    /// re-created never repeats an old generation.
    pub(super) overlay_generation_clock: u64,
    /// Pages whose overlay was drawn (in view, overlays enabled) in the current frame; cleared by
    /// `begin_draw_tracking`, filled by `draw_overlay_on_page`. Only these pages' pending work
    /// keeps the canvas repainting.
    pub(super) drawn_pages: HashSet<usize>,
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
            own_write_stamps: HashMap::new(),
            overlay_upload_min_interval_s: 0.0,
            overlay_last_upload_s: HashMap::new(),
            overlay_prepare_tx,
            overlay_prepare_rx,
            overlay_prepare_thread: Some(overlay_prepare_thread),
            overlay_prepare_inflight: HashMap::new(),
            overlay_prepared_pages: HashMap::new(),
            overlay_prepare_superseded: HashMap::new(),
            overlay_prepare_next_job_id: 1,
            overlay_render_suppressed_pages: HashSet::new(),
            overlay_page_generations: HashMap::new(),
            overlay_generation_clock: 0,
            drawn_pages: HashSet::new(),
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

/// The canvas's synced revision after its own model write moved the model from revision
/// `before` to `after`.
///
/// Advances to `after` only when the canvas was fully synced before the write (`synced == before`):
/// then the only change between `synced` and `after` is the canvas's own, which it already shows.
/// Otherwise a foreign change landed after the last sync; jumping past it would hide it forever,
/// so `synced` is kept and the next `delta_since` delivers the foreign pages. It also lists the
/// canvas's own page again; sync skips re-fetching that page while its model stamp still equals
/// the one recorded in `own_write_stamps`, so local-only preview pixels drawn after the write
/// survive (see [`plan_overlay_sync`]).
fn synced_revision_after_own_write(synced: u64, before: u64, after: u64) -> u64 {
    if synced == before { after } else { synced }
}

/// Runs the canvas's own `write` against the locked model and updates the canvas's `synced`
/// revision by [`synced_revision_after_own_write`]. When the write targets `page` and changed it
/// (its model stamp moved), the new stamp is recorded in `own_write_stamps`; page-less writes
/// (`set_visible`) pass `None`. The caller holds the lock only for this call.
fn write_own_change<R>(
    locked: &mut CleanOverlaysModel,
    synced: &mut u64,
    own_write_stamps: &mut HashMap<usize, u64>,
    page: Option<usize>,
    write: impl FnOnce(&mut CleanOverlaysModel) -> R,
) -> R {
    let before = locked.revision();
    let stamp_before = page.and_then(|idx| locked.page_change_revision(idx));
    let result = write(locked);
    *synced = synced_revision_after_own_write(*synced, before, locked.revision());
    if let Some(idx) = page
        && let Some(stamp) = locked.page_change_revision(idx)
        && Some(stamp) != stamp_before
    {
        own_write_stamps.insert(idx, stamp);
    }
    result
}

/// What a canvas sync does with one page listed in a model delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OverlaySyncAction {
    /// Cached page changed by someone else: re-read its current image from the model.
    Refetch,
    /// Cached page whose model content is still this canvas's own last write: keep the cached
    /// image untouched (it may carry newer local-only preview pixels).
    KeepOwnWrite,
    /// Page not cached by this canvas: copy nothing, only drop stale GPU/prepare state; it loads
    /// lazily through `ensure_overlay_for_page_size`.
    DropUncached,
}

/// Decides, for every page in a delta's `changed` list, what the sync does with it (see
/// [`OverlaySyncAction`]). `model_stamp` is `CleanOverlaysModel::page_change_revision`, read under
/// the same lock as the delta.
pub(super) fn plan_overlay_sync(
    changed: &[usize],
    is_cached: impl Fn(usize) -> bool,
    own_write_stamps: &HashMap<usize, u64>,
    model_stamp: impl Fn(usize) -> Option<u64>,
) -> Vec<(usize, OverlaySyncAction)> {
    changed
        .iter()
        .map(|&idx| {
            let action = if !is_cached(idx) {
                OverlaySyncAction::DropUncached
            } else if own_write_stamps.get(&idx).is_some_and(|own| model_stamp(idx) == Some(*own)) {
                OverlaySyncAction::KeepOwnWrite
            } else {
                OverlaySyncAction::Refetch
            };
            (idx, action)
        })
        .collect()
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
        self.own_write_stamps.clear();
        self.overlay_prepare_inflight.clear();
        self.overlay_prepared_pages.clear();
        self.overlay_prepare_superseded.clear();
    }

    pub(super) fn model_handle(&self) -> Option<Arc<Mutex<CleanOverlaysModel>>> {
        self.overlays_model.clone()
    }

    /// Replaces the set of pages whose overlay is not painted (uploads continue, see the field).
    pub(super) fn set_render_suppressed_pages(&mut self, pages: &[usize]) {
        self.overlay_render_suppressed_pages.clear();
        self.overlay_render_suppressed_pages.extend(pages.iter().copied());
    }

    /// Records that `page_idx`'s `overlay_images` entry changed (or was dropped): every mutation
    /// of that map calls it, so `page_generation` differs from any earlier reading.
    fn note_page_changed(&mut self, page_idx: usize) {
        self.overlay_generation_clock = self.overlay_generation_clock.saturating_add(1);
        self.overlay_page_generations.insert(page_idx, self.overlay_generation_clock);
    }

    /// The change generation of `page_idx`'s CPU overlay: equal readings mean the page's pixels
    /// were not changed in between by anyone (this canvas, a model sync, a lazy load). `0` for a
    /// page that never had an overlay on this canvas.
    pub(super) fn page_generation(&self, page_idx: usize) -> u64 {
        self.overlay_page_generations.get(&page_idx).copied().unwrap_or(0)
    }

    /// Whether waiting for `page_idx`'s GPU overlay is over, as of the LAST `draw`: the page was
    /// not processed by it (out of view, overlays hidden, no model — its pending work cannot
    /// progress, so there is nothing to wait for), or it has no pending work left, i.e. its
    /// textures show exactly `overlay_images`.
    pub(super) fn page_settled(&self, page_idx: usize) -> bool {
        !self.drawn_pages.contains(&page_idx) || !self.page_has_pending_work(page_idx)
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
            write_own_change(&mut locked, &mut self.synced_overlays_revision, &mut self.own_write_stamps, None, |model| model.set_visible(visible));
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

        // A read-only lazy load: the synced revision is deliberately NOT advanced, so changes to
        // OTHER pages made since the last sync are still delivered by the next sync.
        let model_snapshot = if let Some(model) = self.overlays_model.clone() {
            match model.lock() {
                Ok(locked) => locked.get(page_idx).cloned(),
                Err(_) => None,
            }
        } else {
            None
        };
        if let Some(image) = model_snapshot {
            self.reset_prepare_state(page_idx);
            self.overlay_last_upload_s.remove(&page_idx);
            self.overlay_textures.remove(&page_idx);
            self.overlay_texture_last_used_frame.remove(&page_idx);
            self.overlay_images.insert(page_idx, Arc::new(image));
            self.note_page_changed(page_idx);
            self.own_write_stamps.remove(&page_idx);
            self.mark_dirty_full(page_idx);
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
        self.note_page_changed(page_idx);
        self.mark_dirty_full(page_idx);
        if let Some(model) = self.overlays_model.as_ref()
            && let Ok(mut locked) = model.lock()
        {
            write_own_change(&mut locked, &mut self.synced_overlays_revision, &mut self.own_write_stamps, Some(page_idx), |model| model.ensure_overlay(page_idx, [w, h]));
        }
        true
    }

    /// Pulls clean-overlay changes newer than `synced_overlays_revision` from the shared model.
    ///
    /// `CleanOverlaysModel::delta_since` is non-destructive, so every canvas sharing the model
    /// (Cleaning and Typing) receives each change. Under one brief lock this plans every changed
    /// page ([`plan_overlay_sync`]) and clones only the pages to re-fetch; uncached pages copy
    /// nothing and pages still holding this canvas's own write are left untouched.
    pub(super) fn sync_from_model(&mut self) {
        let Some((delta, plan, mut fetched)) = ({
            let Some(model) = self.overlays_model.as_ref() else {
                return;
            };
            let Ok(locked) = model.lock() else {
                return;
            };
            locked.delta_since(self.synced_overlays_revision).map(|delta| {
                let plan = plan_overlay_sync(
                    &delta.changed,
                    |idx| self.overlay_images.contains_key(&idx),
                    &self.own_write_stamps,
                    |idx| locked.page_change_revision(idx),
                );
                let fetched: HashMap<usize, egui::ColorImage> = plan
                    .iter()
                    .filter(|(_, action)| *action == OverlaySyncAction::Refetch)
                    .filter_map(|&(idx, _)| locked.get(idx).map(|image| (idx, image.clone())))
                    .collect();
                (delta, plan, fetched)
            })
        }) else {
            return;
        };
        if let Some(visible) = delta.visibility {
            self.apply_model_visibility(visible);
        }
        for (idx, action) in plan {
            match action {
                OverlaySyncAction::KeepOwnWrite => continue,
                OverlaySyncAction::Refetch | OverlaySyncAction::DropUncached => {}
            }
            self.reset_prepare_state(idx);
            self.own_write_stamps.remove(&idx);
            self.note_page_changed(idx);
            // An absent or zero-sized model overlay means "no clean": the cached page is dropped.
            match fetched.remove(&idx).filter(|img| img.size[0] > 0 && img.size[1] > 0) {
                Some(img) => {
                    // The texture is kept so the old pixels stay on screen until the re-upload.
                    self.overlay_images.insert(idx, Arc::new(img));
                    self.mark_dirty_full(idx);
                }
                None => {
                    self.overlay_images.remove(&idx);
                    self.overlay_textures.remove(&idx);
                    self.overlay_dirty_tiles.remove(&idx);
                    self.overlay_last_upload_s.remove(&idx);
                }
            }
        }
        self.synced_overlays_revision = delta.revision;
    }

    pub(super) fn reset_prepare_state(&mut self, page_idx: usize) {
        self.overlay_prepare_inflight.remove(&page_idx);
        self.overlay_prepared_pages.remove(&page_idx);
        self.overlay_prepare_superseded.remove(&page_idx);
    }

    /// Whether `page_idx` has a full prepare in flight or prepared tiles not yet uploaded.
    fn prepare_pending(&self, page_idx: usize) -> bool {
        self.overlay_prepare_inflight.contains_key(&page_idx) || self.overlay_prepared_pages.contains_key(&page_idx)
    }

    /// Marks `tiles` of `page_idx` dirty, and superseded in its pending prepare (if any): their
    /// CPU pixels just changed, so the prepare's snapshot of them is stale.
    fn mark_tiles_dirty(&mut self, page_idx: usize, tiles: impl IntoIterator<Item = usize> + Clone) {
        if self.prepare_pending(page_idx) {
            self.overlay_prepare_superseded.entry(page_idx).or_default().extend(tiles.clone());
        }
        self.overlay_dirty_tiles.entry(page_idx).or_default().extend(tiles);
    }

    pub(super) fn request_prepare(&mut self, page_idx: usize, image: Arc<egui::ColorImage>) {
        if self.prepare_pending(page_idx) {
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
            // The snapshot just taken holds every change so far: nothing is superseded yet.
            self.overlay_prepare_superseded.remove(&page_idx);
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
                let superseded = self.overlay_prepare_superseded.get(&page_idx);
                // The current CPU overlay, when it still has the prepared size (a resize resets the
                // whole texture on the next upload anyway).
                let current = self.overlay_images.get(&page_idx).filter(|ov| ov.size == prepared.size);
                while prepared.next_upload_tile < prepared.tiles.len() && *tile_budget > 0 {
                    let tile = &prepared.tiles[prepared.next_upload_tile];
                    if uploaded_any && tile.rgba.len() > *bytes_budget {
                        break;
                    }
                    // A tile changed after the snapshot is built from the current pixels: the
                    // dirty-tile path may already have uploaded them, and the late prepared copy
                    // must not overwrite them with older ones.
                    let tile_img = match current.filter(|_| superseded.is_some_and(|tiles| tiles.contains(&tile.tile_idx))) {
                        Some(ov) => build_overlay_tile_image(ov, tile.origin_px[0], tile.origin_px[1], tile.size_px[0], tile.size_px[1]),
                        None => egui::ColorImage::from_rgba_premultiplied([tile.size_px[0], tile.size_px[1]], &tile.rgba),
                    };
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
            self.overlay_prepare_superseded.remove(&page_idx);
        }
    }

    /// Starts a frame's draw tracking: forgets which pages were drawn last frame. Called once per
    /// `CanvasView::draw`, before the scene pass.
    pub(super) fn begin_draw_tracking(&mut self) {
        self.drawn_pages.clear();
    }

    /// Whether `page_idx` holds GPU work that only drawing it can finish: a prepare in flight,
    /// prepared tiles not yet uploaded, or dirty tiles.
    fn page_has_pending_work(&self, page_idx: usize) -> bool {
        self.overlay_prepare_inflight.contains_key(&page_idx)
            || self.overlay_prepared_pages.contains_key(&page_idx)
            || self
                .overlay_dirty_tiles
                .get(&page_idx)
                .is_some_and(|dirty| !dirty.is_empty())
    }

    /// Whether this frame drew a page that still has pending GPU work, i.e. whether the canvas
    /// must request another frame to finish it.
    ///
    /// Pages not drawn this frame never count: their work can only progress while they are drawn
    /// (off screen, overlays hidden), so repainting for them would spin the
    /// GUI at full frame rate without progress. Their dirty tiles are uploaded when they are next
    /// drawn, which happens on whatever event brings them into view.
    pub(super) fn has_pending_work(&self) -> bool {
        self.drawn_pages
            .iter()
            .any(|&page_idx| self.page_has_pending_work(page_idx))
    }

    pub(super) fn memory_usage_snapshot(
        &self,
        pinned_pages: &BTreeSet<usize>,
    ) -> Vec<CacheResourceInfo> {
        self.overlay_textures
            .iter()
            .map(|(page_idx, page_tex)| {
                let texture_bytes = page_tex
                    .tiles
                    .iter()
                    .map(|tile| {
                        tile.size_px[0]
                            .saturating_mul(tile.size_px[1])
                            .saturating_mul(4)
                    })
                    .fold(0usize, usize::saturating_add);
                // A prepared payload not yet uploaded is CPU memory of the same page: evicting the
                // page frees it too, so it counts toward what the eviction frees.
                let prepared_bytes = self
                    .overlay_prepared_pages
                    .get(page_idx)
                    .map(|prepared| prepared.tiles.iter().map(|tile| tile.rgba.len()).fold(0usize, usize::saturating_add))
                    .unwrap_or(0);
                let estimated_bytes = texture_bytes.saturating_add(prepared_bytes);
                let pinned = pinned_pages.contains(page_idx);
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
                    visible: pinned,
                    // Everything here rebuilds from `overlay_images`. Only a pinned (visible)
                    // page's prepare in progress is protected: it is being uploaded right now.
                    reconstructable: !pinned
                        || (!self.overlay_prepare_inflight.contains_key(page_idx)
                            && !self.overlay_prepared_pages.contains_key(page_idx)),
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
            let preparing = self.overlay_prepare_inflight.contains_key(&page_idx)
                || self.overlay_prepared_pages.contains_key(&page_idx);
            if preparing && request.pinned_pages.contains(&page_idx) {
                continue;
            }
            if self.overlay_textures.remove(&page_idx).is_some() {
                // The texture and its prepare work go together: drawing the page again finds no
                // texture and re-prepares it from `overlay_images`, so no half-built texture and
                // no orphaned payload remain. Its dirty tiles are covered by that full prepare.
                self.reset_prepare_state(page_idx);
                self.overlay_dirty_tiles.remove(&page_idx);
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
            overlay.pixels.fill(Color32::TRANSPARENT);
            should_sync_model = true;
        }
        if should_sync_model {
            self.note_page_changed(page_idx);
            self.mark_dirty_full(page_idx);
            if let Some(model) = self.overlays_model.as_ref()
                && let Some(overlay) = self.overlay_images.get(&page_idx)
                && let Ok(mut locked) = model.lock()
            {
                write_own_change(&mut locked, &mut self.synced_overlays_revision, &mut self.own_write_stamps, Some(page_idx), |model| model.replace(page_idx, overlay));
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
        self.note_page_changed(page_idx);
        if sync_model
            && let Some(model) = self.overlays_model.as_ref()
            && let Ok(mut locked) = model.lock()
        {
            write_own_change(&mut locked, &mut self.synced_overlays_revision, &mut self.own_write_stamps, Some(page_idx), |model| {
                model.replace_region(page_idx, size_px, RegionPx { x: target.x, y: target.y, w: target.w, h: target.h }, chunk)
            });
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
        self.note_page_changed(page_idx);
        if sync_model
            && let Some(model) = self.overlays_model.as_ref()
            && let Ok(mut locked) = model.lock()
        {
            write_own_change(&mut locked, &mut self.synced_overlays_revision, &mut self.own_write_stamps, Some(page_idx), |model| {
                model.replace_region(page_idx, size_px, RegionPx { x: clipped.x, y: clipped.y, w: clipped.w, h: clipped.h }, chunk)
            });
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
            write_own_change(&mut locked, &mut self.synced_overlays_revision, &mut self.own_write_stamps, Some(page_idx), |model| model.replace(page_idx, dst));
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
        self.overlay_dirty_tiles.remove(&page_idx);
        self.mark_tiles_dirty(page_idx, 0..tiles_x.saturating_mul(tiles_y));
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
        let tiles: Vec<usize> = (tile_y0..=tile_y1)
            .flat_map(|ty| (tile_x0..=tile_x1).map(move |tx| ty * tiles_x + tx))
            .collect();
        self.mark_tiles_dirty(page_idx, tiles);
    }

    /// Advances `page_idx`'s pending GPU work by up to the frame's remaining budgets: creates or
    /// resets its texture page, starts a full background prepare when needed, uploads prepared
    /// tiles and then dirty tiles (or turns many dirty tiles into one full re-prepare). Leftover
    /// work stays queued for the next call. Returns whether the page has a non-empty overlay
    /// (otherwise there is nothing to upload or paint).
    pub(super) fn upload_page_work(
        &mut self,
        ctx: &egui::Context,
        page_idx: usize,
        tile_budget: &mut usize,
        bytes_budget: &mut usize,
        pixel_sampling_nearest: bool,
    ) -> bool {
        let Some(ov_size) = self.overlay_images.get(&page_idx).map(|ov| ov.size) else {
            return false;
        };
        if ov_size[0] == 0 || ov_size[1] == 0 {
            return false;
        }
        let texture_options = if pixel_sampling_nearest {
            egui::TextureOptions::NEAREST
        } else {
            egui::TextureOptions::LINEAR
        };

        let dirty_tiles = self.overlay_dirty_tiles.remove(&page_idx);
        let now_s = ctx.input(|i| i.time);
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
            ctx,
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
                // A dirty tile the texture does not have yet (a half-built texture whose prepare
                // was reset, e.g. by a model refetch) can only appear through a full prepare;
                // without one pending it would stay dirty forever, repainting for nothing and
                // never letting the page settle.
                if !remaining_dirty.is_empty()
                    && !self.prepare_pending(page_idx)
                    && let Some(ov) = self.overlay_images.get(&page_idx).cloned()
                {
                    self.request_prepare(page_idx, ov);
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

        true
    }

    /// Draws `page_idx`'s clean overlay into `image_rect` after advancing its GPU work.
    ///
    /// Records the page in `drawn_pages`, then uploads under the frame's shared budgets
    /// ([`Self::upload_page_work`]). A page in `overlay_render_suppressed_pages` stops there: its
    /// uploads progress (so a tool preview standing in for it can retire as soon as the textures
    /// are current), but nothing is painted.
    pub(super) fn draw_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        image_rect: Rect,
        tile_budget: &mut usize,
        bytes_budget: &mut usize,
        pixel_sampling_nearest: bool,
    ) {
        self.drawn_pages.insert(page_idx);
        if !self.upload_page_work(ui.ctx(), page_idx, tile_budget, bytes_budget, pixel_sampling_nearest) {
            return;
        }
        let Some(page_tex) = self.overlay_textures.get(&page_idx) else {
            return;
        };
        self.overlay_texture_last_used_frame
            .insert(page_idx, ui.ctx().cumulative_frame_nr());
        if self.overlay_render_suppressed_pages.contains(&page_idx) {
            return;
        }
        let src_w = page_tex.size[0] as f32;
        let src_h = page_tex.size[1] as f32;
        if src_w <= 0.0 || src_h <= 0.0 {
            return;
        }
        // Viewport cull: skip tiles whose on-screen rect does not intersect the
        // visible clip rect. `intersects` keeps partially-visible edge tiles.
        let viewport_rect = ui.clip_rect();
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
            if !dst.intersects(viewport_rect) {
                continue;
            }
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
    use crate::types::OverlayPreparedTile;
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

    fn two_page_model() -> Arc<Mutex<CleanOverlaysModel>> {
        Arc::new(Mutex::new(CleanOverlaysModel::new_from_pages(&[
            PathBuf::from("001.png"),
            PathBuf::from("002.png"),
        ])))
    }

    fn opaque_pixel() -> egui::ColorImage {
        egui::ColorImage::filled([1, 1], Color32::from_rgba_unmultiplied(10, 20, 30, 255))
    }

    /// A write to the shared model made by someone other than the canvas under test.
    fn foreign_write(model: &Arc<Mutex<CleanOverlaysModel>>, page_idx: usize) {
        match model.lock() {
            Ok(mut locked) => assert!(locked.replace_region(page_idx, [1, 1], RegionPx { x: 0, y: 0, w: 1, h: 1 }, &opaque_pixel())),
            Err(err) => panic!("test failed to lock CleanOverlaysModel: {err}"),
        }
    }

    fn changed_since(model: &Arc<Mutex<CleanOverlaysModel>>, known: u64) -> Option<Vec<usize>> {
        match model.lock() {
            Ok(locked) => locked.delta_since(known).map(|delta| delta.changed),
            Err(err) => panic!("test failed to lock CleanOverlaysModel: {err}"),
        }
    }

    fn model_revision(model: &Arc<Mutex<CleanOverlaysModel>>) -> u64 {
        match model.lock() {
            Ok(locked) => locked.revision(),
            Err(err) => panic!("test failed to lock CleanOverlaysModel: {err}"),
        }
    }

    #[test]
    fn synced_revision_advances_only_when_fully_synced_before_own_write() {
        assert_eq!(synced_revision_after_own_write(5, 5, 6), 6);
        assert_eq!(synced_revision_after_own_write(4, 5, 6), 4, "a foreign change at 5 must not be skipped");
        assert_eq!(synced_revision_after_own_write(5, 5, 5), 5, "a no-op write keeps the revision");
    }

    #[test]
    fn own_write_when_synced_skips_only_the_own_change() {
        let model = two_page_model();
        let mut runtime = OverlayRuntimeState::default();
        runtime.set_model(Arc::clone(&model));
        runtime.synced_overlays_revision = model_revision(&model);

        let target = OverlayRectPx { x: 0, y: 0, w: 1, h: 1 };
        assert!(runtime.replace_overlay_region_px(0, [1, 1], target, &opaque_pixel(), true));

        assert_eq!(runtime.synced_overlays_revision, model_revision(&model));
        assert_eq!(changed_since(&model, runtime.synced_overlays_revision), None);
        runtime.shutdown();
    }

    #[test]
    fn own_write_does_not_leap_over_a_foreign_change() {
        let model = two_page_model();
        let mut runtime = OverlayRuntimeState::default();
        runtime.set_model(Arc::clone(&model));
        runtime.synced_overlays_revision = model_revision(&model);

        foreign_write(&model, 1);
        let target = OverlayRectPx { x: 0, y: 0, w: 1, h: 1 };
        assert!(runtime.replace_overlay_region_px(0, [1, 1], target, &opaque_pixel(), true));

        assert_eq!(changed_since(&model, runtime.synced_overlays_revision), Some(vec![0, 1]), "the foreign page 1 must still be delivered");
        runtime.shutdown();
    }

    #[test]
    fn lazy_overlay_load_does_not_advance_the_synced_revision() {
        let model = two_page_model();
        let mut runtime = OverlayRuntimeState::default();
        runtime.set_model(Arc::clone(&model));
        foreign_write(&model, 0);
        runtime.synced_overlays_revision = model_revision(&model);

        foreign_write(&model, 1);
        assert!(runtime.ensure_overlay_for_page_size(0, [1, 1]));

        assert_eq!(changed_since(&model, runtime.synced_overlays_revision), Some(vec![1]));
        runtime.shutdown();
    }

    fn red_pixel() -> egui::ColorImage {
        egui::ColorImage::filled([1, 1], Color32::from_rgba_unmultiplied(200, 0, 0, 255))
    }

    fn cached_pixel(runtime: &OverlayRuntimeState, page_idx: usize) -> Option<Color32> {
        runtime.overlay_image(page_idx).map(|image| image.pixels[0])
    }

    #[test]
    fn plan_overlay_sync_classifies_uncached_own_and_foreign_pages() {
        let own = HashMap::from([(1, 7), (2, 7)]);
        let stamps = HashMap::from([(0, 9), (1, 7), (2, 9), (3, 9)]);
        let plan = plan_overlay_sync(&[0, 1, 2, 3], |idx| idx != 3, &own, |idx| stamps.get(&idx).copied());
        assert_eq!(
            plan,
            vec![
                (0, OverlaySyncAction::Refetch),
                (1, OverlaySyncAction::KeepOwnWrite),
                (2, OverlaySyncAction::Refetch),
                (3, OverlaySyncAction::DropUncached),
            ]
        );
    }

    #[test]
    fn two_canvases_both_receive_a_change_and_never_copy_uncached_pages() {
        let model = two_page_model();
        foreign_write(&model, 0);
        let mut cleaning = OverlayRuntimeState::default();
        let mut typing = OverlayRuntimeState::default();
        for runtime in [&mut cleaning, &mut typing] {
            runtime.set_model(Arc::clone(&model));
            assert!(runtime.ensure_overlay_for_page_size(0, [1, 1]));
            runtime.sync_from_model();
        }

        match model.lock() {
            Ok(mut locked) => {
                assert!(locked.replace_region(0, [1, 1], RegionPx { x: 0, y: 0, w: 1, h: 1 }, &red_pixel()));
                assert!(locked.replace_region(1, [1, 1], RegionPx { x: 0, y: 0, w: 1, h: 1 }, &red_pixel()));
            }
            Err(err) => panic!("test failed to lock CleanOverlaysModel: {err}"),
        }
        cleaning.sync_from_model();
        typing.sync_from_model();

        for runtime in [&cleaning, &typing] {
            assert_eq!(cached_pixel(runtime, 0), Some(red_pixel().pixels[0]));
            assert!(runtime.overlay_image(1).is_none(), "an uncached page must not be copied");
            assert_eq!(runtime.synced_overlays_revision, model_revision(&model));
        }
        cleaning.shutdown();
        typing.shutdown();
    }

    #[test]
    fn sync_drops_a_cached_page_whose_overlay_is_gone() {
        let model = two_page_model();
        foreign_write(&model, 0);
        let mut runtime = OverlayRuntimeState::default();
        runtime.set_model(Arc::clone(&model));
        assert!(runtime.ensure_overlay_for_page_size(0, [1, 1]));
        runtime.sync_from_model();

        match model.lock() {
            Ok(mut locked) => assert!(locked.detach_page_overlay(0)),
            Err(err) => panic!("test failed to lock CleanOverlaysModel: {err}"),
        }
        runtime.sync_from_model();

        assert!(runtime.overlay_image(0).is_none());
        runtime.shutdown();
    }

    #[test]
    fn sync_keeps_local_preview_on_own_page_but_refetches_after_a_foreign_edit() {
        let model = two_page_model();
        let mut runtime = OverlayRuntimeState::default();
        runtime.set_model(Arc::clone(&model));
        runtime.sync_from_model();

        // Foreign write lands between the sync and the canvas's own write, so the synced
        // revision stays behind and the next delta re-lists the canvas's own page 0.
        foreign_write(&model, 1);
        assert!(runtime.ensure_editable_overlay_for_page(0, [1, 1]));
        let target = OverlayRectPx { x: 0, y: 0, w: 1, h: 1 };
        assert!(runtime.replace_overlay_region_px(0, [1, 1], target, &red_pixel(), false));
        assert_eq!(changed_since(&model, runtime.synced_overlays_revision), Some(vec![0, 1]));

        runtime.sync_from_model();
        assert_eq!(cached_pixel(&runtime, 0), Some(red_pixel().pixels[0]), "local-only preview must survive the sync");

        foreign_write(&model, 0);
        runtime.sync_from_model();
        assert_eq!(cached_pixel(&runtime, 0), Some(opaque_pixel().pixels[0]), "a foreign edit of the page is re-fetched");
        runtime.shutdown();
    }

    fn prepared_page(size: [usize; 2]) -> OverlayPreparedPage {
        OverlayPreparedPage { size, tiles: Vec::new(), next_upload_tile: 0 }
    }

    /// A GPU texture page of `size` holding `tiles` one-pixel tiles.
    fn texture_page(ctx: &egui::Context, size: [usize; 2], tiles: usize) -> OverlayTexturePage {
        OverlayTexturePage {
            size,
            texture_options: egui::TextureOptions::LINEAR,
            tiles: (0..tiles)
                .map(|tile_idx| OverlayTextureTile {
                    texture: ctx.load_texture(format!("test-overlay-tile-{tile_idx}"), opaque_pixel(), egui::TextureOptions::LINEAR),
                    origin_px: [0, 0],
                    size_px: [1, 1],
                })
                .collect(),
        }
    }

    #[test]
    fn only_pages_drawn_this_frame_count_as_pending_work() {
        let mut runtime = OverlayRuntimeState::default();
        runtime.begin_draw_tracking();
        runtime.overlay_prepared_pages.insert(3, prepared_page([1, 1]));
        runtime.overlay_prepare_inflight.insert(4, 1);
        runtime.overlay_dirty_tiles.insert(5, HashSet::from([0]));
        runtime.overlay_dirty_tiles.insert(6, HashSet::new());
        runtime.drawn_pages.insert(6);
        assert!(!runtime.has_pending_work(), "off-screen work and empty dirty sets must not drive repaints");

        for page_idx in [3, 4, 5] {
            runtime.begin_draw_tracking();
            runtime.drawn_pages.insert(page_idx);
            assert!(runtime.has_pending_work(), "drawn page {page_idx} with pending work must drive repaints");
        }

        // A frame that drew nothing (overlays hidden, pages off screen) never repaints for work.
        runtime.begin_draw_tracking();
        assert!(!runtime.has_pending_work());
        runtime.shutdown();
    }

    /// THE BUG this guards (S-096): a page whose overlay a tool preview stands in for must keep
    /// uploading, or the preview could never learn that the committed stroke reached the GPU.
    /// Suppression is per page and never stops the work; settling is what the preview waits on.
    #[test]
    fn a_render_suppressed_page_still_uploads_and_then_settles() {
        let ctx = egui::Context::default();
        let mut runtime = OverlayRuntimeState::default();
        runtime.overlay_images.insert(0, Arc::new(opaque_pixel()));
        runtime.overlay_textures.insert(0, texture_page(&ctx, [1, 1], 1));
        runtime.overlay_dirty_tiles.insert(0, HashSet::from([0]));
        runtime.set_render_suppressed_pages(&[0]);
        assert!(runtime.overlay_render_suppressed_pages.contains(&0));
        assert!(!runtime.overlay_render_suppressed_pages.contains(&1), "suppression is per page");

        runtime.begin_draw_tracking();
        runtime.drawn_pages.insert(0);
        assert!(!runtime.page_settled(0), "a drawn page with dirty tiles is not settled");
        assert!(runtime.has_pending_work(), "a suppressed page with pending work keeps the canvas repainting");

        let (mut tiles, mut bytes) = (2, usize::MAX);
        assert!(runtime.upload_page_work(&ctx, 0, &mut tiles, &mut bytes, false));
        assert_eq!(tiles, 1, "the dirty tile was uploaded under the frame budget");
        assert!(runtime.page_settled(0));
        assert!(!runtime.has_pending_work());

        runtime.set_render_suppressed_pages(&[]);
        assert!(runtime.overlay_render_suppressed_pages.is_empty());
        runtime.shutdown();
    }

    /// The scene-level half of S-096: `draw_overlay_on_page` of a suppressed page must still
    /// upload (and count the page as drawn) and paint nothing; an unsuppressed page paints.
    #[test]
    fn draw_overlay_on_page_uploads_but_does_not_paint_a_suppressed_page() {
        let ctx = egui::Context::default();
        let mut runtime = OverlayRuntimeState::default();
        runtime.overlay_images.insert(0, Arc::new(opaque_pixel()));
        runtime.overlay_textures.insert(0, texture_page(&ctx, [1, 1], 1));
        runtime.overlay_dirty_tiles.insert(0, HashSet::from([0]));
        let image_rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(10.0, 10.0));
        let draw_frame = |runtime: &mut OverlayRuntimeState| {
            runtime.begin_draw_tracking();
            let (mut tiles, mut bytes) = (2, usize::MAX);
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                runtime.draw_overlay_on_page(ui, 0, image_rect, &mut tiles, &mut bytes, false);
            });
            // No painter applies the texture changes here; epaint asserts they are handled.
            output.textures_delta.clear();
            output.shapes.len()
        };

        runtime.set_render_suppressed_pages(&[0]);
        assert_eq!(draw_frame(&mut runtime), 0, "a suppressed page paints nothing");
        assert!(runtime.drawn_pages.contains(&0), "but it counts as drawn");
        assert!(runtime.page_settled(0), "and its dirty tile was uploaded");

        runtime.set_render_suppressed_pages(&[]);
        assert!(draw_frame(&mut runtime) > 0, "an unsuppressed page paints its tiles");
        runtime.shutdown();
    }

    /// Polls the prepare worker until `page_idx` has no prepare in flight (bounded wait).
    fn wait_for_prepare(runtime: &mut OverlayRuntimeState, page_idx: usize) {
        for _ in 0..1000 {
            runtime.poll_prepare_results();
            if !runtime.overlay_prepare_inflight.contains_key(&page_idx) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the overlay prepare worker did not answer");
    }

    /// The last image set on `texture` in the context's pending texture delta.
    fn last_set_image(ctx: &egui::Context, texture: egui::TextureId) -> Option<Arc<egui::ColorImage>> {
        let mut delta = ctx.tex_manager().write().take_delta();
        let image = delta.set.get(&texture).and_then(|deltas| deltas.last()).map(|delta| match &delta.image {
            egui::ImageData::Color(image) => Arc::clone(image),
        });
        // No painter applies the texture changes here; epaint asserts they are handled.
        delta.clear();
        image
    }

    /// M1: an edit that lands while a full prepare is in flight is uploaded by the dirty-tile path
    /// first; the late prepare result must not overwrite that tile with its older snapshot.
    #[test]
    fn a_late_full_prepare_never_overwrites_a_newer_tile() {
        let ctx = egui::Context::default();
        let mut runtime = OverlayRuntimeState::default();
        let size = [OVERLAY_TILE_SIDE + 1, 1];
        let old = Color32::from_rgba_unmultiplied(10, 20, 30, 255);
        runtime.overlay_images.insert(0, Arc::new(egui::ColorImage::filled(size, old)));
        let tile = |origin_px: [usize; 2], size_px: [usize; 2], name: &str| OverlayTextureTile {
            texture: ctx.load_texture(name, egui::ColorImage::filled(size_px, old), egui::TextureOptions::LINEAR),
            origin_px,
            size_px,
        };
        runtime.overlay_textures.insert(0, OverlayTexturePage {
            size,
            texture_options: egui::TextureOptions::LINEAR,
            tiles: vec![tile([0, 0], [OVERLAY_TILE_SIDE, 1], "m1-0"), tile([OVERLAY_TILE_SIDE, 0], [1, 1], "m1-1")],
        });
        let tile1 = runtime.overlay_textures[&0].tiles[1].texture.id();
        // Stroke 1 started a full re-prepare from the current pixels...
        let snapshot = Arc::clone(&runtime.overlay_images[&0]);
        runtime.request_prepare(0, snapshot);
        // ...and stroke 2 commits into tile 1 before it answers.
        let target = OverlayRectPx { x: OVERLAY_TILE_SIDE, y: 0, w: 1, h: 1 };
        assert!(runtime.replace_overlay_region_px(0, size, target, &red_pixel(), false));
        let (mut tiles, mut bytes) = (8, usize::MAX);
        assert!(runtime.upload_page_work(&ctx, 0, &mut tiles, &mut bytes, false));
        wait_for_prepare(&mut runtime, 0);
        ctx.tex_manager().write().take_delta().clear();

        runtime.begin_draw_tracking();
        runtime.drawn_pages.insert(0);
        let (mut tiles, mut bytes) = (8, usize::MAX);
        assert!(runtime.upload_page_work(&ctx, 0, &mut tiles, &mut bytes, false));
        assert!(runtime.page_settled(0));
        let uploaded = last_set_image(&ctx, tile1).expect("the prepared tile 1 was uploaded");
        assert_eq!(uploaded.pixels, red_pixel().pixels, "tile 1 shows stroke 2, not the prepare's older snapshot");
        runtime.shutdown();
    }

    /// M2: a half-built texture (a prepare reset mid-upload, e.g. by a model refetch) on a
    /// one-tile-wide page must still get its missing tiles and settle, instead of re-queueing
    /// them as dirty forever.
    #[test]
    fn missing_tiles_of_a_half_built_texture_are_prepared_and_the_page_settles() {
        let ctx = egui::Context::default();
        let mut runtime = OverlayRuntimeState::default();
        let size = [1, 2 * OVERLAY_TILE_SIDE + 1];
        runtime.overlay_images.insert(0, Arc::new(egui::ColorImage::filled(size, Color32::WHITE)));
        runtime.overlay_textures.insert(0, OverlayTexturePage {
            size,
            texture_options: egui::TextureOptions::LINEAR,
            tiles: vec![OverlayTextureTile {
                texture: ctx.load_texture("m2-0", egui::ColorImage::filled([1, OVERLAY_TILE_SIDE], Color32::WHITE), egui::TextureOptions::LINEAR),
                origin_px: [0, 0],
                size_px: [1, OVERLAY_TILE_SIDE],
            }],
        });
        runtime.mark_dirty_full(0);
        runtime.begin_draw_tracking();
        runtime.drawn_pages.insert(0);
        for _ in 0..1000 {
            let (mut tiles, mut bytes) = (2, usize::MAX);
            assert!(runtime.upload_page_work(&ctx, 0, &mut tiles, &mut bytes, false));
            if runtime.page_settled(0) {
                break;
            }
            runtime.poll_prepare_results();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(runtime.page_settled(0), "the page must settle");
        assert_eq!(runtime.overlay_textures[&0].tiles.len(), 3, "every tile exists");
        runtime.shutdown();
    }

    /// Waiting is pointless for a page the last draw did not process: its work cannot progress.
    #[test]
    fn a_page_not_drawn_last_frame_counts_as_settled_despite_pending_work() {
        let mut runtime = OverlayRuntimeState::default();
        runtime.overlay_dirty_tiles.insert(3, HashSet::from([0]));
        runtime.overlay_prepare_inflight.insert(4, 1);
        runtime.begin_draw_tracking();
        assert!(runtime.page_settled(3) && runtime.page_settled(4));
        runtime.drawn_pages.insert(4);
        assert!(!runtime.page_settled(4), "a prepare in flight for a drawn page is pending");
        runtime.shutdown();
    }

    /// Every change of a page's CPU overlay moves its generation: own edits, a model sync of a
    /// foreign change (an undo looks the same), a drop. A page nobody touched keeps it.
    #[test]
    fn the_page_generation_moves_on_every_overlay_change_and_only_then() {
        let model = two_page_model();
        foreign_write(&model, 0);
        let mut runtime = OverlayRuntimeState::default();
        runtime.set_model(Arc::clone(&model));
        assert_eq!(runtime.page_generation(0), 0, "never cached");
        assert!(runtime.ensure_overlay_for_page_size(0, [1, 1]));
        runtime.sync_from_model();
        let loaded = runtime.page_generation(0);
        assert_ne!(loaded, 0);
        runtime.sync_from_model();
        assert_eq!(runtime.page_generation(0), loaded, "a sync with nothing new changes nothing");

        let target = OverlayRectPx { x: 0, y: 0, w: 1, h: 1 };
        assert!(runtime.replace_overlay_region_px(0, [1, 1], target, &red_pixel(), true));
        let own = runtime.page_generation(0);
        assert!(own > loaded);
        runtime.sync_from_model();
        assert_eq!(runtime.page_generation(0), own, "the canvas's own write is not re-applied by the sync");

        foreign_write(&model, 0);
        runtime.sync_from_model();
        let foreign = runtime.page_generation(0);
        assert!(foreign > own, "a foreign change (or an undo) moves the generation");

        match model.lock() {
            Ok(mut locked) => assert!(locked.detach_page_overlay(0)),
            Err(err) => panic!("test failed to lock CleanOverlaysModel: {err}"),
        }
        runtime.sync_from_model();
        assert!(runtime.page_generation(0) > foreign, "dropping the page moves it too");
        assert_eq!(runtime.page_generation(1), 0);
        runtime.shutdown();
    }

    fn eviction_request(pinned: &[usize]) -> CacheEvictionRequest {
        CacheEvictionRequest {
            profile: ms_memory::MemoryProfile::Medium,
            pressure: ms_memory::MemoryPressure::Critical,
            target_free_bytes: u64::MAX,
            pinned_pages: pinned.iter().copied().collect(),
        }
    }

    fn prepared_tile() -> OverlayPreparedTile {
        OverlayPreparedTile { tile_idx: 0, origin_px: [0, 0], size_px: [1, 1], rgba: vec![0; 4] }
    }

    #[test]
    fn off_screen_prepared_payloads_are_kept_and_never_drive_repaints() {
        let ctx = egui::Context::default();
        let mut runtime = OverlayRuntimeState::default();
        runtime.overlay_images.insert(1, Arc::new(opaque_pixel()));
        runtime.overlay_textures.insert(1, texture_page(&ctx, [1, 1], 0));
        runtime.overlay_prepared_pages.insert(1, OverlayPreparedPage { size: [1, 1], tiles: vec![prepared_tile()], next_upload_tile: 0 });
        runtime.begin_draw_tracking();
        runtime.drawn_pages.insert(0);

        assert!(!runtime.has_pending_work());
        assert!(runtime.overlay_prepared_pages.contains_key(&1), "finished work of a page that left the view is kept");
        runtime.shutdown();
    }

    #[test]
    fn eviction_frees_off_screen_prepare_work_with_its_texture_but_spares_pinned_uploads() {
        let ctx = egui::Context::default();
        let mut runtime = OverlayRuntimeState::default();
        for page_idx in 0..3 {
            runtime.overlay_images.insert(page_idx, Arc::new(opaque_pixel()));
            runtime.overlay_textures.insert(page_idx, texture_page(&ctx, [1, 1], 0));
        }
        // 0: visible, upload in progress -> protected.
        runtime.overlay_prepared_pages.insert(0, OverlayPreparedPage { size: [1, 1], tiles: vec![prepared_tile()], next_upload_tile: 0 });
        // 1: off screen, finished payload -> freed with the texture.
        runtime.overlay_prepared_pages.insert(1, OverlayPreparedPage { size: [1, 1], tiles: vec![prepared_tile()], next_upload_tile: 0 });
        runtime.overlay_dirty_tiles.insert(1, HashSet::from([0]));
        // 2: off screen, prepare in flight -> its result will be discarded.
        runtime.overlay_prepare_inflight.insert(2, 5);

        let snapshot = runtime.memory_usage_snapshot(&BTreeSet::from([0]));
        let bytes_of = |page: usize| snapshot.iter().find(|r| r.page_idx == Some(page)).map(|r| (r.estimated_bytes, r.reconstructable));
        assert_eq!(bytes_of(0), Some((4, false)));
        assert_eq!(bytes_of(1), Some((4, true)), "the payload counts toward the freed bytes");
        assert_eq!(bytes_of(2), Some((0, true)));

        let report = runtime.evict_cache(&eviction_request(&[0]));

        assert_eq!(report.resources.len(), 2);
        assert!(runtime.overlay_prepared_pages.contains_key(&0) && runtime.overlay_textures.contains_key(&0));
        assert!(!runtime.overlay_prepared_pages.contains_key(&1) && !runtime.overlay_textures.contains_key(&1));
        assert!(!runtime.overlay_dirty_tiles.contains_key(&1), "a full re-prepare replaces the dirty tiles");
        assert!(!runtime.overlay_prepare_inflight.contains_key(&2) && !runtime.overlay_textures.contains_key(&2));
        assert!(runtime.overlay_images.contains_key(&1), "the CPU image stays: the page rebuilds when drawn");
        runtime.shutdown();
    }
}
