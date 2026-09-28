/*
File: tab/export.rs

Purpose:
Export pipeline for the typing tab: rasterizing typed overlays onto page images and
writing the result, plus the async export job polling/request methods on
`TypingTextOverlayLayer`. Covers overlay compositing math local to export (bilinear quad
UV sampling, textured-triangle rasterization, source-over blending, clean-overlay snapshot
loading, mask sampling).

Two pipelines, chosen by `resolve_export_route` from the request's format + destination:
- ONE-TO-ONE (`Png`/`Psd`, no re-pagination): the historical streaming worker pool. Each
  worker composes, encodes and WRITES one page, so no composed page outlives its worker
  and page order does not matter.
- COLLECTING (`Pdf`, or any re-paginating run): workers only COMPOSE; the pages are
  re-ordered by the consumer, optionally re-sliced by `export_repaginate::RibbonSlicer`, and
  written as numbered PNGs or appended to a `pdf_export::TypingPdfBuilder`. Because the
  consumer must see the pages IN ORDER while composition stays parallel, a sliding window
  (`TypingComposeWindow`) keeps peak memory at a few composed pages instead of the whole
  chapter, and `TypingComposeReport` guarantees that a claimed ordinal is reported even when
  its worker panics — otherwise the window would never reopen and the run would hang.

Notes:
Extracted verbatim from `tab.rs`. Free fns and methods are `pub(super)` so `tab.rs`
and sibling submodules of `tab` can use them. `use super::*;` pulls in the parent
module's types and imports. Some export helpers (compositing/deform variants) remain
in `tab.rs` and are reused from here as descendants of module `tab`.
The export worker barriers the shared layer saver before composing: under the autosave
gate the saver holds enqueued writes, and the page flatten can fall back to the staging
`layers.json`.
*/

use super::*;
// `write_image` is a `PngEncoder` method from this trait; needed for the in-memory
// PNG encode that replaces `image::save_buffer`/`fs::write` on the storage seam.
use image::ImageEncoder;
use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Condvar;

use crate::export_repaginate::{RibbonSlicer, TypingRepaginateSettings, group_pages_into_ribbons, repaginated_page_file_name};
use crate::pdf_export::TypingPdfBuilder;

/// The checked pairing of an export format with its destination: the ONE place that decides
/// which pipeline a request runs through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TypingExportRoute {
    /// One output file per source page, written into this folder while the pages are composed
    /// (`Png`/`Psd` without re-pagination). Bounded memory, no ordering requirement.
    StreamedFiles { dir: PathBuf },
    /// Composed pages stitched into same-width ribbons, re-sliced, and written into this
    /// folder as numbered PNG pages.
    RepaginatedPng { dir: PathBuf },
    /// Composed pages — re-sliced first when re-pagination is on — appended to one PDF
    /// document, which is written to this file when the run finishes.
    Pdf { file: PathBuf },
}

/// Resolves `request` into the pipeline that can honour it, rejecting every combination that
/// has no honest implementation instead of guessing one.
///
/// Pure and cheap, so both the GUI-thread dispatcher (which needs the per-page output paths
/// and wants an invalid request to fail before a thread is spawned) and the worker thread
/// call it on the same request and necessarily agree.
///
/// # Errors
/// - `typing.errors.export_psd_repaginate_unsupported` when PSD meets re-pagination: a
///   re-sliced page is no longer the page whose layer stack a PSD carries, so there is
///   nothing honest to write. The panel hides the section for PSD, so this is a guard against
///   a plumbing mistake, not a reachable UI state — and it fails loudly rather than ignoring
///   the flag.
/// - `typing.errors.export_destination_mismatch` when the destination's SHAPE contradicts the
///   format (a folder for PDF, or a single file for PNG/PSD). That is a programming error in
///   the panel plumbing; it is reported instead of reinterpreted.
pub(super) fn resolve_export_route(request: &TypingExportRequest) -> Result<TypingExportRoute, String> {
    match (request.format, &request.destination) {
        (TypingExportFormat::Png, TypingExportDestination::Folder(dir)) => {
            if request.repaginate.enabled {
                Ok(TypingExportRoute::RepaginatedPng { dir: dir.clone() })
            } else {
                Ok(TypingExportRoute::StreamedFiles { dir: dir.clone() })
            }
        }
        (TypingExportFormat::Psd, TypingExportDestination::Folder(dir)) => {
            if request.repaginate.enabled {
                return Err(t!("typing.errors.export_psd_repaginate_unsupported").to_string());
            }
            Ok(TypingExportRoute::StreamedFiles { dir: dir.clone() })
        }
        (TypingExportFormat::Pdf, TypingExportDestination::PdfFile(file)) => Ok(TypingExportRoute::Pdf { file: file.clone() }),
        (TypingExportFormat::Png | TypingExportFormat::Psd, TypingExportDestination::PdfFile(_)) | (TypingExportFormat::Pdf, TypingExportDestination::Folder(_)) => {
            Err(t!("typing.errors.export_destination_mismatch").to_string())
        }
    }
}

/// Creates the directory the route writes into: the picked folder, or the PDF file's parent.
///
/// Routed through the storage seam like every other export write (the web build has no real
/// filesystem). A PDF path with no parent component needs nothing created.
///
/// # Errors
/// `typing.errors.create_output_dir_error` when the directory cannot be created.
fn prepare_export_output_location(route: &TypingExportRoute) -> Result<(), String> {
    let dir: Option<&Path> = match route {
        TypingExportRoute::StreamedFiles { dir } | TypingExportRoute::RepaginatedPng { dir } => Some(dir.as_path()),
        TypingExportRoute::Pdf { file } => file.parent().filter(|parent| !parent.as_os_str().is_empty()),
    };
    let Some(dir) = dir else {
        return Ok(());
    };
    ms_storage::global::storage()
        .create_dir_all(dir.to_string_lossy().as_ref())
        .map_err(|err| tf!("typing.errors.create_output_dir_error", output_dir = dir.display(), err = err))
}

/// Number of worker threads an export pool uses: every core but one (the GUI thread keeps a
/// core), at least one, and never more than there are pages to compose.
#[must_use]
pub(super) fn export_worker_count(job_count: usize) -> usize {
    thread::available_parallelism().map(|v| v.get()).unwrap_or(1).saturating_sub(1).max(1).min(job_count)
}

/// How many source pages the COLLECTING pipeline may keep in flight for a pool of
/// `worker_count` workers — the width of its `TypingComposeWindow`.
///
/// Two pages per worker: enough that a worker always has the next page to start on while the
/// consumer walks the run in order, and small enough that peak memory stays a handful of
/// composed pages. The `max(2)` keeps a single-worker pool from serializing completely. This is
/// the ONE owner of that number, so a test can size a run against the real window.
#[must_use]
pub(super) fn compose_window_pages(worker_count: usize) -> usize {
    worker_count.max(2).saturating_mul(2)
}

