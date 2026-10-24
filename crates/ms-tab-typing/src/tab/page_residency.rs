/*
File: tab/page_residency.rs

Purpose:
Makes one page's layer data resident in the typing tab: its doc payload (rasters, text nodes,
groups) decoded from disk and inserted into the shared `LayerDoc`, then projected into this tab's
per-page caches (`raster_layers_by_page`, `bands_by_page`, `groups_by_page`) — or, without a
resident doc page, the disk cache read straight into those caches.

Main responsibilities:
- split a page load into its DISK half (`load_page_from_disk`: file reads + PNG decode, no lock,
  `Send`) and its APPLY half (`TypingTextOverlayLayer::apply_page_load`: brief doc lock, memoized
  insert, projection), so the same two halves serve the blocking load and the background one;
- run the disk half for the canvas draw and the «Слои» panel on ONE background worker per chapter
  (`TypingPageResidencyLoader`), which wakes the GUI when a page is decoded; the GUI thread only
  polls (`poll_page_residency_loads`) and applies.

Key structures:
- `TypingPageLoadRequest`: the owned inputs of one disk half.
- `TypingPageLoadOutcome`: what the disk half produced.
- `TypingPageResidencyLoader`: the worker's channels and the pages it is decoding.

Key functions:
- `TypingTextOverlayLayer::request_page_residency` (non-blocking, draw path),
  `ensure_raster_layers_for_page` (blocking, `doc_layers.rs`), `poll_page_residency_loads`,
  `apply_page_load`, `invalidate_pending_page_loads`.

Notes:
- A page the shared doc already holds (the PS editor made it resident) needs no disk at all: it is
  projected inline, which is exactly where a blocking load of such a page ends up (the doc insert
  is memoized and would discard the decoded payload).
- Staleness: every result carries the `page_load_generation` its request was sent under. Any
  invalidation of a page projection (chapter reload, migration eviction, raster-cache drop) bumps
  the generation, so a decode that read the disk before it is discarded and the page is requested
  again on its next draw. A result for a page that became resident meanwhile is discarded too.
- If the worker cannot be spawned or dies, the error is logged and the tab falls back to the
  blocking load for the rest of the chapter (the behaviour before this loader existed).
*/

use super::*;
use ms_models::layer_model::layer_doc::{DecodedPagePayload, LayerDoc};
use ms_models::layer_model::ordering::Band;
use ms_models::layer_model::persist::{self, PageRasters};

/// Everything the disk half of one page load reads, captured on the GUI thread (owned, `Send`).
#[derive(Debug, Clone)]
pub(super) struct TypingPageLoadRequest {
    pub(super) page_idx: usize,
    /// Staging `layers/` dir (read first).
    primary: PathBuf,
    /// Committed `layers/` dir (read fallback).
    fallback: Option<PathBuf>,
    /// Un-migrated legacy `text_images/` dir, `None` once the chapter migrated (`doc_legacy_text_dir`).
    legacy: Option<PathBuf>,
    /// A shared `LayerDoc` is wired: decode the doc payload; otherwise read only the disk cache.
    pub(super) with_doc: bool,
}

/// Disk half of one page load. No `Debug`: `DecodedPagePayload` does not implement it.
pub(super) enum TypingPageLoadOutcome {
    /// The doc payload decoded; the apply moves it into the doc (memoized) and projects it.
    Decoded(DecodedPagePayload),
    /// No doc payload: the decode failed (`decode_error`) or no doc is wired (`None`). Carries the
    /// disk-cache read the apply uses when the doc does not hold the page.
    DiskCache {
        decode_error: Option<String>,
        bands: Vec<Band>,
        rasters: Result<PageRasters, String>,
    },
}

/// Runs the disk half of `request` (file reads, PNG decode, legacy migration; no lock, no egui):
/// safe on any thread. `page_sizes` must be the FULL chapter page-size map when `with_doc`
/// (`LayerDoc::decode_page_payload` recovers a chapter-wide ribbon scale from every page); it is
/// unused otherwise.
pub(super) fn load_page_from_disk(request: &TypingPageLoadRequest, page_sizes: &HashMap<usize, [usize; 2]>) -> TypingPageLoadOutcome {
    if request.with_doc {
        match LayerDoc::decode_page_payload(
            request.page_idx,
            &request.primary,
            request.fallback.as_deref(),
            request.legacy.as_deref(),
            page_sizes,
        ) {
            Ok(payload) => return TypingPageLoadOutcome::Decoded(payload),
            Err(err) => {
                let (bands, rasters) = read_disk_cache(request);
                return TypingPageLoadOutcome::DiskCache { decode_error: Some(err), bands, rasters };
            }
        }
    }
    let (bands, rasters) = read_disk_cache(request);
    TypingPageLoadOutcome::DiskCache { decode_error: None, bands, rasters }
}

