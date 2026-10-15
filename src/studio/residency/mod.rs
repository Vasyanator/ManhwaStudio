/*
File: src/studio/residency/mod.rs

Purpose:
`PageResidency`, the studio's source-page residency pipeline, owned by `StudioApp`: the unified
background decode pool for source pages AND clean overlays, strictly ordered promotion of decoded
source pages on the GUI thread, the frame-budgeted tiled GPU upload, the follow-up page-cache
loader, the source-page geometry (`PageImageInfo`) and resident textures (`PageTexture`) it lends
to the drawing tab, and the memory-pressure eviction of what it owns.

Key structures:
- `PageResidency`: all of the above; knows no tab type (the page window to keep and the pixel
  inspection flag are passed in by `StudioApp`; the live canvas `cache_pages` setting is read from
  its owner, the `CleanOverlaysModel` it is handed).
- `UploadTask`: the page whose tiles are being uploaded across frames.

Key functions:
- `PageResidency::spawn`: builds the page metadata and starts the unified loader pool (a refused
  loader thread fails every page, `fail_unstarted_loader`).
- `PageResidency::poll_decoded`: drains source events (in-order promotion), uploads tiles under the
  per-frame budget, applies overlays as they arrive.
- `PageResidency::poll_page_cache`: starts the page-cache loader once everything is loaded, drains it.
- `PageResidency::lend_mut` / `page_infos`: the per-frame loan to the drawing tab.
- `PageResidency::shutdown`: releases decode workers parked on the look-ahead window.
- `memory.rs`: `trim_gpu`, `poll_pressure`, `evict_for_pressure`.

Notes:
`loader.rs` holds the worker pools and pure loader decisions, `decode.rs` the worker-side decode.
Everything here runs on the GUI thread and only polls channels and uploads prepared tiles.
*/

mod decode;
mod loader;
mod memory;

use self::decode::{DecodedPage, DecodedTile};
use self::loader::{
    LOADER_QUEUE_CAPACITY, LoaderEvent, OverlayLoaderEvent, PageCacheLoaderEvent,
    collect_missing_page_cache_jobs, collect_overlay_job_candidates,
    should_seed_page_cache_on_initial_load, spawn_loader_thread, spawn_page_cache_loader_thread,
};
use crate::memory_manager::{MemoryManager, MemoryPressure};
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::page_view::{PageImageInfo, PageTexture, SourcePageLoadState, TextureTile};
use crate::project::ProjectData;
use crate::runtime_log;
use eframe::egui::{self, ColorImage, TextureOptions};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use web_time::Instant;

const EVENT_POLL_BUDGET: usize = 8;
const UPLOAD_TILE_BUDGET_PER_FRAME: usize = 4;
const UPLOAD_BYTES_BUDGET_PER_FRAME: usize = 24 * 1024 * 1024;
const OVERLAY_EVENT_POLL_BUDGET: usize = 8;

/// The source-page residency of one studio session (see the file header). One per `StudioApp`;
/// a project reload builds a new app and therefore a new pipeline.
pub(in crate::studio) struct PageResidency {
    /// Process-shared memory profile owner (the settings tab holds another handle and may change
    /// the profile at runtime).
    memory_manager: Arc<MemoryManager>,
    /// Pressure classified by the last due poll; only used to log changes.
    memory_pressure: MemoryPressure,
    last_memory_pressure_poll: Option<Instant>,
    /// Number of project pages; promotion is complete when the frontier reaches it.
    total_pages: usize,
    /// Geometry + load state per page, kept apart from `textures` so canvas layout stays stable
    /// after GPU eviction.
    page_infos: HashMap<usize, PageImageInfo>,
    textures: HashMap<usize, PageTexture>,
    failed_pages: HashSet<usize>,
    load_errors: Vec<String>,
    loader_rx: Receiver<LoaderEvent>,
    /// Promoted pages, in page order, waiting for GPU upload.
    decoded_queue: VecDeque<DecodedPage>,
    /// Decoded pages that arrived ahead of the promotion frontier.
    decoded_pending_by_idx: HashMap<usize, DecodedPage>,
    /// The promotion frontier: lowest page index still awaiting in-order promotion.
    next_decode_idx_to_enqueue: usize,
    /// The promotion frontier published to the decode worker pool so it can throttle how far
    /// ahead it decodes (see `loader::DECODE_AHEAD_WINDOW`). Mirrors `next_decode_idx_to_enqueue`;
    /// updated whenever promotion advances it.
    decode_promotion_progress: Arc<AtomicUsize>,
    /// Raised by [`PageResidency::shutdown`] (from `shutdown_blocking`) to release decode workers
    /// parked on the look-ahead window. Without it, returning to the launcher mid-load freezes the
    /// promotion frontier and any parked worker (plus the pool-joining spawner thread) would
    /// spin/block forever, leaking threads on every launcher<->studio round-trip during a load.
    loader_shutdown: Arc<AtomicBool>,
    upload_task: Option<UploadTask>,
    loader_finished: bool,
    /// Receives overlay results from the unified loader pool (`spawn_loader_thread`); overlay
    /// jobs are interleaved with source-page jobs in page order, so overlays arrive alongside
    /// their source pages rather than after the whole source stage.
    overlay_loader_rx: Receiver<OverlayLoaderEvent>,
    overlay_loader_finished: bool,
    page_cache_loader_rx: Option<Receiver<PageCacheLoaderEvent>>,
    page_cache_loader_started: bool,
    page_cache_loader_finished: bool,
}