/// Runs a whole export on the background thread that owns it.
///
/// Validates the format/destination pairing, creates the output location, loads the клин
/// snapshots, then drives the one-to-one or the collecting pipeline. `progress_tx` receives
/// one `Progress` event per SOURCE page processed, in both pipelines.
///
/// # Errors
/// The first failure of the run, already localized: an invalid request
/// (`resolve_export_route`), an output location that cannot be created, a клин snapshot that
/// cannot be loaded, or the first page/document failure the pipeline hit. The one-to-one
/// pipeline still writes every page it can before reporting; the collecting pipeline writes
/// NOTHING once a page has failed, because a half-stitched document would be silent data loss.
pub(super) fn export_typing_pages(
    mut jobs: Vec<TypingExportPageJob>,
    request: TypingExportRequest,
    clean_overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    progress_tx: mpsc::Sender<TypingExportEvent>,
) -> Result<TypingExportResult, String> {
    let route = resolve_export_route(&request)?;
    prepare_export_output_location(&route)?;
    let total = jobs.len();
    if jobs.is_empty() {
        return Ok(TypingExportResult { exported: 0, total, destination: request.destination, warnings: Vec::new() });
    }
    prepare_export_clean_overlay_snapshots(&mut jobs, clean_overlays_model)?;
    // Only the collecting pipeline re-paginates; `resolve_export_route` has already rejected
    // the one format that cannot (PSD).
    let repaginate = request.repaginate.enabled.then_some(request.repaginate);
    match route {
        TypingExportRoute::StreamedFiles { .. } => export_typing_pages_streamed(jobs, request.destination, progress_tx),
        TypingExportRoute::RepaginatedPng { dir } => {
            // The file NAMES of a re-paginated run need the run's final page count up front —
            // it selects the zero padding, and a padding that widened halfway through would
            // produce two different naming schemes in one folder. The count depends on every
            // page's height, and a composed page always has its SOURCE page's pixel size
            // (`flatten_typing_export_page_rgba` builds on the decoded source), so the sizes
            // are scanned from the page headers before anything is composed. The scan also
            // fails fast on re-pagination settings that resolve to no valid height, instead of
            // after minutes of composition — and the consumer re-checks each composed page
            // against the scanned size, so the assumption is verified rather than trusted.
            let sizes = scan_export_page_sizes(&jobs)?;
            let total_output = repaginated_output_page_count(&sizes, &request.repaginate)?;
            let sink = TypingCollectedSink::RepaginatedPng { dir, base: request.output_base_name, total_output, written: 0 };
            export_typing_pages_collected(jobs, sink, repaginate, Some(sizes), request.destination, progress_tx, compose_typing_export_page)
        }
        TypingExportRoute::Pdf { file } => {
            // A PDF names no page, so it needs no size pre-scan: the document simply takes the
            // pages in order, at whatever size each composes to.
            let sink = TypingCollectedSink::Pdf { file, builder: TypingPdfBuilder::new() };
            export_typing_pages_collected(jobs, sink, repaginate, None, request.destination, progress_tx, compose_typing_export_page)
        }
    }
}

/// The historical ONE-TO-ONE pipeline: a pool of workers pops pages and each composes,
/// encodes and writes its own output file.
///
/// Nothing is collected, so results may arrive in any order and peak memory is bounded by the
/// worker count. A page that fails does NOT abort the run — every other page is still written
/// and the first error is reported at the end — because the output files are independent of
/// one another.
fn export_typing_pages_streamed(jobs: Vec<TypingExportPageJob>, destination: TypingExportDestination, progress_tx: mpsc::Sender<TypingExportEvent>) -> Result<TypingExportResult, String> {
    let total = jobs.len();
    let worker_count = export_worker_count(jobs.len());
    let queue = Arc::new(Mutex::new(VecDeque::from(jobs)));
    let (tx, rx) = mpsc::channel::<Result<Vec<String>, String>>();
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
    // Deduplicated across pages: the same ambiguous font is normally used on every page,
    // and the status line must state the problem once, not once per page.
    let mut warnings: Vec<String> = Vec::new();
    for result in rx {
        processed = processed.saturating_add(1);
        match result {
            Ok(page_warnings) => {
                exported = exported.saturating_add(1);
                for warning in page_warnings {
                    if !warnings.contains(&warning) {
                        warnings.push(warning);
                    }
                }
            }
            Err(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
        // A closed progress channel means the tab dropped this export; there is nobody left to
        // report to and the run finishes quietly, so the send result is deliberately ignored.
        let _ = progress_tx.send(TypingExportEvent::Progress { done: processed, total });
    }
    for handle in worker_handles {
        let _ = handle.join();
    }
    if let Some(err) = first_error {
        return Err(err);
    }
    // Worker completion order is not deterministic, so sort for a stable status line.
    warnings.sort();
    Ok(TypingExportResult { exported, total, destination, warnings })
}

/// One composed page on its way from an export worker to the ordered consumer: straight
/// (un-premultiplied) RGBA8, row-major, exactly `width_px * height_px * 4` bytes.
pub(super) struct TypingComposedPage {
    pub(super) rgba: Vec<u8>,
    pub(super) width_px: u32,
    pub(super) height_px: u32,
}

impl std::fmt::Debug for TypingComposedPage {
    /// Prints the dimensions and the buffer SIZE only: the derived form would dump tens of
    /// megabytes of pixels into a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypingComposedPage").field("width_px", &self.width_px).field("height_px", &self.height_px).field("rgba_len", &self.rgba.len()).finish()
    }
}

/// A composition outcome tagged with the position of its source page in export order, so the
/// consumer can restore that order.
#[derive(Debug)]
struct TypingComposedMessage {
    ordinal: usize,
    composed: Result<TypingComposedPage, String>,
}

/// How a collecting-pipeline worker turns one job into pixels.
///
/// Every real export passes [`compose_typing_export_page`]; the parameter exists because a
/// composition that genuinely UNWINDS cannot be provoked from job data (every anticipated
/// failure of the real one is a `Result`), and the pool's panic-safety has to be testable.
type TypingComposeFn = fn(&TypingExportPageJob) -> Result<TypingComposedPage, String>;

/// Owns the promise that one claimed ordinal reaches the consumer EXACTLY once.
///
/// The collecting pipeline's consumer walks ordinals in order and only opens the window
/// (`TypingComposeWindow::advance_to`) for an ordinal it has seen, so an ordinal that is
/// claimed and never reported wedges the whole run: the surviving workers block on the gate
/// forever, their `tx` clones are never dropped, and the consumer's `for message in rx` never
/// ends. A worker that PANICS mid-composition is exactly that case — it sends nothing.
///
/// So the guard is created the moment the ordinal leaves the queue and is disarmed only by
/// [`Self::report`]. Dropped un-disarmed — which happens only while a panic unwinds out of the
/// worker — it reports the ordinal as a failure, the consumer advances past it, and the run
/// ends as a REPORTED error instead of a hang.
///
/// `catch_unwind` is deliberately not used instead: the job, the gate and the sender are not
/// `UnwindSafe`, so it would need `AssertUnwindSafe` around the whole body and would still
/// have to re-raise or translate the payload by hand. The guard expresses the same invariant
/// where it belongs — on the ordinal — and costs one `bool`.
#[derive(Debug)]
struct TypingComposeReport<'a> {
    ordinal: usize,
    tx: &'a mpsc::Sender<TypingComposedMessage>,
    reported: bool,
}