/// The disk-cache read of a page (its band order and raster layers), used without a resident doc page.
fn read_disk_cache(request: &TypingPageLoadRequest) -> (Vec<Band>, Result<PageRasters, String>) {
    let bands = persist::load_page_bands(&request.primary, request.fallback.as_deref(), request.page_idx);
    let rasters = persist::load_page_rasters(&request.primary, request.fallback.as_deref(), request.page_idx);
    (bands, rasters)
}

/// One page request to the background loader.
struct TypingPageLoadJob {
    request: TypingPageLoadRequest,
    /// `page_load_generation` when the request was sent.
    generation: u64,
}

/// One decoded page from the background loader.
struct TypingPageLoadDone {
    request: TypingPageLoadRequest,
    generation: u64,
    outcome: TypingPageLoadOutcome,
}

/// The per-chapter background page loader: one worker thread that runs `load_page_from_disk` for
/// each requested page in request order and wakes the GUI after each result. Dropping it closes
/// the request channel, which ends the worker after its current page.
pub(super) struct TypingPageResidencyLoader {
    request_tx: mpsc::Sender<TypingPageLoadJob>,
    result_rx: Receiver<TypingPageLoadDone>,
    /// Pages requested and not yet received, so a page drawn every frame is requested once.
    in_flight: HashSet<usize>,
}

impl TypingPageResidencyLoader {
    /// Spawns the worker. `page_paths` are the chapter's `(page_idx, image path)` pairs; the worker
    /// builds the full page-size map from them (image headers) once, before its first decode.
    /// `ctx` is woken after every result.
    fn spawn(ctx: &egui::Context, page_paths: Vec<(usize, PathBuf)>) -> std::io::Result<Self> {
        let (request_tx, request_rx) = mpsc::channel::<TypingPageLoadJob>();
        let (result_tx, result_rx) = mpsc::channel::<TypingPageLoadDone>();
        let ctx = ctx.clone();
        thread::Builder::new().name("typing-page-residency".to_string()).spawn(move || {
            let mut page_sizes: Option<HashMap<usize, [usize; 2]>> = None;
            // Ends when the loader (the request sender) is dropped: chapter reload or tab teardown.
            while let Ok(job) = request_rx.recv() {
                let sizes = page_sizes.get_or_insert_with(|| load_typing_page_sizes(&page_paths));
                let outcome = load_page_from_disk(&job.request, sizes);
                let done = TypingPageLoadDone { request: job.request, generation: job.generation, outcome };
                if result_tx.send(done).is_err() {
                    break; // the loader was dropped while this page decoded
                }
                // The GUI may be idle; without a frame the result would wait for the next input event.
                ctx.request_repaint();
            }
        })?;
        Ok(Self { request_tx, result_rx, in_flight: HashSet::new() })
    }
}

impl TypingTextOverlayLayer {
    /// The owned disk-half inputs for `page_idx`, or `None` when no layers dir is wired yet (the
    /// page then has an empty projection, `insert_empty_page_projection`).
    pub(super) fn page_load_request(&self, page_idx: usize) -> Option<TypingPageLoadRequest> {
        Some(TypingPageLoadRequest {
            page_idx,
            primary: self.layers_primary_dir.clone()?,
            fallback: self.layers_fallback_dir.clone(),
            legacy: self.doc_legacy_text_dir.clone(),
            with_doc: self.layer_doc.is_some(),
        })
    }

    /// An empty projection for a page of a chapter with no layers dir wired: not taken from the
    /// doc, so no page revision describes it.
    pub(super) fn insert_empty_page_projection(&mut self, page_idx: usize) {
        self.projected_page_revision.remove(&page_idx);
        self.raster_layers_by_page.insert(page_idx, Vec::new());
        self.bands_by_page.insert(page_idx, Vec::new());
        self.groups_by_page.insert(page_idx, Vec::new());
    }

