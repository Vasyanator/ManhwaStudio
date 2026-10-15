/*
File: crates/ms-launcher/src/new_project/quick_download/controller.rs

Purpose:
UI-facing controller and worker thread of the quick downloader: it starts one background
download per URL, streams progress back to the launcher window, and converts the decoded
images into ribbon pages.

Main responsibilities:
- own the worker channel and its cancel flag and expose a non-blocking `begin_download` /
  `poll` / `cancel` set (a new download, `cancel` and drop all stop the running worker);
- run URL normalization, plan building and image download off the GUI thread;
- download images in parallel while preserving the plan order;
- fetch each page through its candidate URLs with retry, hedged fallback and verification.

Key structures:
- QuickDownloadController
- QuickDownloadEvent
- QuickDownloadSuccess
- QuickDownloadAbort, ChainError, Cancelled - typed "cancelled vs failed" outcomes of a run

Key functions:
- spawn_quick_download()
- load_quick_download()
- download_images_ordered()
- cancellable_sleep() - the production backoff wait, interrupted by the run's cancel flag
- fetch_page_with_fallbacks() - per-page retry/fallback/hedging state machine
- PageFetcher - per-download shared state (injected fetch + sleep, referer, hedge delay)
- DeadHosts - per-download memory of hosts that failed whole pages, so later pages skip them

Notes:
Nothing here knows about a specific site; host handling lives in `plan.rs` and `sites/`.
The retry policy follows the project "Network downloads" rule. Each run owns one cancel flag,
checked before every page, before every attempt, inside the backoff wait and inside the hedge
waits; an HTTP request already in flight is not interrupted (`ureq` has no cancel) and ends
within the socket timeouts, its result dropped.
*/

use super::http::{FetchFailureKind, ImageFetchFailure, fetch_image_bytes, install_on_download_pool};
use super::plan::{
    PlannedImage, QuickDownloadError, SiteDownloadPlan, build_site_download_plan, digest_hex,
};
use super::url_util::{extract_host, normalize_http_url};
use crate::new_project::ribbon::{ImportedImage, RibbonPage, build_ribbon_pages};
use image::DynamicImage;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use ms_thread as thread;
use std::time::Duration;

/// Attempts on a URL that still has another candidate URL after it. Two, like the official
/// MangaDex reader on its primary node: a broken mirror should cost little before the page
/// moves on to the next origin.
const ATTEMPTS_BEFORE_FALLBACK: u32 = 2;
/// Attempts on a page's LAST candidate URL (the only one for most sites): nothing remains to
/// fall back to, so transient failures get one more try.
const ATTEMPTS_ON_LAST_URL: u32 = 3;
/// First backoff wait between transient retries of one URL; doubled per retry. The attempt
/// budgets above bound it: at most `ATTEMPTS_ON_LAST_URL - 1` retries, so the longest single
/// wait is 1 s and no separate cap is needed.
const RETRY_BACKOFF_BASE: Duration = Duration::from_millis(500);
/// How long an attempt on a URL that still has a later candidate may run before that
/// candidate is started in parallel (hedging). 5 s like the official MangaDex reader: a
/// healthy page answers well within it, while a mirror stuck in a TLS handshake would
/// otherwise hold the page for the whole 20 s socket timeout.
const HEDGE_DELAY: Duration = Duration::from_secs(5);
/// Pages that must have failed on a host (every attempt on its URL spent, any failure kind)
/// before the rest of the download skips that host for pages that still have another URL.
const DEAD_HOST_PAGE_THRESHOLD: usize = 3;
/// Longest slice a cancellable wait (backoff, hedge delay, waiting for hedge lanes) sleeps or
/// blocks before it looks at the cancel flag again: the bound on how late a cancel is noticed
/// by a waiting worker.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Marker error: the run's cancel flag was set, so the operation stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cancelled;

/// Why a download run produced no pages.
#[derive(Debug)]
enum QuickDownloadAbort {
    /// The controller cancelled the run (a newer download, `cancel`, or the controller being
    /// dropped). Not a failure: logged at info level and never shown to the user.
    Cancelled,
    /// The run failed; the error carries the user and log messages.
    Failed(QuickDownloadError),
}

impl From<QuickDownloadError> for QuickDownloadAbort {
    fn from(error: QuickDownloadError) -> Self {
        Self::Failed(error)
    }
}

impl From<Cancelled> for QuickDownloadAbort {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}

/// Why a walk over a page's candidate URLs produced no image.
#[derive(Debug)]
enum ChainError {
    /// The run was cancelled during the walk; nothing about the URLs is concluded.
    Cancelled,
    /// Every candidate failed or was skipped.
    Failed(ChainFailure),
}

impl From<Cancelled> for ChainError {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}

/// Production backoff wait of the quick downloader: sleeps `wait` in slices of at most
/// `CANCEL_POLL_INTERVAL`, checking `cancel` before each slice, so a cancel interrupts the wait
/// instead of being noticed only after it.
///
/// # Errors
/// Returns `Cancelled` as soon as `cancel` is seen set (also when it was set before the call).
fn cancellable_sleep(wait: Duration, cancel: &AtomicBool) -> Result<(), Cancelled> {
    let mut remaining = wait;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled);
        }
        if remaining.is_zero() {
            return Ok(());
        }
        let slice = remaining.min(CANCEL_POLL_INTERVAL);
        thread::sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
}

/// Per-download memory of failing hosts, shared by the parallel page workers of one
/// `download_images_ordered` run (never across downloads).
///
/// A host is "dead" once `DEAD_HOST_PAGE_THRESHOLD` pages failed on it; later pages skip its
/// URLs while they still have another candidate, so a black-holed mirror costs a few pages'
/// timeouts instead of every page's. The lock is held only to read or bump a counter.
#[derive(Debug, Default)]
struct DeadHosts {
    failed_pages: Mutex<HashMap<String, usize>>,
}

impl DeadHosts {
    /// Whether `host` has reached the dead threshold in this download.
    fn is_dead(&self, host: &str) -> bool {
        self.counters()
            .get(host)
            .is_some_and(|&count| count >= DEAD_HOST_PAGE_THRESHOLD)
    }

    /// Records that one page spent all its attempts on `host`; returns `true` exactly once per
    /// host, on the failure that makes it dead (the caller logs that transition).
    fn record_page_failure(&self, host: &str) -> bool {
        let mut counters = self.counters();
        let count = counters.entry(host.to_string()).or_insert(0);
        *count += 1;
        *count == DEAD_HOST_PAGE_THRESHOLD
    }

    /// Locks the counters. A poisoned lock (a worker panicked) is recovered: every critical
    /// section is a single map read or increment, so the map is always consistent.
    fn counters(&self) -> std::sync::MutexGuard<'_, HashMap<String, usize>> {
        self.failed_pages.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Handle of the single in-flight download: the receiving end of the worker channel and the
/// run's cancel flag (shared with the worker, its page tasks and their hedge threads).
#[derive(Debug)]
struct PendingQuickDownload {
    rx: Receiver<QuickDownloadWorkerEvent>,
    cancel: Arc<AtomicBool>,
}

/// Launcher-side state of the quick downloader. Holds at most one running download and
/// never blocks the GUI thread.
///
/// Every channel belongs to one run, so events of a cancelled or replaced run can never reach
/// a later run's receiver. Starting a new download, `cancel`, and dropping the controller set
/// the running worker's cancel flag; that worker then stops at its next cancel check and
/// delivers nothing.
pub struct QuickDownloadController {
    pending: Option<PendingQuickDownload>,
}

/// Result of a finished download: the normalized source URL and the built ribbon pages.
pub struct QuickDownloadSuccess {
    pub source_url: String,
    pub pages: Vec<RibbonPage>,
    pub downloaded_images: usize,
}

/// UI-facing event drained by `QuickDownloadController::poll`.
pub enum QuickDownloadEvent {
    Progress {
        stage: String,
        current: usize,
        total: usize,
    },
    Loaded(QuickDownloadSuccess),
    Failed {
        user_message: String,
        log_message: String,
    },
    WorkerDisconnected,
}

/// Internal worker-to-UI message; converted into `QuickDownloadEvent` while polling.
enum QuickDownloadWorkerEvent {
    Progress {
        stage: &'static str,
        current: usize,
        total: usize,
    },
    Finished(Result<LoadedQuickDownload, QuickDownloadError>),
}

/// Worker-side payload of a successful download, mirrored into `QuickDownloadSuccess`.
struct LoadedQuickDownload {
    source_url: String,
    pages: Vec<RibbonPage>,
    downloaded_images: usize,
}

impl QuickDownloadController {
    /// Creates an idle controller with no pending download.
    pub fn new() -> Self {
        Self { pending: None }
    }