impl<'a> TypingComposeReport<'a> {
    /// Arms the guard for `ordinal`, which the caller has just taken off the job queue.
    #[must_use]
    fn new(ordinal: usize, tx: &'a mpsc::Sender<TypingComposedMessage>) -> Self {
        Self { ordinal, tx, reported: false }
    }

    /// Reports `composed` for this ordinal and disarms the guard.
    ///
    /// # Errors
    /// The send error when the consumer is already gone (the export was dropped), which tells
    /// the worker to stop. The guard stays disarmed either way: there is nobody to report to.
    fn report(mut self, composed: Result<TypingComposedPage, String>) -> Result<(), mpsc::SendError<TypingComposedMessage>> {
        self.reported = true;
        self.tx.send(TypingComposedMessage { ordinal: self.ordinal, composed })
    }
}

impl Drop for TypingComposeReport<'_> {
    /// Reports an un-disarmed ordinal as a failure so the consumer can advance past it.
    fn drop(&mut self) {
        if self.reported {
            return;
        }
        ms_log::runtime_log::log_error(format!("[typing] export: a worker panicked while composing page {}; reporting it as a failed page so the run can end", self.ordinal.saturating_add(1)));
        // A send failure here means the consumer is already gone, so the run is already over
        // and there is nothing left to report to — and this runs during a panic unwind, where
        // propagating anything would abort the process. Ignoring it is the whole handling.
        let _ = self.tx.send(TypingComposedMessage {
            ordinal: self.ordinal,
            composed: Err(tf!("typing.errors.export_page_panic", page = self.ordinal.saturating_add(1))),
        });
    }
}

/// Sliding-window gate that keeps the COLLECTING pipeline's memory bounded.
///
/// The consumer must take composed pages in export order (the ribbon slicer and the PDF
/// writer are both sequential) while composition stays parallel. Without a gate the workers
/// race ahead and every composed page piles up in the reorder buffer — roughly 180 MB for a
/// 40-page chapter and 640 MB for a ribbon chapter. So a worker may not START composing the
/// page at `ordinal` until `ordinal < next_consumed + window`; at most `window` pages are
/// alive at any moment (being composed, in the channel, or waiting in the reorder buffer).
///
/// WHY THIS CANNOT DEADLOCK — the job queue hands pages out in strictly increasing ordinal:
/// - A worker that blocks holds some ordinal `k` and holds NO queue lock, and every ordinal
///   below `k` has already been handed out.
/// - The lowest unconsumed ordinal `n` can never block: `n < n + window` for any `window >= 1`,
///   so the worker holding `n` always passes the gate.
/// - Therefore, if `n` is still IN the queue, no worker can hold an ordinal above it, so no
///   worker is blocked and one of them will pop `n`. And if `n` has already been handed out,
///   it is being composed or already sent. Either way the consumer keeps receiving,
///   `next_consumed` advances, and every blocked worker is woken.
#[derive(Debug)]
struct TypingComposeWindow {
    /// Ordinal of the lowest source page the consumer has NOT consumed yet.
    next_consumed: Mutex<usize>,
    /// Signalled whenever `next_consumed` advances.
    advanced: Condvar,
    /// How far ahead of `next_consumed` composition may run. Always >= 1.
    window: usize,
}

impl TypingComposeWindow {
    /// Creates a gate allowing `window` pages ahead of the consumer. `window` is clamped to at
    /// least 1, because a zero window would block every worker forever.
    #[must_use]
    fn new(window: usize) -> Self {
        Self { next_consumed: Mutex::new(0), advanced: Condvar::new(), window: window.max(1) }
    }