/// Incremental GPU upload of one promoted page: tiles `0..next_tile` are already textures.
struct UploadTask {
    idx: usize,
    tiles: Vec<DecodedTile>,
    next_tile: usize,
    uploaded_tiles: Vec<TextureTile>,
}

impl PageResidency {
    /// Builds the page metadata (every page `Loading`) and starts the unified loader pool for
    /// `project`'s source pages and their clean overlays. `cache_pages` is the canvas page-cache
    /// setting at open time (the project's value, which `CleanOverlaysModel` is seeded with); with a
    /// memory budget for it, the initial decode also seeds the shared page cache. Later changes of
    /// the setting are read from the model by [`Self::poll_page_cache`].
    ///
    /// When the OS refuses the loader thread, every page is reported failed at once
    /// ([`Self::fail_unstarted_loader`]), so no page stays `Loading` forever.
    pub(in crate::studio) fn spawn(project: &ProjectData, cache_pages: bool, memory_manager: Arc<MemoryManager>) -> Self {
        let cache_pages_on_initial_load =
            should_seed_page_cache_on_initial_load(cache_pages, memory_manager.budget());
        let pages: Vec<(usize, PathBuf)> = project
            .pages
            .iter()
            .map(|p| (p.idx, p.path.clone()))
            .collect();
        let page_infos = project
            .pages
            .iter()
            .map(|page| {
                (
                    page.idx,
                    PageImageInfo {
                        width_px: 0,
                        height_px: 0,
                        load_state: SourcePageLoadState::Loading,
                    },
                )
            })
            .collect();
        let (tx, rx) = sync_channel(LOADER_QUEUE_CAPACITY);
        let (overlay_tx, overlay_rx) = sync_channel(LOADER_QUEUE_CAPACITY);
        let mut residency = Self::from_channels(project.pages.len(), page_infos, rx, overlay_rx, memory_manager);
        if let Err(err) = spawn_loader_thread(
            pages,
            collect_overlay_job_candidates(project),
            tx,
            overlay_tx,
            cache_pages_on_initial_load,
            Arc::clone(&residency.decode_promotion_progress),
            Arc::clone(&residency.loader_shutdown),
        ) {
            runtime_log::log_error(format!(
                "[residency] could not start the page loader thread; every page of '{}' is reported as failed to load. Error: {err}. Possible cause: the OS thread limit or low memory; reopening the project retries",
                project.paths.project_dir.display()
            ));
            residency.fail_unstarted_loader(&err.to_string());
        }
        residency
    }

    /// The loader pool never started (`spawn`): marks every page `Failed`, records one load error
    /// naming `error`, and finishes both loader streams, so promotion skips every page, nothing is
    /// waited on or repainted for, and the page-cache step may proceed.
    fn fail_unstarted_loader(&mut self, error: &str) {
        for (idx, info) in &mut self.page_infos {
            info.load_state = SourcePageLoadState::Failed;
            self.failed_pages.insert(*idx);
        }
        self.load_errors.push(format!("page loader thread: {error}"));
        self.loader_finished = true;
        self.overlay_loader_finished = true;
        self.promote_decoded_pages_in_order();
    }