    /// Returns `true` while a download worker is running.
    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    /// Starts a download for `url`. A download still running is cancelled first (its worker
    /// stops and its events are dropped with its channel).
    pub fn begin_download(&mut self, url: String) {
        let label = url.clone();
        self.begin_with(label, move |progress_tx, cancel| load_quick_download(&url, progress_tx, cancel));
    }

    /// Cancels the running download, if any, and forgets it; returns `true` when one was
    /// running. The worker notices the flag within `CANCEL_POLL_INTERVAL` while waiting, or
    /// once its in-flight requests end (bounded by the socket timeouts); it delivers no event.
    pub fn cancel(&mut self) -> bool {
        let Some(pending) = self.pending.take() else {
            return false;
        };
        pending.cancel.store(true, Ordering::Relaxed);
        ms_log::runtime_log::log_info("[new-project] quick download cancel requested");
        true
    }

    /// Cancels any running download, then starts `job` on a new worker with a fresh channel
    /// and cancel flag. `label` names the run in logs (the URL in production). Production
    /// passes `load_quick_download`; tests pass scripted jobs.
    fn begin_with<J>(&mut self, label: String, job: J)
    where
        J: FnOnce(&Sender<QuickDownloadWorkerEvent>, &Arc<AtomicBool>) -> Result<LoadedQuickDownload, QuickDownloadAbort>
            + Send
            + 'static,
    {
        self.cancel();
        let cancel = Arc::new(AtomicBool::new(false));
        let rx = spawn_quick_download(label, Arc::clone(&cancel), job);
        self.pending = Some(PendingQuickDownload { rx, cancel });
    }

    /// Drains the worker channel without blocking; returns the terminal event, or the
    /// last progress update seen in this frame, or `None` when nothing changed.
    pub fn poll(&mut self, ctx: &egui::Context) -> Option<QuickDownloadEvent> {
        let pending = self.pending.take()?;
        let mut last_progress = None;
        loop {
            match pending.rx.try_recv() {
                Ok(QuickDownloadWorkerEvent::Progress {
                    stage,
                    current,
                    total,
                }) => {
                    ctx.request_repaint();
                    last_progress = Some(QuickDownloadEvent::Progress {
                        stage: stage.to_string(),
                        current,
                        total,
                    });
                }
                Ok(QuickDownloadWorkerEvent::Finished(result)) => match result {
                    Ok(success) => {
                        ctx.request_repaint();
                        return Some(QuickDownloadEvent::Loaded(QuickDownloadSuccess {
                            source_url: success.source_url,
                            pages: success.pages,
                            downloaded_images: success.downloaded_images,
                        }));
                    }
                    Err(err) => {
                        return Some(QuickDownloadEvent::Failed {
                            user_message: err.user_message,
                            log_message: err.log_message,
                        });
                    }
                },
                Err(mpsc::TryRecvError::Empty) => {
                    self.pending = Some(pending);
                    return last_progress;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Some(QuickDownloadEvent::WorkerDisconnected);
                }
            }
        }
    }
}

impl Drop for QuickDownloadController {
    /// Dropping the controller (launcher teardown) stops its worker instead of letting it
    /// download into a channel nobody reads.
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Spawns the download worker thread running `job` and returns the receiving end of its event
/// channel. A spawn failure is reported through the same channel instead of panicking.
///
/// A run that ends `QuickDownloadAbort::Cancelled` is logged at info level and sends nothing
/// (its receiver is already gone); a result that cannot be delivered because the run was
/// cancelled meanwhile is logged at info level too, any other undeliverable result as a warning.
fn spawn_quick_download<J>(url: String, cancel: Arc<AtomicBool>, job: J) -> Receiver<QuickDownloadWorkerEvent>
where
    J: FnOnce(&Sender<QuickDownloadWorkerEvent>, &Arc<AtomicBool>) -> Result<LoadedQuickDownload, QuickDownloadAbort>
        + Send
        + 'static,
{
    let (tx, rx) = mpsc::channel();
    let tx_worker = tx.clone();
    let url_for_thread = url.clone();
    match thread::Builder::new()
        .name("new-project-quick-download".to_string())
        .spawn(move || {
            let result = match job(&tx_worker, &cancel) {
                Ok(loaded) => Ok(loaded),
                Err(QuickDownloadAbort::Failed(err)) => Err(err),
                Err(QuickDownloadAbort::Cancelled) => {
                    ms_log::runtime_log::log_info(format!(
                        "[new-project] quick download of '{url_for_thread}' cancelled; nothing downloaded is kept"
                    ));
                    return;
                }
            };
            if tx_worker
                .send(QuickDownloadWorkerEvent::Finished(result))
                .is_err()
            {
                if cancel.load(Ordering::Relaxed) {
                    ms_log::runtime_log::log_info(format!(
                        "[new-project] quick download of '{url_for_thread}' finished after it was cancelled; result discarded"
                    ));
                } else {
                    ms_log::runtime_log::log_warn(
                        "[new-project] failed to send quick download result to UI",
                    );
                }
            }
        }) {
        Ok(_) => {}
        Err(err) => {
            ms_log::runtime_log::log_error(format!(
                "[new-project] failed to spawn quick downloader for '{url}': {err}"
            ));
            if tx
                .send(QuickDownloadWorkerEvent::Finished(Err(
                    QuickDownloadError {
                        user_message: t!("launcher.new_project.quick_dl.start_error").to_string(),
                        log_message: format!("failed to spawn quick downloader for '{url}': {err}"),
                    },
                )))
                .is_err()
            {
                ms_log::runtime_log::log_warn(
                    "[new-project] failed to deliver quick downloader spawn error",
                );
            }
        }
    }
    rx
}

/// Worker body: normalizes the URL, resolves the site plan, downloads every image and
/// builds ribbon pages. `cancel` is checked after the plan was resolved, throughout the image
/// download, and before the ribbon is built.
///
/// # Errors
/// Returns `QuickDownloadAbort::Failed` for an invalid URL, an unsupported/failed site plan,
/// an empty image list, or any download/decode failure, and `QuickDownloadAbort::Cancelled`
/// when `cancel` was set (no partial pages are returned).
fn load_quick_download(
    url: &str,
    progress_tx: &Sender<QuickDownloadWorkerEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<LoadedQuickDownload, QuickDownloadAbort> {
    let normalized = normalize_http_url(url).map_err(|err| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.invalid_url_error").to_string(),
        log_message: format!("invalid quick download url '{url}': {err}"),
    })?;
    let plan = build_site_download_plan(&normalized)?;
    // Resolving the plan is network work of its own (site pages, APIs) that is not
    // cancellable from inside; a cancel that arrived meanwhile stops the run here.
    if cancel.load(Ordering::Relaxed) {
        return Err(QuickDownloadAbort::Cancelled);
    }
    if plan.images.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!("quick downloader found zero images for '{normalized}'"),
        }
        .into());
    }
    let images = download_images_ordered(&plan, progress_tx, fetch_image_bytes, cancellable_sleep, HEDGE_DELAY, cancel)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(QuickDownloadAbort::Cancelled);
    }
    let pages = build_ribbon_pages(images);
    Ok(LoadedQuickDownload {
        source_url: normalized,
        downloaded_images: pages.len(),
        pages,
    })
}