    /// Blocks the calling worker until the page at `ordinal` fits inside the window.
    ///
    /// A poisoned mutex is recovered rather than propagated: the counter is a plain `usize`
    /// that cannot be left inconsistent by a panicking holder, and an export must not wedge
    /// its worker pool because an unrelated thread panicked.
    fn wait_for_slot(&self, ordinal: usize) {
        let mut next_consumed = self.next_consumed.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while ordinal >= next_consumed.saturating_add(self.window) {
            next_consumed = self.advanced.wait(next_consumed).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Publishes the consumer's new low-water mark and wakes every waiting worker.
    fn advance_to(&self, next_consumed: usize) {
        {
            let mut locked = self.next_consumed.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *locked = next_consumed;
        }
        self.advanced.notify_all();
    }
}

/// Where the collecting pipeline's OUTPUT pages go.
///
/// An output page is not a source page: with re-pagination on, one output page may span
/// several source pages or be a fragment of one.
#[derive(Debug)]
pub(super) enum TypingCollectedSink {
    /// Re-paginated PNG pages written into `dir` as `"{base} {NNN}.png"`, numbered
    /// continuously across every ribbon of the run.
    RepaginatedPng {
        dir: PathBuf,
        /// Already-sanitized chapter base name; may be empty, which numbers the pages alone.
        base: String,
        /// Output pages the run will write. Only selects the zero padding, and is known up
        /// front from the source page sizes (see `repaginated_output_page_count`).
        total_output: usize,
        /// Output pages written so far; also the 0-based number of the next one.
        written: usize,
    },
    /// One multi-page PDF, assembled in memory and written to `file` when the run finishes.
    Pdf { file: PathBuf, builder: TypingPdfBuilder },
}

impl TypingCollectedSink {
    /// Writes or appends ONE output page. `rgba` is straight RGBA8, `width_px * height_px * 4`
    /// bytes long, and is only borrowed.
    ///
    /// # Errors
    /// `typing.errors.save_page_error` when the page cannot be written through the storage
    /// seam, and `typing.errors.export_pdf_error` when the PDF writer rejects the page (zero
    /// size, buffer length, page count, compression).
    pub(super) fn push_page(&mut self, rgba: &[u8], width_px: u32, height_px: u32) -> Result<(), String> {
        match self {
            Self::RepaginatedPng { dir, base, total_output, written } => {
                let path = dir.join(repaginated_page_file_name(base, *written, *total_output));
                let bytes = encode_export_page_png(rgba, width_px, height_px, &path)?;
                ms_storage::global::storage()
                    .write(path.to_string_lossy().as_ref(), &bytes)
                    .map_err(|err| tf!("typing.errors.save_page_error", job = path.display(), err = err))?;
                *written = written.saturating_add(1);
                Ok(())
            }
            Self::Pdf { builder, .. } => builder.push_page(rgba, width_px, height_px).map_err(|err| tf!("typing.errors.export_pdf_error", err = err)),
        }
    }

    /// Closes the sink and returns how many OUTPUT pages it wrote. For the PDF sink this is
    /// where the document is serialized and written — a single write, because the storage
    /// seam has no streaming writer.
    ///
    /// # Errors
    /// `typing.errors.export_pdf_error` when the document cannot be serialized, and
    /// `typing.errors.save_page_error` when it cannot be written.
    pub(super) fn finish(self) -> Result<usize, String> {
        match self {
            Self::RepaginatedPng { written, .. } => Ok(written),
            Self::Pdf { file, builder } => {
                let pages = builder.page_count();
                let bytes = builder.finish().map_err(|err| tf!("typing.errors.export_pdf_error", err = err))?;
                ms_storage::global::storage()
                    .write(file.to_string_lossy().as_ref(), &bytes)
                    .map_err(|err| tf!("typing.errors.save_page_error", job = file.display(), err = err))?;
                Ok(pages)
            }
        }
    }
}

/// The ribbon currently being filled by [`TypingRibbonStage`].
#[derive(Debug)]
struct TypingRibbonState {
    /// Pixel width shared by every page of this ribbon.
    width_px: u32,
    slicer: RibbonSlicer,
}

/// Streaming re-pagination placed in front of a [`TypingCollectedSink`]: composed pages
/// arrive in export order, consecutive pages of equal width are stitched into a ribbon, and
/// the ribbon is re-sliced into output pages.
///
/// A width change ends the ribbon (pages are never rescaled) and its short tail is emitted as
/// is. Grouping incrementally is what `export_repaginate::group_pages_into_ribbons` expresses
/// for a run whose page sizes are all known at once; here the pages arrive one at a time.
#[derive(Debug)]
pub(super) struct TypingRibbonStage {
    settings: TypingRepaginateSettings,
    current: Option<TypingRibbonState>,
}

impl TypingRibbonStage {
    /// Creates a stage that cuts pages according to `settings`.
    #[must_use]
    pub(super) fn new(settings: TypingRepaginateSettings) -> Self {
        Self { settings, current: None }
    }

    /// Feeds one composed page, emitting into `sink` every output page that completes.
    ///
    /// # Errors
    /// `typing.errors.export_repaginate_height_error` when the settings resolve to no valid
    /// target height for this ribbon's width (a `None` from `target_height_px` means "cannot
    /// re-paginate" and must never be replaced by a default), and
    /// `typing.errors.export_repaginate_error` when the slicer rejects the buffer. Sink
    /// errors propagate unchanged.
    pub(super) fn push_page(&mut self, page: &TypingComposedPage, sink: &mut TypingCollectedSink) -> Result<(), String> {
        // Taking ownership of the ribbon (instead of borrowing it) is what lets the
        // "continue / start a new one" decision be written without an impossible `None` arm.
        let mut ribbon = match self.current.take() {
            Some(ribbon) if ribbon.width_px == page.width_px => ribbon,
            Some(ribbon) => {
                Self::flush_ribbon(ribbon, sink)?;
                self.new_ribbon(page.width_px)?
            }
            None => self.new_ribbon(page.width_px)?,
        };
        let sliced = ribbon.slicer.push_page(&page.rgba, page.height_px);
        self.current = Some(ribbon);
        for output in sliced.map_err(|err| tf!("typing.errors.export_repaginate_error", err = err))? {
            sink.push_page(&output.rgba, output.width_px, output.height_px)?;
        }
        Ok(())
    }

    /// Emits the last ribbon's tail. Called once, after the final page has been fed.
    ///
    /// # Errors
    /// Propagates the sink's write errors.
    pub(super) fn finish(&mut self, sink: &mut TypingCollectedSink) -> Result<(), String> {
        match self.current.take() {
            Some(ribbon) => Self::flush_ribbon(ribbon, sink),
            None => Ok(()),
        }
    }

    /// Starts a ribbon for pages `width_px` wide.
    ///
    /// # Errors
    /// `typing.errors.export_repaginate_height_error` when the width is zero or the settings
    /// resolve to no target height for it.
    fn new_ribbon(&self, width_px: u32) -> Result<TypingRibbonState, String> {
        let width = NonZeroU32::new(width_px).ok_or_else(|| tf!("typing.errors.export_repaginate_height_error", width = width_px))?;
        let target = self.settings.target_height_px(width_px).ok_or_else(|| tf!("typing.errors.export_repaginate_height_error", width = width_px))?;
        Ok(TypingRibbonState { width_px, slicer: RibbonSlicer::new(width, target) })
    }

    /// Emits a finished ribbon's tail — the rows left over after its last full page. The tail
    /// is SHORTER than the target height and is never padded.
    fn flush_ribbon(mut ribbon: TypingRibbonState, sink: &mut TypingCollectedSink) -> Result<(), String> {
        match ribbon.slicer.finish() {
            Some(tail) => sink.push_page(&tail.rgba, tail.width_px, tail.height_px),
            None => Ok(()),
        }
    }
}

/// The COLLECTING pipeline: workers compose pages in parallel, the consumer takes them in
/// export order and feeds them — through the optional re-pagination stage — to `sink`.
///
/// `expected_sizes`, when given, holds the `(width, height)` the run planned for each source
/// page, in export order; every composed page is checked against it (see
/// `TypingExportRoute::RepaginatedPng` for why the plan exists). `repaginate` is `Some` only
/// when re-pagination is enabled.
///
/// `exported` in the result counts SOURCE pages composed, never output pages, so that it stays
/// comparable with `total` and with the progress the panel has been showing.
///
/// `compose` is the composition every worker runs; production always passes
/// [`compose_typing_export_page`] (see [`TypingComposeFn`]).
///
/// # Errors
/// The first failure of the run. Once a page has failed, the run keeps DRAINING the workers
/// (they must not block on the window) but stops feeding the sink and writes no document:
/// a PDF or a ribbon missing a page in the middle is silent data loss, not a partial success.
/// A worker that PANICS surfaces as `typing.errors.export_page_panic` for the page it held
/// (via [`TypingComposeReport`]), and `typing.errors.export_incomplete_pages` guards the run's
/// post-condition if a page is ever lost some other way.
pub(super) fn export_typing_pages_collected(
    jobs: Vec<TypingExportPageJob>,
    mut sink: TypingCollectedSink,
    repaginate: Option<TypingRepaginateSettings>,
    expected_sizes: Option<Vec<(u32, u32)>>,
    destination: TypingExportDestination,
    progress_tx: mpsc::Sender<TypingExportEvent>,
    compose: TypingComposeFn,
) -> Result<TypingExportResult, String> {
    let total = jobs.len();
    let worker_count = export_worker_count(total);
    let gate = Arc::new(TypingComposeWindow::new(compose_window_pages(worker_count)));
    let queue: Arc<Mutex<VecDeque<(usize, TypingExportPageJob)>>> = Arc::new(Mutex::new(jobs.into_iter().enumerate().collect()));
    let (tx, rx) = mpsc::channel::<TypingComposedMessage>();
    let mut worker_handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let tx = tx.clone();
        let queue = Arc::clone(&queue);
        let gate = Arc::clone(&gate);
        worker_handles.push(thread::spawn(move || {
            loop {
                // Popping in order is what makes the window gate deadlock-free; see
                // `TypingComposeWindow`.
                let next = {
                    let mut locked = queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    locked.pop_front()
                };
                let Some((ordinal, job)) = next else {
                    break;
                };
                // Armed before anything that can unwind: from here the ordinal MUST reach the
                // consumer, or the window never reopens for it and the pool wedges. See
                // `TypingComposeReport`.
                let report = TypingComposeReport::new(ordinal, &tx);
                // Wait BEFORE composing, not after: the point of the window is that the
                // page's pixels are never allocated until there is room for them.
                gate.wait_for_slot(ordinal);
                let composed = compose(&job);
                if report.report(composed).is_err() {
                    break;
                }
            }
        }));
    }
    drop(tx);

    let mut stage = repaginate.map(TypingRibbonStage::new);
    let mut reordered: BTreeMap<usize, Result<TypingComposedPage, String>> = BTreeMap::new();
    let mut next_ordinal = 0usize;
    let mut composed_ok = 0usize;
    let mut first_error: Option<String> = None;
    for message in rx {
        reordered.insert(message.ordinal, message.composed);
        // Consume every page that is now contiguous with what has already been consumed.
        while let Some(entry) = reordered.remove(&next_ordinal) {
            match entry {
                Ok(page) => {
                    composed_ok = composed_ok.saturating_add(1);
                    if first_error.is_none()
                        && let Err(err) = feed_composed_page(&page, next_ordinal, expected_sizes.as_deref(), stage.as_mut(), &mut sink)
                    {
                        first_error = Some(err);
                    }
                }
                Err(err) => {
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
            next_ordinal = next_ordinal.saturating_add(1);
            // Freeing the window slot must happen even for a page that failed or was skipped,
            // or the workers would block behind it forever.
            gate.advance_to(next_ordinal);
            // Progress is counted in SOURCE pages, whatever the output pagination does, so
            // `TypingExportUiStatus::Running { done, total }` keeps meaning what it means.
            // A closed channel means the tab dropped this export; nobody is left to report to.
            let _ = progress_tx.send(TypingExportEvent::Progress { done: next_ordinal, total });
        }
    }
    for handle in worker_handles {
        // A panicking worker has already reported its ordinal through `TypingComposeReport`,
        // so the run's ERROR is set; the join payload adds the panic's own message, which is
        // diagnostic only and belongs in the log rather than in the user-facing string.
        if let Err(payload) = handle.join() {
            ms_log::runtime_log::log_error(format!("[typing] export: worker thread panicked: {}", panic_payload_message(payload.as_ref())));
        }
    }
    if let Some(err) = first_error {
        return Err(err);
    }
    // POST-CONDITION of the consumer loop: it advances `next_ordinal` once per ordinal it
    // consumed, failed pages included, so a run that ends short has LOST a page somewhere.
    // Reporting success here would write a PDF (or a page set) silently missing pages — the
    // `TypingComposeReport` guard exists precisely so this cannot happen, and this check is
    // what keeps a future hole in that promise from becoming data loss instead of an error.
    if next_ordinal != total {
        return Err(tf!("typing.errors.export_incomplete_pages", done = next_ordinal, total = total));
    }
    if let Some(stage) = stage.as_mut() {
        stage.finish(&mut sink)?;
    }
    let written = sink.finish()?;
    ms_log::trace_log!(cat::PERSIST, "export collected pages={} output_pages={}", composed_ok, written);
    Ok(TypingExportResult { exported: composed_ok, total, destination, warnings: Vec::new() })
}

/// Renders a `thread::join` panic payload as text for the log: the `&str` / `String` message a
/// `panic!` carries, or a placeholder for a payload of any other type.
#[must_use]
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return message;
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.as_str();
    }
    "<non-string panic payload>"
}

/// Checks one composed page against the size the run planned for it and hands it on.
///
/// # Errors
/// `typing.errors.export_page_size_mismatch` when the composed page is not the size the run
/// planned for — which would silently break the output page COUNT the file names were built
/// from. Re-pagination and sink errors propagate unchanged.
fn feed_composed_page(page: &TypingComposedPage, ordinal: usize, expected_sizes: Option<&[(u32, u32)]>, stage: Option<&mut TypingRibbonStage>, sink: &mut TypingCollectedSink) -> Result<(), String> {
    // `expected_sizes` is built by scanning the SAME jobs, so an entry always exists when the
    // run planned sizes at all; a missing one simply means this route planned none.
    if let Some(expected) = expected_sizes.and_then(|sizes| sizes.get(ordinal))
        && *expected != (page.width_px, page.height_px)
    {
        return Err(tf!(
            "typing.errors.export_page_size_mismatch",
            page = ordinal.saturating_add(1),
            expected = format!("{}x{}", expected.0, expected.1),
            actual = format!("{}x{}", page.width_px, page.height_px)
        ));
    }
    match stage {
        Some(stage) => stage.push_page(page, sink),
        None => sink.push_page(&page.rgba, page.width_px, page.height_px),
    }
}

/// Composes one page for the collecting pipeline: the same composite the one-to-one pipeline
/// writes, returned as pixels instead of being encoded and saved.
///
/// # Errors
/// Whatever `flatten_typing_export_page_rgba` reports, plus
/// `typing.errors.export_page_size_error` when a dimension does not fit in `u32` (the size
/// every downstream engine speaks).
pub(super) fn compose_typing_export_page(job: &TypingExportPageJob) -> Result<TypingComposedPage, String> {
    let (rgba, width, height) = flatten_typing_export_page_rgba(job)?;
    let width_px = u32::try_from(width).map_err(|_| tf!("typing.errors.export_page_size_error", job = job.page_path.display()))?;
    let height_px = u32::try_from(height).map_err(|_| tf!("typing.errors.export_page_size_error", job = job.page_path.display()))?;
    Ok(TypingComposedPage { rgba, width_px, height_px })
}

/// Reads every source page's pixel size from its file header, in export order.
///
/// # Errors
/// `typing.errors.open_page_error` when a page cannot be read or its header cannot be parsed.
fn scan_export_page_sizes(jobs: &[TypingExportPageJob]) -> Result<Vec<(u32, u32)>, String> {
    jobs.iter()
        .map(|job| {
            let page_path_str = job.page_path.to_string_lossy();
            // The storage seam has no header-only read, so the bytes are read in full here and
            // only the HEADER is parsed; the pixels are decoded later, by the composition pass.
            let bytes = ms_storage::global::storage()
                .read(page_path_str.as_ref())
                .map_err(|err| tf!("typing.errors.open_page_error", job = job.page_path.display(), err = err))?;
            image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .map_err(|err| tf!("typing.errors.open_page_error", job = job.page_path.display(), err = err))?
                .into_dimensions()
                .map_err(|err| tf!("typing.errors.open_page_error", job = job.page_path.display(), err = err))
        })
        .collect()
}

/// Number of output pages a re-paginated run will write, given every source page's
/// `(width, height)` in export order.
///
/// Pages are grouped into same-width ribbons exactly as the streaming stage groups them
/// (`group_pages_into_ribbons`), and each ribbon contributes `ceil(rows / target_height)`
/// pages — the last of which is the ribbon's short tail.
///
/// # Errors
/// `typing.errors.export_repaginate_height_error` when a ribbon's width resolves to no target
/// height under `settings`; this is the run's fail-fast check on the re-pagination settings.
pub(super) fn repaginated_output_page_count(sizes: &[(u32, u32)], settings: &TypingRepaginateSettings) -> Result<usize, String> {
    let widths: Vec<u32> = sizes.iter().map(|(width, _)| *width).collect();
    let mut total = 0usize;
    for ribbon in group_pages_into_ribbons(&widths) {
        // Indexing through `skip`/`take` rather than a range index: the ranges are in-bounds
        // and non-empty by the helper's contract, and this cannot panic if that ever changes.
        let mut width_px = 0u32;
        let mut rows = 0u64;
        for (width, height) in sizes.iter().skip(ribbon.start).take(ribbon.len()) {
            width_px = *width;
            rows = rows.saturating_add(u64::from(*height));
        }
        let target = settings.target_height_px(width_px).ok_or_else(|| tf!("typing.errors.export_repaginate_height_error", width = width_px))?;
        // A page count beyond `usize` is unreachable (it would need more rows than a 64-bit
        // address space holds); saturating only ever widens the zero padding.
        total = total.saturating_add(usize::try_from(rows.div_ceil(u64::from(target.get()))).unwrap_or(usize::MAX));
    }
    Ok(total)
}

/// Encodes a straight RGBA8 page as PNG bytes in memory, with the same default `PngEncoder`
/// parameters `image::save_buffer` uses for a `.png` path.
///
/// `output_path` is only used to name the file in the error message.
///
/// # Errors
/// `typing.errors.save_page_error` when the encoder rejects the buffer or its dimensions.
fn encode_export_page_png(rgba: &[u8], width_px: u32, height_px: u32, output_path: &Path) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    image::codecs::png::PngEncoder::new(&mut buf)
        .write_image(rgba, width_px, height_px, image::ColorType::Rgba8.into())
        .map_err(|err| tf!("typing.errors.save_page_error", job = output_path.display(), err = err))?;
    Ok(buf)
}