    /// The pipeline state over already-created loader channels, before anything was received.
    fn from_channels(
        total_pages: usize,
        page_infos: HashMap<usize, PageImageInfo>,
        loader_rx: Receiver<LoaderEvent>,
        overlay_loader_rx: Receiver<OverlayLoaderEvent>,
        memory_manager: Arc<MemoryManager>,
    ) -> Self {
        Self {
            memory_manager,
            memory_pressure: MemoryPressure::Normal,
            last_memory_pressure_poll: None,
            total_pages,
            page_infos,
            textures: HashMap::new(),
            failed_pages: HashSet::new(),
            load_errors: Vec::new(),
            loader_rx,
            decoded_queue: VecDeque::new(),
            decoded_pending_by_idx: HashMap::new(),
            next_decode_idx_to_enqueue: 0,
            decode_promotion_progress: Arc::new(AtomicUsize::new(0)),
            loader_shutdown: Arc::new(AtomicBool::new(false)),
            upload_task: None,
            loader_finished: false,
            overlay_loader_rx,
            overlay_loader_finished: false,
            page_cache_loader_rx: None,
            page_cache_loader_started: false,
            page_cache_loader_finished: false,
        }
    }

    /// Per-frame drain of the unified pool, in this order: source events (strict in-order
    /// promotion), the budgeted tile upload, then overlay events (applied to `overlays` as they
    /// arrive, no promotion). Requests a repaint while source work remains.
    pub(in crate::studio) fn poll_decoded(&mut self, ctx: &egui::Context, overlays: &Arc<Mutex<CleanOverlaysModel>>) {
        self.poll_loader_events(overlays);
        self.upload_textures_incremental(ctx);
        self.poll_overlay_loader_events(overlays);
    }

    /// Per-frame page-cache step: starts the page-cache loader once every source page is uploaded
    /// and every overlay delivered (when the canvas `cache_pages` setting — owned by `overlays`,
    /// `CleanOverlaysModel::cache_pages_enabled`, whichever tab or Settings changed it — is on and
    /// the memory profile allows a source-page CPU cache), then drains its events into `overlays`'
    /// page cache.
    pub(in crate::studio) fn poll_page_cache(&mut self, project: &ProjectData, overlays: &Arc<Mutex<CleanOverlaysModel>>) {
        self.ensure_page_cache_loader_started(project, overlays);
        self.poll_page_cache_loader_events(overlays);
    }

    /// Whether the overlay loader or a started page-cache loader still has work (the caller keeps
    /// repainting while it does).
    pub(in crate::studio) fn background_loads_pending(&self) -> bool {
        !self.overlay_loader_finished
            || (self.page_cache_loader_started && !self.page_cache_loader_finished)
    }

    /// Whether clean overlays are still being delivered (a late delivery would overwrite a clean
    /// mutation made meanwhile).
    pub(in crate::studio) fn overlays_loading(&self) -> bool {
        !self.overlay_loader_finished
    }

    /// The per-frame loan to the drawing tab: page geometry plus the resident textures, which the
    /// tab may re-upload into or mark used.
    pub(in crate::studio) fn lend_mut(&mut self) -> (&HashMap<usize, PageImageInfo>, &mut HashMap<usize, PageTexture>) {
        (&self.page_infos, &mut self.textures)
    }

    /// Center of page `idx` in page-local source pixels, or `None` while its geometry is unknown
    /// (not decoded yet, failed, or zero-sized).
    pub(in crate::studio) fn page_center(&self, idx: usize) -> Option<egui::Vec2> {
        self.page_infos.get(&idx).and_then(|info| {
            if info.width_px == 0 || info.height_px == 0 {
                return None;
            }
            Some(egui::vec2(
                info.width_px as f32 * 0.5,
                info.height_px as f32 * 0.5,
            ))
        })
    }

    /// Number of pages whose source decode finished (available or failed).
    pub(in crate::studio) fn loaded_pages_count(&self) -> usize {
        self.page_infos
            .values()
            .filter(|info| info.load_state != SourcePageLoadState::Loading)
            .count()
    }

