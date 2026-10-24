/*
File: src/studio/residency/loader.rs

Purpose:
The background loader threads of the page residency pipeline: the unified decode pool for source
pages AND clean overlays, and the follow-up page-cache loader. Also the pure loader decisions
(look-ahead window, worker count, initial page-cache seeding) and the job lists built from a
`ProjectData`.

Key structures:
- `LoaderEvent` / `OverlayLoaderEvent` / `PageCacheLoaderEvent`: what each pool streams to the GUI.
- `LoaderJob`: one unit of work of the unified pool (source page or its clean overlay).
- `GuiWaker` / `WakingSender`: how a pool wakes the GUI thread after each result (no GUI polling).

Key functions:
- `spawn_loader_thread`: the unified pool, sorted by (page index, source-before-overlay), with the
  `DECODE_AHEAD_WINDOW` look-ahead throttle and the shutdown escape hatch.
- `spawn_page_cache_loader_thread`: decodes pages missing from the shared page RGBA cache.
- `loadable_clean_overlay`: the "staged over committed" clean resolution per page.
- `decode_idx_within_window`, `decode_worker_count`, `should_seed_page_cache_on_initial_load`.

- `run_decode_pool`: starts and joins the workers of one pool (spawn failures handled here).

Notes:
Every event reaches the GUI through a `WakingSender`, which requests a repaint after each send and
when it is dropped (so a `Finished` or a disconnect is never left unread until the next input), on
every target where workers may wake the GUI (`WORKERS_WAKE_GUI`; not wasm, where the GUI polls).
Both pool joiners send `Finished` only after every worker returned, so a `Finished` event means
every job kind of that pool is done. A worker panic is logged by the joiner and does not stop the
`Finished` event. Every thread goes through `ms_thread::Builder`: a pool thread the OS refuses is
returned as `Err` to the GUI-side caller (which reports the pages); a refused decode worker is
logged and the pool runs with fewer workers, or with one inline on the joiner thread when none
started.
*/

use super::decode::{DecodedPage, PreparedOverlay, decode_overlay, decode_page, decode_page_rgba};
use crate::models::clean_assign::{LOADER_CLEAN_SCOPE, PageCleanPaths, PageCleanResolution, probe_page_clean};
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::project::ProjectData;
use crate::runtime_log;
use ms_thread::{self as thread, JoinHandle};
use std::collections::{HashSet, VecDeque};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use eframe::egui;
use std::sync::mpsc::{SendError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use web_time::Duration;

/// Bounded capacity of every loader channel: the GUI-side back-pressure on the pools.
pub(super) const LOADER_QUEUE_CAPACITY: usize = 2;
/// Maximum number of pages workers may decode ahead of the next SOURCE page still waiting
/// for in-order promotion. Both job kinds of the unified loader pool park on this bound:
/// for source jobs it bounds `decoded_pending_by_idx` so a slow early page cannot let later
/// pages pile up unbounded in RAM; overlay jobs park on the same source frontier so decoded
/// overlays stay roughly in step with their source pages. Must be >= 1 so the source page
/// that unblocks promotion is always inside the window (otherwise the loader would deadlock).
pub(super) const DECODE_AHEAD_WINDOW: usize = 8;
const ENV_IMAGE_DECODE_THREADS: &str = "MF_IMAGE_DECODE_THREADS";

/// Whether the loader workers wake the GUI thread themselves (`WakingSender`). Off on wasm: the web
/// build is inactive (`STATE.md` S-004) and a `Context::request_repaint` from a Web Worker was never
/// verified there, so on wasm the GUI keeps polling every frame while loads run, as it always did
/// (`PageResidency::poll_decoded`, `PageResidency::background_loads_need_polling`).
pub(super) const WORKERS_WAKE_GUI: bool = !cfg!(target_arch = "wasm32");

/// The GUI-thread wake-up shared by every loader pool of one `PageResidency`. Empty until the GUI
/// arms it with its `egui::Context` on its first poll (`PageResidency` is built without one); a
/// result sent before that is still seen by that first poll, which arms BEFORE it drains.
#[derive(Clone, Default)]
pub(super) struct GuiWaker(Arc<OnceLock<egui::Context>>);

impl GuiWaker {
    /// Stores `ctx` for the workers on the first call; later calls are no-ops. Never arms when
    /// [`WORKERS_WAKE_GUI`] is off, so no worker touches the context there.
    pub(super) fn arm(&self, ctx: &egui::Context) {
        if WORKERS_WAKE_GUI {
            self.0.get_or_init(|| ctx.clone());
        }
    }

    /// Requests a GUI repaint when armed (callable from any thread; eframe wakes the event loop).
    fn wake(&self) {
        if let Some(ctx) = self.0.get() {
            ctx.request_repaint();
        }
    }
}

/// A loader channel sender that wakes the GUI after every successful send AND when dropped. The
/// drop wake comes after the inner sender is dropped, so the frame it causes sees the disconnect of
/// a pool that died without `Finished` (otherwise nothing would wake the GUI to notice it).
pub(super) struct WakingSender<T> {
    /// `Some` for the whole life of the value; taken only in `drop` (drop order: sender, then wake).
    tx: Option<SyncSender<T>>,
    waker: GuiWaker,
}

impl<T> WakingSender<T> {
    pub(super) fn new(tx: SyncSender<T>, waker: GuiWaker) -> Self {
        Self { tx: Some(tx), waker }
    }

    /// Sends `event` (blocking while the bounded channel is full) and wakes the GUI to drain it.
    ///
    /// # Errors
    /// The GUI side dropped its receiver (the app was torn down); the event is handed back.
    pub(super) fn send(&self, event: T) -> Result<(), SendError<T>> {
        match &self.tx {
            Some(tx) => {
                tx.send(event)?;
                self.waker.wake();
                Ok(())
            }
            // Unreachable: `tx` is taken only in `drop`. Reported like a dropped receiver.
            None => Err(SendError(event)),
        }
    }
}

impl<T> Clone for WakingSender<T> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone(), waker: self.waker.clone() }
    }
}