/// Exports ONE page one-to-one — composes it, encodes it and writes its own file — and returns
/// the localized warnings its assembly produced (empty in the normal case; PNG never produces
/// any).
///
/// A warning is NOT an error: the page is written either way. Today the only source is the
/// PSD font-name ambiguity — see `psd_export::AmbiguousExportFont`.
///
/// # Errors
/// `typing.errors.export_destination_mismatch` when the job did not come from a one-to-one
/// route (no output path, or the PDF format, neither of which can write a per-page file);
/// `typing.errors.save_page_error` when the page cannot be encoded or written; whatever the
/// composite or the PSD writer reports otherwise.
pub(super) fn export_typing_single_page(job: TypingExportPageJob) -> Result<Vec<String>, String> {
    // Unreachable by construction: `resolve_export_route` sends only the one-to-one routes
    // here and those build every job with an output path. Failing closed beats guessing a
    // path or silently skipping the page.
    let Some(output_path) = job.output_path.clone() else {
        return Err(t!("typing.errors.export_destination_mismatch").to_string());
    };
    match job.export_format {
        TypingExportFormat::Png => {
            let (base_rgba, base_w, base_h) = flatten_typing_export_page_rgba(&job)?;
            let width_px = u32::try_from(base_w).map_err(|_| tf!("typing.errors.export_page_size_error", job = job.page_path.display()))?;
            let height_px = u32::try_from(base_h).map_err(|_| tf!("typing.errors.export_page_size_error", job = job.page_path.display()))?;
            let buf = encode_export_page_png(&base_rgba, width_px, height_px, &output_path)?;
            ms_storage::global::storage()
                .write(output_path.to_string_lossy().as_ref(), &buf)
                .map_err(|err| tf!("typing.errors.save_page_error", job = output_path.display(), err = err))?;
            Ok(Vec::new())
        }
        TypingExportFormat::Psd => {
            let built = super::super::psd_export::export_typing_single_page_psd(&job)?;
            ms_storage::global::storage()
                .write(output_path.to_string_lossy().as_ref(), &built.bytes)
                .map_err(|err| tf!("typing.errors.save_page_error", job = output_path.display(), err = err))?;
            Ok(built.warnings)
        }
        // A PDF is one document, not a file per page: `resolve_export_route` always sends it
        // to the collecting pipeline, so reaching this arm means the routing itself is wrong.
        TypingExportFormat::Pdf => Err(t!("typing.errors.export_destination_mismatch").to_string()),
    }
}