    /// Every load failure so far (source pages, overlays, page cache), as display lines.
    pub(in crate::studio) fn load_errors(&self) -> &[String] {
        &self.load_errors
    }

    /// Releases decode workers parked on the look-ahead window. Must run when the GUI stops
    /// draining the loader channels (teardown): the frontier then never advances again. Workers
    /// blocked on `send` exit on their own once the receivers drop with `self`.
    pub(in crate::studio) fn shutdown(&self) {
        self.loader_shutdown.store(true, AtomicOrdering::Release);
    }

    fn poll_loader_events(&mut self, overlays: &Arc<Mutex<CleanOverlaysModel>>) {
        for _ in 0..EVENT_POLL_BUDGET {
            match self.loader_rx.try_recv() {
                Ok(LoaderEvent::Decoded(page)) => {
                    if let Some(info) = self.page_infos.get_mut(&page.idx) {
                        info.width_px = page.width;
                        info.height_px = page.height;
                        info.load_state = SourcePageLoadState::Available;
                    }
                    store_decoded_page_cache(&page, overlays);
                    if page.idx >= self.next_decode_idx_to_enqueue {
                        self.decoded_pending_by_idx.insert(page.idx, page);
                        self.promote_decoded_pages_in_order();
                    }
                }
                Ok(LoaderEvent::Failed { idx, path, error }) => {
                    if let Some(info) = self.page_infos.get_mut(&idx) {
                        info.load_state = SourcePageLoadState::Failed;
                    }
                    self.failed_pages.insert(idx);
                    self.load_errors.push(format!("{path}: {error}"));
                    self.promote_decoded_pages_in_order();
                }
                Ok(LoaderEvent::Finished) => {
                    self.loader_finished = true;
                    self.promote_decoded_pages_in_order();
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.loader_finished = true;
                    self.promote_decoded_pages_in_order();
                    break;
                }
            }
        }
    }

    fn promote_decoded_pages_in_order(&mut self) {
        let total_pages = self.total_pages;
        while self.next_decode_idx_to_enqueue < total_pages {
            if self.failed_pages.contains(&self.next_decode_idx_to_enqueue) {
                self.next_decode_idx_to_enqueue += 1;
                continue;
            }
            let Some(page) = self
                .decoded_pending_by_idx
                .remove(&self.next_decode_idx_to_enqueue)
            else {
                break;
            };
            self.decoded_queue.push_back(page);
            self.next_decode_idx_to_enqueue += 1;
        }
        // Publish promotion progress so the decode worker pool can slide its look-ahead
        // window and avoid decoding pages far beyond what has been promoted (see 2.10).
        self.decode_promotion_progress
            .store(self.next_decode_idx_to_enqueue, AtomicOrdering::Release);
    }