/// Downloads every image of `plan` in parallel and returns them in plan order.
///
/// The work runs on the downloader's own thread pool (`install_on_download_pool`), so the
/// concurrency is the network fan-out rather than the core count and the global rayon pool
/// stays free for compute work. Progress is streamed through `progress_tx` as images
/// complete, so the reported order is arbitrary while the returned vector is sorted back by
/// index.
///
/// Each page goes through `fetch_page_with_fallbacks` (retries, hedged fallback URLs, digest
/// check), so a page fails only after all of its candidate URLs did. All pages of this run
/// share one `PageFetcher`, and with it one `DeadHosts`. `fetch`, `sleep` and `hedge_delay`
/// are injected (production: `fetch_image_bytes`, `cancellable_sleep`, `HEDGE_DELAY`); `sleep`
/// gets the run's `cancel` flag and must return `Cancelled` once it sees it set. `cancel` is
/// checked before each page starts, before each attempt and during every wait.
///
/// # Errors
/// Returns `QuickDownloadAbort::Failed` with the error of the first page that failed on every
/// candidate URL, or a pool-creation failure, and `QuickDownloadAbort::Cancelled` when `cancel`
/// was set; no partial result is produced either way.
fn download_images_ordered<F, S>(
    plan: &SiteDownloadPlan,
    progress_tx: &Sender<QuickDownloadWorkerEvent>,
    fetch: F,
    sleep: S,
    hedge_delay: Duration,
    cancel: &Arc<AtomicBool>,
) -> Result<Vec<ImportedImage>, QuickDownloadAbort>
where
    F: Fn(&str, Option<&str>) -> Result<Vec<u8>, ImageFetchFailure> + Send + Sync + 'static,
    S: Fn(Duration, &AtomicBool) -> Result<(), Cancelled> + Send + Sync + 'static,
{
    let total = plan.images.len();
    let downloaded = Arc::new(AtomicUsize::new(0));
    let progress_tx = progress_tx.clone();
    let fetcher = Arc::new(PageFetcher {
        fetch,
        sleep,
        referer: plan.referer.clone(),
        dead_hosts: DeadHosts::default(),
        hedge_delay,
        cancel: Arc::clone(cancel),
    });

    // One image per rayon task (`with_max_len(1)`): the default chunking would hand several
    // URLs to one worker and leave the rest of the pool idle on a short chapter.
    let downloads = install_on_download_pool(|| {
        plan.images
            .par_iter()
            .enumerate()
            .with_max_len(1)
            .map(|(index, page)| {
                // Between pages: a cancelled run starts no further page (the collect below
                // stops at the first error, pages already running stop at their own checks).
                if fetcher.is_cancelled() {
                    return Err(QuickDownloadAbort::Cancelled);
                }
                let image = fetch_page_with_fallbacks(index, page, &fetcher)?;
                let current = downloaded.fetch_add(1, Ordering::Relaxed) + 1;
                let _ = progress_tx.send(QuickDownloadWorkerEvent::Progress {
                    stage: "download",
                    current,
                    total,
                });
                Ok::<(usize, ImportedImage), QuickDownloadAbort>((
                    index,
                    ImportedImage {
                        name: format!("{:04}.png", index + 1),
                        image,
                    },
                ))
            })
            .collect::<Result<Vec<_>, _>>()
    })?;

    let mut indexed = downloads?;
    indexed.sort_by_key(|(index, _)| *index);
    Ok(indexed.into_iter().map(|(_, image)| image).collect())
}

/// Wait before retry number `retry` (1-based) of the same URL: `RETRY_BACKOFF_BASE` doubled
/// per retry (saturating; the attempt budgets keep `retry` small).
fn retry_backoff(retry: u32) -> Duration {
    let factor = 2_u32.saturating_pow(retry.saturating_sub(1));
    RETRY_BACKOFF_BASE.saturating_mul(factor)
}

/// Everything one download run shares between its page workers and their hedge threads: the
/// injected fetch and sleep, the plan's `Referer`, the run's `DeadHosts`, the hedge delay and
/// the run's cancel flag. Held in an `Arc` because a hedged attempt may outlive the page that
/// started it.
struct PageFetcher<F, S> {
    fetch: F,
    sleep: S,
    referer: Option<String>,
    dead_hosts: DeadHosts,
    hedge_delay: Duration,
    cancel: Arc<AtomicBool>,
}

/// What a page's walk over (a suffix of) its candidate URLs tried before failing.
#[derive(Debug, Default)]
struct ChainFailure {
    /// One entry per attempt made: URL, attempt number, technical error.
    attempts: Vec<String>,
    /// URLs skipped without a request because their host is dead for this download.
    skipped: Vec<String>,
    /// The most recent failure; its user message becomes the page's.
    last_error: Option<QuickDownloadError>,
}

impl ChainFailure {
    /// Appends `other`'s attempts and skips; its last error wins if it has one.
    fn absorb(&mut self, other: Self) {
        self.attempts.extend(other.attempts);
        self.skipped.extend(other.skipped);
        if other.last_error.is_some() {
            self.last_error = other.last_error;
        }
    }
}

/// Result of one attempt that may have been hedged.
enum HedgedAttempt {
    /// The attempt finished within the hedge delay; the caller applies the usual
    /// retry/fallback rules to it.
    Finished(Result<DynamicImage, ImageFetchFailure>),
    /// The delay passed, so the rest of the chain raced the attempt; this is the race's
    /// outcome, which already covers every later candidate URL.
    Raced(Result<DynamicImage, ChainFailure>),
    /// The run was cancelled while waiting for the attempt or the race; the lanes are
    /// abandoned (they stop at their own cancel checks, results dropped).
    Cancelled,
}

/// A message to the page thread from one of the two racing lanes of a hedge.
enum LaneResult {
    /// The hedged attempt on the current URL.
    Current(Result<DynamicImage, ImageFetchFailure>),
    /// The walk over the remaining candidate URLs.
    Rest(Result<DynamicImage, ChainError>),
}