impl<T> Drop for WakingSender<T> {
    fn drop(&mut self) {
        drop(self.tx.take());
        self.waker.wake();
    }
}

/// Source-page results of the unified pool, promoted strictly in page order on the GUI thread.
pub(super) enum LoaderEvent {
    Decoded(DecodedPage),
    Failed {
        idx: usize,
        path: String,
        error: String,
    },
    Finished,
}

/// Clean-overlay results of the unified pool, applied to `CleanOverlaysModel` as they arrive.
pub(super) enum OverlayLoaderEvent {
    Decoded {
        idx: usize,
        overlay: PreparedOverlay,
    },
    Failed {
        idx: usize,
        path: String,
        error: String,
    },
    Finished,
}

/// One unit of work for the unified loader pool (`spawn_loader_thread`): either a source-page
/// decode or that page's clean-overlay decode. Overlay results bypass in-order promotion and
/// are applied to `CleanOverlaysModel` as soon as they reach the GUI thread.
enum LoaderJob {
    SourcePage { idx: usize, path: PathBuf },
    CleanOverlay { idx: usize, path: PathBuf },
}

impl LoaderJob {
    fn page_idx(&self) -> usize {
        match self {
            LoaderJob::SourcePage { idx, .. } | LoaderJob::CleanOverlay { idx, .. } => *idx,
        }
    }

    /// Sort rank within one page index: the source decode must precede the same page's overlay
    /// decode so the job that advances the promotion frontier is always popped first (see the
    /// deadlock-freedom argument in `spawn_loader_thread`).
    fn kind_rank(&self) -> u8 {
        match self {
            LoaderJob::SourcePage { .. } => 0,
            LoaderJob::CleanOverlay { .. } => 1,
        }
    }
}

/// Results of the page-cache loader (`spawn_page_cache_loader_thread`).
pub(super) enum PageCacheLoaderEvent {
    Decoded {
        idx: usize,
        image: image::RgbaImage,
    },
    Failed {
        idx: usize,
        path: String,
        error: String,
    },
    Finished,
}