    fn upload_textures_incremental(&mut self, ctx: &egui::Context) {
        let mut uploaded_tiles = 0usize;
        let mut uploaded_bytes = 0usize;

        loop {
            if uploaded_tiles >= UPLOAD_TILE_BUDGET_PER_FRAME
                || uploaded_bytes >= UPLOAD_BYTES_BUDGET_PER_FRAME
            {
                break;
            }

            if self.upload_task.is_none() {
                let Some(page) = self.decoded_queue.pop_front() else {
                    break;
                };
                self.upload_task = Some(UploadTask {
                    idx: page.idx,
                    tiles: page.tiles,
                    next_tile: 0,
                    uploaded_tiles: Vec::new(),
                });
            }

            let mut page_finished = false;
            if let Some(task) = self.upload_task.as_mut() {
                if task.next_tile >= task.tiles.len() {
                    page_finished = true;
                } else {
                    let tile = &task.tiles[task.next_tile];
                    let color_image = ColorImage::from_rgba_unmultiplied(
                        [tile.size_px[0] as usize, tile.size_px[1] as usize],
                        &tile.rgba,
                    );
                    let texture = ctx.load_texture(
                        format!("page-{}-tile-{}", task.idx, task.next_tile),
                        color_image,
                        TextureOptions::LINEAR,
                    );
                    task.uploaded_tiles.push(TextureTile {
                        linear_texture: Some(texture),
                        nearest_texture: None,
                        // Tile sides are bounded by DECODE_TILE_SIDE (2048) and origins by
                        // page dimensions, all far below f32's exact-integer limit (2^24),
                        // so these u32->f32 casts are exact. There is no f32::from(u32).
                        #[allow(clippy::cast_precision_loss)]
                        origin_px: egui::vec2(tile.origin_px[0] as f32, tile.origin_px[1] as f32),
                        #[allow(clippy::cast_precision_loss)]
                        size_px: egui::vec2(tile.size_px[0] as f32, tile.size_px[1] as f32),
                        rgba: Arc::from(tile.rgba.as_slice()),
                    });
                    task.next_tile += 1;
                    uploaded_tiles += 1;
                    uploaded_bytes += tile.rgba.len();
                    page_finished = task.next_tile >= task.tiles.len();
                }
            }

            if page_finished && let Some(task) = self.upload_task.take() {
                self.textures.insert(
                    task.idx,
                    PageTexture {
                        tiles: task.uploaded_tiles,
                        linear_last_used_frame: ctx.cumulative_frame_nr(),
                        nearest_last_used_frame: 0,
                    },
                );
            }
        }

        if !self.loader_finished
            || !self.decoded_queue.is_empty()
            || !self.decoded_pending_by_idx.is_empty()
            || self.upload_task.is_some()
        {
            ctx.request_repaint();
        }
    }

    fn main_pages_fully_loaded(&self) -> bool {
        self.loader_finished
            && self.decoded_queue.is_empty()
            && self.decoded_pending_by_idx.is_empty()
            && self.upload_task.is_none()
            && self.next_decode_idx_to_enqueue >= self.total_pages
    }

    /// Starts the page-cache loader once (see [`Self::poll_page_cache`]). The cheap residency checks
    /// run first, so the model lock is taken only once everything is loaded and the loader has not
    /// started yet; it is held just for the flag read.
    fn ensure_page_cache_loader_started(&mut self, project: &ProjectData, overlays: &Arc<Mutex<CleanOverlaysModel>>) {
        if self.page_cache_loader_started
            || !self.memory_manager_allows_source_page_cache()
            || !self.main_pages_fully_loaded()
            || !self.overlay_loader_finished
        {
            return;
        }
        let cache_pages = match overlays.lock() {
            Ok(model) => model.cache_pages_enabled(),
            Err(err) => {
                // The page cache is an optional accelerator and every later step (job collection,
                // event delivery) needs this same lock, so give it up for the session — marked
                // started AND finished, which also stops the per-frame retry (and its log line).
                runtime_log::log_error(format!(
                    "[residency] CleanOverlaysModel lock poisoned; page-cache loader not started for this session (the shared page RGBA cache is not pre-filled); error={err}"
                ));
                self.page_cache_loader_started = true;
                self.page_cache_loader_finished = true;
                return;
            }
        };
        if !cache_pages {
            return;
        }
        let jobs = collect_missing_page_cache_jobs(project, overlays);
        if jobs.is_empty() {
            self.page_cache_loader_finished = true;
            return;
        }
        self.page_cache_loader_started = true;
        let (tx, rx) = sync_channel(LOADER_QUEUE_CAPACITY);
        match spawn_page_cache_loader_thread(jobs, tx) {
            Ok(()) => self.page_cache_loader_rx = Some(rx),
            Err(err) => {
                // Optional accelerator: give it up for the session (started AND finished), as for
                // the poisoned lock above, so no per-frame retry spins on a refused thread.
                runtime_log::log_error(format!(
                    "[residency] could not start the page-cache loader thread; the shared page RGBA cache is not pre-filled this session. Error: {err}. Possible cause: the OS thread limit or low memory"
                ));
                self.page_cache_loader_finished = true;
            }
        }
    }

