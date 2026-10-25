/*
File: tab/flatten_to_file.rs

Purpose:
Native "flatten ONE page into an image file" API of the typing tab, used by the image-set
(single-image) mode's «Сохранить» / «Сохранить как» (the app-side session controller drives it
from any active tab, one page at a time).

Key structures:
- `FlattenToFileRequest`: page, `FlattenDestination` (a file, or compose only), `ImageEncoding`,
  and the optional "unchanged if" `PixelDigest`.
- `FlattenReadiness`: per-frame answer of `prepare_flatten_to_file` (ready, or still preparing).
- `FlattenToFileReport` (page, composite digest, `FlattenOutcome`) / `FlattenToFileError`: the
  typed outcome polled by the caller.
- `FlattenToFileState` (private): the in-flight worker receiver and the layer-preload pass tracker.

Key functions:
- `TypingTabState::prepare_flatten_to_file()`: starts and drives everything the composite of ONE
  page reads (chapter loader, migration, whole-chapter layer preload, clip-mask loader,
  re-projection of THAT page from a shared-document edit made in another tab) WITHOUT the typing
  tab drawing; call it every frame until it reports `Ready`.
- `TypingTabState::request_flatten_to_file()`: gates (the export gates + mutual exclusion with a
  project export), builds the page job on the GUI thread and spawns the worker.
- `TypingTabState::poll_flatten_to_file()`: non-blocking result poll.
- `flatten_job_to_file()`: the worker body (клин snapshot, `resolve_flatten_input`,
  `flatten_page_rgba`, `PixelDigest`, then — only when the page changed and a file is wanted —
  `encode_rgba` and the symlink-preserving `ms_docstore::write_bytes_atomic`).

Notes:
Native-only (`write_bytes_atomic` does not exist on wasm); the module is not compiled there. The
composite is exactly the project export's (`build_page_job` + the same flatten), so a saved file
matches a PNG export of the page. The digest is taken over the flattened straight RGBA8 BEFORE
encoding, i.e. over exactly what `ms_project::single_image` hashed when it wrote the scratch page,
so an untouched page compares equal to its `opened_digest` and is never re-encoded. The GUI thread
never does file I/O here beyond what the existing residency / loader-start paths already do; the
saver barrier, decode, digest, encode and write run on the worker.
*/

use super::*;
use crate::image_encode::{EncodeError, ImageEncoding, encode_rgba};
use ms_docstore::{AtomicWriteError, Durability, write_bytes_atomic};
use ms_project::PixelDigest;

/// Where the composite of a flatten goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlattenDestination {
    /// Encode and write the page to this file, as the user named it. An existing target is
    /// canonicalized before the write, so a symlink keeps pointing at its (rewritten) target
    /// instead of being replaced by a file. Its parent directory must exist.
    File(PathBuf),
    /// Compose and digest the page only: nothing is encoded or written. Lets the caller learn
    /// whether a page changed (and its digest) before it chooses where to write it.
    ComposeOnly,
}

/// One page to flatten (into one image file, or compose only).
#[derive(Debug, Clone)]
pub struct FlattenToFileRequest {
    /// `Page::idx` of the page to compose.
    pub page_idx: usize,
    /// Where the composite goes.
    pub destination: FlattenDestination,
    /// Format, JPEG quality, alpha policy and ICC profile of the written file. Unused for
    /// [`FlattenDestination::ComposeOnly`] and for an unchanged page.
    pub encoding: ImageEncoding,
    /// When `Some(d)` and the composite's digest equals `d`, the page counts as unchanged: the
    /// outcome is [`FlattenOutcome::Unchanged`] and nothing is encoded or written, whatever the
    /// destination. `None` always proceeds to the destination.
    pub unchanged_if: Option<PixelDigest>,
}

/// Whether a flatten-to-file may be dispatched now (see [`TypingTabState::prepare_flatten_to_file`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlattenReadiness {
    /// Every input of the composite is resident and nothing conflicting runs:
    /// [`TypingTabState::request_flatten_to_file`] will not refuse with `Busy`.
    Ready,
    /// Still loading or blocked by a running job. `done` / `total` is the layer-preload progress
    /// (`0 / 0` while waiting on something else: the chapter loader, masks, an export or a save).
    Preparing {
        /// Pages applied by the current layer-preload pass.
        done: usize,
        /// Pages the current layer-preload pass has to apply.
        total: usize,
    },
}

/// What a successful flatten did with the composite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlattenOutcome {
    /// The page changed (or no `unchanged_if` was given) and was encoded and written.
    Written {
        /// The requested target (not its canonicalized form), so the caller keeps the path the
        /// user named.
        target: PathBuf,
        /// Size of the encoded file in bytes.
        bytes_written: usize,
    },
    /// The composite's digest equals the request's `unchanged_if`: nothing was encoded or written.
    Unchanged,
    /// [`FlattenDestination::ComposeOnly`] on a changed page: composed and digested, nothing written.
    Composed,
}

/// A successful flatten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlattenToFileReport {
    /// The flattened page (`Page::idx`).
    pub page_idx: usize,
    /// Digest of the flattened straight RGBA8 composite, taken before any encoding (comparable with
    /// `ms_project::SourceImageFacts::opened_digest`).
    pub digest: PixelDigest,
    /// What was done with the composite.
    pub outcome: FlattenOutcome,
}

/// Why a flatten-to-file was refused or failed. `Display` is technical English for logs; the caller
/// picks the localized user text by variant.
#[derive(Debug, thiserror::Error)]
pub enum FlattenToFileError {
    /// Refused at dispatch: a flatten or a project export is running or pending, a project save is
    /// busy, or an input of the composite is not ready yet (call `prepare_flatten_to_file` for the
    /// same page until it reports `Ready`). Nothing was started.
    #[error("the typing tab is busy (flatten, export, save, or inputs still loading)")]
    Busy,
    /// `page_idx` is not a page of the open project. Nothing was started.
    #[error("page {0} is not a page of the open project")]
    NoSuchPage(usize),
    /// The page could not be composed (source page or клин unreadable / undecodable); the message is
    /// the export pipeline's (localized) error text.
    #[error("the page could not be composed: {0}")]
    Compose(String),
    /// The composed page could not be encoded.
    #[error("the composed page could not be encoded: {0}")]
    Encode(#[from] EncodeError),
    /// The encoded bytes could not be written to `path` (the canonicalized target); the previous
    /// file, if any, is intact and no temp file is left behind.
    #[error("could not write {}: {reason}", path.display())]
    Write {
        /// The path the write was attempted at.
        path: PathBuf,
        /// Technical cause.
        reason: String,
        /// The OS refused the write for lack of permission or because the file system is
        /// read-only (`io::ErrorKind::PermissionDenied` / `ReadOnlyFilesystem`): the caller may
        /// offer another location instead of retrying in place.
        permission_denied: bool,
    },
    /// The worker ended without reporting a result (it panicked); whether the target was replaced
    /// is unknown, though an atomic write never leaves a torn file.
    #[error("the flatten-to-file worker ended without a result")]
    WorkerLost,
}

/// Progress of the whole-chapter layer-preload pass a flatten preparation started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FlattenLayersPrep {
    /// No pass was started for the current preparation.
    #[default]
    Idle,
    /// This preparation started a pass that is still running (or has not been observed draining).
    PassRunning,
    /// The pass drained, or could not start (no doc / no layers dir wired; logged): the layer inputs
    /// are as resident as they will get, exactly the export's give-up semantics.
    PassDone,
}

/// Flatten-to-file state owned by `TypingTabState` (not by the overlay layer, whose chapter (re)load
/// resets its export state: an in-flight write must still be reported).
#[derive(Default)]
pub(super) struct FlattenToFileState {
    /// Result channel of the in-flight worker; `Some` while one runs.
    rx: Option<Receiver<Result<FlattenToFileReport, FlattenToFileError>>>,
    /// Layer-preload pass tracker of the current preparation; reset on dispatch.
    layers_prep: FlattenLayersPrep,
}

impl FlattenToFileState {
    /// True while a flatten worker runs.
    pub(super) fn is_running(&self) -> bool {
        self.rx.is_some()
    }
}

/// Everything the flatten-to-file dispatch gate reads, as plain values (the pure core of the gate).
#[derive(Debug, Clone, Copy)]
struct FlattenGate {
    /// A flatten worker is running.
    flatten_running: bool,
    /// A project export is running or deferred.
    export_busy: bool,
    /// The chapter loader and migration have settled (`chapter_load_settled`).
    chapter_settled: bool,
    /// The layer inputs are resident (page resident, or the preparation's pass drained).
    layers_ready: bool,
    /// A whole-chapter layer-preload pass is running.
    preload_active: bool,
    /// The clip-mask loader has drained for this chapter.
    masks_ready: bool,
    /// A project save is pending or in flight.
    save_busy: bool,
}

/// The flatten-to-file dispatch decision: the project export's gate (`export_dispatch_ready`: preload
/// pass drained, masks loaded, no save, and — for the one flattened page — a projection current with
/// the shared document, so the job never composes a stale snapshot of a PS-tab edit) plus mutual
/// exclusion with a running/deferred export and with another flatten, a settled chapter load and
/// resident layer inputs (so building the job never falls back to a synchronous page load on the GUI
/// thread).
///
/// `doc_synced` answers whether the flattened page's projection is current with the shared document
/// (`page_projection_current`, possibly after re-projecting it). It is LAZY and evaluated last, only
/// when every cheap term of `gate` holds (short-circuit), exactly like the export's `docs_synced`:
/// the prepare form re-projects a stale page, which must not run every frame of the preload pass.
fn flatten_dispatch_ready(gate: FlattenGate, doc_synced: impl FnOnce() -> bool) -> bool {
    !gate.flatten_running && !gate.export_busy && gate.chapter_settled && gate.layers_ready && export_dispatch_ready(gate.preload_active, gate.masks_ready, gate.save_busy, doc_synced)
}