/// Spawns the unified source-page + clean-overlay decode pool.
///
/// Decodes `pages` (`(page_idx, source_path)`) and the loadable overlays among
/// `overlay_candidates` in a worker pool and streams each result to the GUI thread as a
/// `LoaderEvent` over `tx` / an `OverlayLoaderEvent` over `overlay_tx` (bounded channels, which
/// provide the GUI-side back-pressure; every send wakes the GUI). `cache_pages_immediately` requests seeding the
/// shared page RGBA cache during decode. `promotion_progress` is the GUI-updated frontier (next
/// page index awaiting promotion); workers park while a job is more than `DECODE_AHEAD_WINDOW`
/// pages ahead of it so decoded-but-unpromotable pages cannot pile up unbounded. The job queue is
/// popped in ascending page-index order, which keeps that look-ahead window deadlock-free.
/// `shutdown` releases parked workers once the GUI stops draining the channels.
///
/// # Errors
/// Returns the OS error when the pool thread itself could not be started: nothing runs and no
/// event is ever sent (both senders are dropped, so the receivers disconnect); the caller must
/// report every page as failed itself (`PageResidency::spawn`).
pub(super) fn spawn_loader_thread(
    pages: Vec<(usize, PathBuf)>,
    overlay_candidates: Vec<(usize, PageCleanPaths, PathBuf)>,
    tx: WakingSender<LoaderEvent>,
    overlay_tx: WakingSender<OverlayLoaderEvent>,
    cache_pages_immediately: bool,
    promotion_progress: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<()> {
    // The handle is dropped: the pool thread is detached and answers only through its channels.
    let _detached = thread::Builder::new().name("page-loader-pool".to_owned()).spawn(move || {
        // Overlay candidates are filtered for existence here, off the GUI thread, so opening a
        // project does not pay a per-page `stat` on slow filesystems. Pages without an overlay
        // file simply contribute no overlay job.
        let mut merged: Vec<LoaderJob> = pages
            .into_iter()
            .map(|(idx, path)| LoaderJob::SourcePage { idx, path })
            .chain(
                overlay_candidates
                    .into_iter()
                    .filter_map(|(idx, clean_paths, page_path)| {
                        loadable_clean_overlay(&clean_paths, &page_path)
                            .map(|path| LoaderJob::CleanOverlay { idx, path })
                    }),
            )
            .collect();
        if merged.is_empty() {
            // A send error means the GUI side dropped its receiver (the app was torn down):
            // there is nobody left to tell, so the error carries no information.
            let _ = tx.send(LoaderEvent::Finished);
            let _ = overlay_tx.send(OverlayLoaderEvent::Finished);
            return;
        }

        // Enforce ascending page-index order (with each page's source decode ahead of its
        // overlay decode) so the look-ahead back-pressure below stays deadlock-free
        // regardless of how `project.pages` was constructed: the source job that unblocks
        // promotion is always popped before any job that can park a worker.
        merged.sort_by_key(|job| (job.page_idx(), job.kind_rank()));
        debug_assert!(
            merged
                .windows(2)
                .all(|w| (w[0].page_idx(), w[0].kind_rank()) <= (w[1].page_idx(), w[1].kind_rank())),
            "loader jobs must be in ascending (page index, source-before-overlay) order"
        );

        let worker_count = decode_worker_count(merged.len());
        let jobs = Arc::new(Mutex::new(VecDeque::from(merged)));

        let make_worker = || {
            let tx = tx.clone();
            let overlay_tx = overlay_tx.clone();
            let jobs = Arc::clone(&jobs);
            let promotion_progress = Arc::clone(&promotion_progress);
            let shutdown = Arc::clone(&shutdown);
            move || {
                loop {
                    let job = {
                        let mut queue =
                            jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        queue.pop_front()
                    };

                    let Some(job) = job else {
                        return;
                    };

                    // Look-ahead throttle: do not decode jobs for pages more than
                    // DECODE_AHEAD_WINDOW beyond the next SOURCE page awaiting promotion.
                    // Source jobs park so decoded-but-unpromotable pages cannot pile up
                    // unbounded in the GUI-side pending map; overlay jobs park on the same
                    // SOURCE frontier so decoded overlays stay roughly in step with their
                    // source pages. Deadlock-freedom: the queue is sorted by (page index,
                    // source-before-overlay) before this loop, so jobs are popped in that
                    // order and the frontier's source job — always inside the window —
                    // precedes every job that can park. It was therefore popped before any
                    // worker parked, and it either completes or fails, letting the GUI
                    // advance the frontier (every send wakes the GUI and the GUI drains both
                    // event channels every frame, so a worker blocked on `send` also makes
                    // progress). The frontier thus
                    // keeps advancing and every parked worker eventually re-enters the
                    // window; overlay jobs never gate promotion themselves. The `shutdown`
                    // check is the app-exit escape hatch: once the GUI stops draining the
                    // channels (window closed, e.g. return-to-launcher mid-load) the
                    // frontier freezes forever, and without it a parked worker — which
                    // never touches a channel — would spin here for the rest of the
                    // process, keeping the pool joiner blocked with it.
                    loop {
                        if shutdown.load(AtomicOrdering::Acquire) {
                            return;
                        }
                        let progress = promotion_progress.load(AtomicOrdering::Acquire);
                        if decode_idx_within_window(job.page_idx(), progress) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }

                    match job {
                        LoaderJob::SourcePage { idx, path } => {
                            let event = match decode_page(idx, &path, cache_pages_immediately) {
                                Ok(page) => LoaderEvent::Decoded(page),
                                Err(error) => LoaderEvent::Failed {
                                    idx,
                                    path: path.display().to_string(),
                                    error,
                                },
                            };
                            if tx.send(event).is_err() {
                                return;
                            }
                        }
                        LoaderJob::CleanOverlay { idx, path } => {
                            let event = match decode_overlay(idx, &path) {
                                Ok(overlay) => OverlayLoaderEvent::Decoded { idx, overlay },
                                Err(error) => OverlayLoaderEvent::Failed {
                                    idx,
                                    path: path.display().to_string(),
                                    error,
                                },
                            };
                            if overlay_tx.send(event).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        };
        run_decode_pool("source/overlay", worker_count, make_worker, spawn_decode_worker);

        // Both `Finished` events are deliberately sent only after the whole pool drains, so
        // `loader_finished` and `overlay_loader_finished` each imply both job kinds are done.
        // A send error means the GUI side dropped its receiver (the app was torn down): there
        // is nobody left to tell, so the error carries no information.
        let _ = tx.send(LoaderEvent::Finished);
        let _ = overlay_tx.send(OverlayLoaderEvent::Finished);
    })?;
    Ok(())
}

/// Starts one decode worker thread (the production `spawn` of [`run_decode_pool`]).
fn spawn_decode_worker<W: FnOnce() + Send + 'static>(work: W) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name("page-decode-worker".to_owned()).spawn(work)
}

/// Runs one decode pool (`pool` names it in the log): starts up to `worker_count` workers, each
/// built by `make_worker` and started through `spawn`, then joins them (`join_decode_workers`).
///
/// The workers share one job queue, so any number of them drains it. When `spawn` refuses a
/// worker (OS thread limit, low memory) the refusal is logged and no further start is tried; when
/// it refused the very first one, one worker runs inline on the calling joiner thread instead, so
/// every job is still decoded and the pool's `Finished` still follows. Returns once every worker
/// (inline or spawned) returned.
fn run_decode_pool<W, M, S>(pool: &str, worker_count: usize, mut make_worker: M, mut spawn: S)
where
    W: FnOnce() + Send + 'static,
    M: FnMut() -> W,
    S: FnMut(W) -> std::io::Result<JoinHandle<()>>,
{
    let mut workers = Vec::with_capacity(worker_count);
    for started in 0..worker_count {
        match spawn(make_worker()) {
            Ok(worker) => workers.push(worker),
            Err(err) => {
                runtime_log::log_error(format!(
                    "[loader] could not start a {pool} decode worker thread ({started} of {worker_count} started); error={err}. Possible cause: the OS thread limit or low memory; the pool continues with the workers it has"
                ));
                break;
            }
        }
    }
    if workers.is_empty() {
        runtime_log::log_warn(format!(
            "[loader] no {pool} decode worker thread could be started; decoding on the pool thread itself"
        ));
        make_worker()();
    }
    join_decode_workers(workers, pool);
}

/// Joins every worker of one decode pool (`pool` names it in the log). A panicked worker is
/// logged with its panic message; the joiner carries on so the pool still reports `Finished`
/// (the jobs that worker held are simply never reported, as before).
fn join_decode_workers(workers: Vec<JoinHandle<()>>, pool: &str) {
    for worker in workers {
        if let Err(payload) = worker.join() {
            let message = payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            runtime_log::log_error(format!(
                "[loader] a {pool} decode worker panicked: {message}. Possible cause: a decoder bug on one page image; the pages that worker held stay unloaded"
            ));
        }
    }
}

/// Resolves one page's overlay candidate on the loader thread: the canonical clean the loader
/// should decode, or `None`. The binding rule is the owner's (`clean_assign::probe_page_clean` in
/// `StagedOverCommitted` scope): the staged `_unsaved/clean_layers/<stem>.png` shadows the
/// committed one, exactly as the save-merge will, so a resumed session (launcher "restore", or the
/// reload after a structural page op, which flushes dirty клин pages into staging first) shows the
/// clean edits a save would commit. A missing file or an exact-size mismatch of the EFFECTIVE file
/// yields `None` (the mismatch is warned about); an unreadable clean or page still yields the file,
/// so decode reports its own detailed failure and an unreadable page does not hide an otherwise
/// decodable overlay. A page loaded from staging arrives through `load_prepared_overlay` and is
/// therefore NOT dirty: it is already persisted in staging, so the autosave does not rewrite it
/// and the save-merge still commits it.
///
/// Warning dedup needs no retained state: each candidate is examined exactly once per
/// loader spawn (the single filtering pass in `spawn_loader_thread`), so the warning is
/// naturally scoped to one project load instead of accumulating paths for the whole
/// process lifetime.
fn loadable_clean_overlay(clean_paths: &PageCleanPaths, page_path: &Path) -> Option<PathBuf> {
    let resolution = probe_page_clean(clean_paths, page_path, LOADER_CLEAN_SCOPE);
    if let PageCleanResolution::SizeMismatch { file, clean, page } = &resolution {
        runtime_log::log_warn(format!(
            "[overlay-loader] skipping '{}' size {}x{}: source page '{}' size is {}x{}",
            file.display(), clean[0], clean[1], page_path.display(), page[0], page[1]
        ));
    }
    resolution.loadable_file().map(Path::to_path_buf)
}

/// Whether the initial source decode also seeds the shared page RGBA cache: only when canvas
/// page caching is on AND the memory profile grants the source-page CPU cache a budget.
pub(super) fn should_seed_page_cache_on_initial_load(
    cache_pages_enabled: bool,
    memory_budget: crate::memory_manager::MemoryBudget,
) -> bool {
    cache_pages_enabled && memory_budget.source_page_cpu_cache_bytes > 0
}

fn decode_worker_count(job_count: usize) -> usize {
    if job_count == 0 {
        return 1;
    }
    let logical = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let default_threads = logical.saturating_sub(1).max(1);
    let configured_threads = env::var(ENV_IMAGE_DECODE_THREADS)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|threads| *threads > 0);
    configured_threads
        .unwrap_or(default_threads)
        .clamp(1, job_count)
}

/// Returns true when page `idx` is close enough to the latest promoted page `progress`
/// to be decoded now, given the `DECODE_AHEAD_WINDOW` look-ahead bound.
///
/// `progress` is the lowest page index still awaiting in-order promotion. A page exactly at
/// `progress` (the one that unblocks promotion) is always inside the window, so the loader
/// pool always has at least one eligible job and cannot deadlock.
fn decode_idx_within_window(idx: usize, progress: usize) -> bool {
    idx < progress.saturating_add(DECODE_AHEAD_WINDOW)
}

/// Candidate clean-overlay paths (the files the clean model writes, `PageCleanPaths::for_writer`)
/// and their source pages, in page order. Pure path building; existence and header dimensions
/// are checked by `spawn_loader_thread`, off the GUI thread.
pub(super) fn collect_overlay_job_candidates(project: &ProjectData) -> Vec<(usize, PageCleanPaths, PathBuf)> {
    project
        .pages
        .iter()
        .map(|page| (page.idx, PageCleanPaths::for_writer(&project.paths, page), page.path.clone()))
        .collect()
}

/// The `(page_idx, path)` of every project page whose RGBA is not yet in the shared page cache
/// of `clean_overlays_model` (a poisoned model lock counts as "nothing cached").
pub(super) fn collect_missing_page_cache_jobs(
    project: &ProjectData,
    clean_overlays_model: &Arc<Mutex<CleanOverlaysModel>>,
) -> Vec<(usize, PathBuf)> {
    let cached_indexes = match clean_overlays_model.lock() {
        Ok(model) => project
            .pages
            .iter()
            .filter_map(|page| model.has_cached_page_rgba(page.idx).then_some(page.idx))
            .collect::<HashSet<_>>(),
        Err(err) => {
            runtime_log::log_error(format!(
                "[residency] CleanOverlaysModel lock poisoned while listing cached pages; treating none as cached; error={err}"
            ));
            HashSet::new()
        }
    };
    project
        .pages
        .iter()
        .filter(|page| !cached_indexes.contains(&page.idx))
        .map(|page| (page.idx, page.path.clone()))
        .collect()
}

/// Spawns the page-cache loader: decodes `jobs` (`(page_idx, path)`) into plain RGBA in a worker
/// pool and streams them over `tx` (each send wakes the GUI), then sends `Finished` once every
/// worker returned.
///
/// # Errors
/// Returns the OS error when the pool thread could not be started: nothing runs, `tx` is dropped
/// and the caller gives the optional page cache up (`PageResidency::ensure_page_cache_loader_started`).
pub(super) fn spawn_page_cache_loader_thread(
    jobs: Vec<(usize, PathBuf)>,
    tx: WakingSender<PageCacheLoaderEvent>,
) -> std::io::Result<()> {
    // The handle is dropped: the pool thread is detached and answers only through `tx`.
    let _detached = thread::Builder::new().name("page-cache-loader-pool".to_owned()).spawn(move || {
        if jobs.is_empty() {
            // A send error means the GUI side dropped its receiver (the app was torn down):
            // there is nobody left to tell, so the error carries no information.
            let _ = tx.send(PageCacheLoaderEvent::Finished);
            return;
        }

        let worker_count = decode_worker_count(jobs.len());
        let queue = Arc::new(Mutex::new(VecDeque::from(jobs)));

        let make_worker = || {
            let tx = tx.clone();
            let queue = Arc::clone(&queue);
            move || {
                loop {
                    let job = {
                        let mut q = queue.lock().unwrap_or_else(|p| p.into_inner());
                        q.pop_front()
                    };
                    let Some((idx, path)) = job else {
                        return;
                    };
                    let event = match decode_page_rgba(idx, &path) {
                        Ok(image) => PageCacheLoaderEvent::Decoded { idx, image },
                        Err(error) => PageCacheLoaderEvent::Failed {
                            idx,
                            path: path.display().to_string(),
                            error,
                        },
                    };
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            }
        };
        run_decode_pool("page-cache", worker_count, make_worker, spawn_decode_worker);
        // A send error means the GUI side dropped its receiver (the app was torn down): there is
        // nobody left to tell, so the error carries no information.
        let _ = tx.send(PageCacheLoaderEvent::Finished);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DECODE_AHEAD_WINDOW, GuiWaker, WORKERS_WAKE_GUI, WakingSender, decode_idx_within_window, run_decode_pool, should_seed_page_cache_on_initial_load, spawn_decode_worker};
    use crate::memory_manager::{MemoryBudget, MemoryProfile};
    use eframe::egui;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{TryRecvError, sync_channel};

    /// A fresh context and the number of repaint requests it forwarded to the integration. egui
    /// forwards only a request that shortens the pending delay, i.e. the FIRST one before a pass
    /// (fork `crates/egui/crates/egui/src/context.rs:156-170`), so each test checks one wake on a
    /// fresh context. `has_requested_repaint` cannot be used: a fresh viewport starts with one
    /// outstanding repaint (`ViewportRepaintInfo::default`, `context.rs:346-356`).
    fn counting_context() -> (egui::Context, Arc<AtomicUsize>) {
        let ctx = egui::Context::default();
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        ctx.set_request_repaint_callback(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        (ctx, wakes)
    }

    /// An unarmed waker wakes nobody; once armed, a successful send wakes the GUI.
    #[test]
    fn a_waking_sender_wakes_the_gui_after_a_send() -> Result<(), Box<dyn std::error::Error>> {
        let (ctx, wakes) = counting_context();
        let waker = GuiWaker::default();
        let (tx, rx) = sync_channel(4);
        let sender = WakingSender::new(tx, waker.clone());
        sender.send(1_u8)?;
        assert_eq!(wakes.load(Ordering::SeqCst), 0, "not armed yet");
        waker.arm(&ctx);
        sender.send(2)?;
        assert_eq!(wakes.load(Ordering::SeqCst), usize::from(WORKERS_WAKE_GUI));
        assert_eq!(rx.try_recv()?, 1);
        assert_eq!(rx.try_recv()?, 2);
        Ok(())
    }

    /// Dropping the last sender wakes the GUI, and by then the channel is already disconnected, so
    /// the frame it causes sees the end of a pool that died without `Finished`.
    #[test]
    fn dropping_a_waking_sender_wakes_the_gui_after_the_disconnect() {
        let (ctx, wakes) = counting_context();
        let waker = GuiWaker::default();
        waker.arm(&ctx);
        let (tx, rx) = sync_channel::<u8>(1);
        drop(WakingSender::new(tx, waker));
        assert_eq!(wakes.load(Ordering::SeqCst), usize::from(WORKERS_WAKE_GUI));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    }

    /// A send to a dropped receiver hands the event back and wakes nobody (the drop wake still
    /// fires afterwards: it does not know whether anyone listens).
    #[test]
    fn a_failed_send_does_not_wake_the_gui() {
        let (ctx, wakes) = counting_context();
        let waker = GuiWaker::default();
        waker.arm(&ctx);
        let (tx, rx) = sync_channel::<u8>(1);
        drop(rx);
        let sender = WakingSender::new(tx, waker);
        assert!(sender.send(7).is_err());
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
        drop(sender);
        assert_eq!(wakes.load(Ordering::SeqCst), usize::from(WORKERS_WAKE_GUI));
    }

    /// A worker that only counts how many times it ran.
    fn counting_worker(runs: &Arc<AtomicUsize>) -> impl FnOnce() + Send + 'static {
        let runs = Arc::clone(runs);
        move || {
            runs.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// When the OS refuses every decode worker, one worker runs inline on the pool thread, so the
    /// jobs are still decoded and `Finished` still follows.
    #[test]
    fn decode_pool_runs_inline_when_no_worker_starts() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut attempts = 0usize;
        run_decode_pool("test", 4, || counting_worker(&runs), |_work| {
            attempts += 1;
            Err(std::io::Error::other("thread limit"))
        });
        assert_eq!(attempts, 1, "a refused start stops further attempts");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "exactly one inline worker ran");
    }

    /// A refusal after some workers started keeps the pool on those workers (no inline run).
    #[test]
    fn decode_pool_keeps_the_workers_that_started() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut attempts = 0usize;
        run_decode_pool("test", 4, || counting_worker(&runs), |work| {
            attempts += 1;
            if attempts <= 2 { spawn_decode_worker(work) } else { Err(std::io::Error::other("thread limit")) }
        });
        assert_eq!(attempts, 3);
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the two started workers ran and were joined");
    }

    /// Without refusals every requested worker is started and joined.
    #[test]
    fn decode_pool_starts_every_worker() {
        let runs = Arc::new(AtomicUsize::new(0));
        run_decode_pool("test", 3, || counting_worker(&runs), spawn_decode_worker);
        assert_eq!(runs.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn initial_page_cache_seed_requires_enabled_cache_and_memory_budget() {
        assert!(!should_seed_page_cache_on_initial_load(
            true,
            MemoryBudget::for_profile(MemoryProfile::Minimal)
        ));
        assert!(!should_seed_page_cache_on_initial_load(
            false,
            MemoryBudget::for_profile(MemoryProfile::Medium)
        ));
        assert!(should_seed_page_cache_on_initial_load(
            true,
            MemoryBudget::for_profile(MemoryProfile::Medium)
        ));
    }

    #[test]
    fn decode_window_always_admits_the_page_that_unblocks_promotion() {
        // The page exactly at the promotion frontier must always be decodable, otherwise the
        // loader could deadlock waiting for a page no worker is allowed to decode. Page counts
        // are far from usize::MAX in practice, so the frontier is always < progress + window.
        for progress in [0usize, 1, 7, 42, 100_000] {
            assert!(decode_idx_within_window(progress, progress));
        }
    }

    #[test]
    fn decode_window_bounds_look_ahead() {
        let progress = 10usize;
        // Last in-window index and first out-of-window index relative to the frontier.
        assert!(decode_idx_within_window(
            progress + DECODE_AHEAD_WINDOW - 1,
            progress
        ));
        assert!(!decode_idx_within_window(
            progress + DECODE_AHEAD_WINDOW,
            progress
        ));
        assert!(!decode_idx_within_window(
            progress + DECODE_AHEAD_WINDOW + 5,
            progress
        ));
    }

    #[test]
    fn decode_window_does_not_overflow_near_usize_max() {
        // saturating_add keeps the bound well-defined (no panic) even when progress is at the
        // maximum; the comparison still resolves without overflowing.
        assert!(decode_idx_within_window(usize::MAX - 1, usize::MAX));
        assert!(!decode_idx_within_window(usize::MAX, usize::MAX));
    }

    /// The overlay loader resolves in the save-merge's scope: a staged clean shadows the committed
    /// one, and the size check applies to that effective file.
    #[test]
    fn overlay_loader_reads_staged_over_committed() -> Result<(), Box<dyn std::error::Error>> {
        use crate::models::clean_assign::PageCleanPaths;
        let temp = tempfile::tempdir()?;
        let committed_dir = temp.path().join("clean_layers");
        let staged_dir = temp.path().join("unsaved_clean_layers");
        std::fs::create_dir_all(&committed_dir)?;
        std::fs::create_dir_all(&staged_dir)?;
        let save = |path: &std::path::Path, size: [u32; 2]| image::RgbaImage::new(size[0], size[1]).save(path);
        let page_path = temp.path().join("000.png");
        save(&page_path, [10, 10])?;
        let clean_paths = PageCleanPaths { committed: committed_dir.join("000.png"), staged: staged_dir.join("000.png") };

        // Mismatched committed file alone: skipped.
        save(&clean_paths.committed, [12, 10])?;
        assert_eq!(super::loadable_clean_overlay(&clean_paths, &page_path), None);
        // A fitting staged twin shadows it and is loaded.
        save(&clean_paths.staged, [10, 10])?;
        assert_eq!(super::loadable_clean_overlay(&clean_paths, &page_path), Some(clean_paths.staged.clone()));
        // A mismatched staged twin shadows a fitting committed file: skipped, not a fallback.
        save(&clean_paths.committed, [10, 10])?;
        save(&clean_paths.staged, [12, 10])?;
        assert_eq!(super::loadable_clean_overlay(&clean_paths, &page_path), None);
        // Without staging the committed file is read.
        std::fs::remove_file(&clean_paths.staged)?;
        assert_eq!(super::loadable_clean_overlay(&clean_paths, &page_path), Some(clean_paths.committed.clone()));
        Ok(())
    }
}