    fn poll_overlay_loader_events(&mut self, overlays: &Arc<Mutex<CleanOverlaysModel>>) {
        for _ in 0..OVERLAY_EVENT_POLL_BUDGET {
            match self.overlay_loader_rx.try_recv() {
                Ok(OverlayLoaderEvent::Decoded { idx, overlay }) => {
                    match overlays.lock() {
                        Ok(mut overlays) => overlays.load_prepared_overlay(idx, overlay.rgba, overlay.color),
                        Err(err) => runtime_log::log_error(format!(
                            "[residency] CleanOverlaysModel lock poisoned; decoded clean overlay dropped; page_idx={idx}; error={err}"
                        )),
                    }
                }
                Ok(OverlayLoaderEvent::Failed { idx, path, error }) => {
                    self.load_errors
                        .push(format!("overlay {idx} {path}: {error}"));
                }
                Ok(OverlayLoaderEvent::Finished) => {
                    self.overlay_loader_finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.overlay_loader_finished = true;
                    break;
                }
            }
        }
    }

    fn poll_page_cache_loader_events(&mut self, overlays: &Arc<Mutex<CleanOverlaysModel>>) {
        let Some(rx) = self.page_cache_loader_rx.as_ref() else {
            return;
        };
        for _ in 0..OVERLAY_EVENT_POLL_BUDGET {
            match rx.try_recv() {
                Ok(PageCacheLoaderEvent::Decoded { idx, image }) => {
                    match overlays.lock() {
                        // `false` only means `idx` is outside the model's page list; the model is
                        // built from the same project pages, and the cache is an optional
                        // accelerator, so there is nothing to report.
                        Ok(mut overlays) => {
                            let _ = overlays.store_cached_page_rgba(idx, image);
                        }
                        Err(err) => runtime_log::log_error(format!(
                            "[residency] CleanOverlaysModel lock poisoned; decoded page-cache image dropped; page_idx={idx}; error={err}"
                        )),
                    }
                }
                Ok(PageCacheLoaderEvent::Failed { idx, path, error }) => {
                    self.load_errors
                        .push(format!("page cache {idx} {path}: {error}"));
                }
                Ok(PageCacheLoaderEvent::Finished) => {
                    self.page_cache_loader_finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.page_cache_loader_finished = true;
                    break;
                }
            }
        }
    }

    fn memory_manager_allows_source_page_cache(&self) -> bool {
        self.memory_manager.budget().source_page_cpu_cache_bytes > 0
    }
}