/// Windows characters that cannot appear in a file name, replaced by `_` by
/// [`sanitize_file_name_component`]. Linux only forbids `/`, but a chapter exported on Linux
/// is routinely opened on Windows, so the strictest set wins.
const FILE_NAME_FORBIDDEN_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Builds the export base name of the open chapter: `"<title> <chapter>"`, sanitized for a
/// file name.
///
/// The two halves are the FOLDER names of `paths.title_dir` and `paths.project_dir`, because
/// those are the names the user picked and sees in the launcher. A half whose folder name is
/// unusable is dropped; when nothing usable is left the result is EMPTY, which is a supported
/// input downstream (`export_repaginate::repaginated_page_file_name` then names pages by
/// their number alone) and must not be replaced by an invented name.
#[must_use]
pub(super) fn export_base_name(project: &ProjectData) -> String {
    export_base_name_from_dirs(&project.paths.title_dir, &project.paths.project_dir)
}

/// The naming rule behind [`export_base_name`], on the two directories it reads.
///
/// Split out so the rule can be exercised without a whole `ProjectData` fixture, and so that
/// "which directories name a chapter" stays a single decision made in the caller above.
#[must_use]
pub(super) fn export_base_name_from_dirs(title_dir: &Path, project_dir: &Path) -> String {
    let title = sanitized_dir_name(title_dir);
    let chapter = sanitized_dir_name(project_dir);
    match (title.is_empty(), chapter.is_empty()) {
        (true, true) => String::new(),
        (true, false) => chapter,
        (false, true) => title,
        (false, false) => format!("{title} {chapter}"),
    }
}

/// The last component of `dir`, sanitized for use inside a file name. Empty when the path has
/// no final component or it is not valid UTF-8.
#[must_use]
fn sanitized_dir_name(dir: &Path) -> String {
    dir.file_name().and_then(|name| name.to_str()).map(sanitize_file_name_component).unwrap_or_default()
}

/// Makes `raw` safe to use as ONE component of a file name on every platform this app builds
/// for.
///
/// Keeps Unicode letters (a Cyrillic title must survive verbatim), digits, spaces and
/// `- _ . ( )`; replaces every character Windows forbids ([`FILE_NAME_FORBIDDEN_CHARS`]) and
/// every control character with `_`; collapses runs of whitespace into one space; trims
/// leading and trailing whitespace and dots (Windows silently drops a trailing dot); and
/// appends `_` to a reserved Windows DEVICE name, which cannot be a file name there even with
/// an extension.
///
/// Returns an EMPTY string when nothing usable is left. Callers must handle that rather than
/// write a nameless file.
#[must_use]
pub(super) fn sanitize_file_name_component(raw: &str) -> String {
    let mut collapsed = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        // A control character is replaced before the whitespace test, so `\n` and `\t` become
        // `_` rather than silently turning into a space.
        let mapped = if ch.is_control() || FILE_NAME_FORBIDDEN_CHARS.contains(&ch) { '_' } else { ch };
        if mapped.is_whitespace() {
            // A run of spaces becomes one space, and leading whitespace is dropped outright.
            pending_space = !collapsed.is_empty();
            continue;
        }
        if pending_space {
            collapsed.push(' ');
            pending_space = false;
        }
        collapsed.push(mapped);
    }
    let trimmed = collapsed.trim_matches(|ch: char| ch == '.' || ch.is_whitespace());
    if is_windows_reserved_file_name(trimmed) { format!("{trimmed}_") } else { trimmed.to_string() }
}