    /// Non-blocking form of `ensure_raster_layers_for_page` for the canvas draw and the «Слои»
    /// panel. Returns `true` when `page_idx` is resident now; otherwise its disk half was handed to
    /// the background loader (or already runs there) and the page draws without its layers until
    /// `poll_page_residency_loads` applies the result (the worker wakes the GUI for that frame).
    pub(super) fn request_page_residency(&mut self, ctx: &egui::Context, page_idx: usize) -> bool {
        if self.raster_layers_by_page.contains_key(&page_idx) {
            return true;
        }
        let Some(request) = self.page_load_request(page_idx) else {
            self.insert_empty_page_projection(page_idx);
            return true;
        };
        // The doc already holds the page (the PS editor loaded it): no disk read is needed.
        if request.with_doc && self.project_resident_doc_page(page_idx) {
            self.finish_page_load(page_idx);
            return true;
        }
        if self.page_residency_loader_failed {
            self.ensure_raster_layers_for_page(page_idx);
            return true;
        }
        if self.page_residency_loader.as_ref().is_some_and(|loader| loader.in_flight.contains(&page_idx)) {
            return false;
        }
        if self.page_residency_loader.is_none() {
            let page_paths: Vec<(usize, PathBuf)> = self.page_image_paths.iter().map(|(idx, path)| (*idx, path.clone())).collect();
            match TypingPageResidencyLoader::spawn(ctx, page_paths) {
                Ok(loader) => self.page_residency_loader = Some(loader),
                Err(err) => {
                    ms_log::runtime_log::log_error(format!(
                        "[typing] could not start the background page loader: {err}\n\
                         Effect: pages of this chapter load their layers on the interface thread, \
                         which can stall it briefly on the first visit of a page."
                    ));
                    self.page_residency_loader_failed = true;
                    self.ensure_raster_layers_for_page(page_idx);
                    return true;
                }
            }
        }
        let generation = self.page_load_generation;
        let sent = self.page_residency_loader.as_mut().is_some_and(|loader| {
            let sent = loader.request_tx.send(TypingPageLoadJob { request, generation }).is_ok();
            if sent {
                loader.in_flight.insert(page_idx);
            }
            sent
        });
        if !sent {
            self.page_residency_loader_lost("its request channel is closed");
            self.ensure_raster_layers_for_page(page_idx);
            return true;
        }
        ms_log::trace_log!(cat::PERSIST, "page_residency request page={} generation={}", page_idx, generation);
        false
    }