impl<F, S> PageFetcher<F, S>
where
    F: Fn(&str, Option<&str>) -> Result<Vec<u8>, ImageFetchFailure> + Send + Sync + 'static,
    S: Fn(Duration, &AtomicBool) -> Result<(), Cancelled> + Send + Sync + 'static,
{
    /// One request to `url`, then the digest check and decode. A wrong digest or undecodable
    /// bytes are what this URL serves, not network weather, so they are `Permanent`.
    fn attempt(&self, url: &str, sha256: Option<&[u8; 32]>) -> Result<DynamicImage, ImageFetchFailure> {
        let bytes = (self.fetch)(url, self.referer.as_deref())?;
        verify_and_decode(url, &bytes, sha256).map_err(|error| ImageFetchFailure {
            kind: FetchFailureKind::Permanent,
            error,
        })
    }

    /// Whether the run's cancel flag is set.
    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Counts one page whose attempts on `url`'s host are spent, logging the dead transition.
    fn record_host_failure(&self, url: &str) {
        let host = extract_host(url).unwrap_or_default();
        if self.dead_hosts.record_page_failure(&host) {
            ms_log::runtime_log::log_warn(format!(
                "[new-project] quick download: host '{host}' failed {DEAD_HOST_PAGE_THRESHOLD} pages, later pages skip it while they have another source"
            ));
        }
    }

    /// Walks `urls` (a page's candidates, or a suffix of them) in order with the retry,
    /// fallback, dead-host and hedging rules described on `fetch_page_with_fallbacks`, checking
    /// the cancel flag before every attempt and inside every wait.
    ///
    /// # Errors
    /// Returns `ChainError::Failed` with every attempt and skip when no URL produced a valid
    /// image, and `ChainError::Cancelled` as soon as the run is seen cancelled.
    fn fetch_chain(
        self: &Arc<Self>,
        page_number: usize,
        urls: &[String],
        sha256: Option<[u8; 32]>,
    ) -> Result<DynamicImage, ChainError> {
        let mut chain = ChainFailure::default();
        for (url_index, url) in urls.iter().enumerate() {
            let rest = &urls[url_index + 1..];
            let is_last_url = rest.is_empty();
            if !is_last_url && self.dead_hosts.is_dead(&extract_host(url).unwrap_or_default()) {
                ms_log::runtime_log::log_info(format!(
                    "[new-project] quick download page {page_number}: skipping '{url}', its host is marked dead for this download"
                ));
                chain.skipped.push(url.clone());
                continue;
            }
            let budget = if is_last_url {
                ATTEMPTS_ON_LAST_URL
            } else {
                ATTEMPTS_BEFORE_FALLBACK
            };
            for attempt in 1..=budget {
                // Between attempts (and URLs): a cancelled run sends no further request.
                if self.is_cancelled() {
                    return Err(ChainError::Cancelled);
                }
                let outcome = if is_last_url {
                    // Nothing left to hedge with: plain attempt on this thread.
                    self.attempt(url, sha256.as_ref())
                } else {
                    match self.hedged_attempt(page_number, url, attempt, rest, sha256) {
                        HedgedAttempt::Finished(outcome) => outcome,
                        HedgedAttempt::Raced(Ok(image)) => return Ok(image),
                        HedgedAttempt::Raced(Err(raced)) => {
                            chain.absorb(raced);
                            return Err(ChainError::Failed(chain));
                        }
                        HedgedAttempt::Cancelled => return Err(ChainError::Cancelled),
                    }
                };
                let failure = match outcome {
                    Ok(image) => {
                        if !chain.attempts.is_empty() {
                            ms_log::runtime_log::log_info(format!(
                                "[new-project] quick download page {page_number}: fetched from '{url}' after {} failed attempt(s)",
                                chain.attempts.len()
                            ));
                        }
                        return Ok(image);
                    }
                    Err(failure) => failure,
                };
                chain.attempts.push(format!("'{url}' attempt {attempt}: {}", failure.error.log_message));
                let retry = failure.kind == FetchFailureKind::Transient && attempt < budget;
                if retry {
                    let wait = retry_backoff(attempt);
                    ms_log::runtime_log::log_warn(format!(
                        "[new-project] quick download page {page_number}: attempt {attempt}/{budget} on '{url}' failed, retrying in {} ms: {}",
                        wait.as_millis(),
                        failure.error.log_message
                    ));
                    chain.last_error = Some(failure.error);
                    // The injected sleep must return `Cancelled` once it sees the flag, so a
                    // cancel interrupts the wait instead of being noticed after it.
                    (self.sleep)(wait, &self.cancel)?;
                    continue;
                }
                let next = if is_last_url { "no source left" } else { "falling back to the next source" };
                ms_log::runtime_log::log_warn(format!(
                    "[new-project] quick download page {page_number}: '{url}' failed after {attempt} attempt(s), {next}: {}",
                    failure.error.log_message
                ));
                chain.last_error = Some(failure.error);
                break;
            }
            // Reaching here means every attempt on this URL failed (success returned above).
            self.record_host_failure(url);
        }
        Err(ChainError::Failed(chain))
    }

    /// Waits up to `timeout` for a lane message, in slices of at most `CANCEL_POLL_INTERVAL`
    /// with a cancel check before each, so a waiting page notices a cancel promptly.
    ///
    /// # Errors
    /// Returns `Cancelled` when the run is seen cancelled before a message arrived.
    fn recv_lane_unless_cancelled(
        &self,
        lane_rx: &Receiver<LaneResult>,
        timeout: Duration,
    ) -> Result<Result<LaneResult, mpsc::RecvTimeoutError>, Cancelled> {
        let mut remaining = timeout;
        loop {
            if self.is_cancelled() {
                return Err(Cancelled);
            }
            let slice = remaining.min(CANCEL_POLL_INTERVAL);
            match lane_rx.recv_timeout(slice) {
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    remaining = remaining.saturating_sub(slice);
                    if remaining.is_zero() {
                        return Ok(Err(mpsc::RecvTimeoutError::Timeout));
                    }
                }
                other => return Ok(other),
            }
        }
    }

    /// Runs attempt `attempt` on `url` on its own thread and waits up to `hedge_delay` for it.
    /// If it has not finished by then, the walk over `rest` starts on a second thread and the
    /// first VALID image from either lane wins (the official MangaDex reader does the same
    /// with a 5 s delay). A hedged attempt gets no further retries on `url`. Both waits check
    /// the cancel flag (`recv_lane_unless_cancelled`); a cancel returns
    /// `HedgedAttempt::Cancelled` and abandons the lanes.
    ///
    /// The lanes run on dedicated short-lived threads, never on the download pool: the pool
    /// is fixed-size and its workers block right here waiting for the lanes, so lanes queued
    /// behind them could never run. A losing lane is not cancelled (`ureq` has no cancel); it
    /// finishes on its own within the socket timeouts and its result is dropped — a losing
    /// failure of the current URL still counts against its host in `DeadHosts`. If a thread
    /// cannot be spawned the attempt or walk runs inline on this thread instead, unhedged.
    fn hedged_attempt(
        self: &Arc<Self>,
        page_number: usize,
        url: &str,
        attempt: u32,
        rest: &[String],
        sha256: Option<[u8; 32]>,
    ) -> HedgedAttempt {
        let (lane_tx, lane_rx) = mpsc::channel::<LaneResult>();
        let current_tx = lane_tx.clone();
        let me = Arc::clone(self);
        let current_url = url.to_string();
        let spawned = thread::Builder::new()
            .name("quick-dl-attempt".to_string())
            .spawn(move || {
                let outcome = me.attempt(&current_url, sha256.as_ref());
                if let Err(mpsc::SendError(LaneResult::Current(Err(failure)))) =
                    current_tx.send(LaneResult::Current(outcome))
                {
                    // The page already returned with the other lane's image: this failure is
                    // seen by nobody else, so record and log it here.
                    ms_log::runtime_log::log_warn(format!(
                        "[new-project] quick download page {page_number}: abandoned attempt on '{current_url}' failed after the next source won: {}",
                        failure.error.log_message
                    ));
                    me.record_host_failure(&current_url);
                }
            });
        if let Err(err) = spawned {
            ms_log::runtime_log::log_warn(format!(
                "[new-project] quick download page {page_number}: could not start a hedged attempt thread, fetching '{url}' unhedged: {err}"
            ));
            return HedgedAttempt::Finished(self.attempt(url, sha256.as_ref()));
        }
        match self.recv_lane_unless_cancelled(&lane_rx, self.hedge_delay) {
            Err(Cancelled) => return HedgedAttempt::Cancelled,
            Ok(Ok(LaneResult::Current(outcome))) => return HedgedAttempt::Finished(outcome),
            // Only the current lane exists so far.
            Ok(Ok(LaneResult::Rest(_)) | Err(mpsc::RecvTimeoutError::Timeout)) => {}
            Ok(Err(mpsc::RecvTimeoutError::Disconnected)) => {
                return HedgedAttempt::Finished(Err(lost_lane_failure(url)));
            }
        }

        ms_log::runtime_log::log_info(format!(
            "[new-project] quick download page {page_number}: attempt {attempt} on '{url}' has not finished after {} ms, starting the next source in parallel",
            self.hedge_delay.as_millis()
        ));
        let me = Arc::clone(self);
        let rest_urls = rest.to_vec();
        let rest_tx = lane_tx;
        let spawned = thread::Builder::new()
            .name("quick-dl-hedge".to_string())
            .spawn(move || {
                let outcome = me.fetch_chain(page_number, &rest_urls, sha256);
                // A send error means the current lane already won; the result is not needed.
                if rest_tx.send(LaneResult::Rest(outcome)).is_err() {
                    ms_log::runtime_log::log_info(format!(
                        "[new-project] quick download page {page_number}: hedge result discarded, the first source won"
                    ));
                }
            });
        let mut rest_pending = true;
        if let Err(err) = spawned {
            ms_log::runtime_log::log_warn(format!(
                "[new-project] quick download page {page_number}: could not start the hedge thread, waiting for '{url}' only: {err}"
            ));
            rest_pending = false;
        }
        let mut current_pending = true;
        let mut raced = ChainFailure::default();
        while current_pending || rest_pending {
            // No overall deadline: each lane ends within the socket timeouts. The timeout only
            // bounds one slice between cancel checks, so `Timeout` just waits again.
            let message = match self.recv_lane_unless_cancelled(&lane_rx, CANCEL_POLL_INTERVAL) {
                Err(Cancelled) => return HedgedAttempt::Cancelled,
                Ok(Err(mpsc::RecvTimeoutError::Timeout)) => continue,
                Ok(Ok(message)) => Ok(message),
                Ok(Err(mpsc::RecvTimeoutError::Disconnected)) => Err(mpsc::RecvError),
            };
            match message {
                Ok(LaneResult::Current(Ok(image))) => {
                    ms_log::runtime_log::log_info(format!(
                        "[new-project] quick download page {page_number}: '{url}' answered first after hedging"
                    ));
                    return HedgedAttempt::Raced(Ok(image));
                }
                Ok(LaneResult::Rest(Ok(image))) => {
                    ms_log::runtime_log::log_info(format!(
                        "[new-project] quick download page {page_number}: the next source won the race against '{url}'"
                    ));
                    return HedgedAttempt::Raced(Ok(image));
                }
                Ok(LaneResult::Current(Err(failure))) => {
                    current_pending = false;
                    ms_log::runtime_log::log_warn(format!(
                        "[new-project] quick download page {page_number}: hedged attempt on '{url}' failed: {}",
                        failure.error.log_message
                    ));
                    raced.attempts.push(format!("'{url}' attempt {attempt}: {}", failure.error.log_message));
                    raced.last_error = Some(failure.error);
                    self.record_host_failure(url);
                }
                Ok(LaneResult::Rest(Err(ChainError::Failed(rest_failure)))) => {
                    rest_pending = false;
                    raced.absorb(rest_failure);
                }
                // The rest lane only stops this way when the run is cancelled.
                Ok(LaneResult::Rest(Err(ChainError::Cancelled))) => return HedgedAttempt::Cancelled,
                Err(mpsc::RecvError) => {
                    // Every sender is gone without a verdict: a lane thread panicked.
                    let lost = lost_lane_failure(url);
                    raced.attempts.push(lost.error.log_message.clone());
                    raced.last_error = Some(lost.error);
                    break;
                }
            }
        }
        HedgedAttempt::Raced(Err(raced))
    }
}