impl TypingTabState {
    /// Starts and advances everything a flatten of page `page_idx` reads, and reports whether
    /// [`Self::request_flatten_to_file`] for that page may dispatch now. Call it EVERY frame (from
    /// any active tab) while a save is being prepared: the typing tab's own `draw` drives these
    /// loaders only while it is drawn, so without this a flatten requested from another tab would
    /// wait forever (plan R6).
    ///
    /// Each call: wires the overlay and clip-mask loaders for `project` (idempotent), polls the
    /// overlay loader, the eager migration and the mask loader, applies a bounded batch of the
    /// whole-chapter layer preload, starts that preload once when pages are not resident, and
    /// and — LAST, only once every cheap gate term holds — re-projects `page_idx` from the shared
    /// `LayerDoc` when another tab (the PS editor) changed that page since this tab last projected it,
    /// first polling the in-flight jobs that hold that re-projection back. Not ready while that page's
    /// projection lags the document.
    /// Polling here as well as in `draw` is harmless (non-blocking `try_recv`s). Requests a repaint
    /// while not ready so the frame loop keeps draining the workers.
    ///
    /// `ctx` is needed by the mask loader's error reporting and for the repaint request. A
    /// `page_idx` that is not a page of `project` never becomes a problem here: the request refuses
    /// it with `NoSuchPage`.
    pub fn prepare_flatten_to_file(&mut self, ctx: &egui::Context, project: &ProjectData, page_idx: usize) -> FlattenReadiness {
        self.text_overlays.ensure_loader_started(project);
        self.mask_layer.ensure_loader_started(project);
        self.text_overlays.poll_loader();
        self.text_overlays.poll_migration();
        self.mask_layer.poll_loader(ctx);
        self.text_overlays.drive_page_preload();

        let chapter_settled = self.text_overlays.chapter_load_settled(project);
        let mut preload_active = self.text_overlays.preload_all_pages_active();
        // The preload is started only after the chapter load settled: a migration finishing later
        // would evict the pages the pass just made resident.
        if chapter_settled && !preload_active {
            match self.flatten_to_file.layers_prep {
                FlattenLayersPrep::PassDone => {}
                FlattenLayersPrep::PassRunning => self.flatten_to_file.layers_prep = FlattenLayersPrep::PassDone,
                FlattenLayersPrep::Idle => {
                    if !self.text_overlays.all_pages_loaded(project) {
                        self.text_overlays.begin_preload_all_pages(project);
                        preload_active = self.text_overlays.preload_all_pages_active();
                        self.flatten_to_file.layers_prep = if preload_active {
                            FlattenLayersPrep::PassRunning
                        } else {
                            // `begin_preload_all_pages` already logged why it could not start; the
                            // job's own residency pass composites whatever it can (export semantics).
                            FlattenLayersPrep::PassDone
                        };
                    }
                }
            }
        }
        let layers_ready = !preload_active && (self.flatten_to_file.layers_prep == FlattenLayersPrep::PassDone || self.text_overlays.all_pages_loaded(project));
        let gate = FlattenGate {
            flatten_running: self.flatten_to_file.is_running(),
            export_busy: self.export_in_progress() || self.text_overlays.has_pending_export(),
            chapter_settled,
            layers_ready,
            preload_active,
            masks_ready: self.mask_layer.masks_loaded(project),
            save_busy: self.save_busy,
        };
        // Cross-tab sync without `draw` (review High-1), as the gate's LAST term: a page already
        // resident in this tab's projection is never re-read by `build_page_job`, so an edit the PS
        // tab made in the shared document since typing last projected THIS page must be re-projected
        // here, or the file would be written from the stale projection. The flattened page need not
        // be the canvas's current one (an image set saves every page), so the page is named
        // explicitly. Deferred behind the cheap terms (S-050): none of them depends on the polls the
        // drive runs, and while the preload pass inserts pages a re-projection is wasted work.
        let text_overlays = &mut self.text_overlays;
        if flatten_dispatch_ready(gate, || {
            text_overlays.drive_doc_sync_without_draw(ctx, page_idx);
            text_overlays.page_projection_current(page_idx)
        }) {
            return FlattenReadiness::Ready;
        }
        ctx.request_repaint();
        let (done, total) = if preload_active { self.text_overlays.preload_all_pages_progress() } else { (0, 0) };
        FlattenReadiness::Preparing { done, total }
    }

    /// Dispatches the flatten of `request.page_idx` towards `request.destination` on a worker thread.
    ///
    /// The GUI thread only snapshots: the page job (`build_page_job`: on-screen rasters, overlays,
    /// bands, groups — the exact project-export composite), the page's clip mask and the layer-saver
    /// handle. The worker barriers the saver (as the project export does), resolves the клин and the
    /// page source, flattens and digests the page, then — unless the digest equals
    /// `request.unchanged_if` or the destination is compose-only — encodes and writes atomically
    /// ([`flatten_job_to_file`]). Collect the outcome with [`Self::poll_flatten_to_file`]. While it
    /// runs a project export is deferred / refused. Prepare the SAME page first
    /// ([`Self::prepare_flatten_to_file`]).
    ///
    /// # Errors
    /// [`FlattenToFileError::NoSuchPage`] when `page_idx` is not a page of `project` (checked first);
    /// [`FlattenToFileError::Busy`] when another flatten or a project export runs or is deferred, a
    /// project save is busy, or an input is not ready (see [`Self::prepare_flatten_to_file`]). Both
    /// refuse before anything is started; they are logged.
    pub fn request_flatten_to_file(&mut self, ctx: &egui::Context, project: &ProjectData, request: FlattenToFileRequest) -> Result<(), FlattenToFileError> {
        let FlattenToFileRequest { page_idx, destination, encoding, unchanged_if } = request;
        if !project.pages.iter().any(|page| page.idx == page_idx) {
            ms_log::runtime_log::log_warn(format!("[typing] flatten to file refused: page {page_idx} is not a page of the open project.\nDestination: {}", destination_label(&destination)));
            return Err(FlattenToFileError::NoSuchPage(page_idx));
        }
        let preload_active = self.text_overlays.preload_all_pages_active();
        let gate = FlattenGate {
            flatten_running: self.flatten_to_file.is_running(),
            export_busy: self.export_in_progress() || self.text_overlays.has_pending_export(),
            chapter_settled: self.text_overlays.chapter_load_settled(project),
            layers_ready: !preload_active && (self.flatten_to_file.layers_prep == FlattenLayersPrep::PassDone || self.text_overlays.all_page_indices_resident(&[page_idx])),
            preload_active,
            masks_ready: self.mask_layer.masks_loaded(project),
            save_busy: self.save_busy,
        };
        // `None` in the refusal log: a cheap term failed first, so the projection was not consulted.
        let mut doc_synced = None;
        if !flatten_dispatch_ready(gate, || *doc_synced.insert(self.text_overlays.page_projection_current(page_idx))) {
            ms_log::runtime_log::log_warn(format!(
                "[typing] flatten to file refused: busy or not prepared.\nPage: {page_idx}\nDestination: {}\nGate: {gate:?}\nDoc synced: {doc_synced:?}\nPossible cause: the caller did not wait for prepare_flatten_to_file(page {page_idx}) to report Ready",
                destination_label(&destination)
            ));
            return Err(FlattenToFileError::Busy);
        }
        let mask = self.mask_layer.export_mask_snapshot_for_page(page_idx);
        // The PSD font index is irrelevant here (the flatten never names fonts), so it stays empty.
        let Some(job) = self.text_overlays.build_page_job(project, page_idx, None, mask, TypingExportFormat::Png, crate::psd_export::FontPostScriptNames::default()) else {
            // Unreachable after the page check above; `build_page_job` logged it.
            return Err(FlattenToFileError::NoSuchPage(page_idx));
        };
        let clean_overlays_model = self.text_overlays.clean_overlays_model.clone();
        // Cheap `Sender` clone under a brief lock; the barrier itself blocks only the worker.
        let saver_handle = self.layer_doc.as_ref().and_then(|doc| doc.lock().ok().and_then(|guard| guard.saver_handle()));
        ms_log::runtime_log::log_info(format!(
            "[typing] flatten to file: dispatch.\nPage: {page_idx}\nDestination: {}\nSkip if unchanged: {}\nFormat: {:?}\nAlpha: {:?}\nICC: {}",
            destination_label(&destination),
            unchanged_if.is_some(),
            encoding.format,
            encoding.alpha,
            encoding.icc_profile.as_ref().map_or(0, Vec::len)
        ));
        let (tx, rx) = mpsc::channel();
        let repaint_ctx = ctx.clone();
        thread::spawn(move || {
            if let Some(handle) = saver_handle {
                let failed_pages = handle.barrier_blocking();
                if !failed_pages.is_empty() {
                    ms_log::runtime_log::log_warn(format!(
                        "[typing] flatten to file: the layer saver reported failed staging writes for pages {failed_pages:?}; a page that falls back to the staging manifest may compose its previous layer order"
                    ));
                }
            }
            let result = flatten_job_to_file(job, clean_overlays_model.as_ref(), &destination, &encoding, unchanged_if);
            // The receiver is gone only when the tab state was dropped (app closing / rebuilt); there
            // is nobody left to report to, and the outcome is already logged by `flatten_job_to_file`.
            let _ = tx.send(result);
            repaint_ctx.request_repaint();
        });
        self.flatten_to_file.rx = Some(rx);
        self.flatten_to_file.layers_prep = FlattenLayersPrep::Idle;
        Ok(())
    }