/// Whether `name` is one of the Windows reserved device names (`CON`, `PRN`, `AUX`, `NUL`,
/// `COM1`..`COM9`, `LPT1`..`LPT9`), which cannot be used as a file name there.
///
/// The check is case-insensitive and looks only at the part before the first dot, because
/// Windows treats `CON.txt` as the device too.
#[must_use]
fn is_windows_reserved_file_name(name: &str) -> bool {
    // `split` always yields at least one item, so the fallback is unreachable and merely
    // avoids an `unwrap`.
    let stem = name.split('.').next().unwrap_or(name).trim().to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let numbered_device = stem.starts_with("COM") || stem.starts_with("LPT");
    numbered_device && stem.len() == 4 && stem.as_bytes().get(3).is_some_and(|digit| matches!(digit, b'1'..=b'9'))
}

pub(super) fn prepare_export_clean_overlay_snapshots(
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

pub(super) fn load_clean_overlay_snapshot_for_export(
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

pub(super) fn load_clean_overlay_rgba_from_disk(
    clean_overlay_path: Option<&Path>,
) -> Result<Option<image::RgbaImage>, String> {
    let Some(clean_overlay_path) = clean_overlay_path else {
        return Ok(None);
    };
    let path_str = clean_overlay_path.to_string_lossy();
    let bytes = ms_storage::global::storage()
        .read(path_str.as_ref())
        .map_err(|err| {
            tf!("typing.errors.open_clean_overlay_error", clean_overlay_path = clean_overlay_path.display(), err = err)
        })?;
    let clean = image::load_from_memory(&bytes)
        .map_err(|err| {
            tf!("typing.errors.open_clean_overlay_error", clean_overlay_path = clean_overlay_path.display(), err = err)
        })?
        .to_rgba8();
    Ok(Some(clean))
}

pub(super) fn rasterize_textured_triangle(
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

pub(super) fn sample_overlay_bilinear_rgba(rgba: &[u8], size: [usize; 2], s: f32, t: f32) -> [u8; 4] {
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

pub(super) fn blend_source_over(dst: &mut [u8], src: &[u8]) {
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

pub(super) fn default_quad_uv_for_page(
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

pub(super) fn export_bilinear_quad_uv(quad_uv: [[f32; 2]; 4], tu: f32, tv: f32) -> [f32; 2] {
    let t = tu.clamp(0.0, 1.0);
    let v = tv.clamp(0.0, 1.0);
    let top_u = quad_uv[0][0] + (quad_uv[1][0] - quad_uv[0][0]) * t;
    let top_v = quad_uv[0][1] + (quad_uv[1][1] - quad_uv[0][1]) * t;
    let bot_u = quad_uv[3][0] + (quad_uv[2][0] - quad_uv[3][0]) * t;
    let bot_v = quad_uv[3][1] + (quad_uv[2][1] - quad_uv[3][1]) * t;
    [top_u + (bot_u - top_u) * v, top_v + (bot_v - top_v) * v]
}

pub(super) fn bilinear_quad_page_px(quad_px: [[f32; 2]; 4], tu: f32, tv: f32) -> [f32; 2] {
    let t = tu.clamp(0.0, 1.0);
    let v = tv.clamp(0.0, 1.0);
    let top_x = quad_px[0][0] + (quad_px[1][0] - quad_px[0][0]) * t;
    let top_y = quad_px[0][1] + (quad_px[1][1] - quad_px[0][1]) * t;
    let bot_x = quad_px[3][0] + (quad_px[2][0] - quad_px[3][0]) * t;
    let bot_y = quad_px[3][1] + (quad_px[2][1] - quad_px[3][1]) * t;
    [top_x + (bot_x - top_x) * v, top_y + (bot_y - top_y) * v]
}

pub(super) fn export_clip_overlay_rgba_if_needed(
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

pub(super) fn export_sample_mask_active(mask: &TypingMaskExportPage, u: f32, v: f32) -> bool {
    if mask.width == 0 || mask.height == 0 {
        return false;
    }
    let x = (u.clamp(0.0, 1.0) * (mask.width.saturating_sub(1)) as f32).round() as usize;
    let y = (v.clamp(0.0, 1.0) * (mask.height.saturating_sub(1)) as f32).round() as usize;
    mask.data
        .get(y.saturating_mul(mask.width).saturating_add(x))
        .is_some_and(|v| *v > 0)
}

impl TypingTextOverlayLayer {
    pub(super) fn poll_export_jobs(&mut self, ctx: &egui::Context) -> bool {
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
                            ms_log::trace_log!(
                                cat::PERSIST,
                                "export result=ok exported={} total={} destination={:?}",
                                result.exported,
                                result.total,
                                result.destination
                            );
                            for warning in &result.warnings {
                                ms_log::trace_log!(cat::PERSIST, "export warning={}", warning);
                            }
                            self.create_status_error = None;
                            self.export_status = TypingExportUiStatus::Success {
                                done: result.exported,
                                total: result.total,
                                warnings: result.warnings,
                            };
                        }
                        Err(err) => {
                            ms_log::trace_log!(cat::PERSIST, "export result=err err={}", err);
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
                    let err = t!("typing.export.channel_error").to_string();
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

    /// Builds the per-page text/image overlay export snapshot from the CURRENT `self.overlays`, keyed by
    /// page and sorted bottom-to-top by unified band-Z (the on-screen draw order). Skips overlays with a
    /// zero dimension or an RGBA buffer whose length does not match `w*h*4`.
    ///
    /// Contract: the caller MUST have made every export page resident first (the Phase 2 preload gate or
    /// the in-function residency pass), because a page's text overlays for migrated/v3 chapters are
    /// materialized into `self.overlays` only on load. Snapshotting before that silently drops their text.
    /// Kept as a small pure helper so the ordering fix is unit-testable without driving the async export.
    pub(super) fn build_export_overlay_snapshots(
        &self,
    ) -> HashMap<usize, Vec<TypingExportOverlaySnapshot>> {
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
        overlays_by_page
    }

    /// Dispatches the whole-project export described by `request`.
    ///
    /// `font_post_script_names` is the panel's `identity -> PostScript name per face`
    /// snapshot, taken on the GUI thread at dispatch (the worker has no font list) and
    /// used only by the PSD writer; the PNG and PDF formats ignore it.
    ///
    /// Refuses, with a create-status error and no dispatch, when an export is already
    /// running, when the project has no pages, or when the request itself is impossible
    /// (see `resolve_export_route`) — the last one is checked HERE, on the GUI thread, so an
    /// invalid pairing never costs a spawned thread.
    ///
    /// The spawned worker first barriers the layer saver (forcing every held staging write out)
    /// so no disk fallback of the page flatten reads a stale `layers.json`.
    pub(super) fn request_export(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        masks_snapshot: HashMap<usize, TypingMaskExportPage>,
        request: TypingExportRequest,
        font_post_script_names: crate::psd_export::FontPostScriptNames,
    ) {
        if self.export_rx.is_some() {
            self.set_create_error(ctx, t!("typing.export.already_running_error"));
            return;
        }
        if project.pages.is_empty() {
            self.set_create_error(ctx, t!("typing.export.no_pages_error"));
            return;
        }
        let route = match resolve_export_route(&request) {
            Ok(route) => route,
            Err(err) => {
                self.set_create_error(ctx, err);
                return;
            }
        };
        ms_log::trace_log!(
            cat::PERSIST,
            "export dispatch pages={} format={:?} repaginate={} route={:?}",
            project.pages.len(),
            request.format,
            request.repaginate.enabled,
            route
        );
        // Only a one-to-one route gives each source page an output file of its own; the
        // collecting routes name their output pages themselves (numbered slices, or PDF pages).
        let per_page_output: Option<(PathBuf, &str)> = match &route {
            TypingExportRoute::StreamedFiles { dir } => Some((
                dir.clone(),
                match request.format {
                    TypingExportFormat::Png => "png",
                    TypingExportFormat::Psd => "psd",
                    // `StreamedFiles` is never resolved for `Pdf`; the arm exists because the
                    // enum is project-owned and must be matched exhaustively.
                    TypingExportFormat::Pdf => "pdf",
                },
            )),
            TypingExportRoute::RepaginatedPng { .. } | TypingExportRoute::Pdf { .. } => None,
        };
        let export_format = request.format;
        let clean_overlays_model = self.clean_overlays_model.clone();

        // Phase 2 gate: the export trigger defers dispatch until the async whole-project preload PASS
        // drains, so by here the overlay/raster snapshots are built from materialized state for every
        // page that could load. This matters for migrated/v3 chapters: their text overlays for
        // never-visited pages are materialized only during load (`sync_from_doc`), so building the
        // overlay snapshot before the pages were resident silently dropped their text. A page can still
        // be non-resident here for a benign reason (the no-doc best-effort fallback, or a page whose
        // decode genuinely failed — the pass gives up on it to avoid hanging), so this is a diagnostic
        // guard, not a hard precondition — the in-function residency pass below materializes whatever it
        // can and a still-missing page is simply omitted from the export.
        if !self.all_pages_loaded(project) {
            ms_log::runtime_log::log_warn(
                "[typing] export: not all pages resident at dispatch; relying on the in-function \
                 residency pass to materialize overlays before snapshotting",
            );
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
        let saved_selected_raster_page = self.selected_raster_page;
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
        self.selected_raster_page = saved_selected_raster_page;
        self.selected_overlay_idx = saved_selected_overlay;
        self.pending_select_raster_uid = saved_pending_select;

        // Build the text/image overlay snapshot AFTER the residency pass above (ordering fix): the pass
        // (`ensure_raster_layers_for_page` -> `sync_from_doc`) MATERIALIZES the doc's text nodes for
        // never-visited pages into `self.overlays`, so snapshotting here — not before the pass — captures
        // every page's text. Building it earlier dropped text for migrated/v3 pages the user never opened.
        let mut overlays_by_page = self.build_export_overlay_snapshots();

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
                TypingExportPageJob {
                    page_idx: page.idx,
                    page_path: page.path.clone(),
                    output_path: per_page_output.as_ref().map(|(dir, ext)| dir.join(format!("{stem}.{ext}"))),
                    clean_overlay_path: {
                        // `is_file()` via the storage seam: exists AND is not a directory.
                        let store = ms_storage::global::storage();
                        let is_file = {
                            let s = clean_overlay_path.to_string_lossy();
                            store.exists(s.as_ref()) && !store.is_dir(s.as_ref())
                        };
                        is_file.then_some(clean_overlay_path)
                    },
                    clean_overlay_rgba: None,
                    overlays: overlays_by_page.remove(&page.idx).unwrap_or_default(),
                    rasters: rasters_by_page.remove(&page.idx).unwrap_or_default(),
                    mask: masks_snapshot.get(&page.idx).cloned(),
                    export_format,
                    layers_primary_dir: self.layers_primary_dir.clone(),
                    layers_fallback_dir: self.layers_fallback_dir.clone(),
                    font_post_script_names: font_post_script_names.clone(),
                }
            })
            .collect::<Vec<_>>();
        let total_pages = jobs.len();
        self.export_status = TypingExportUiStatus::Running {
            done: 0,
            total: total_pages,
        };
        // The page flatten can fall back to the staging `layers.json` (text band-Z when a page has
        // no raster snapshot, rasters when the doc projection is empty). The layer saver may HOLD
        // enqueued writes for minutes under the autosave gate, so the worker barriers it first —
        // writing every held job into staging — before any page reads it. Cheap `Sender` clone here;
        // the barrier blocks the export worker, never the GUI thread. `None` = no saver (every
        // write was synchronous, nothing to wait for).
        let saver_handle = self
            .layer_doc
            .as_ref()
            .and_then(|doc| doc.lock().ok().and_then(|guard| guard.saver_handle()));
        let (tx, rx) = mpsc::channel::<TypingExportEvent>();
        thread::spawn(move || {
            if let Some(handle) = saver_handle {
                let failed_pages = handle.barrier_blocking();
                if !failed_pages.is_empty() {
                    ms_log::runtime_log::log_warn(format!(
                        "[typing] export: the layer saver reported failed staging text writes for pages \
                         {failed_pages:?}; a page that falls back to the staging manifest may export its \
                         previous layer order"
                    ));
                }
            }
            let result = export_typing_pages(jobs, request, clean_overlays_model, tx.clone());
            // The receiver is gone only when the tab dropped this export; there is nobody left
            // to hand the result to, so the send result is deliberately ignored.
            let _ = tx.send(TypingExportEvent::Finished(result));
        });
        self.export_rx = Some(TypingExportRenderState { rx });
    }

    pub(super) fn export_status_for_ui(&self) -> TypingExportUiStatus {
        self.export_status.clone()
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
pub(crate) fn flatten_typing_export_page_rgba(
    job: &TypingExportPageJob,
) -> Result<(Vec<u8>, usize, usize), String> {
    let page_path_str = job.page_path.to_string_lossy();
    let page_bytes = ms_storage::global::storage()
        .read(page_path_str.as_ref())
        .map_err(|err| {
            tf!("typing.errors.open_page_error", job = job.page_path.display(), err = err)
        })?;
    let mut base = image::load_from_memory(&page_bytes)
        .map_err(|err| {
            tf!("typing.errors.open_page_error", job = job.page_path.display(), err = err)
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
    use ms_models::layer_model::ordering::Band;
    use ms_models::layer_model::persist;
    struct RasterDraw {
        visible: bool,
        opacity: f32,
        transform: ms_models::layer_model::manifest::TransformRec,
        deform: Option<ms_models::layer_model::manifest::DeformRec>,
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
pub(crate) fn export_overlay_clipped_rgba(
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

pub(crate) fn composite_overlay_full_image_over(
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

pub(crate) fn composite_overlay_at_page_position_over(
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

pub(crate) fn composite_overlay_mesh_over_page(
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

pub(crate) fn direct_overlay_blit_top_left_px(overlay: &TypingExportOverlaySnapshot) -> Option<[i32; 2]> {
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

pub(crate) fn export_overlay_deform_mesh_for_page(
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