/// The failure reported when a lane thread ended without sending a result (it panicked).
fn lost_lane_failure(url: &str) -> ImageFetchFailure {
    ImageFetchFailure {
        kind: FetchFailureKind::Permanent,
        error: QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.download_page_error").to_string(),
            log_message: format!("download thread for '{url}' ended without a result"),
        },
    }
}

/// Downloads, verifies and decodes page `index` (0-based) of a plan, walking its candidate
/// URLs (`PlannedImage::candidate_urls`) in order.
///
/// Per URL: a `Transient` fetch failure is retried after `retry_backoff` until the URL's
/// budget is spent (`ATTEMPTS_BEFORE_FALLBACK`, or `ATTEMPTS_ON_LAST_URL` on the last URL); a
/// `Permanent` failure, a SHA-256 mismatch against `page.sha256`, or undecodable bytes moves
/// to the next URL at once. An attempt on a URL that still has a later candidate is hedged:
/// if it has not finished within `hedge_delay`, the later candidates start in parallel and
/// the first valid image wins (`PageFetcher::hedged_attempt`). A URL whose attempts are
/// spent counts one page failure for its host in `DeadHosts`; a URL on a dead host is skipped
/// without a request (so not hedged either), unless it is the page's LAST candidate. Every
/// retry, fallback, hedge, skip and newly dead host is logged with the page number and URL.
///
/// # Errors
/// Returns `QuickDownloadAbort::Cancelled` when the run's cancel flag was seen set (also when
/// the walk failed after a cancel: that failure may be the cancel's doing). Otherwise, when
/// every URL failed, `QuickDownloadAbort::Failed` with the LAST failure's user message and a
/// log message listing every attempt made and every URL skipped.
fn fetch_page_with_fallbacks<F, S>(
    index: usize,
    page: &PlannedImage,
    fetcher: &Arc<PageFetcher<F, S>>,
) -> Result<DynamicImage, QuickDownloadAbort>
where
    F: Fn(&str, Option<&str>) -> Result<Vec<u8>, ImageFetchFailure> + Send + Sync + 'static,
    S: Fn(Duration, &AtomicBool) -> Result<(), Cancelled> + Send + Sync + 'static,
{
    let page_number = index + 1;
    let urls = page.candidate_urls().map(str::to_string).collect::<Vec<_>>();
    let chain = match fetcher.fetch_chain(page_number, &urls, page.sha256) {
        Ok(image) => return Ok(image),
        Err(ChainError::Cancelled) => return Err(QuickDownloadAbort::Cancelled),
        Err(ChainError::Failed(chain)) => chain,
    };
    if fetcher.is_cancelled() {
        return Err(QuickDownloadAbort::Cancelled);
    }
    let user_message = match chain.last_error {
        Some(error) => error.user_message,
        // Unreachable in practice (the last URL is never skipped and every budget is >= 1),
        // but a missing error still yields a real, localized failure rather than a panic.
        None => t!("launcher.new_project.quick_dl.download_page_error").to_string(),
    };
    let skipped_note = if chain.skipped.is_empty() {
        String::new()
    } else {
        format!("; skipped (host marked dead): {}", chain.skipped.join(", "))
    };
    Err(QuickDownloadAbort::Failed(QuickDownloadError {
        user_message,
        log_message: format!(
            "page {page_number} failed on all {} source URL(s) after {} attempt(s): {}{skipped_note}",
            urls.len(),
            chain.attempts.len(),
            chain.attempts.join("; ")
        ),
    }))
}