    /// Non-blocking poll of the in-flight flatten: `None` while it runs or when none was requested,
    /// otherwise its outcome (exactly once).
    pub fn poll_flatten_to_file(&mut self) -> Option<Result<FlattenToFileReport, FlattenToFileError>> {
        let rx = self.flatten_to_file.rx.as_ref()?;
        match rx.try_recv() {
            Ok(result) => {
                self.flatten_to_file.rx = None;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.flatten_to_file.rx = None;
                ms_log::runtime_log::log_error("[typing] flatten to file: the worker ended without a result.\nPossible cause: a panic while composing or writing the page");
                Some(Err(FlattenToFileError::WorkerLost))
            }
        }
    }
}

/// A log label for `destination` (the path, or "compose only").
fn destination_label(destination: &FlattenDestination) -> String {
    match destination {
        FlattenDestination::File(target) => target.display().to_string(),
        FlattenDestination::ComposeOnly => "compose only".to_string(),
    }
}

/// Worker body of a flatten: fills the job's клин snapshot (model overlay, else the file the
/// overlay loader would load), resolves and flattens the page exactly as the project export does
/// and digests the straight RGBA8 composite. Then: `Unchanged` when the digest equals
/// `unchanged_if`; `Composed` for [`FlattenDestination::ComposeOnly`]; otherwise encodes with
/// `encoding` and writes to the target (see [`write_to_target`]) for `Written`. Every outcome and
/// failure is logged here with the page, the destination and the cause (never pixels). Blocking
/// I/O and a full-page hash: worker only.
///
/// # Errors
/// `Compose` (клин or page source unreadable), `Encode`, `Write` (the last two only on the path
/// that writes).
pub(super) fn flatten_job_to_file(mut job: TypingExportPageJob, clean_overlays_model: Option<&Arc<Mutex<CleanOverlaysModel>>>, destination: &FlattenDestination, encoding: &ImageEncoding, unchanged_if: Option<PixelDigest>) -> Result<FlattenToFileReport, FlattenToFileError> {
    let page_idx = job.page_idx;
    let result = compose_encode_write(&mut job, clean_overlays_model, destination, encoding, unchanged_if);
    match &result {
        Ok(report) => match &report.outcome {
            FlattenOutcome::Written { target, bytes_written } => ms_log::runtime_log::log_info(format!("[typing] flatten to file: written.\nPage: {page_idx}\nTarget: {}\nBytes: {bytes_written}", target.display())),
            FlattenOutcome::Unchanged => ms_log::runtime_log::log_info(format!("[typing] flatten to file: page unchanged since the given digest; nothing encoded or written.\nPage: {page_idx}\nDestination: {}", destination_label(destination))),
            FlattenOutcome::Composed => ms_log::runtime_log::log_info(format!("[typing] flatten to file: page composed (compose only); nothing written.\nPage: {page_idx}")),
        },
        Err(err) => ms_log::runtime_log::log_error(format!(
            "[typing] flatten to file failed.\nPage: {page_idx}\nSource: {}\nDestination: {}\nFormat: {:?}\nError: {err}",
            job.page_path.display(),
            destination_label(destination),
            encoding.format
        )),
    }
    result
}

/// The un-logged steps of [`flatten_job_to_file`].
fn compose_encode_write(job: &mut TypingExportPageJob, clean_overlays_model: Option<&Arc<Mutex<CleanOverlaysModel>>>, destination: &FlattenDestination, encoding: &ImageEncoding, unchanged_if: Option<PixelDigest>) -> Result<FlattenToFileReport, FlattenToFileError> {
    prepare_export_clean_overlay_snapshots(std::slice::from_mut(job), clean_overlays_model.cloned()).map_err(FlattenToFileError::Compose)?;
    let flat = flatten_page_rgba(resolve_flatten_input(job).map_err(FlattenToFileError::Compose)?);
    // Hashed BEFORE encoding: the same straight RGBA8 layout the open path hashed when it wrote the
    // scratch page, so an untouched page matches its `opened_digest` whatever the target format.
    let digest = PixelDigest::of_rgba8(flat.width(), flat.height(), flat.as_raw());
    let outcome = if unchanged_if == Some(digest) {
        FlattenOutcome::Unchanged
    } else {
        match destination {
            FlattenDestination::ComposeOnly => FlattenOutcome::Composed,
            FlattenDestination::File(target) => {
                let bytes = encode_rgba(&flat, encoding)?;
                write_to_target(target, &bytes)?;
                FlattenOutcome::Written { target: target.clone(), bytes_written: bytes.len() }
            }
        }
    };
    Ok(FlattenToFileReport { page_idx: job.page_idx, digest, outcome })
}

/// True for the I/O error kinds that mean "this location is not writable for us"
/// (`FlattenToFileError::Write::permission_denied`).
fn is_permission_kind(kind: std::io::ErrorKind) -> bool {
    matches!(kind, std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem)
}