    /// Applies every page the background loader finished since the last call (GUI thread, once
    /// per frame from `draw`). A result whose generation is stale, or whose page became resident
    /// meanwhile, is discarded (the page is requested again on its next draw if still needed).
    /// Returns whether anything changed, so the caller repaints.
    pub(super) fn poll_page_residency_loads(&mut self) -> bool {
        let Some(loader) = self.page_residency_loader.as_mut() else {
            return false;
        };
        let mut finished = Vec::new();
        let mut disconnected = false;
        loop {
            match loader.result_rx.try_recv() {
                Ok(done) => {
                    loader.in_flight.remove(&done.request.page_idx);
                    finished.push(done);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        let changed = !finished.is_empty() || disconnected;
        for done in finished {
            let page_idx = done.request.page_idx;
            if done.generation != self.page_load_generation || self.raster_layers_by_page.contains_key(&page_idx) {
                ms_log::trace_log!(cat::PERSIST, "page_residency discard page={} generation={} current={}", page_idx, done.generation, self.page_load_generation);
                continue;
            }
            self.apply_page_load(&done.request, done.outcome);
        }
        if disconnected {
            self.page_residency_loader_lost("its worker thread ended unexpectedly");
        }
        changed
    }

    /// Logs a dead background loader and switches the chapter to blocking page loads.
    fn page_residency_loader_lost(&mut self, cause: &str) {
        ms_log::runtime_log::log_error(format!(
            "[typing] the background page loader stopped: {cause}\n\
             Effect: pages of this chapter load their layers on the interface thread from now on."
        ));
        self.page_residency_loader = None;
        self.page_residency_loader_failed = true;
    }

    /// Makes every in-flight background page load stale. Called wherever a page projection is
    /// dropped, so a decode that read the disk before the drop can never be applied after it.
    pub(super) fn invalidate_pending_page_loads(&mut self) {
        self.page_load_generation = self.page_load_generation.wrapping_add(1);
    }

    /// Ends this chapter's background loader (chapter reload): pending results are dropped, and
    /// a new loader with the new chapter's pages starts on the next request.
    pub(super) fn reset_page_residency_loader(&mut self) {
        self.invalidate_pending_page_loads();
        self.page_residency_loader = None;
        self.page_residency_loader_failed = false;
    }

    /// Apply half of a page load (GUI thread, cheap: a brief doc lock and the projection).
    ///
    /// A decoded payload goes into the doc through the memoized `LayerDoc::insert_decoded_page` (a
    /// page already resident keeps its live state) and the doc page is projected. Without a decoded
    /// payload the resident doc page is projected if there is one (live edits survive a disk decode
    /// failure); otherwise the disk cache becomes the projection. A poisoned doc lock after a
    /// successful decode falls back to the disk cache, read here (blocking, as it always was).
    /// Never touches an in-flight edit render: its result is validated on its own (render token +
    /// overlay identity in `apply_edit_overlay_render_result`; `sync_from_doc` keeps overlay
    /// indices stable).
    pub(super) fn apply_page_load(&mut self, request: &TypingPageLoadRequest, outcome: TypingPageLoadOutcome) {
        let page_idx = request.page_idx;
        let disk_cache = match outcome {
            TypingPageLoadOutcome::Decoded(payload) => {
                let inserted = self.layer_doc.clone().is_some_and(|doc| match doc.lock() {
                    Ok(mut guard) => {
                        guard.insert_decoded_page(page_idx, payload);
                        self.sync_from_doc(page_idx, &guard);
                        true
                    }
                    Err(_) => false,
                });
                if inserted { None } else { Some(read_disk_cache(request)) }
            }
            TypingPageLoadOutcome::DiskCache { decode_error, bands, rasters } => {
                let resident_projected = match decode_error {
                    Some(err) => {
                        ms_log::runtime_log::log_warn(format!(
                            "[typing] layer payload decode for page {page_idx} failed: {err}"
                        ));
                        // A resident page remains valid after a disk decode failure, so preserve its
                        // live edits by projecting it instead of replacing it with disk-loaded state.
                        self.project_resident_doc_page(page_idx)
                    }
                    None => false,
                };
                if resident_projected { None } else { Some((bands, rasters)) }
            }
        };
        if let Some((bands, rasters)) = disk_cache {
            self.apply_disk_cache_projection(page_idx, bands, rasters);
        }
        self.finish_page_load(page_idx);
    }

    /// Projects `page_idx` from the shared doc if the doc holds it; `false` (nothing done) when no
    /// doc is wired, its lock is poisoned or the page is not resident there.
    fn project_resident_doc_page(&mut self, page_idx: usize) -> bool {
        let Some(doc) = self.layer_doc.clone() else {
            return false;
        };
        let Ok(guard) = doc.lock() else {
            return false;
        };
        if guard.page(page_idx).is_none() {
            return false;
        }
        self.sync_from_doc(page_idx, &guard);
        true
    }

    /// Makes the disk cache the projection of `page_idx`. No doc revision describes it, so the page
    /// reads as possibly stale until the doc path re-projects (or adopts) it.
    fn apply_disk_cache_projection(&mut self, page_idx: usize, bands: Vec<Band>, rasters: Result<PageRasters, String>) {
        self.projected_page_revision.remove(&page_idx);
        self.bands_by_page.insert(page_idx, bands);
        let (groups, layers) = match rasters {
            Ok(page) => (page.groups, page
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
                    group_uid: l.group_uid,
                    clipped_image: None,
                    texture: None,
                })
                .collect()),
            Err(err) => {
                ms_log::runtime_log::log_warn(format!(
                    "[typing] load PS raster layers for page {page_idx} failed: {err}"
                ));
                (Vec::new(), Vec::new())
            }
        };
        self.groups_by_page.insert(page_idx, groups);
        self.raster_layers_by_page.insert(page_idx, layers);
    }

    /// Common tail of every page load: a just-created raster is selected from the final
    /// source-of-truth projection (doc or disk cache).
    fn finish_page_load(&mut self, page_idx: usize) {
        if let Some((pending_page, uid)) = self.pending_select_raster_uid.clone()
            && pending_page == page_idx
            && let Some(idx) = self
                .raster_layers_by_page
                .get(&page_idx)
                .and_then(|layers| layers.iter().position(|layer| layer.uid == uid))
        {
            self.selected_raster_idx = Some(idx);
            self.selected_raster_page = Some(page_idx);
            self.selected_overlay_idx = None;
            self.pending_select_raster_uid = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A unique, initially empty scratch directory under the system temp dir (removed by the test).
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ms_typing_page_residency_{name}_{}", std::process::id()));
        // A leftover from an aborted earlier run would make the test read stale files.
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("remove a stale scratch dir");
        }
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        dir
    }

    /// Polls until the background loader has answered every request (bounded wait).
    fn drain_loads(layer: &mut TypingTextOverlayLayer) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while layer.page_residency_loader.as_ref().is_some_and(|loader| !loader.in_flight.is_empty()) {
            layer.poll_page_residency_loads();
            assert!(Instant::now() < deadline, "the background page loader never answered");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The draw path never blocks: the first request only queues the page, and the page becomes
    /// resident when its background result is polled — through the same apply as the blocking load.
    #[test]
    fn a_requested_page_becomes_resident_when_its_background_load_is_polled() {
        let dir = scratch_dir("resident");
        let ctx = egui::Context::default();
        let mut layer = TypingTextOverlayLayer { layers_primary_dir: Some(dir.clone()), ..Default::default() };
        assert!(!layer.request_page_residency(&ctx, 0), "a never-loaded page is not resident at once");
        assert!(!layer.request_page_residency(&ctx, 0), "a page in flight is not requested twice");
        assert_eq!(layer.page_residency_loader.as_ref().map(|loader| loader.in_flight.len()), Some(1));
        drain_loads(&mut layer);
        assert!(layer.raster_layers_by_page.contains_key(&0), "the polled result made the page resident");
        assert!(layer.request_page_residency(&ctx, 0));
        std::fs::remove_dir_all(&dir).expect("remove the scratch dir");
    }

    /// A load sent before its page projection was invalidated is discarded, never applied, and the
    /// page is simply requested again.
    #[test]
    fn a_load_that_predates_an_invalidation_is_discarded() {
        let dir = scratch_dir("stale");
        let ctx = egui::Context::default();
        let mut layer = TypingTextOverlayLayer { layers_primary_dir: Some(dir.clone()), ..Default::default() };
        assert!(!layer.request_page_residency(&ctx, 2));
        layer.invalidate_pending_page_loads();
        drain_loads(&mut layer);
        assert!(!layer.raster_layers_by_page.contains_key(&2), "a stale result must not be applied");
        assert!(!layer.request_page_residency(&ctx, 2), "the page is requested again");
        drain_loads(&mut layer);
        assert!(layer.raster_layers_by_page.contains_key(&2));
        std::fs::remove_dir_all(&dir).expect("remove the scratch dir");
    }

    /// A page the shared doc already holds is projected inline: no disk read, no loader started.
    #[test]
    fn a_page_resident_in_the_doc_is_projected_without_a_background_load() {
        let dir = scratch_dir("doc_resident");
        let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
        page_sizes.insert(0, [100, 100]);
        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &page_sizes).expect("load an empty doc page");
        let ctx = egui::Context::default();
        let mut layer = TypingTextOverlayLayer { layers_primary_dir: Some(dir.clone()), ..Default::default() };
        layer.set_layer_doc(Arc::new(Mutex::new(doc)));
        assert!(layer.request_page_residency(&ctx, 0));
        assert!(layer.raster_layers_by_page.contains_key(&0));
        assert!(layer.page_residency_loader.is_none(), "no worker for a page the doc already holds");
        std::fs::remove_dir_all(&dir).expect("remove the scratch dir");
    }

    /// Without a layers dir there is nothing to read: the page gets an empty projection at once.
    #[test]
    fn without_a_layers_dir_the_page_is_resident_and_empty_at_once() {
        let ctx = egui::Context::default();
        let mut layer = TypingTextOverlayLayer::default();
        assert!(layer.request_page_residency(&ctx, 5));
        assert_eq!(layer.raster_layers_by_page.get(&5).map(Vec::len), Some(0));
        assert!(layer.page_residency_loader.is_none());
    }
}