/// Checks `bytes` against the expected SHA-256 (when the plan has one), then decodes them as an
/// image — from the bytes, never from the URL extension.
///
/// # Errors
/// Returns `QuickDownloadError` (`download_page_error`) on a digest mismatch, or
/// (`decode_image_error`) when the bytes are not a decodable image.
fn verify_and_decode(
    url: &str,
    bytes: &[u8],
    expected_sha256: Option<&[u8; 32]>,
) -> Result<DynamicImage, QuickDownloadError> {
    if let Some(expected) = expected_sha256 {
        let actual = Sha256::digest(bytes);
        if actual.as_slice() != expected.as_slice() {
            return Err(QuickDownloadError {
                user_message: t!("launcher.new_project.quick_dl.download_page_error").to_string(),
                log_message: format!(
                    "sha256 mismatch for '{url}': expected {}, got {} ({} bytes)",
                    digest_hex(expected),
                    digest_hex(actual.as_slice()),
                    bytes.len()
                ),
            });
        }
    }
    image::load_from_memory(bytes).map_err(|err| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.decode_image_error").to_string(),
        log_message: format!("failed to decode downloaded image '{url}': {err}"),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ATTEMPTS_ON_LAST_URL, CANCEL_POLL_INTERVAL, Cancelled, DEAD_HOST_PAGE_THRESHOLD, DeadHosts,
        FetchFailureKind, ImageFetchFailure, LoadedQuickDownload, PageFetcher, QuickDownloadAbort,
        QuickDownloadController, QuickDownloadError, QuickDownloadEvent, QuickDownloadWorkerEvent,
        RETRY_BACKOFF_BASE, cancellable_sleep, download_images_ordered, fetch_page_with_fallbacks,
        retry_backoff,
    };
    use crate::new_project::quick_download::plan::{PlannedImage, SiteDownloadPlan};
    use image::{DynamicImage, GenericImageView, ImageFormat};
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const PRIMARY: &str = "https://node.example/data/h/1.png";
    const FALLBACK: &str = "https://origin.example/data/h/1.png";
    /// A hedge delay no scripted answer ever reaches: the race never starts, and since the
    /// wait ends as soon as the attempt answers, nothing actually waits this long.
    const NO_HEDGE: Duration = Duration::from_secs(3600);

    /// PNG bytes of a `width`x1 image, so a decoded page can be told apart by its width.
    fn png(width: u32) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(width, 1)
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("encoding a tiny PNG into memory succeeds");
        bytes.into_inner()
    }

    fn failure(kind: FetchFailureKind, message: &str) -> ImageFetchFailure {
        ImageFetchFailure {
            kind,
            error: QuickDownloadError {
                user_message: format!("user: {message}"),
                log_message: message.to_string(),
            },
        }
    }

    /// The scripted fetcher and recording sleep as trait objects, so the fetcher type is nameable.
    type TestFetch = Box<dyn Fn(&str, Option<&str>) -> Result<Vec<u8>, ImageFetchFailure> + Send + Sync>;
    type TestSleep = Box<dyn Fn(Duration, &AtomicBool) -> Result<(), Cancelled> + Send + Sync>;

    /// The failure inside an abort; a cancel where none was requested fails the test.
    fn failed(abort: QuickDownloadAbort) -> QuickDownloadError {
        match abort {
            QuickDownloadAbort::Failed(error) => error,
            QuickDownloadAbort::Cancelled => panic!("the run was not cancelled, it must not report a cancel"),
        }
    }

    /// A cancel flag that is not set.
    fn not_cancelled() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    /// One scripted answer of the fake fetcher.
    enum Answer {
        /// Answer at once.
        Now(Result<Vec<u8>, ImageFetchFailure>),
        /// Block until the paired sender sends or is dropped, then answer: a hung connection.
        After(Receiver<()>, Result<Vec<u8>, ImageFetchFailure>),
        /// Release a hung answer (the paired `After`), then answer at once.
        Releasing(Sender<()>, Result<Vec<u8>, ImageFetchFailure>),
    }

    fn now(result: Result<Vec<u8>, ImageFetchFailure>) -> Answer {
        Answer::Now(result)
    }

    /// A scripted fetcher shared with the hedge threads: each URL answers with its queued
    /// answers in order and every call is recorded; an unknown or exhausted URL panics, so an
    /// unexpected request fails the test (on a lane thread, as a lost-lane failure).
    struct Script {
        answers: Mutex<HashMap<String, Vec<Answer>>>,
        calls: Mutex<Vec<String>>,
        sleeps: Mutex<Vec<Duration>>,
        /// The cancel flag of every fetcher built from this script.
        cancel: Arc<AtomicBool>,
    }

    impl Script {
        fn new(answers: Vec<(&str, Vec<Answer>)>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(
                    answers
                        .into_iter()
                        .map(|(url, mut queue)| {
                            queue.reverse();
                            (url.to_string(), queue)
                        })
                        .collect(),
                ),
                calls: Mutex::new(Vec::new()),
                sleeps: Mutex::new(Vec::new()),
                cancel: not_cancelled(),
            })
        }

        fn answer(&self, url: &str) -> Result<Vec<u8>, ImageFetchFailure> {
            self.calls.lock().expect("calls lock").push(url.to_string());
            let next = self
                .answers
                .lock()
                .expect("answers lock")
                .get_mut(url)
                .and_then(Vec::pop)
                .unwrap_or_else(|| panic!("unexpected fetch of '{url}'"));
            match next {
                Answer::Now(result) => result,
                Answer::After(release, result) => {
                    // Either a send or the sender being dropped releases the hang.
                    let _released = release.recv();
                    result
                }
                Answer::Releasing(release, result) => {
                    release.send(()).expect("the hung attempt is still waiting");
                    result
                }
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls lock").clone()
        }

        fn sleeps(&self) -> Vec<Duration> {
            self.sleeps.lock().expect("sleeps lock").clone()
        }

        /// A fetcher over this script with the plan referer the assertions expect.
        fn fetcher(
            self: &Arc<Self>,
            hedge_delay: Duration,
        ) -> Arc<PageFetcher<TestFetch, TestSleep>> {
            let for_fetch = Arc::clone(self);
            let for_sleep = Arc::clone(self);
            Arc::new(PageFetcher {
                fetch: Box::new(move |url: &str, referer: Option<&str>| {
                    assert_eq!(referer, Some("https://ref.example/"));
                    for_fetch.answer(url)
                }),
                sleep: Box::new(move |wait: Duration, _cancel: &AtomicBool| {
                    for_sleep.sleeps.lock().expect("sleeps lock").push(wait);
                    Ok(())
                }),
                referer: Some("https://ref.example/".to_string()),
                dead_hosts: DeadHosts::default(),
                hedge_delay,
                cancel: Arc::clone(&self.cancel),
            })
        }

        fn run(self: &Arc<Self>, page: &PlannedImage) -> Result<DynamicImage, QuickDownloadError> {
            fetch_page_with_fallbacks(0, page, &self.fetcher(NO_HEDGE)).map_err(failed)
        }
    }

    fn page_with_fallback(sha256: Option<[u8; 32]>) -> PlannedImage {
        PlannedImage {
            url: PRIMARY.to_string(),
            fallbacks: vec![FALLBACK.to_string()],
            sha256,
        }
    }

    #[test]
    fn backoff_doubles_from_the_base() {
        assert_eq!(retry_backoff(1), RETRY_BACKOFF_BASE);
        assert_eq!(retry_backoff(2), RETRY_BACKOFF_BASE * 2);
        assert_eq!(retry_backoff(3), RETRY_BACKOFF_BASE * 4);
        // The budgets allow at most `ATTEMPTS_ON_LAST_URL - 1` retries: the longest wait is 1 s.
        assert_eq!(retry_backoff(ATTEMPTS_ON_LAST_URL - 1), Duration::from_secs(1));
    }

    #[test]
    fn primary_404_moves_to_the_fallback_without_waiting() {
        let script = Script::new(vec![
            (PRIMARY, vec![now(Err(failure(FetchFailureKind::Permanent, "status 404")))]),
            (FALLBACK, vec![now(Ok(png(3)))]),
        ]);
        let image = script.run(&page_with_fallback(None)).expect("fallback serves the page");
        assert_eq!(image.dimensions(), (3, 1));
        assert_eq!(script.calls(), [PRIMARY, FALLBACK]);
        assert!(script.sleeps().is_empty());
    }

    #[test]
    fn transient_primary_failure_is_retried_after_a_backoff() {
        let script = Script::new(vec![(
            PRIMARY,
            vec![now(Err(failure(FetchFailureKind::Transient, "tls init failed"))), now(Ok(png(2)))],
        )]);
        let image = script.run(&page_with_fallback(None)).expect("retry succeeds");
        assert_eq!(image.dimensions(), (2, 1));
        assert_eq!(script.calls(), [PRIMARY, PRIMARY]);
        assert_eq!(script.sleeps(), [RETRY_BACKOFF_BASE]);
    }

    #[test]
    fn digest_mismatch_moves_to_the_fallback() {
        let good = png(5);
        let expected: [u8; 32] = Sha256::digest(&good).into();
        let script = Script::new(vec![(PRIMARY, vec![now(Ok(png(4)))]), (FALLBACK, vec![now(Ok(good))])]);
        let image = script.run(&page_with_fallback(Some(expected))).expect("fallback bytes match");
        assert_eq!(image.dimensions(), (5, 1));
        assert_eq!(script.calls(), [PRIMARY, FALLBACK]);
        assert!(script.sleeps().is_empty());
    }

    #[test]
    fn undecodable_bytes_move_to_the_fallback() {
        let script = Script::new(vec![
            (PRIMARY, vec![now(Ok(b"<html>not an image</html>".to_vec()))]),
            (FALLBACK, vec![now(Ok(png(6)))]),
        ]);
        let image = script.run(&page_with_fallback(None)).expect("fallback decodes");
        assert_eq!(image.dimensions(), (6, 1));
        assert_eq!(script.calls(), [PRIMARY, FALLBACK]);
    }

    #[test]
    fn every_source_failing_names_each_attempt_and_keeps_the_last_user_message() {
        let script = Script::new(vec![
            (
                PRIMARY,
                vec![
                    now(Err(failure(FetchFailureKind::Transient, "reset one"))),
                    now(Err(failure(FetchFailureKind::Transient, "reset two"))),
                ],
            ),
            (FALLBACK, vec![now(Err(failure(FetchFailureKind::Permanent, "status 404 on origin")))]),
        ]);
        let Err(err) = script.run(&page_with_fallback(None)) else {
            panic!("every source failed, the page must fail");
        };
        // The primary spends its pre-fallback budget of 2, the fallback stops at the 404.
        assert_eq!(script.calls(), [PRIMARY, PRIMARY, FALLBACK]);
        assert_eq!(script.sleeps(), [RETRY_BACKOFF_BASE]);
        assert_eq!(err.user_message, "user: status 404 on origin");
        for needle in ["page 1", "3 attempt(s)", "reset one", "reset two", "status 404 on origin", PRIMARY, FALLBACK] {
            assert!(err.log_message.contains(needle), "missing '{needle}' in {}", err.log_message);
        }
    }

    #[test]
    fn single_url_page_gets_the_last_url_budget() {
        let transient = || now(Err(failure(FetchFailureKind::Transient, "timeout")));
        let script = Script::new(vec![(PRIMARY, vec![transient(), transient(), transient()])]);
        let page = PlannedImage::single(PRIMARY.to_string());
        assert!(script.run(&page).is_err());
        assert_eq!(script.calls().len(), usize::try_from(ATTEMPTS_ON_LAST_URL).expect("small"));
        assert_eq!(script.sleeps(), [retry_backoff(1), retry_backoff(2)]);
    }

    #[test]
    fn hung_primary_is_hedged_and_the_fallback_wins() {
        let (release, hang) = mpsc::channel();
        let script = Script::new(vec![
            (PRIMARY, vec![Answer::After(hang, Err(failure(FetchFailureKind::Transient, "tls init failed")))]),
            (FALLBACK, vec![now(Ok(png(7)))]),
        ]);
        // A zero delay hedges at once; the primary is still blocked on `hang`.
        let fetcher = script.fetcher(Duration::ZERO);
        let image = fetch_page_with_fallbacks(0, &page_with_fallback(None), &fetcher).expect("fallback wins");
        assert_eq!(image.dimensions(), (7, 1));
        // With a zero delay the hedge may start before the primary's thread even reaches the
        // fetcher, so only the fallback call is certain here.
        assert!(script.calls().contains(&FALLBACK.to_string()), "{:?}", script.calls());
        assert!(script.sleeps().is_empty(), "the hedged primary is not retried");
        // Let the abandoned primary finish; its result is discarded.
        drop(release);
    }

    #[test]
    fn primary_answering_before_the_hedge_delay_never_starts_the_fallback() {
        let script = Script::new(vec![(PRIMARY, vec![now(Ok(png(8)))])]);
        let image = fetch_page_with_fallbacks(0, &page_with_fallback(None), &script.fetcher(NO_HEDGE))
            .expect("primary serves the page");
        assert_eq!(image.dimensions(), (8, 1));
        assert_eq!(script.calls(), [PRIMARY]);
    }

    #[test]
    fn hedged_race_lost_by_both_lanes_lists_both() {
        let (release, hang) = mpsc::channel();
        let script = Script::new(vec![
            (PRIMARY, vec![Answer::After(hang, Err(failure(FetchFailureKind::Transient, "tls init failed")))]),
            // The fallback fails, then releases the hung primary, which fails too.
            (
                FALLBACK,
                vec![Answer::Releasing(release, Err(failure(FetchFailureKind::Permanent, "status 404 on origin")))],
            ),
        ]);
        let fetcher = script.fetcher(Duration::ZERO);
        let Err(err) = fetch_page_with_fallbacks(0, &page_with_fallback(None), &fetcher).map_err(failed) else {
            panic!("both lanes failed, the page must fail");
        };
        // Both lanes ran concurrently, so their call order is not fixed.
        let mut calls = script.calls();
        calls.sort();
        assert_eq!(calls, [PRIMARY, FALLBACK]);
        for needle in ["tls init failed", "status 404 on origin", PRIMARY, FALLBACK, "2 attempt(s)"] {
            assert!(err.log_message.contains(needle), "missing '{needle}' in {}", err.log_message);
        }
        // Each URL's attempts are spent for this page: one failure per host.
        let counters = fetcher.dead_hosts.counters();
        assert_eq!(counters.get("node.example"), Some(&1));
        assert_eq!(counters.get("origin.example"), Some(&1));
    }

    fn node_url(page: usize) -> String {
        format!("https://node.example/data/h/{page}.png")
    }

    fn origin_url(page: usize) -> String {
        format!("https://origin.example/data/h/{page}.png")
    }

    #[test]
    fn host_failing_threshold_pages_is_skipped_by_later_pages() {
        let pages = DEAD_HOST_PAGE_THRESHOLD + 2;
        let node_urls = (1..=pages).map(node_url).collect::<Vec<_>>();
        let origin_urls = (1..=pages).map(origin_url).collect::<Vec<_>>();
        let mut answers = Vec::new();
        // Only the first THRESHOLD pages may hit the node (any failure kind counts); a fetch
        // of a later page's node URL is unscripted and would fail the page.
        for (number, url) in node_urls.iter().enumerate().take(DEAD_HOST_PAGE_THRESHOLD) {
            let answer = if number % 2 == 0 {
                vec![now(Err(failure(FetchFailureKind::Permanent, "status 404")))]
            } else {
                vec![
                    now(Err(failure(FetchFailureKind::Transient, "tls init failed"))),
                    now(Err(failure(FetchFailureKind::Transient, "tls init failed"))),
                ]
            };
            answers.push((url.as_str(), answer));
        }
        for url in &origin_urls {
            answers.push((url.as_str(), vec![now(Ok(png(1)))]));
        }
        let script = Script::new(answers);
        let fetcher = script.fetcher(NO_HEDGE);
        for index in 0..pages {
            let page = PlannedImage {
                url: node_urls[index].clone(),
                fallbacks: vec![origin_urls[index].clone()],
                sha256: None,
            };
            fetch_page_with_fallbacks(index, &page, &fetcher).expect("the origin serves every page");
        }
        let node_calls = script.calls().iter().filter(|url| url.contains("node.example")).count();
        assert_eq!(node_calls, 1 + 2 + 1, "404, two transient tries, 404 - then never again");
        assert!(fetcher.dead_hosts.is_dead("node.example"));
        assert!(!fetcher.dead_hosts.is_dead("origin.example"));
    }

    #[test]
    fn dead_host_is_still_tried_when_it_is_the_last_candidate() {
        let script = Script::new(vec![(PRIMARY, vec![now(Ok(png(2)))])]);
        let fetcher = script.fetcher(NO_HEDGE);
        for _ in 0..DEAD_HOST_PAGE_THRESHOLD {
            fetcher.dead_hosts.record_page_failure("node.example");
        }
        let image = fetch_page_with_fallbacks(7, &PlannedImage::single(PRIMARY.to_string()), &fetcher)
            .expect("a page with no alternative still tries the dead host");
        assert_eq!(image.dimensions(), (2, 1));
        assert_eq!(script.calls(), [PRIMARY]);
    }

    #[test]
    fn dead_host_transition_is_reported_once() {
        let dead_hosts = DeadHosts::default();
        let transitions = (0..DEAD_HOST_PAGE_THRESHOLD + 3)
            .filter(|_| dead_hosts.record_page_failure("node.example"))
            .count();
        assert_eq!(transitions, 1);
    }

    #[test]
    fn ordered_download_is_all_or_nothing() {
        let urls = (1..=5).map(|n| format!("https://cdn.example/{n}.png")).collect::<Vec<_>>();
        let plan = SiteDownloadPlan {
            images: PlannedImage::from_urls(urls),
            referer: None,
        };
        let fetch = |url: &str, _referer: Option<&str>| {
            if url.ends_with("/3.png") {
                Err(failure(FetchFailureKind::Permanent, "status 410 gone"))
            } else {
                Ok(png(1))
            }
        };
        let sleep = |_wait: Duration, _cancel: &AtomicBool| panic!("a permanent failure must not wait");
        let (tx, _rx) = mpsc::channel();
        let Err(err) = download_images_ordered(&plan, &tx, fetch, sleep, NO_HEDGE, &not_cancelled()).map_err(failed) else {
            panic!("one page failing on every URL must fail the chapter");
        };
        assert_eq!(err.user_message, "user: status 410 gone");
        assert!(err.log_message.starts_with("page 3 "), "{}", err.log_message);
    }

    #[test]
    fn ordered_download_returns_pages_in_plan_order_whatever_the_completion_order() {
        let urls = (1..=6).map(|n| format!("https://cdn.example/{n}.png")).collect::<Vec<_>>();
        let plan = SiteDownloadPlan {
            images: PlannedImage::from_urls(urls),
            referer: None,
        };
        // Earlier pages answer later, so completion order is roughly reversed.
        let fetch = |url: &str, _referer: Option<&str>| {
            let number: u32 = url
                .trim_start_matches("https://cdn.example/")
                .trim_end_matches(".png")
                .parse()
                .expect("test url carries its page number");
            std::thread::sleep(Duration::from_millis(u64::from(7 - number) * 15));
            Ok(png(number))
        };
        let sleep = |_wait: Duration, _cancel: &AtomicBool| panic!("no retry expected");
        let (tx, rx) = mpsc::channel();
        let images = download_images_ordered(&plan, &tx, fetch, sleep, NO_HEDGE, &not_cancelled()).expect("all pages load");
        let widths = images.iter().map(|page| page.image.width()).collect::<Vec<_>>();
        assert_eq!(widths, [1, 2, 3, 4, 5, 6]);
        let names = images.iter().map(|page| page.name.as_str()).collect::<Vec<_>>();
        assert_eq!(names, ["0001.png", "0002.png", "0003.png", "0004.png", "0005.png", "0006.png"]);
        drop(tx);
        assert_eq!(rx.iter().count(), 6, "one progress event per page");
    }

    /// Upper bound for "promptly" in the cancel tests: far below every wait being cancelled
    /// (60 s sleeps, the 3600 s hedge delay), far above `CANCEL_POLL_INTERVAL`.
    const PROMPT: Duration = Duration::from_secs(5);

    /// Sets `cancel` from another thread after `delay`.
    fn cancel_after(cancel: &Arc<AtomicBool>, delay: Duration) {
        let cancel = Arc::clone(cancel);
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            cancel.store(true, Ordering::Relaxed);
        });
    }

    #[test]
    fn cancellable_sleep_waits_out_an_uncancelled_wait() {
        let started = Instant::now();
        assert_eq!(cancellable_sleep(CANCEL_POLL_INTERVAL * 2, &AtomicBool::new(false)), Ok(()));
        assert!(started.elapsed() >= CANCEL_POLL_INTERVAL * 2);
        assert_eq!(cancellable_sleep(Duration::ZERO, &AtomicBool::new(true)), Err(Cancelled));
    }

    #[test]
    fn cancel_interrupts_a_running_backoff_sleep() {
        let cancel = not_cancelled();
        cancel_after(&cancel, Duration::from_millis(30));
        let started = Instant::now();
        assert_eq!(cancellable_sleep(Duration::from_secs(60), &cancel), Err(Cancelled));
        assert!(started.elapsed() < PROMPT, "took {:?}", started.elapsed());
    }

    #[test]
    fn cancel_during_backoff_returns_promptly_without_another_attempt() {
        let cancel = not_cancelled();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in_fetch = Arc::clone(&calls);
        let cancel_in_fetch = Arc::clone(&cancel);
        let fetcher = Arc::new(PageFetcher {
            fetch: move |_url: &str, _referer: Option<&str>| {
                calls_in_fetch.fetch_add(1, Ordering::Relaxed);
                // The cancel lands while the worker sits in the following backoff wait.
                cancel_after(&cancel_in_fetch, Duration::from_millis(20));
                Err(failure(FetchFailureKind::Transient, "connection reset"))
            },
            sleep: cancellable_sleep,
            referer: None,
            dead_hosts: DeadHosts::default(),
            hedge_delay: NO_HEDGE,
            cancel: Arc::clone(&cancel),
        });
        let started = Instant::now();
        let outcome = fetch_page_with_fallbacks(0, &PlannedImage::single(PRIMARY.to_string()), &fetcher);
        assert!(matches!(outcome, Err(QuickDownloadAbort::Cancelled)), "{outcome:?}");
        assert!(started.elapsed() < RETRY_BACKOFF_BASE, "the 500 ms backoff was waited out: {:?}", started.elapsed());
        assert_eq!(calls.load(Ordering::Relaxed), 1, "no attempt after the cancel");
        assert!(fetcher.dead_hosts.counters().is_empty(), "a cancel is not a host failure");
    }

    #[test]
    fn cancel_while_waiting_for_a_hung_attempt_returns_promptly() {
        let (release, hang) = mpsc::channel();
        let script = Script::new(vec![(
            PRIMARY,
            vec![Answer::After(hang, Err(failure(FetchFailureKind::Transient, "tls init failed")))],
        )]);
        cancel_after(&script.cancel, Duration::from_millis(30));
        let started = Instant::now();
        let outcome = fetch_page_with_fallbacks(0, &page_with_fallback(None), &script.fetcher(NO_HEDGE));
        assert!(matches!(outcome, Err(QuickDownloadAbort::Cancelled)), "{outcome:?}");
        assert!(started.elapsed() < PROMPT, "took {:?}", started.elapsed());
        assert!(!script.calls().contains(&FALLBACK.to_string()), "a cancelled page starts no hedge");
        // Let the abandoned attempt finish; its result is discarded.
        drop(release);
    }

    #[test]
    fn cancelled_run_starts_no_page() {
        let plan = SiteDownloadPlan {
            images: PlannedImage::from_urls(vec![PRIMARY.to_string()]),
            referer: None,
        };
        let fetch = |url: &str, _referer: Option<&str>| panic!("a cancelled run must not fetch '{url}'");
        let sleep = |_wait: Duration, _cancel: &AtomicBool| panic!("a cancelled run must not wait");
        let (tx, _rx) = mpsc::channel();
        let outcome = download_images_ordered(&plan, &tx, fetch, sleep, NO_HEDGE, &Arc::new(AtomicBool::new(true)));
        assert!(matches!(outcome, Err(QuickDownloadAbort::Cancelled)), "the run must end cancelled");
    }

    /// Polls `controller` until it returns a terminal event or `PROMPT` passes; returns every
    /// progress stage seen and the terminal event.
    fn poll_until_finished(controller: &mut QuickDownloadController) -> (Vec<String>, QuickDownloadEvent) {
        let ctx = egui::Context::default();
        let started = Instant::now();
        let mut stages = Vec::new();
        while started.elapsed() < PROMPT {
            match controller.poll(&ctx) {
                Some(QuickDownloadEvent::Progress { stage, .. }) => stages.push(stage),
                Some(terminal) => return (stages, terminal),
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        panic!("the download did not finish within {PROMPT:?}");
    }

    #[test]
    fn second_download_stops_the_first_and_never_sees_its_events() {
        let mut controller = QuickDownloadController::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (stopped_tx, stopped_rx) = mpsc::channel();
        controller.begin_with("old".to_string(), move |tx, cancel| {
            let plan = SiteDownloadPlan {
                images: PlannedImage::from_urls(vec![PRIMARY.to_string()]),
                referer: None,
            };
            let started_tx = Mutex::new(started_tx);
            // Always transient: without a cancel this page would wait 500 ms + 1 s in backoff.
            let fetch = move |_url: &str, _referer: Option<&str>| {
                started_tx.lock().expect("started lock").send(()).expect("the test waits for the start");
                Err(failure(FetchFailureKind::Transient, "connection reset"))
            };
            let result = download_images_ordered(&plan, tx, fetch, cancellable_sleep, NO_HEDGE, cancel);
            // An event of the old run after its cancel: its channel is gone, so it can reach
            // nobody, least of all the new run's receiver.
            let delivered = tx
                .send(QuickDownloadWorkerEvent::Progress { stage: "old-run", current: 1, total: 1 })
                .is_ok();
            stopped_tx
                .send((matches!(result, Err(QuickDownloadAbort::Cancelled)), delivered))
                .expect("the test waits for the stop");
            result.map(|images| LoadedQuickDownload {
                source_url: "old".to_string(),
                downloaded_images: images.len(),
                pages: Vec::new(),
            })
        });
        started_rx.recv_timeout(PROMPT).expect("the first download started");
        let replaced_at = Instant::now();
        controller.begin_with("new".to_string(), |tx, _cancel| {
            tx.send(QuickDownloadWorkerEvent::Progress { stage: "new-run", current: 0, total: 1 })
                .expect("the controller holds the new receiver");
            Ok(LoadedQuickDownload {
                source_url: "new".to_string(),
                downloaded_images: 0,
                pages: Vec::new(),
            })
        });

        let (was_cancelled, delivered) = stopped_rx.recv_timeout(PROMPT).expect("the first worker stopped");
        assert!(was_cancelled, "the first run ends cancelled, not failed");
        assert!(!delivered, "the first run's channel must be closed");
        assert!(replaced_at.elapsed() < RETRY_BACKOFF_BASE * 2, "the first worker sat out its backoff");

        let (stages, terminal) = poll_until_finished(&mut controller);
        assert!(stages.iter().all(|stage| stage != "old-run"), "{stages:?}");
        let QuickDownloadEvent::Loaded(success) = terminal else {
            panic!("the second download must load");
        };
        assert_eq!(success.source_url, "new");
        assert!(!controller.is_loading());
    }

    #[test]
    fn dropping_or_cancelling_the_controller_stops_its_worker() {
        for drop_it in [true, false] {
            let mut controller = QuickDownloadController::new();
            let (stopped_tx, stopped_rx) = mpsc::channel();
            controller.begin_with("held".to_string(), move |_tx, cancel| {
                let outcome = cancellable_sleep(Duration::from_secs(60), cancel);
                stopped_tx.send(outcome).expect("the test waits for the stop");
                Err(QuickDownloadAbort::Cancelled)
            });
            if drop_it {
                drop(controller);
            } else {
                assert!(controller.cancel());
                assert!(!controller.is_loading());
                assert!(!controller.cancel(), "nothing left to cancel");
            }
            let outcome = stopped_rx.recv_timeout(PROMPT).expect("the worker stopped promptly");
            assert_eq!(outcome, Err(Cancelled));
        }
    }
}