/// Atomically replaces `target` with `bytes` (`write_bytes_atomic`, contents fsynced). An EXISTING
/// target is canonicalized first, so a symlink's target file is rewritten and the link survives; a
/// missing target is written as named. The replaced file's permissions (Unix mode bits, the
/// Windows read-only flag) are re-applied to the new file, which the rename otherwise gives the
/// defaults of a new file; a failure to do so is logged and does not fail the save (the bytes are
/// already in place). Owner, ACLs and hard links are not preserved: the rename gives the path a new
/// inode. On error the previous file is intact and no temp is left.
///
/// # Errors
/// `Write` with the attempted path when the existing target cannot be resolved (e.g. a dangling
/// symlink) or the atomic write fails (missing parent, no permission, target is a directory, ...);
/// `permission_denied` is set from the I/O error kind.
fn write_to_target(target: &Path, bytes: &[u8]) -> Result<(), FlattenToFileError> {
    let resolved = match std::fs::symlink_metadata(target) {
        Ok(_) => std::fs::canonicalize(target).map_err(|err| FlattenToFileError::Write { path: target.to_path_buf(), reason: format!("cannot resolve the existing target: {err}"), permission_denied: is_permission_kind(err.kind()) })?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => target.to_path_buf(),
        Err(err) => return Err(FlattenToFileError::Write { path: target.to_path_buf(), reason: format!("cannot inspect the target: {err}"), permission_denied: is_permission_kind(err.kind()) }),
    };
    // Captured before the rename replaces the file. A missing file has none to keep; any other
    // inspection error only costs the permission carry-over, so it is logged, not fatal.
    let previous_permissions = match std::fs::metadata(&resolved) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!("[typing] flatten to file: could not read the existing file's permissions; the new file keeps the defaults.\nPath: {}\nError: {err}", resolved.display()));
            None
        }
    };
    write_bytes_atomic(&resolved, bytes, Durability::Contents).map_err(|err| {
        let permission_denied = match &err {
            AtomicWriteError::TempWrite { kind, .. } | AtomicWriteError::Rename { kind, .. } => is_permission_kind(*kind),
            // Not produced with `Durability::Contents`; a directory fsync failure is not a
            // permission verdict on the target either way.
            AtomicWriteError::DirSync { .. } => false,
        };
        FlattenToFileError::Write { path: resolved.clone(), reason: err.to_string(), permission_denied }
    })?;
    if let Some(permissions) = previous_permissions
        && let Err(err) = std::fs::set_permissions(&resolved, permissions)
    {
        ms_log::runtime_log::log_warn(format!("[typing] flatten to file: the file was written but its previous permissions could not be restored.\nPath: {}\nError: {err}", resolved.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_encode::{AlphaPolicy, ImageSaveFormat};

    /// Page side of the fixture page, px.
    const PAGE_PX: u32 = 16;
    /// Max per-channel difference accepted after a JPEG round trip of the flat fixture regions.
    const JPEG_TOLERANCE: u8 = 8;

    /// A fresh, uniquely named directory under the system temp dir; removed by [`TempDir::drop`].
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ms_typing_flatten_to_file_{tag}_{}", std::process::id()));
            // A leftover from a crashed earlier run of this exact test; absence is the normal case.
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create test dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // Best-effort cleanup of a temp-dir fixture; a failure only leaves files in the OS temp dir.
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn encoding(format: ImageSaveFormat) -> ImageEncoding {
        ImageEncoding { format, jpeg_quality: 95, alpha: AlphaPolicy::DropIfOpaque, icc_profile: None }
    }

    /// A page-0 job over an opaque grey page with one opaque red overlay square in the middle.
    fn job_with_overlay(dir: &Path) -> TypingExportPageJob {
        let page_path = dir.join("page.png");
        image::RgbaImage::from_pixel(PAGE_PX, PAGE_PX, image::Rgba([90, 90, 90, 255])).save(&page_path).expect("write page");
        let overlay = TypingExportOverlaySnapshot {
            page_idx: 0,
            center_page_px: [8.0, 8.0],
            mask_clip_enabled: false,
            layer_idx: 0,
            user_scale: 1.0,
            angle_deg: 0.0,
            deform_mesh: None,
            size_px: [8, 8],
            source_rgba: [220u8, 20, 30, 255].repeat(64),
            render_data_json: None,
            uid: "t0".to_string(),
            group_uid: None,
            visible: true,
        };
        TypingExportPageJob {
            page_idx: 0,
            page_path,
            output_path: None,
            clean_paths: None,
            clean_overlay_rgba: None,
            overlays: vec![overlay],
            rasters: Vec::new(),
            bands: vec![ms_models::layer_model::ordering::Band::PinnedText { uid: "t0".to_string(), z: 0 }],
            groups: Vec::new(),
            mask: None,
            export_format: TypingExportFormat::Png,
            layers_primary_dir: None,
            layers_fallback_dir: None,
            font_post_script_names: Default::default(),
        }
    }

    /// `flatten_job_to_file` into a file, with no "unchanged" digest (always writes).
    fn to_file(job: TypingExportPageJob, target: &Path, enc: &ImageEncoding) -> Result<FlattenToFileReport, FlattenToFileError> {
        flatten_job_to_file(job, None, &FlattenDestination::File(target.to_path_buf()), enc, None)
    }

    /// The `(target, bytes_written)` of a `Written` outcome; panics on any other outcome.
    fn written(report: &FlattenToFileReport) -> (&Path, usize) {
        match &report.outcome {
            FlattenOutcome::Written { target, bytes_written } => (target.as_path(), *bytes_written),
            other @ (FlattenOutcome::Unchanged | FlattenOutcome::Composed) => panic!("expected Written, got {other:?}"),
        }
    }

    fn expected_composite(job: &TypingExportPageJob) -> image::RgbaImage {
        flatten_page_rgba(resolve_flatten_input(job).expect("resolve fixture"))
    }

    fn temp_artifacts(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir).expect("list dir").map(|entry| entry.expect("dir entry").path()).filter(|path| ms_docstore::is_temp_artifact(path)).collect()
    }

    #[test]
    fn writes_png_and_webp_that_decode_to_the_composite() {
        let dir = TempDir::new("lossless");
        let job = job_with_overlay(&dir.0);
        let expected = expected_composite(&job);
        // The fixture really composites the overlay (not just the page).
        assert_eq!(expected.get_pixel(8, 8).0, [220, 20, 30, 255]);
        assert_eq!(expected.get_pixel(1, 1).0, [90, 90, 90, 255]);
        for (format, name) in [(ImageSaveFormat::Png, "out.png"), (ImageSaveFormat::WebpLossless, "out.webp")] {
            let target = dir.0.join(name);
            let report = to_file(job_with_overlay(&dir.0), &target, &encoding(format)).expect("flatten to file");
            assert_eq!(written(&report).0, target);
            let bytes = std::fs::read(&target).expect("read written file");
            assert_eq!(written(&report).1, bytes.len());
            let decoded = image::load_from_memory(&bytes).expect("decode written file").to_rgba8();
            assert_eq!(decoded, expected, "{format:?} round trip");
        }
        assert!(temp_artifacts(&dir.0).is_empty());
    }

    #[test]
    fn writes_jpeg_that_decodes_to_the_composite_within_tolerance() {
        let dir = TempDir::new("jpeg");
        let job = job_with_overlay(&dir.0);
        let expected = expected_composite(&job);
        let target = dir.0.join("out.jpg");
        to_file(job, &target, &encoding(ImageSaveFormat::Jpeg)).expect("flatten to file");
        let decoded = image::open(&target).expect("decode jpeg").to_rgb8();
        // Probe the interiors of the flat regions, away from the overlay edge where JPEG blends.
        for (x, y) in [(1u32, 1u32), (14, 14), (8, 8), (9, 7)] {
            let want = expected.get_pixel(x, y);
            let got = decoded.get_pixel(x, y);
            for channel in 0..3 {
                assert!(want[channel].abs_diff(got[channel]) <= JPEG_TOLERANCE, "({x},{y}) channel {channel}: want {} got {}", want[channel], got[channel]);
            }
        }
    }

    #[test]
    fn missing_target_dir_is_a_write_error_and_creates_nothing() {
        let dir = TempDir::new("missing_dir");
        let job = job_with_overlay(&dir.0);
        let missing = dir.0.join("no_such_dir");
        let target = missing.join("out.png");
        let err = to_file(job, &target, &encoding(ImageSaveFormat::Png)).expect_err("parent is missing");
        assert!(matches!(&err, FlattenToFileError::Write { path, permission_denied: false, .. } if path == &target), "{err:?}");
        assert!(!missing.exists(), "no directory may be created");
        assert!(temp_artifacts(&dir.0).is_empty());
    }

    #[test]
    fn failed_write_keeps_the_old_file_and_leaves_no_temp() {
        let dir = TempDir::new("old_kept");
        let job = job_with_overlay(&dir.0);
        // A directory squatting on the target name: the final rename must fail, after the temp
        // was written, which exercises the cleanup path of the atomic write.
        let target = dir.0.join("out.png");
        std::fs::create_dir(&target).expect("create blocking dir");
        let marker = target.join("old.txt");
        std::fs::write(&marker, b"old").expect("write marker");
        let err = to_file(job, &target, &encoding(ImageSaveFormat::Png)).expect_err("target is a directory");
        assert!(matches!(err, FlattenToFileError::Write { .. }), "{err:?}");
        assert_eq!(std::fs::read(&marker).expect("old content still there"), b"old");
        assert!(temp_artifacts(&dir.0).is_empty(), "the temp file must be removed");
    }

    /// A target directory the process may not write into: `Write` with `permission_denied`, no file
    /// and no temp. Skipped (with a note) when the process can write anyway, e.g. running as root.
    #[cfg(unix)]
    #[test]
    fn read_only_target_dir_is_a_permission_denied_write_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("read_only_dir");
        let locked = dir.0.join("locked");
        std::fs::create_dir(&locked).expect("create dir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).expect("chmod read-only");
        if std::fs::write(locked.join("probe"), b"x").is_ok() {
            eprintln!("skipped: this process can write into a read-only directory (running as root?)");
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
            return;
        }
        let target = locked.join("out.png");
        let result = to_file(job_with_overlay(&dir.0), &target, &encoding(ImageSaveFormat::Png));
        // Restore write access first so `TempDir` can clean up whatever the assertions do.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
        let err = result.expect_err("a read-only directory must refuse the write");
        assert!(matches!(&err, FlattenToFileError::Write { permission_denied: true, .. }), "{err:?}");
        assert!(!target.exists());
        assert!(temp_artifacts(&locked).is_empty());
    }

    #[test]
    fn existing_target_is_replaced() {
        let dir = TempDir::new("replace");
        let job = job_with_overlay(&dir.0);
        let target = dir.0.join("out.png");
        std::fs::write(&target, b"previous bytes").expect("write old file");
        to_file(job_with_overlay(&dir.0), &target, &encoding(ImageSaveFormat::Png)).expect("flatten to file");
        let decoded = image::open(&target).expect("decode").to_rgba8();
        assert_eq!(decoded, expected_composite(&job));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_target_rewrites_the_link_target_and_keeps_the_link() {
        let dir = TempDir::new("symlink");
        let job = job_with_overlay(&dir.0);
        let real = dir.0.join("real.png");
        std::fs::write(&real, b"previous bytes").expect("write real file");
        let link = dir.0.join("link.png");
        std::os::unix::fs::symlink(&real, &link).expect("create symlink");
        let report = to_file(job_with_overlay(&dir.0), &link, &encoding(ImageSaveFormat::Png)).expect("flatten to file");
        assert_eq!(written(&report).0, link, "the report names the path the user chose");
        assert!(std::fs::symlink_metadata(&link).expect("link metadata").file_type().is_symlink(), "the link must survive");
        assert_eq!(image::open(&real).expect("decode real").to_rgba8(), expected_composite(&job));
    }

    /// Review Low-2: an in-place save keeps the replaced file's permission bits (the atomic rename
    /// would otherwise give the new file the defaults of a fresh file).
    #[cfg(unix)]
    #[test]
    fn replacing_an_existing_file_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("permissions");
        let target = dir.0.join("out.png");
        std::fs::write(&target, b"previous bytes").expect("write old file");
        // Distinct from both a umask default (0644) and a private temp file's (0600).
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).expect("chmod");
        to_file(job_with_overlay(&dir.0), &target, &encoding(ImageSaveFormat::Png)).expect("flatten to file");
        let mode = std::fs::metadata(&target).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn unreadable_page_is_a_compose_error() {
        let dir = TempDir::new("compose");
        let mut job = job_with_overlay(&dir.0);
        job.page_path = dir.0.join("missing_page.png");
        let target = dir.0.join("out.png");
        let err = to_file(job, &target, &encoding(ImageSaveFormat::Png)).expect_err("page is missing");
        assert!(matches!(err, FlattenToFileError::Compose(_)), "{err:?}");
        assert!(!target.exists());
    }

    #[test]
    fn invalid_jpeg_quality_is_an_encode_error() {
        let dir = TempDir::new("encode");
        let job = job_with_overlay(&dir.0);
        let target = dir.0.join("out.jpg");
        let enc = ImageEncoding { jpeg_quality: 0, ..encoding(ImageSaveFormat::Jpeg) };
        let err = to_file(job, &target, &enc).expect_err("quality 0 is invalid");
        assert!(matches!(err, FlattenToFileError::Encode(EncodeError::InvalidJpegQuality(0))), "{err:?}");
        assert!(!target.exists());
    }

    #[test]
    fn dispatch_gate_table() {
        let ready = FlattenGate { flatten_running: false, export_busy: false, chapter_settled: true, layers_ready: true, preload_active: false, masks_ready: true, save_busy: false };
        assert!(flatten_dispatch_ready(ready, || true));
        assert!(!flatten_dispatch_ready(ready, || false), "a stale projection blocks the dispatch");
        let blocked = [
            FlattenGate { flatten_running: true, ..ready },
            FlattenGate { export_busy: true, ..ready },
            FlattenGate { chapter_settled: false, ..ready },
            FlattenGate { layers_ready: false, ..ready },
            FlattenGate { preload_active: true, ..ready },
            FlattenGate { masks_ready: false, ..ready },
            FlattenGate { save_busy: true, ..ready },
        ];
        for gate in blocked {
            // A failed cheap term short-circuits: the (re-projecting) doc term is never evaluated.
            assert!(!flatten_dispatch_ready(gate, || panic!("doc term evaluated although {gate:?} blocks")), "{gate:?}");
        }
    }

    /// A one-page `ProjectData` whose every path lies under `dir` (nothing is created on disk).
    fn project_under(dir: &Path) -> ProjectData {
        let chapter = dir.join("chapter");
        let unsaved = dir.join("chapter_unsaved");
        let paths = ms_project::ProjectPaths {
            project_dir: chapter.clone(),
            title_dir: dir.to_path_buf(),
            notes_file: dir.join("notes.json"),
            char_favorites_file: dir.join("char_favorites.json"),
            color_presets_file: dir.join("color_presets.json"),
            bubbles_file: chapter.join("bubbles.json"),
            src_dir: chapter.join("src"),
            clean_layers_dir: chapter.join("clean_layers"),
            cleaned_dir: chapter.join("cleaned"),
            alt_vers_dir: chapter.join("alt_vers"),
            saved_dir: chapter.join("saved"),
            image_bubbles_dir: chapter.join("image_bubbles"),
            text_images_dir: chapter.join("text_images"),
            layers_dir: chapter.join("layers"),
            text_detection_dir: chapter.join("text_detection"),
            characters_dir: dir.join("characters"),
            terms_file: dir.join("terms.json"),
            settings_file: chapter.join("settings.json"),
            unsaved_dir: unsaved.clone(),
            unsaved_bubbles_file: unsaved.join("bubbles.json"),
            unsaved_clean_layers_dir: unsaved.join("clean_layers"),
            unsaved_image_bubbles_dir: unsaved.join("image_bubbles"),
            unsaved_text_images_dir: unsaved.join("text_images"),
            unsaved_layers_dir: unsaved.join("layers"),
        };
        ProjectData {
            project_dir: chapter.clone(),
            image_dir: paths.src_dir.clone(),
            pages: vec![ms_project::Page { idx: 0, path: paths.src_dir.join("001.png") }],
            bubbles: Arc::new(Vec::new()),
            paths,
            comic_type: None,
            canvas_settings: ms_project::CanvasSettings::default(),
            settings_data: serde_json::Value::Null,
            session: ms_project::SessionKind::Project,
        }
    }

    fn request(dir: &Path) -> FlattenToFileRequest {
        FlattenToFileRequest { page_idx: 0, destination: FlattenDestination::File(dir.join("out.png")), encoding: encoding(ImageSaveFormat::Png), unchanged_if: None }
    }

    #[test]
    fn request_is_refused_while_a_flatten_runs() {
        let dir = TempDir::new("busy_flatten");
        let project = project_under(&dir.0);
        let mut state = TypingTabState::default();
        let (_tx, rx) = mpsc::channel();
        state.flatten_to_file.rx = Some(rx);
        let result = state.request_flatten_to_file(&egui::Context::default(), &project, request(&dir.0));
        assert!(matches!(result, Err(FlattenToFileError::Busy)), "{result:?}");
        assert!(state.flatten_to_file.is_running(), "the running job is untouched");
        assert!(!dir.0.join("out.png").exists());
    }

    #[test]
    fn request_is_refused_while_a_project_export_runs() {
        let dir = TempDir::new("busy_export");
        let project = project_under(&dir.0);
        let mut state = TypingTabState::default();
        let (_tx, rx) = mpsc::channel();
        state.text_overlays.export_rx = Some(TypingExportRenderState { rx });
        let result = state.request_flatten_to_file(&egui::Context::default(), &project, request(&dir.0));
        assert!(matches!(result, Err(FlattenToFileError::Busy)), "{result:?}");
        assert!(!state.flatten_to_file.is_running());
    }

    #[test]
    fn request_is_refused_before_the_inputs_are_prepared() {
        let dir = TempDir::new("unprepared");
        let project = project_under(&dir.0);
        let mut state = TypingTabState::default();
        // Nothing was prepared: no chapter load, no masks.
        let result = state.request_flatten_to_file(&egui::Context::default(), &project, request(&dir.0));
        assert!(matches!(result, Err(FlattenToFileError::Busy)), "{result:?}");
        assert!(!state.flatten_to_file.is_running());
    }

    /// Plan R6: the whole pipeline — loaders, preload, mask loader, dispatch, worker, poll — runs to
    /// completion through this API alone, WITHOUT `TypingTabState::draw` ever being called (the
    /// single-image save may be triggered from any tab).
    #[test]
    fn prepare_request_poll_completes_without_the_typing_tab_drawing() {
        let dir = TempDir::new("end_to_end");
        let project = project_under(&dir.0);
        std::fs::create_dir_all(&project.paths.src_dir).expect("create src dir");
        let page = image::RgbaImage::from_pixel(PAGE_PX, PAGE_PX, image::Rgba([40, 80, 120, 255]));
        page.save(&project.pages[0].path).expect("write page");
        let mut state = TypingTabState::default();
        state.set_layer_doc(Arc::new(Mutex::new(ms_models::layer_model::layer_doc::LayerDoc::new())));
        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while state.prepare_flatten_to_file(&ctx, &project, 0) != FlattenReadiness::Ready {
            assert!(std::time::Instant::now() < deadline, "preparation never became ready");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(state.mask_layer.masks_loaded(&project), "the mask loader drained without draw");
        let target = dir.0.join("out.png");
        state.request_flatten_to_file(&ctx, &project, FlattenToFileRequest { page_idx: 0, destination: FlattenDestination::File(target.clone()), encoding: encoding(ImageSaveFormat::Png), unchanged_if: None }).expect("dispatch");
        assert!(state.flatten_to_file_in_progress());
        assert_eq!(state.prepare_flatten_to_file(&ctx, &project, 0), FlattenReadiness::Preparing { done: 0, total: 0 }, "not ready while a flatten runs");
        let second = state.request_flatten_to_file(&ctx, &project, FlattenToFileRequest { page_idx: 0, destination: FlattenDestination::File(target.clone()), encoding: encoding(ImageSaveFormat::Png), unchanged_if: None });
        assert!(matches!(second, Err(FlattenToFileError::Busy)), "{second:?}");
        let outcome = loop {
            if let Some(outcome) = state.poll_flatten_to_file() {
                break outcome;
            }
            assert!(std::time::Instant::now() < deadline, "the worker never reported");
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        let report = outcome.expect("flatten succeeded");
        assert_eq!(written(&report).0, target);
        assert_eq!(report.page_idx, 0);
        assert_eq!(image::open(&target).expect("decode").to_rgba8(), page);
        assert!(!state.flatten_to_file_in_progress());
        let wrong_page = state.request_flatten_to_file(&ctx, &project, FlattenToFileRequest { page_idx: 7, destination: FlattenDestination::File(target), encoding: encoding(ImageSaveFormat::Png), unchanged_if: None });
        assert!(matches!(wrong_page, Err(FlattenToFileError::NoSuchPage(7))), "{wrong_page:?}");
    }

    /// Prepares `page_idx` until `Ready`, dispatches it into `target` and waits for the worker's outcome.
    fn flatten_until_done(state: &mut TypingTabState, ctx: &egui::Context, project: &ProjectData, page_idx: usize, target: &Path) -> Result<FlattenToFileReport, FlattenToFileError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while state.prepare_flatten_to_file(ctx, project, page_idx) != FlattenReadiness::Ready {
            assert!(std::time::Instant::now() < deadline, "preparation never became ready");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        state.request_flatten_to_file(ctx, project, FlattenToFileRequest { page_idx, destination: FlattenDestination::File(target.to_path_buf()), encoding: encoding(ImageSaveFormat::Png), unchanged_if: None })?;
        loop {
            if let Some(outcome) = state.poll_flatten_to_file() {
                return outcome;
            }
            assert!(std::time::Instant::now() < deadline, "the worker never reported");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Review High-1: another tab (the PS editor) edits a raster through the shared document while
    /// the typing tab is NOT drawn. The page is already resident in typing's projection, so without a
    /// re-projection on the flatten path the written file would be the pre-edit composite.
    #[test]
    fn flatten_reflects_a_doc_edit_made_while_the_typing_tab_is_not_drawn() {
        use ms_models::layer_model::layer_doc::LayerDoc;
        let dir = TempDir::new("doc_edit");
        let project = project_under(&dir.0);
        std::fs::create_dir_all(&project.paths.src_dir).expect("create src dir");
        image::RgbaImage::from_pixel(PAGE_PX, PAGE_PX, image::Rgba([40, 80, 120, 255])).save(&project.pages[0].path).expect("write page");
        let transform = ms_models::layer_model::manifest::TransformRec { cx: 8.0, cy: 8.0, rotation: 0.0, scale: 1.0 };
        let blue = egui::ColorImage::filled([4, 4], egui::Color32::from_rgb(0, 0, 255));
        ms_models::layer_model::persist::add_page_raster(&project.paths.unsaved_layers_dir, None, &ms_models::layer_model::persist::PageRasterSpec { page_idx: 0, uid: "r0", name: "R", visible: true, opacity: 1.0, transform, image: &blue }).expect("seed raster");
        let doc = Arc::new(Mutex::new(LayerDoc::new()));
        let mut state = TypingTabState::default();
        state.set_layer_doc(Arc::clone(&doc));
        let ctx = egui::Context::default();

        let first = dir.0.join("first.png");
        flatten_until_done(&mut state, &ctx, &project, 0, &first).expect("first flatten");
        assert_eq!(image::open(&first).expect("decode first").to_rgba8().get_pixel(8, 8).0, [0, 0, 255, 255], "the seeded raster is composited");

        // The PS editor's edit path: new pixels straight into the shared document, version bumped.
        let green = egui::ColorImage::filled([4, 4], egui::Color32::from_rgb(0, 255, 0));
        doc.lock().expect("doc lock").set_raster_pixels(0, "r0", green.clone(), green, Vec::new(), true);

        let second = dir.0.join("second.png");
        flatten_until_done(&mut state, &ctx, &project, 0, &second).expect("second flatten");
        assert_eq!(image::open(&second).expect("decode second").to_rgba8().get_pixel(8, 8).0, [0, 255, 0, 255], "the flatten must compose the document's current pixels");
    }

    #[test]
    fn poll_reports_a_lost_worker_once() {
        let mut state = TypingTabState::default();
        assert!(state.poll_flatten_to_file().is_none(), "nothing requested");
        let (tx, rx) = mpsc::channel::<Result<FlattenToFileReport, FlattenToFileError>>();
        state.flatten_to_file.rx = Some(rx);
        assert!(state.poll_flatten_to_file().is_none(), "still running");
        drop(tx);
        assert!(matches!(state.poll_flatten_to_file(), Some(Err(FlattenToFileError::WorkerLost))));
        assert!(state.poll_flatten_to_file().is_none(), "reported exactly once");
    }

    /// A page job with no layers, overlay, клин or mask over `page_path` (an untouched page).
    fn bare_job(page_idx: usize, page_path: PathBuf) -> TypingExportPageJob {
        TypingExportPageJob {
            page_idx,
            page_path,
            output_path: None,
            clean_paths: None,
            clean_overlay_rgba: None,
            overlays: Vec::new(),
            rasters: Vec::new(),
            bands: Vec::new(),
            groups: Vec::new(),
            mask: None,
            export_format: TypingExportFormat::Png,
            layers_primary_dir: None,
            layers_fallback_dir: None,
            font_post_script_names: Default::default(),
        }
    }

    /// The untouched-page contract the image-set Save relies on: opening an image (opaque PNG,
    /// JPEG, translucent PNG with fully transparent pixels that still carry colour) and flattening
    /// its scratch page with nothing on it gives EXACTLY the digest the open path recorded, so Save
    /// skips the page instead of re-encoding it.
    #[test]
    fn untouched_page_digest_equals_the_opened_digest() {
        use std::sync::atomic::AtomicBool;
        let dir = TempDir::new("untouched_digest");
        let user_dir = dir.0.join("user");
        std::fs::create_dir_all(&user_dir).expect("create user dir");
        let opaque = image::RgbaImage::from_fn(PAGE_PX, PAGE_PX, |x, y| image::Rgba([u8::try_from(x * 15).expect("< 256"), u8::try_from(y * 15).expect("< 256"), 77, 255]));
        let translucent = image::RgbaImage::from_fn(PAGE_PX, PAGE_PX, |x, y| image::Rgba([200, u8::try_from(x * 10).expect("< 256"), 50, u8::try_from((x + y) * 8).expect("< 256")]));
        let opaque_png = user_dir.join("opaque.png");
        opaque.save(&opaque_png).expect("write opaque png");
        let jpeg = user_dir.join("photo.jpg");
        image::DynamicImage::ImageRgba8(opaque.clone()).to_rgb8().save(&jpeg).expect("write jpeg");
        let translucent_png = user_dir.join("translucent.png");
        translucent.save(&translucent_png).expect("write translucent png");
        assert!(translucent.pixels().any(|p| p[3] == 0) && translucent.pixels().any(|p| p[3] != 0 && p[3] != 255), "the fixture covers alpha 0 and partial alpha");

        let scratch = ms_project::single_image::SingleImageScratch::reserve(&dir.0.join("scratch")).expect("reserve scratch");
        let data = ms_project::single_image::open_image_set(&[opaque_png, jpeg, translucent_png], Vec::new(), &scratch, &serde_json::json!({}), &mut |_, _| {}, &AtomicBool::new(false)).expect("open image set");
        let ms_project::SessionKind::SingleImage(session) = data.session() else {
            panic!("an image set opens as a single-image session");
        };
        assert_eq!(data.pages.len(), 3);
        for page in &data.pages {
            let opened = session.source(page.idx).expect("facts for every page").opened_digest;
            let report = flatten_job_to_file(bare_job(page.idx, page.path.clone()), None, &FlattenDestination::ComposeOnly, &encoding(ImageSaveFormat::Png), None).expect("compose");
            assert_eq!(report.page_idx, page.idx);
            assert_eq!(report.digest, opened, "page {} must flatten to its opened pixels", page.idx);
            assert_eq!(report.outcome, FlattenOutcome::Composed);
            let skipped = flatten_job_to_file(bare_job(page.idx, page.path.clone()), None, &FlattenDestination::File(dir.0.join("never.png")), &encoding(ImageSaveFormat::Png), Some(opened)).expect("skip");
            assert_eq!(skipped.outcome, FlattenOutcome::Unchanged);
        }
        assert!(!dir.0.join("never.png").exists(), "an unchanged page is never written");
        scratch.remove().expect("remove scratch");
    }

    /// `unchanged_if` equal to the composite's digest: nothing is encoded or written, whatever the
    /// destination; a different digest proceeds; `ComposeOnly` never writes.
    #[test]
    fn unchanged_digest_skips_and_compose_only_never_writes() {
        let dir = TempDir::new("unchanged");
        let target = dir.0.join("out.png");
        let composed = flatten_job_to_file(job_with_overlay(&dir.0), None, &FlattenDestination::ComposeOnly, &encoding(ImageSaveFormat::Png), None).expect("compose only");
        assert_eq!(composed.outcome, FlattenOutcome::Composed);
        let expected = expected_composite(&job_with_overlay(&dir.0));
        assert_eq!(composed.digest, PixelDigest::of_rgba8(expected.width(), expected.height(), expected.as_raw()), "the digest covers the composite, overlay included");

        let unchanged = flatten_job_to_file(job_with_overlay(&dir.0), None, &FlattenDestination::File(target.clone()), &encoding(ImageSaveFormat::Png), Some(composed.digest)).expect("unchanged");
        assert_eq!(unchanged.outcome, FlattenOutcome::Unchanged);
        assert_eq!(unchanged.digest, composed.digest);
        assert!(!target.exists(), "an unchanged page creates no file");
        // Even an invalid encoding is never reached on the skip path.
        let bad_quality = ImageEncoding { jpeg_quality: 0, ..encoding(ImageSaveFormat::Jpeg) };
        let skipped = flatten_job_to_file(job_with_overlay(&dir.0), None, &FlattenDestination::File(target.clone()), &bad_quality, Some(composed.digest)).expect("skip before encoding");
        assert_eq!(skipped.outcome, FlattenOutcome::Unchanged);

        let other = PixelDigest::of_rgba8(1, 1, &[0, 0, 0, 0]);
        let compose_changed = flatten_job_to_file(job_with_overlay(&dir.0), None, &FlattenDestination::ComposeOnly, &encoding(ImageSaveFormat::Png), Some(other)).expect("compose changed");
        assert_eq!(compose_changed.outcome, FlattenOutcome::Composed);
        assert!(!target.exists());

        let changed = flatten_job_to_file(job_with_overlay(&dir.0), None, &FlattenDestination::File(target.clone()), &encoding(ImageSaveFormat::Png), Some(other)).expect("changed page is written");
        assert_eq!(written(&changed).0, target);
        assert_eq!(changed.digest, composed.digest);
        assert_eq!(image::open(&target).expect("decode").to_rgba8(), expected);
    }

    /// The stale-projection hazard of an image set: two pages resident in typing's projection, the
    /// PS editor edits page 1 through the shared document while typing's canvas shows page 0 and is
    /// not drawn. Page 1 must read as stale (a global "last projected version" adopted by a sync of
    /// page 0 used to mark it current), the flatten preparation of page 1 must re-project it, and the
    /// written file must hold the edit. The interactive path (`maybe_reproject_from_doc_version` after
    /// a switch to page 1) re-projects it the same way.
    #[test]
    fn a_doc_edit_on_a_non_current_page_is_reprojected_before_its_flatten() {
        use ms_models::layer_model::layer_doc::LayerDoc;
        let dir = TempDir::new("two_pages");
        let mut project = project_under(&dir.0);
        project.pages.push(ms_project::Page { idx: 1, path: project.paths.src_dir.join("002.png") });
        std::fs::create_dir_all(&project.paths.src_dir).expect("create src dir");
        let transform = ms_models::layer_model::manifest::TransformRec { cx: 8.0, cy: 8.0, rotation: 0.0, scale: 1.0 };
        let blue = egui::ColorImage::filled([4, 4], egui::Color32::from_rgb(0, 0, 255));
        for page in &project.pages {
            image::RgbaImage::from_pixel(PAGE_PX, PAGE_PX, image::Rgba([40, 80, 120, 255])).save(&page.path).expect("write page");
            ms_models::layer_model::persist::add_page_raster(&project.paths.unsaved_layers_dir, None, &ms_models::layer_model::persist::PageRasterSpec { page_idx: page.idx, uid: &format!("r{}", page.idx), name: "R", visible: true, opacity: 1.0, transform, image: &blue }).expect("seed raster");
        }
        let doc = Arc::new(Mutex::new(LayerDoc::new()));
        let mut state = TypingTabState::default();
        state.set_layer_doc(Arc::clone(&doc));
        let ctx = egui::Context::default();
        assert_eq!(state.canvas.current_page_idx(), 0, "the canvas shows page 0 throughout");

        let first = dir.0.join("first.png");
        let report = flatten_until_done(&mut state, &ctx, &project, 1, &first).expect("first flatten of page 1");
        assert_eq!(report.page_idx, 1);
        assert_eq!(image::open(&first).expect("decode first").to_rgba8().get_pixel(8, 8).0, [0, 0, 255, 255]);
        // Both pages are resident in the projection now (the preparation's whole-chapter pass).
        assert!(state.text_overlays.raster_layers_by_page.contains_key(&0) && state.text_overlays.raster_layers_by_page.contains_key(&1));
        state.text_overlays.maybe_reproject_from_doc_version(0);
        assert!(state.text_overlays.page_projection_current(0));
        assert!(state.text_overlays.page_projection_current(1));

        let green = egui::ColorImage::filled([4, 4], egui::Color32::from_rgb(0, 255, 0));
        doc.lock().expect("doc lock").set_raster_pixels(1, "r1", green.clone(), green, Vec::new(), true);
        // The draw path's per-frame sync of the CURRENT page must not mark page 1 current.
        state.text_overlays.maybe_reproject_from_doc_version(0);
        assert!(state.text_overlays.page_projection_current(0));
        assert!(!state.text_overlays.page_projection_current(1), "page 1 lags the document");

        let projected_green = |state: &TypingTabState| state.text_overlays.raster_layers_by_page.get(&1).and_then(|layers| layers.first()).is_some_and(|layer| layer.image.pixels[0] == egui::Color32::from_rgb(0, 255, 0));
        assert!(!projected_green(&state));
        state.text_overlays.drive_doc_sync_without_draw(&ctx, 1);
        assert!(state.text_overlays.page_projection_current(1));
        assert!(projected_green(&state), "the flattened page was re-projected from the document");

        let second = dir.0.join("second.png");
        flatten_until_done(&mut state, &ctx, &project, 1, &second).expect("second flatten of page 1");
        assert_eq!(image::open(&second).expect("decode second").to_rgba8().get_pixel(8, 8).0, [0, 255, 0, 255], "the file holds the document's current pixels");

        // Interactive path: another edit while page 0 is shown, then a switch to page 1.
        let red = egui::ColorImage::filled([4, 4], egui::Color32::from_rgb(255, 0, 0));
        doc.lock().expect("doc lock").set_raster_pixels(1, "r1", red.clone(), red, Vec::new(), true);
        state.text_overlays.maybe_reproject_from_doc_version(0);
        assert!(!state.text_overlays.page_projection_current(1));
        state.text_overlays.maybe_reproject_from_doc_version(1);
        assert!(state.text_overlays.page_projection_current(1), "showing page 1 re-projects it");
        let projected_red = state.text_overlays.raster_layers_by_page.get(&1).and_then(|layers| layers.first()).is_some_and(|layer| layer.image.pixels[0] == egui::Color32::from_rgb(255, 0, 0));
        assert!(projected_red);
    }

    /// Seeds a chapter whose pages `0..` each carry the given raster uids (blue 4x4, stacked in
    /// order), into `project`'s source and staging directories.
    fn seed_pages(project: &ProjectData, rasters_per_page: &[&[&str]]) {
        std::fs::create_dir_all(&project.paths.src_dir).expect("create src dir");
        let transform = ms_models::layer_model::manifest::TransformRec { cx: 8.0, cy: 8.0, rotation: 0.0, scale: 1.0 };
        let blue = egui::ColorImage::filled([4, 4], egui::Color32::from_rgb(0, 0, 255));
        for (page, uids) in project.pages.iter().zip(rasters_per_page) {
            image::RgbaImage::from_pixel(PAGE_PX, PAGE_PX, image::Rgba([40, 80, 120, 255])).save(&page.path).expect("write page");
            for uid in *uids {
                ms_models::layer_model::persist::add_page_raster(&project.paths.unsaved_layers_dir, None, &ms_models::layer_model::persist::PageRasterSpec { page_idx: page.idx, uid, name: "R", visible: true, opacity: 1.0, transform, image: &blue }).expect("seed raster");
            }
        }
    }

    /// A two-page chapter (page 0: raster `r0`; page 1: `page1_rasters`) with both pages resident in
    /// typing's projection and the clip masks loaded (warmed up by a flatten), the canvas on page 0.
    fn resident_two_page_chapter(dir: &Path, page1_rasters: &[&str]) -> (ProjectData, Arc<Mutex<ms_models::layer_model::layer_doc::LayerDoc>>, TypingTabState, egui::Context) {
        let mut project = project_under(dir);
        project.pages.push(ms_project::Page { idx: 1, path: project.paths.src_dir.join("002.png") });
        seed_pages(&project, &[&["r0"], page1_rasters]);
        let doc = Arc::new(Mutex::new(ms_models::layer_model::layer_doc::LayerDoc::new()));
        let mut state = TypingTabState::default();
        state.set_layer_doc(Arc::clone(&doc));
        let ctx = egui::Context::default();
        flatten_until_done(&mut state, &ctx, &project, 1, &dir.join("warmup.png")).expect("warm-up flatten");
        assert!(state.text_overlays.raster_layers_by_page.contains_key(&0) && state.text_overlays.raster_layers_by_page.contains_key(&1));
        assert!(state.mask_layer.masks_loaded(&project));
        assert_eq!(state.canvas.current_page_idx(), 0);
        (project, doc, state, ctx)
    }

    /// A one-to-one PNG project export into `out`.
    fn png_export_request(out: &Path) -> TypingExportRequest {
        TypingExportRequest {
            destination: TypingExportDestination::Folder(ms_platform::file_dialog::FolderTarget::for_test(out.to_path_buf())),
            format: TypingExportFormat::Png,
            repaginate: crate::export_repaginate::TypingRepaginateSettings::default(),
            output_base_name: "export".to_string(),
            bundle_custom_fonts: false,
            custom_fonts: None,
        }
    }

    /// Polls a dispatched project export until its worker reported, and asserts it succeeded.
    fn wait_for_export(state: &mut TypingTabState, ctx: &egui::Context) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while state.export_in_progress() {
            state.text_overlays.poll_export_jobs(ctx);
            assert!(std::time::Instant::now() < deadline, "the export worker never reported");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(matches!(state.text_overlays.export_status_for_ui(), TypingExportUiStatus::Success { .. }), "the export succeeds");
    }

    /// The uid of page 1's projected raster at `idx`.
    fn page1_raster_uid(state: &TypingTabState, idx: usize) -> Option<String> {
        state.text_overlays.raster_layers_by_page.get(&1).and_then(|layers| layers.get(idx)).map(|layer| layer.uid.clone())
    }

    /// S-029: the PROJECT export of a page already resident in typing's projection whose
    /// `LayerDoc` the PS editor edited afterwards (typing shows page 0 and never re-projected page 1).
    /// `build_page_job` never re-reads a resident page, so the export must re-project it itself: while
    /// an in-flight job holds the re-projection back the deferred export stays pending; once it clears
    /// the export dispatches and the written page holds the document's current pixels.
    #[test]
    fn project_export_of_a_resident_page_uses_its_reprojection_after_a_doc_edit() {
        let dir = TempDir::new("export_doc_edit");
        let (project, doc, mut state, ctx) = resident_two_page_chapter(&dir.0, &["r1"]);
        let green = egui::ColorImage::filled([4, 4], egui::Color32::from_rgb(0, 255, 0));
        doc.lock().expect("doc lock").set_raster_pixels(1, "r1", green.clone(), green, Vec::new(), true);
        state.text_overlays.maybe_reproject_from_doc_version(0);
        assert!(!state.text_overlays.page_projection_current(1), "page 1 lags the document");

        let out = dir.0.join("export");
        state.text_overlays.set_pending_export(png_export_request(&out));
        // An in-flight placement save makes the re-projection back off: the export stays deferred.
        let (save_tx, save_rx) = mpsc::channel();
        state.text_overlays.save_rx = Some(save_rx);
        state.run_pending_export_if_ready(&ctx, &project, false);
        assert!(!state.export_in_progress(), "no dispatch while page 1's projection is stale");
        assert!(state.text_overlays.has_pending_export());
        assert!(!state.text_overlays.page_projection_current(1));

        // The save lands; the next frame re-projects page 1 and dispatches.
        save_tx.send(Ok(())).expect("save job receiver alive");
        state.run_pending_export_if_ready(&ctx, &project, false);
        assert!(state.export_in_progress(), "dispatched once every projection is current");
        assert!(state.text_overlays.page_projection_current(1));
        wait_for_export(&mut state, &ctx);
        let page1 = image::open(out.join("002.png")).expect("decode exported page 1").to_rgba8();
        assert_eq!(page1.get_pixel(8, 8).0, [0, 255, 0, 255], "the export holds the document's current pixels");
        let page0 = image::open(out.join("001.png")).expect("decode exported page 0").to_rgba8();
        assert_eq!(page0.get_pixel(8, 8).0, [0, 0, 255, 255], "the untouched page keeps its raster");
    }

    /// The export's re-projection of a stale resident page keeps `sync_from_doc`'s uid remap: a
    /// raster selected on page 1 (not the canvas's page) that the PS editor moved up through the
    /// document stays selected by uid at its new index, never restored to the old index (which now
    /// names the other raster).
    #[test]
    fn export_reprojection_keeps_a_selected_raster_by_uid_after_a_doc_reorder() {
        let dir = TempDir::new("export_reorder");
        let (project, doc, mut state, ctx) = resident_two_page_chapter(&dir.0, &["a", "b"]);
        assert_eq!(page1_raster_uid(&state, 0).as_deref(), Some("a"), "seeded bottom-to-top");
        state.text_overlays.selected_raster_idx = Some(0);
        state.text_overlays.selected_raster_page = Some(1);
        assert!(doc.lock().expect("doc lock").reorder_node_one(1, "a", true), "the doc moves `a` above `b`");
        assert!(!state.text_overlays.page_projection_current(1));

        state.text_overlays.set_pending_export(png_export_request(&dir.0.join("export")));
        state.run_pending_export_if_ready(&ctx, &project, false);
        assert!(state.export_in_progress(), "dispatched after the re-projection");
        assert_eq!(page1_raster_uid(&state, 0).as_deref(), Some("b"), "page 1 was re-projected in the new order");
        assert_eq!(state.text_overlays.selected_raster_page, Some(1));
        let selected = state.text_overlays.selected_raster_idx.expect("a raster stays selected");
        assert_eq!(page1_raster_uid(&state, selected).as_deref(), Some("a"), "the selection follows the uid");
        wait_for_export(&mut state, &ctx);
        assert_eq!(page1_raster_uid(&state, selected).as_deref(), Some("a"), "building the export jobs keeps the remapped selection");
    }

    /// While the whole-chapter preload pass runs (each applied page bumps the doc's version),
    /// the deferred export's gate never runs the per-page re-projection: a stale resident page keeps
    /// its projected version until the pass drained; then the export re-projects and dispatches.
    #[test]
    fn deferred_export_does_not_reproject_while_the_preload_runs() {
        let dir = TempDir::new("export_preload");
        let (mut project, doc, mut state, ctx) = resident_two_page_chapter(&dir.0, &["r1"]);
        let green = egui::ColorImage::filled([4, 4], egui::Color32::from_rgb(0, 255, 0));
        doc.lock().expect("doc lock").set_raster_pixels(1, "r1", green.clone(), green, Vec::new(), true);
        let projected_before = state.text_overlays.projected_page_revision.get(&1).copied();
        assert!(!state.text_overlays.page_projection_current(1));

        // A third, never-loaded page gives the preload pass a target.
        project.pages.push(ms_project::Page { idx: 2, path: project.paths.src_dir.join("003.png") });
        seed_pages(&project, &[&[], &[], &["r2"]]);
        state.begin_preload_all_pages(&project);
        assert!(state.preload_all_pages_active(), "the preload pass runs");
        state.text_overlays.set_pending_export(png_export_request(&dir.0.join("export")));
        state.run_pending_export_if_ready(&ctx, &project, false);
        assert!(!state.export_in_progress());
        assert_eq!(state.text_overlays.projected_page_revision.get(&1).copied(), projected_before, "no re-projection while the preload runs");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while state.drive_page_preload() {
            assert!(std::time::Instant::now() < deadline, "the preload never drained");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        state.run_pending_export_if_ready(&ctx, &project, false);
        assert!(state.export_in_progress(), "dispatched once the pass drained");
        assert!(state.text_overlays.page_projection_current(1));
        wait_for_export(&mut state, &ctx);
    }

    /// S-050: an edit of page 1 in the shared document advances only page 1's revision, so typing's
    /// projection of page 0 stays current and the per-frame sync of page 0 does not re-project it.
    #[test]
    fn a_doc_edit_on_another_page_leaves_this_page_projection_current() {
        let dir = TempDir::new("other_page_edit");
        let (_project, doc, mut state, _ctx) = resident_two_page_chapter(&dir.0, &["r1"]);
        state.text_overlays.maybe_reproject_from_doc_version(0);
        assert!(state.text_overlays.page_projection_current(0) && state.text_overlays.page_projection_current(1));
        let page0_projected = state.text_overlays.projected_page_revision.get(&0).copied();
        let green = egui::ColorImage::filled([4, 4], egui::Color32::from_rgb(0, 255, 0));
        doc.lock().expect("doc lock").set_raster_pixels(1, "r1", green.clone(), green, Vec::new(), true);
        assert!(state.text_overlays.page_projection_current(0), "an edit of page 1 does not make page 0 stale");
        assert!(!state.text_overlays.page_projection_current(1), "page 1 lags the document");
        state.text_overlays.maybe_reproject_from_doc_version(0);
        assert_eq!(state.text_overlays.projected_page_revision.get(&0).copied(), page0_projected, "page 0 was not re-projected");
    }

    /// S-050: a doc-funnel edit made only through `node_mut` (no bumping mutator) still advances the
    /// edited page's revision, so the PS tab re-projects it, and leaves the other page's alone.
    #[test]
    fn a_node_mut_only_funnel_edit_advances_its_page_revision() {
        let dir = TempDir::new("node_mut_funnel");
        let (_project, doc, mut state, _ctx) = resident_two_page_chapter(&dir.0, &["r1"]);
        let (rev0, rev1) = {
            let guard = doc.lock().expect("doc lock");
            (guard.page_revision(0), guard.page_revision(1))
        };
        assert!(state.text_overlays.route_to_doc(1, |doc| {
            if let Some(node) = doc.node_mut(1, "r1") {
                node.opacity = 0.25;
            }
        }));
        let guard = doc.lock().expect("doc lock");
        assert!(guard.page_revision(1) > rev1, "the funnel marked page 1 changed");
        assert_eq!(guard.page_revision(0), rev0, "page 0 is untouched");
        drop(guard);
        assert!(state.text_overlays.page_projection_current(1), "the funnel re-projected page 1 itself");
    }

    /// S-050: while the whole-chapter preload pass runs, the flatten preparation never runs the
    /// per-page re-projection (it is the gate's last, lazy term): a stale resident page keeps its
    /// projected revision and the preparation is not ready; once the pass drained it re-projects and
    /// reports ready. The pass is held open by a test-owned channel, so the preparation's own
    /// `drive_page_preload` cannot drain it early (no timing dependence).
    #[test]
    fn flatten_prepare_does_not_reproject_while_the_preload_runs() {
        let dir = TempDir::new("flatten_preload");
        let (project, doc, mut state, ctx) = resident_two_page_chapter(&dir.0, &["r1"]);
        let green = egui::ColorImage::filled([4, 4], egui::Color32::from_rgb(0, 255, 0));
        doc.lock().expect("doc lock").set_raster_pixels(1, "r1", green.clone(), green, Vec::new(), true);
        let projected_before = state.text_overlays.projected_page_revision.get(&1).copied();
        assert!(!state.text_overlays.page_projection_current(1));

        // A preload pass with one outstanding (fictional) target page, released by `held_tx`.
        let (held_tx, rx) = mpsc::channel::<TypingPreloadPageResponse>();
        state.text_overlays.preload_all_state = Some(TypingPreloadAllState { rx, remaining: HashSet::from([7]), total: 1, decode_errors: 0 });
        for _ in 0..3 {
            let readiness = state.prepare_flatten_to_file(&ctx, &project, 1);
            assert!(matches!(readiness, FlattenReadiness::Preparing { .. }), "{readiness:?}");
            assert_eq!(state.text_overlays.projected_page_revision.get(&1).copied(), projected_before, "no re-projection while the preload runs");
        }

        // The pass drains (its last target reports a decode failure, which still completes it).
        held_tx.send((7, Err("released by the test".to_string()))).expect("preload receiver alive");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !matches!(state.prepare_flatten_to_file(&ctx, &project, 1), FlattenReadiness::Ready) {
            assert!(std::time::Instant::now() < deadline, "the flatten preparation never became ready");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(!state.preload_all_pages_active());
        assert!(state.text_overlays.page_projection_current(1), "the preparation re-projected page 1");
    }
}