/// Seeds the shared page RGBA cache of `overlays` from a decoded page that carries its full image.
fn store_decoded_page_cache(page: &DecodedPage, overlays: &Arc<Mutex<CleanOverlaysModel>>) {
    let Some(image) = page.cache_rgba.as_ref() else {
        return;
    };
    match overlays.lock() {
        // `false` only means `page.idx` is outside the model's page list; the model is built from
        // the same project pages, and the cache is an optional accelerator, so there is nothing to
        // report.
        Ok(mut overlays) => {
            let _ = overlays.store_cached_page_rgba_arc(page.idx, Arc::clone(image));
        }
        Err(err) => runtime_log::log_error(format!(
            "[residency] CleanOverlaysModel lock poisoned; decoded page not seeded into the page cache; page_idx={}; error={err}",
            page.idx
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{LoaderEvent, PageResidency};
    use super::decode::DecodedPage;
    use crate::memory_manager::{MemoryManager, MemoryProfile};
    use crate::models::clean_overlays_model::CleanOverlaysModel;
    use crate::page_view::{PageImageInfo, SourcePageLoadState};
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::{SyncSender, sync_channel};
    use std::sync::{Arc, Mutex};

    /// A residency over test channels for `total_pages` pages, plus the source-event sender.
    fn residency(total_pages: usize) -> (PageResidency, SyncSender<LoaderEvent>) {
        let (tx, rx) = sync_channel(16);
        // The overlay sender is dropped at once: these tests do not poll overlays.
        let (_, overlay_rx) = sync_channel(1);
        let page_infos = (0..total_pages)
            .map(|idx| (idx, PageImageInfo { width_px: 0, height_px: 0, load_state: SourcePageLoadState::Loading }))
            .collect();
        let memory = Arc::new(MemoryManager::new(MemoryProfile::Medium));
        (PageResidency::from_channels(total_pages, page_infos, rx, overlay_rx, memory), tx)
    }

    fn decoded(idx: usize) -> LoaderEvent {
        LoaderEvent::Decoded(DecodedPage { idx, width: 10, height: 20, tiles: Vec::new(), cache_rgba: None })
    }

    fn overlays() -> Arc<Mutex<CleanOverlaysModel>> {
        Arc::new(Mutex::new(CleanOverlaysModel::new_from_pages(&[])))
    }

    fn queued(residency: &PageResidency) -> Vec<usize> {
        residency.decoded_queue.iter().map(|page| page.idx).collect()
    }

    /// Pages decoded out of order are held back until every earlier page arrived, then queued for
    /// upload strictly in page order; the published frontier follows the promotion.
    #[test]
    fn promotion_is_strictly_in_page_order() -> Result<(), Box<dyn std::error::Error>> {
        let (mut residency, tx) = residency(3);
        let overlays = overlays();
        tx.send(decoded(2))?;
        tx.send(decoded(1))?;
        residency.poll_loader_events(&overlays);
        assert!(queued(&residency).is_empty());
        assert_eq!(residency.decode_promotion_progress.load(Ordering::Acquire), 0);
        // Geometry is published on arrival, before promotion.
        assert_eq!(residency.page_center(2), Some(eframe::egui::vec2(5.0, 10.0)));
        assert_eq!(residency.loaded_pages_count(), 2);

        tx.send(decoded(0))?;
        residency.poll_loader_events(&overlays);
        assert_eq!(queued(&residency), vec![0, 1, 2]);
        assert_eq!(residency.decode_promotion_progress.load(Ordering::Acquire), 3);
        Ok(())
    }

    /// A failed page does not block the pages after it, and its error is recorded.
    #[test]
    fn a_failed_page_is_skipped_by_promotion() -> Result<(), Box<dyn std::error::Error>> {
        let (mut residency, tx) = residency(3);
        let overlays = overlays();
        tx.send(decoded(1))?;
        tx.send(LoaderEvent::Failed { idx: 0, path: "p0".to_string(), error: "bad".to_string() })?;
        residency.poll_loader_events(&overlays);
        assert_eq!(queued(&residency), vec![1]);
        assert_eq!(residency.decode_promotion_progress.load(Ordering::Acquire), 2);
        assert_eq!(residency.load_errors(), ["p0: bad".to_string()]);
        assert_eq!(residency.page_center(0), None);
        assert_eq!(residency.page_infos[&0].load_state, SourcePageLoadState::Failed);
        Ok(())
    }

    /// The main pages count as fully loaded only after `Finished`, a promotion frontier at the end
    /// and nothing left to upload.
    #[test]
    fn main_pages_are_loaded_only_after_finished_and_upload() -> Result<(), Box<dyn std::error::Error>> {
        let (mut residency, tx) = residency(1);
        let overlays = overlays();
        tx.send(decoded(0))?;
        residency.poll_loader_events(&overlays);
        assert!(!residency.main_pages_fully_loaded());
        tx.send(LoaderEvent::Finished)?;
        residency.poll_loader_events(&overlays);
        // Promoted but not uploaded yet.
        assert!(!residency.main_pages_fully_loaded());
        residency.decoded_queue.clear();
        assert!(residency.main_pages_fully_loaded());
        Ok(())
    }

    /// A loader that never started reports every page as failed (none stays `Loading`), records
    /// the error once, and counts as finished for both streams.
    #[test]
    fn an_unstarted_loader_fails_every_page() {
        let (mut residency, _tx) = residency(3);
        residency.fail_unstarted_loader("thread limit");
        assert!((0..3).all(|idx| residency.page_infos[&idx].load_state == SourcePageLoadState::Failed));
        assert_eq!(residency.load_errors(), ["page loader thread: thread limit".to_string()]);
        assert!(residency.loader_finished && residency.overlay_loader_finished);
        assert_eq!(residency.decode_promotion_progress.load(Ordering::Acquire), 3);
        assert!(residency.main_pages_fully_loaded());
    }

    /// A dropped loader (disconnected channel) counts as finished, so a crashed pool cannot keep
    /// the app repainting forever.
    #[test]
    fn a_disconnected_loader_counts_as_finished() {
        let (mut residency, tx) = residency(0);
        drop(tx);
        residency.poll_loader_events(&overlays());
        assert!(residency.loader_finished);
        assert!(residency.main_pages_fully_loaded());
    }
}
