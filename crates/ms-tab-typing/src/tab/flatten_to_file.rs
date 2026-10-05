/*
File: tab/flatten_to_file.rs

Purpose:
Native "flatten ONE page into an image file" API of the typing tab, used by the single-image mode's
«Сохранить» / «Сохранить как» (the app-side session controller drives it from any active tab).

Key structures:
- `FlattenToFileRequest`: page, target file, `image_encode::ImageEncoding`.
- `FlattenReadiness`: per-frame answer of `prepare_flatten_to_file` (ready, or still preparing).
- `FlattenToFileReport` / `FlattenToFileError`: the typed outcome polled by the caller.
- `FlattenToFileState` (private): the in-flight worker receiver and the layer-preload pass tracker.

Key functions:
- `TypingTabState::prepare_flatten_to_file()`: starts and drives everything the composite reads
  (chapter loader, migration, whole-chapter layer preload, clip-mask loader, re-projection of a
  shared-document edit made in another tab) WITHOUT the typing tab drawing; call it every frame
  until it reports `Ready`.
- `TypingTabState::request_flatten_to_file()`: gates (the export gates + mutual exclusion with a
  project export), builds the page job on the GUI thread and spawns the worker.
- `TypingTabState::poll_flatten_to_file()`: non-blocking result poll.
- `flatten_job_to_file()`: the worker body (клин snapshot, `resolve_flatten_input`,
  `flatten_page_rgba`, `encode_rgba`, symlink-preserving `ms_docstore::write_bytes_atomic`).

Notes:
Native-only (`write_bytes_atomic` does not exist on wasm); the module is not compiled there. The
composite is exactly the project export's (`build_page_job` + the same flatten), so a saved file
matches a PNG export of the page. The GUI thread never does file I/O here beyond what the existing
residency / loader-start paths already do; the saver barrier, decode, encode and write run on the
worker.
*/

use super::*;
use crate::image_encode::{EncodeError, ImageEncoding, encode_rgba};
use ms_docstore::{Durability, write_bytes_atomic};

/// One page to flatten into one image file.
#[derive(Debug, Clone)]
pub struct FlattenToFileRequest {
    /// `Page::idx` of the page to compose (the single-image chapter has exactly one page, idx 0).
    pub page_idx: usize,
    /// Target file as the user named it. An existing target is canonicalized before the write, so
    /// a symlink keeps pointing at its (rewritten) target instead of being replaced by a file. Its
    /// parent directory must exist.
    pub target: PathBuf,
    /// Format, JPEG quality, alpha policy and ICC profile of the written file.
    pub encoding: ImageEncoding,
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

/// A successful flatten-to-file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlattenToFileReport {
    /// The requested target (not its canonicalized form), so the caller keeps the path the user named.
    pub target: PathBuf,
    /// Size of the encoded file in bytes.
    pub bytes_written: usize,
}

/// Why a flatten-to-file was refused or failed. `Display` is technical English for logs; the caller
/// picks the localized user text by variant.
#[derive(Debug, thiserror::Error)]
pub enum FlattenToFileError {
    /// Refused at dispatch: a flatten or a project export is running or pending, a project save is
    /// busy, or an input of the composite is not ready yet (call `prepare_flatten_to_file` until it
    /// reports `Ready`). Nothing was started.
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
    /// The tab's layer projection reflects the shared document's current version (a PS-tab edit
    /// re-projected; `doc_projection_current`).
    doc_synced: bool,
}

/// The flatten-to-file dispatch decision: the project export's gate (`export_dispatch_ready`: preload
/// pass drained, masks loaded, no save) plus mutual exclusion with a running/deferred export and with
/// another flatten, a settled chapter load, resident layer inputs (so building the job never falls
/// back to a synchronous page load on the GUI thread) and a projection that is current with the
/// shared document (so the job never composes a stale snapshot of a PS-tab edit).
fn flatten_dispatch_ready(gate: FlattenGate) -> bool {
    !gate.flatten_running && !gate.export_busy && gate.chapter_settled && gate.layers_ready && gate.doc_synced && export_dispatch_ready(gate.preload_active, gate.masks_ready, gate.save_busy)
}

impl TypingTabState {
    /// Starts and advances everything a flatten of the open chapter reads, and reports whether
    /// [`Self::request_flatten_to_file`] may dispatch now. Call it EVERY frame (from any active tab)
    /// while a save is being prepared: the typing tab's own `draw` drives these loaders only while it
    /// is drawn, so without this a flatten requested from another tab would wait forever (plan R6).
    ///
    /// Each call: wires the overlay and clip-mask loaders for `project` (idempotent), polls the
    /// overlay loader, the eager migration and the mask loader, applies a bounded batch of the
    /// whole-chapter layer preload, starts that preload once when pages are not resident, and
    /// re-projects the canvas's current page from the shared `LayerDoc` when another tab (the PS
    /// editor) changed it — first polling the in-flight jobs that hold that re-projection back. Not
    /// ready while the projection lags the document. Polling here as well as in `draw` is harmless
    /// (non-blocking `try_recv`s). Requests a repaint while not ready so the frame loop keeps
    /// draining the workers.
    ///
    /// `ctx` is needed by the mask loader's error reporting and for the repaint request.
    pub fn prepare_flatten_to_file(&mut self, ctx: &egui::Context, project: &ProjectData) -> FlattenReadiness {
        self.text_overlays.ensure_loader_started(project);
        self.mask_layer.ensure_loader_started(project);
        self.text_overlays.poll_loader();
        self.text_overlays.poll_migration();
        self.mask_layer.poll_loader(ctx);
        self.text_overlays.drive_page_preload();
        // Cross-tab sync without `draw` (review High-1): a page already resident in this tab's
        // projection is never re-read by `build_page_job`, so an edit the PS tab made in the shared
        // document since typing last drew must be re-projected here, or the file would be written
        // from the stale projection. The single-image chapter's only page is the canvas's current one.
        self.text_overlays.drive_doc_sync_without_draw(ctx, self.canvas.current_page_idx());

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
            doc_synced: self.text_overlays.doc_projection_current(),
        };
        if flatten_dispatch_ready(gate) {
            return FlattenReadiness::Ready;
        }
        ctx.request_repaint();
        let (done, total) = if preload_active { self.text_overlays.preload_all_pages_progress() } else { (0, 0) };
        FlattenReadiness::Preparing { done, total }
    }

    /// Dispatches the flatten of `request.page_idx` into `request.target` on a worker thread.
    ///
    /// The GUI thread only snapshots: the page job (`build_page_job`: on-screen rasters, overlays,
    /// bands, groups — the exact project-export composite), the page's clip mask and the layer-saver
    /// handle. The worker barriers the saver (as the project export does), resolves the клин and the
    /// page source, flattens, encodes and writes atomically ([`flatten_job_to_file`]). Collect the
    /// outcome with [`Self::poll_flatten_to_file`]. While it runs a project export is deferred / refused.
    ///
    /// # Errors
    /// [`FlattenToFileError::NoSuchPage`] when `page_idx` is not a page of `project` (checked first);
    /// [`FlattenToFileError::Busy`] when another flatten or a project export runs or is deferred, a
    /// project save is busy, or an input is not ready (see [`Self::prepare_flatten_to_file`]). Both
    /// refuse before anything is started; they are logged.
    pub fn request_flatten_to_file(&mut self, ctx: &egui::Context, project: &ProjectData, request: FlattenToFileRequest) -> Result<(), FlattenToFileError> {
        let FlattenToFileRequest { page_idx, target, encoding } = request;
        if !project.pages.iter().any(|page| page.idx == page_idx) {
            ms_log::runtime_log::log_warn(format!("[typing] flatten to file refused: page {page_idx} is not a page of the open project.\nTarget: {}", target.display()));
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
            doc_synced: self.text_overlays.doc_projection_current(),
        };
        if !flatten_dispatch_ready(gate) {
            ms_log::runtime_log::log_warn(format!(
                "[typing] flatten to file refused: busy or not prepared.\nTarget: {}\nGate: {gate:?}\nPossible cause: the caller did not wait for prepare_flatten_to_file to report Ready",
                target.display()
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
            "[typing] flatten to file: dispatch.\nPage: {page_idx}\nTarget: {}\nFormat: {:?}\nAlpha: {:?}\nICC: {}",
            target.display(),
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
            let result = flatten_job_to_file(job, clean_overlays_model.as_ref(), &target, &encoding);
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

/// Worker body of a flatten-to-file: fills the job's клин snapshot (model overlay, else the file the
/// overlay loader would load), resolves and flattens the page exactly as the project export does,
/// encodes it with `encoding` and writes it to `target` (see [`write_to_target`]). Every failure is
/// logged here with the page, the target and the cause (never pixels). Blocking I/O: worker only.
///
/// # Errors
/// `Compose` (клин or page source unreadable), `Encode`, `Write`.
pub(super) fn flatten_job_to_file(mut job: TypingExportPageJob, clean_overlays_model: Option<&Arc<Mutex<CleanOverlaysModel>>>, target: &Path, encoding: &ImageEncoding) -> Result<FlattenToFileReport, FlattenToFileError> {
    let page_idx = job.page_idx;
    let result = compose_encode_write(&mut job, clean_overlays_model, target, encoding);
    match &result {
        Ok(report) => ms_log::runtime_log::log_info(format!(
            "[typing] flatten to file: written.\nPage: {page_idx}\nTarget: {}\nBytes: {}",
            report.target.display(),
            report.bytes_written
        )),
        Err(err) => ms_log::runtime_log::log_error(format!(
            "[typing] flatten to file failed.\nPage: {page_idx}\nSource: {}\nTarget: {}\nFormat: {:?}\nError: {err}",
            job.page_path.display(),
            target.display(),
            encoding.format
        )),
    }
    result
}

/// The un-logged steps of [`flatten_job_to_file`].
fn compose_encode_write(job: &mut TypingExportPageJob, clean_overlays_model: Option<&Arc<Mutex<CleanOverlaysModel>>>, target: &Path, encoding: &ImageEncoding) -> Result<FlattenToFileReport, FlattenToFileError> {
    prepare_export_clean_overlay_snapshots(std::slice::from_mut(job), clean_overlays_model.cloned()).map_err(FlattenToFileError::Compose)?;
    let flat = flatten_page_rgba(resolve_flatten_input(job).map_err(FlattenToFileError::Compose)?);
    let bytes = encode_rgba(&flat, encoding)?;
    write_to_target(target, &bytes)?;
    Ok(FlattenToFileReport { target: target.to_path_buf(), bytes_written: bytes.len() })
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
/// symlink) or the atomic write fails (missing parent, no permission, target is a directory, ...).
fn write_to_target(target: &Path, bytes: &[u8]) -> Result<(), FlattenToFileError> {
    let resolved = match std::fs::symlink_metadata(target) {
        Ok(_) => std::fs::canonicalize(target).map_err(|err| FlattenToFileError::Write { path: target.to_path_buf(), reason: format!("cannot resolve the existing target: {err}") })?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => target.to_path_buf(),
        Err(err) => return Err(FlattenToFileError::Write { path: target.to_path_buf(), reason: format!("cannot inspect the target: {err}") }),
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
    write_bytes_atomic(&resolved, bytes, Durability::Contents).map_err(|err| FlattenToFileError::Write { path: resolved.clone(), reason: err.to_string() })?;
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
            let report = flatten_job_to_file(job_with_overlay(&dir.0), None, &target, &encoding(format)).expect("flatten to file");
            assert_eq!(report.target, target);
            let bytes = std::fs::read(&target).expect("read written file");
            assert_eq!(report.bytes_written, bytes.len());
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
        flatten_job_to_file(job, None, &target, &encoding(ImageSaveFormat::Jpeg)).expect("flatten to file");
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
        let err = flatten_job_to_file(job, None, &target, &encoding(ImageSaveFormat::Png)).expect_err("parent is missing");
        assert!(matches!(&err, FlattenToFileError::Write { path, .. } if path == &target), "{err:?}");
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
        let err = flatten_job_to_file(job, None, &target, &encoding(ImageSaveFormat::Png)).expect_err("target is a directory");
        assert!(matches!(err, FlattenToFileError::Write { .. }), "{err:?}");
        assert_eq!(std::fs::read(&marker).expect("old content still there"), b"old");
        assert!(temp_artifacts(&dir.0).is_empty(), "the temp file must be removed");
    }

    #[test]
    fn existing_target_is_replaced() {
        let dir = TempDir::new("replace");
        let job = job_with_overlay(&dir.0);
        let target = dir.0.join("out.png");
        std::fs::write(&target, b"previous bytes").expect("write old file");
        flatten_job_to_file(job_with_overlay(&dir.0), None, &target, &encoding(ImageSaveFormat::Png)).expect("flatten to file");
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
        let report = flatten_job_to_file(job_with_overlay(&dir.0), None, &link, &encoding(ImageSaveFormat::Png)).expect("flatten to file");
        assert_eq!(report.target, link, "the report names the path the user chose");
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
        flatten_job_to_file(job_with_overlay(&dir.0), None, &target, &encoding(ImageSaveFormat::Png)).expect("flatten to file");
        let mode = std::fs::metadata(&target).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn unreadable_page_is_a_compose_error() {
        let dir = TempDir::new("compose");
        let mut job = job_with_overlay(&dir.0);
        job.page_path = dir.0.join("missing_page.png");
        let target = dir.0.join("out.png");
        let err = flatten_job_to_file(job, None, &target, &encoding(ImageSaveFormat::Png)).expect_err("page is missing");
        assert!(matches!(err, FlattenToFileError::Compose(_)), "{err:?}");
        assert!(!target.exists());
    }

    #[test]
    fn invalid_jpeg_quality_is_an_encode_error() {
        let dir = TempDir::new("encode");
        let job = job_with_overlay(&dir.0);
        let target = dir.0.join("out.jpg");
        let enc = ImageEncoding { jpeg_quality: 0, ..encoding(ImageSaveFormat::Jpeg) };
        let err = flatten_job_to_file(job, None, &target, &enc).expect_err("quality 0 is invalid");
        assert!(matches!(err, FlattenToFileError::Encode(EncodeError::InvalidJpegQuality(0))), "{err:?}");
        assert!(!target.exists());
    }

    #[test]
    fn dispatch_gate_table() {
        let ready = FlattenGate { flatten_running: false, export_busy: false, chapter_settled: true, layers_ready: true, preload_active: false, masks_ready: true, save_busy: false, doc_synced: true };
        assert!(flatten_dispatch_ready(ready));
        let blocked = [
            FlattenGate { flatten_running: true, ..ready },
            FlattenGate { export_busy: true, ..ready },
            FlattenGate { chapter_settled: false, ..ready },
            FlattenGate { layers_ready: false, ..ready },
            FlattenGate { preload_active: true, ..ready },
            FlattenGate { masks_ready: false, ..ready },
            FlattenGate { save_busy: true, ..ready },
            FlattenGate { doc_synced: false, ..ready },
        ];
        for gate in blocked {
            assert!(!flatten_dispatch_ready(gate), "{gate:?}");
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
        FlattenToFileRequest { page_idx: 0, target: dir.join("out.png"), encoding: encoding(ImageSaveFormat::Png) }
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
        while state.prepare_flatten_to_file(&ctx, &project) != FlattenReadiness::Ready {
            assert!(std::time::Instant::now() < deadline, "preparation never became ready");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(state.mask_layer.masks_loaded(&project), "the mask loader drained without draw");
        let target = dir.0.join("out.png");
        state.request_flatten_to_file(&ctx, &project, FlattenToFileRequest { page_idx: 0, target: target.clone(), encoding: encoding(ImageSaveFormat::Png) }).expect("dispatch");
        assert!(state.flatten_to_file_in_progress());
        assert_eq!(state.prepare_flatten_to_file(&ctx, &project), FlattenReadiness::Preparing { done: 0, total: 0 }, "not ready while a flatten runs");
        let second = state.request_flatten_to_file(&ctx, &project, FlattenToFileRequest { page_idx: 0, target: target.clone(), encoding: encoding(ImageSaveFormat::Png) });
        assert!(matches!(second, Err(FlattenToFileError::Busy)), "{second:?}");
        let outcome = loop {
            if let Some(outcome) = state.poll_flatten_to_file() {
                break outcome;
            }
            assert!(std::time::Instant::now() < deadline, "the worker never reported");
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        let report = outcome.expect("flatten succeeded");
        assert_eq!(report.target, target);
        assert_eq!(image::open(&target).expect("decode").to_rgba8(), page);
        assert!(!state.flatten_to_file_in_progress());
        let wrong_page = state.request_flatten_to_file(&ctx, &project, FlattenToFileRequest { page_idx: 7, target, encoding: encoding(ImageSaveFormat::Png) });
        assert!(matches!(wrong_page, Err(FlattenToFileError::NoSuchPage(7))), "{wrong_page:?}");
    }

    /// Prepares until `Ready`, dispatches into `target` and waits for the worker's outcome.
    fn flatten_until_done(state: &mut TypingTabState, ctx: &egui::Context, project: &ProjectData, target: &Path) -> Result<FlattenToFileReport, FlattenToFileError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while state.prepare_flatten_to_file(ctx, project) != FlattenReadiness::Ready {
            assert!(std::time::Instant::now() < deadline, "preparation never became ready");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        state.request_flatten_to_file(ctx, project, FlattenToFileRequest { page_idx: 0, target: target.to_path_buf(), encoding: encoding(ImageSaveFormat::Png) })?;
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
        ms_models::layer_model::persist::add_page_raster(&project.paths.unsaved_layers_dir, None, 0, "r0", "R", true, 1.0, transform, &blue).expect("seed raster");
        let doc = Arc::new(Mutex::new(LayerDoc::new()));
        let mut state = TypingTabState::default();
        state.set_layer_doc(Arc::clone(&doc));
        let ctx = egui::Context::default();

        let first = dir.0.join("first.png");
        flatten_until_done(&mut state, &ctx, &project, &first).expect("first flatten");
        assert_eq!(image::open(&first).expect("decode first").to_rgba8().get_pixel(8, 8).0, [0, 0, 255, 255], "the seeded raster is composited");

        // The PS editor's edit path: new pixels straight into the shared document, version bumped.
        let green = egui::ColorImage::filled([4, 4], egui::Color32::from_rgb(0, 255, 0));
        doc.lock().expect("doc lock").set_raster_pixels(0, "r0", green.clone(), green, Vec::new(), true);

        let second = dir.0.join("second.png");
        flatten_until_done(&mut state, &ctx, &project, &second).expect("second flatten");
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
}
