/*
File: crates/ms-tab-page-manager/src/clean.rs

Purpose:
Owns the page-manager clean-layer worker protocol (inventory scan / probe / attach / delete /
unlink / bind / delete-unassigned), the GUI-side inventory install and link-cache refresh, and
the entry points the clean-card UI calls.

Key items:
- CleanJob / CleanEvent / CleanJobContext / CleanOpError: the serial worker protocol; every
  mutating job answers `Finished { epoch, result, inventory }`.
- run_unlink / run_bind / run_attach: the model + file choreography of each operation.
- PageManagerTabState::{request_unlink, request_bind, request_delete_unassigned,
  clean_mutation_blocked, page_clean_link, unassigned_cleans, clean_bind_targets, clean_bind_fit}:
  the entry points `clean_cards.rs` draws against.
- CleanDialog + draw_clean_dialog(): confirmations (replace-from-file attach, unlink from a menu,
  delete unassigned, bind at a mismatched/unknown size).

Notes:
- Scans are epoch-tagged; results from a superseded epoch are dropped.
- Jobs carry file NAMES and page indices; the worker re-resolves files (staged over committed).
- A replacement clean picked from disk is probed on the worker (header dimensions -> real
  `AttachFit`) before its confirmation dialog is shown.
- Mutating operations report partial success distinctly (`CleanOpError`).

Threading:
All filesystem and image work is performed by the dedicated worker. Model locks are short and
never held across encode/decode or disk I/O. Unlink captures the pixels AND detaches the page
under one lock, so the detach generation invalidates in-flight autosave snapshots before any
file moves (see clean_overlays_model.rs); after its trash step it re-queues a page an edit
re-materialized meanwhile. Replies are drained by `poll_clean_events` (lib.rs), which the app root
calls every frame on every tab. Bind hands the model an exact-size file only through
`load_prepared_overlay` (not dirty). Attach is NOT buffered by the autosave gate: `run_attach`
writes the attached page to `_unsaved/clean_layers` through the guarded writer BEFORE the source
is trashed, and keeps the source when that write fails.
*/

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

use ms_thread as thread;
use eframe::egui;

use image::RgbaImage;
use ms_models::clean_assign::{self, AttachFit, CleanInventory, CleanPageFit, UnassignedClean};
use ms_log::runtime_log;
use ms_models::clean_overlays_model::{save_overlay_snapshots_guarded, CleanOverlaysModel};
use ms_project::{Page, ProjectPaths};
use ms_models::page_view::PageImageInfo;

use super::clean_link;
use super::dialogs::spawn_clean_picker;
use super::PageManagerTabState;

/// A pending user confirmation for a content operation.
pub(super) enum CleanDialog {
    /// "Replace clean from file": attach the probed file at `path` to page `page_idx`.
    Attach { path: PathBuf, page_idx: usize, page_size: [u32; 2], fit: AttachFit, remove_source: bool },
    /// Delete the unassigned clean `file_name` from both trees.
    DeleteUnassigned { file_name: OsString },
    /// Unlink page `page_idx`'s clean (the context-menu path; the in-gap control confirms inline).
    Detach { page_idx: usize },
    /// Bind unassigned clean `file_name` to page `page_idx` although `fit` is not an exact match
    /// (`None`: unknown); it is bound as-is and does not load until the sizes match.
    BindMismatch { file_name: OsString, page_idx: usize, fit: Option<CleanPageFit> },
}

/// The snapshot every MUTATING clean job carries: the scan epoch it was submitted in (echoed in
/// `CleanEvent::Finished`) and the project paths/pages the worker resolves files against and
/// re-scans the inventory with once the job is done.
pub(super) struct CleanJobContext {
    pub(super) epoch: u64,
    pub(super) paths: ProjectPaths,
    pub(super) pages: Vec<Page>,
}

/// Work accepted by the dedicated clean worker.
pub(super) enum CleanJob {
    /// Rescan the clean inventory; `epoch` is echoed back so stale results are dropped.
    Scan { epoch: u64, paths: ProjectPaths, pages: Vec<Page> },
    /// Header-only dimension probe of a replacement clean picked from disk,
    /// producing the REAL `AttachFit` before the confirmation dialog is shown.
    ProbeAttach {
        path: PathBuf,
        page_idx: usize,
        page_size: [u32; 2],
    },
    Attach {
        path: PathBuf,
        page_idx: usize,
        page_size: [u32; 2],
        remove_source: bool,
        model: Arc<Mutex<CleanOverlaysModel>>,
        ctx: CleanJobContext,
    },
    /// Detach page `page_idx`'s clean into an unassigned `<stem>[_<n>]_detached.png` (see
    /// [`run_unlink`]).
    Unlink { page_idx: usize, model: Arc<Mutex<CleanOverlaysModel>>, ctx: CleanJobContext },
    /// Bind the unassigned clean `file_name` to page `page_idx` (see [`run_bind`]).
    Bind { file_name: OsString, page_idx: usize, model: Arc<Mutex<CleanOverlaysModel>>, ctx: CleanJobContext },
    /// Delete the unassigned clean `file_name` from both trees.
    DeleteUnassigned { file_name: OsString, ctx: CleanJobContext },
}

/// Failure of a mutating clean operation, distinguishing partial success so the
/// UI can report exactly what was and was not applied. Error strings are raw
/// worker diagnostics; the UI wraps them in localized messages.
#[derive(Debug)]
pub(super) enum CleanOpError {
    /// Nothing was applied (an unlink that failed to persist the clean restored the model pixels).
    Failed(String),
    /// The overlay WAS replaced in the model, but the source file could not be removed.
    AttachSourceCleanupFailed(String),
    /// The overlay WAS replaced in the model (and stays held for autosave), but writing it to
    /// `_unsaved/clean_layers` failed, so the source file was deliberately KEPT.
    AttachPersistFailed(String),
    /// The clean WAS detached and kept as an unassigned file, but at least one of the page's
    /// canonical files could not be trashed (it may still bind on the next load).
    UnlinkLeftovers(String),
    /// Binding replaced a page's clean: the old clean WAS detached (kept as `detached`, when it had
    /// anything to keep), but the chosen clean could not be bound. The source file is untouched.
    BindFailedAfterUnlink { detached: Option<PathBuf>, error: String },
    /// The clean WAS bound (the page's canonical name holds it), but a copy of the source stayed
    /// behind — the committed twin of a staged source could not be trashed, or a cross-device move
    /// could not remove the source it copied; that copy stays listed as an unassigned clean.
    BindSourceCleanupFailed(String),
    /// The clean file WAS bound on disk at an exact size, but decoding it into the model failed;
    /// the overlay loader will retry it on the next project load.
    BindModelLoadFailed(String),
}

/// Failure of a replacement-clean probe; nothing has been applied.
pub(super) enum ProbeAttachError {
    /// The image decoded but its aspect ratio does not fit the page.
    Incompatible { size: [u32; 2] },
    /// The image header could not be read.
    Unreadable(String),
}

/// Results returned to the GUI thread.
pub(super) enum CleanEvent {
    Scanned { epoch: u64, inventory: CleanInventory },
    AttachProbed {
        path: PathBuf,
        page_idx: usize,
        page_size: [u32; 2],
        outcome: Result<AttachFit, ProbeAttachError>,
    },
    /// A mutating job ended (in any outcome); `inventory` was scanned right after it, with the
    /// job's `epoch`, so the GUI installs it without a separate rescan.
    Finished { epoch: u64, result: Result<(), CleanOpError>, inventory: CleanInventory },
}

/// Attaches the clean file at `path` to page `page_idx` and, unlike the gesture-driven клин edits,
/// persists that page to `paths.unsaved_clean_layers_dir` IMMEDIATELY (attach is an explicit
/// command, outside the autosave gate's buffering) before `remove_source` trashes the source. The
/// source is removed only after a successful write, so a crash can never leave the clean neither on
/// disk nor in memory. Runs on the clean worker; the model lock is held only for the in-memory
/// replace and snapshot capture, never across decode or disk I/O.
///
/// Concurrency with the gate-driven клин autosave: the snapshot is captured WITHOUT removing the
/// page from the model's dirty set (take + immediate restore under the same lock as the replace), so
/// the next autosave pass writes the page again from the then-current pixels. A pass that captured
/// an OLDER snapshot before this replace and finishes its write after ours can briefly leave stale
/// pixels on disk, but it can never win permanently: the page is still dirty and is rewritten on the
/// next due pass. The write goes through `save_overlay_snapshots_guarded`, so a detach racing this
/// write removes the file again instead of resurrecting the clean.
///
/// # Errors
/// `Failed` when nothing was applied (unreadable/incompatible file, poisoned model, the model
/// rejected the page); `AttachPersistFailed` when the overlay is applied in memory but its write
/// failed (source kept); `AttachSourceCleanupFailed` when the write succeeded but trashing failed.
fn run_attach(path: &Path, page_idx: usize, page_size: [u32; 2], remove_source: bool, paths: &ProjectPaths, model: &Mutex<CleanOverlaysModel>) -> Result<(), CleanOpError> {
    let image = clean_assign::load_clean_for_attach(path, page_size).map_err(CleanOpError::Failed)?;
    let snapshot = {
        let mut guard = model.lock().map_err(|_| CleanOpError::Failed("clean overlay model is unavailable".to_string()))?;
        guard.replace_from_rgba(page_idx, image);
        // `take_dirty_save_snapshots` drains the WHOLE dirty set; restoring it at once, under the
        // same lock, leaves the set exactly as the replace left it (every snapshot was captured under
        // this lock, so all of them are current and restored) while handing us this page's pixels.
        let snapshots = guard.take_dirty_save_snapshots();
        guard.restore_dirty_save_snapshots(&snapshots);
        snapshots.into_iter().find(|snapshot| snapshot.page_idx == page_idx)
    };
    // `replace_from_rgba` ignores an out-of-range page or an empty image; the source must then stay.
    let snapshot = snapshot.ok_or_else(|| CleanOpError::Failed(format!("page {page_idx} did not accept the clean layer")))?;
    let written = paths.unsaved_clean_layers_dir.join(snapshot.file_name());
    save_overlay_snapshots_guarded(&paths.unsaved_clean_layers_dir, std::slice::from_ref(&snapshot), model).map_err(|error| {
        runtime_log::log_error(format!(
            "[page-manager::clean] attach could not persist the clean layer; source kept; page={page_idx}; source={}; target={}; error={error:#}",
            path.display(),
            written.display()
        ));
        CleanOpError::AttachPersistFailed(format!("{error:#}"))
    })?;
    // The source may BE the file just written (a staged clean re-attached to its own page, e.g. a
    // size-mismatch orphan rescaled in place): trashing it would delete the only copy.
    if !remove_source || path == written {
        return Ok(());
    }
    // The overlay is applied and persisted; a failed source removal is a PARTIAL success.
    clean_assign::trash_clean_file(paths, path).map_err(|error| CleanOpError::AttachSourceCleanupFailed(error.to_string()))
}

/// Locks the clean model, mapping poisoning to a nothing-applied failure.
fn lock_model(model: &Mutex<CleanOverlaysModel>) -> Result<std::sync::MutexGuard<'_, CleanOverlaysModel>, CleanOpError> {
    model.lock().map_err(|_| CleanOpError::Failed("clean overlay model is unavailable".to_string()))
}

/// What a completed [`run_unlink`] did.
#[derive(Debug, Default)]
struct UnlinkReport {
    /// The unassigned file the page's clean now lives in; `None` when there was nothing to keep
    /// (a materialized page with neither pixels nor a file, e.g. a never-written clear).
    detached: Option<PathBuf>,
    /// Canonical files of the page that could not be trashed (diagnostics).
    leftovers: Vec<String>,
}

/// Detaches page `page_idx`'s clean into an unassigned `<stem>[_<n>]_detached.png` in the
/// committed tree. Immediate: not undone by discarding unsaved changes.
///
/// Order (serial clean worker):
/// 1. ONE short model lock captures the page's current `overlay_rgba` (`Arc`, unsaved edits
///    included) and whether it is materialized, then calls `detach_page_overlay` (drops the page's
///    undo history, bumps its detach generation so an in-flight autosave snapshot cannot rewrite
///    `<stem>.png`).
/// 2. Outside the lock: captured pixels are PNG-encoded and written to a freshly allocated
///    detached name (temp + fsync + no-replace rename); a virtual page instead has its canonical
///    file (staged over committed, whether or not it fits) moved byte-exact to that name.
/// 3. The page's remaining canonical files are trashed (staged removed, committed to
///    `.pageop_trash`).
/// 4. One more short model lock: if the page was RE-materialized meanwhile (the user painted on it
///    in the cleaning tab after step 1), its pixels are re-queued for saving
///    (`mark_overlay_needs_save`). Those strokes form the page's NEW clean — the old one is the
///    detached file — and the autosave may already have written them to the staged `<stem>.png`
///    that step 3 just removed; re-queuing makes the next autosave pass (or the exit flush) write
///    them again. Doing this after the removal, rather than skipping the removal, closes the race
///    without holding the lock across file I/O.
///
/// If step 2 fails, the captured pixels are put back with `replace_prepared_overlay` (the page is
/// dirty again; its undo history is not restored) and `Failed` is returned. A cross-device move
/// that copied the file but could not remove its source (`SourceNotRemoved`) counts as persisted:
/// the source is a canonical file, which step 3 then trashes (or reports as a leftover).
///
/// # Errors
/// `Failed` when nothing was applied: unknown page, poisoned model, nothing to unlink, no free
/// name, or the write/move failed. Leftovers of step 3 are reported in the `Ok` report.
fn run_unlink(page_idx: usize, ctx: &CleanJobContext, model: &Mutex<CleanOverlaysModel>) -> Result<UnlinkReport, CleanOpError> {
    run_unlink_with(page_idx, ctx, model, || {})
}

/// [`run_unlink`] with `before_cleanup` called between steps 2 and 3: the test seam that
/// interleaves a user edit + autosave with the unlink deterministically.
fn run_unlink_with(page_idx: usize, ctx: &CleanJobContext, model: &Mutex<CleanOverlaysModel>, before_cleanup: impl FnOnce()) -> Result<UnlinkReport, CleanOpError> {
    let page = ctx.pages.get(page_idx).ok_or_else(|| CleanOpError::Failed(format!("page {page_idx} no longer exists")))?;
    let canonical = clean_assign::PageCleanPaths::for_writer(&ctx.paths, page);
    let (captured, materialized) = {
        let mut guard = lock_model(model)?;
        let captured = guard.overlay_rgba(page_idx);
        let materialized = clean_link::model_page_materialized(&guard, page_idx);
        guard.detach_page_overlay(page_idx);
        (captured, materialized)
    };
    // The file the loader would read for this page (staged shadows committed).
    let on_disk = [&canonical.staged, &canonical.committed].into_iter().find(|path| path.is_file()).cloned();
    let detached = if captured.is_none() && on_disk.is_none() {
        if !materialized {
            return Err(CleanOpError::Failed(format!("page {} has no clean layer to detach", page_idx + 1)));
        }
        None
    } else {
        let persisted = clean_assign::allocate_detached_clean_path(&ctx.paths, &ctx.pages, clean_assign::writer_clean_stem(&page.path)).and_then(|destination| {
            match (&captured, &on_disk) {
                (Some(rgba), _) => clean_assign::write_new_clean_png(&ctx.paths, &destination, rgba),
                (None, Some(file)) => match clean_assign::move_clean_file(&ctx.paths, file, &destination) {
                    // The copy exists at `destination`; the canonical source is step 3's job.
                    Err(error @ clean_assign::CleanFileOpError::SourceNotRemoved { .. }) => {
                        runtime_log::log_warn(format!("[page-manager::clean] unlink copied the clean across devices but could not remove the source; step 3 retries; page={page_idx}; error={error}"));
                        Ok(())
                    }
                    other => other,
                },
                // Unreachable by the branch condition; kept total instead of panicking.
                (None, None) => Ok(()),
            }
            .map(|()| destination)
        });
        match persisted {
            Ok(destination) => Some(destination),
            Err(error) => {
                runtime_log::log_error(format!("[page-manager::clean] unlink could not persist the detached clean; model restored; page={page_idx}; error={error}"));
                restore_detached_pixels(model, page_idx, captured);
                return Err(CleanOpError::Failed(error.to_string()));
            }
        }
    };
    before_cleanup();
    let leftovers = [&canonical.staged, &canonical.committed]
        .into_iter()
        .filter(|path| path.is_file())
        .filter_map(|path| clean_assign::trash_clean_file(&ctx.paths, path).err().map(|error| error.to_string()))
        .collect();
    // Step 4: see the doc comment. A poisoned model cannot hold new strokes worth saving.
    if let Ok(mut guard) = model.lock()
        && guard.mark_overlay_needs_save(page_idx)
    {
        runtime_log::log_info(format!("[page-manager::clean] page {page_idx} was edited during unlink; its new clean is re-queued for autosave"));
    }
    runtime_log::log_info(format!("[page-manager::clean] unlinked page {page_idx}; detached={:?}", detached.as_deref().map(Path::display).map(|path| path.to_string())));
    Ok(UnlinkReport { detached, leftovers })
}

/// Puts pixels captured by [`run_unlink`] back after its persist step failed: recorded as an edit
/// (history + save-dirty), so the autosave writes them again under the canonical name.
fn restore_detached_pixels(model: &Mutex<CleanOverlaysModel>, page_idx: usize, captured: Option<Arc<RgbaImage>>) {
    let Some(rgba) = captured else {
        return;
    };
    let (Ok(width), Ok(height)) = (usize::try_from(rgba.width()), usize::try_from(rgba.height())) else {
        return;
    };
    let color = egui::ColorImage::from_rgba_unmultiplied([width, height], rgba.as_raw());
    match model.lock() {
        Ok(mut guard) => guard.replace_prepared_overlay(page_idx, rgba, color),
        Err(_) => runtime_log::log_error(format!("[page-manager::clean] could not restore detached pixels: clean overlay model is poisoned; page={page_idx}")),
    }
}

/// Binds the unassigned clean `file_name` to page `page_idx` by renaming its surviving copy
/// (staged over committed) to the page's committed canonical `<stem>.png`.
///
/// 1. If the page has a clean (materialized in the model, or any canonical file), it is first
///    detached with [`run_unlink`]; a total failure aborts, leftovers abort as
///    `BindFailedAfterUnlink`. Otherwise the page's (virtual) overlay is still detached, dropping
///    stale undo history that could re-materialize old pixels over the bound file.
/// 2. The source is moved (never replacing) to the committed canonical path. A staged source's
///    committed twin is then trashed, or it would reappear as an unassigned clean.
/// 3. Only when the bound file's size equals the page's exactly is it decoded here and handed to
///    the model with `load_prepared_overlay` (no history, not dirty) — the loader's own rule. A
///    mismatched file is bound as-is and stays unloaded; the page then shows a problem link.
///
/// # Errors
/// `Failed` when nothing changed; `BindFailedAfterUnlink` when the old clean was detached but the
/// bind did not happen; `BindModelLoadFailed` / `BindSourceCleanupFailed` when the file WAS bound
/// (the latter also for a cross-device move whose source could not be removed).
fn run_bind(file_name: &OsStr, page_idx: usize, ctx: &CleanJobContext, model: &Mutex<CleanOverlaysModel>) -> Result<(), CleanOpError> {
    let page = ctx.pages.get(page_idx).ok_or_else(|| CleanOpError::Failed(format!("page {page_idx} no longer exists")))?;
    let not_unassigned = || CleanOpError::Failed(format!("'{}' is not an unassigned clean file", file_name.to_string_lossy()));
    if clean_assign::is_canonical_clean_name(&ctx.pages, file_name) {
        return Err(not_unassigned());
    }
    let (committed_source, staged_source) = clean_assign::unassigned_clean_paths(&ctx.paths, file_name).ok_or_else(not_unassigned)?;
    let source = [&staged_source, &committed_source]
        .into_iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| CleanOpError::Failed(format!("clean file '{}' no longer exists", file_name.to_string_lossy())))?;
    let canonical = clean_assign::PageCleanPaths::for_writer(&ctx.paths, page);
    let materialized = clean_link::model_page_materialized(&*lock_model(model)?, page_idx);
    let replaced = if materialized || canonical.staged.is_file() || canonical.committed.is_file() {
        let report = run_unlink(page_idx, ctx, model)?;
        if !report.leftovers.is_empty() {
            return Err(CleanOpError::BindFailedAfterUnlink { detached: report.detached, error: report.leftovers.join("; ") });
        }
        Some(report.detached)
    } else {
        lock_model(model)?.detach_page_overlay(page_idx);
        None
    };
    // A cross-device move that copied the file but could not remove the source HAS bound the
    // clean; the surviving source is reported like any other leftover copy below.
    let source_left = match clean_assign::move_clean_file(&ctx.paths, &source, &canonical.committed) {
        Ok(()) => None,
        Err(error @ clean_assign::CleanFileOpError::SourceNotRemoved { .. }) => Some(error.to_string()),
        Err(error) => {
            return Err(match replaced {
                Some(detached) => CleanOpError::BindFailedAfterUnlink { detached, error: error.to_string() },
                None => CleanOpError::Failed(error.to_string()),
            });
        }
    };
    runtime_log::log_info(format!("[page-manager::clean] bound '{}' to page {page_idx} as '{}'", source.display(), canonical.committed.display()));
    let cleanup = if let Some(error) = source_left {
        Err(error)
    } else if source == staged_source && committed_source.is_file() {
        clean_assign::trash_clean_file(&ctx.paths, &committed_source).map_err(|error| error.to_string())
    } else {
        Ok(())
    };
    let loaded = load_bound_clean(&canonical.committed, &page.path).and_then(|prepared| {
        if let Some((rgba, color)) = prepared {
            lock_model(model).map_err(|_| "clean overlay model is unavailable".to_string())?.load_prepared_overlay(page_idx, rgba, color);
        }
        Ok(())
    });
    if let Err(error) = &cleanup {
        runtime_log::log_error(format!("[page-manager::clean] bind left a copy of the source behind; source={}; error={error}", source.display()));
    }
    if let Err(error) = &loaded {
        runtime_log::log_error(format!("[page-manager::clean] bound clean could not be loaded into the model; page={page_idx}; error={error}"));
    }
    loaded.map_err(CleanOpError::BindModelLoadFailed)?;
    cleanup.map_err(CleanOpError::BindSourceCleanupFailed)
}

/// Decodes the just-bound clean `file` for the model when (and only when) its size equals the
/// page's exactly: `Ok(None)` for a mismatched or unreadable-header pair (the loader skips those
/// too; the page shows a problem link), `Err` when an exact-size file fails to decode.
fn load_bound_clean(file: &Path, page_path: &Path) -> Result<Option<(Arc<RgbaImage>, egui::ColorImage)>, String> {
    let header = |path: &Path| image::image_dimensions(path).map(|(width, height)| [width, height]).map_err(|error| error.to_string());
    let fit = clean_assign::classify_clean_fit(header(file), || header(page_path));
    let clean_assign::CleanPageFit::Matches { size: [width, height] } = fit else {
        return Ok(None);
    };
    let rgba = image::open(file).map_err(|error| format!("could not decode clean image '{}': {error}", file.display()))?.to_rgba8();
    if [rgba.width(), rgba.height()] != [width, height] {
        return Err(format!("clean image '{}' decoded at {}x{}, header said {width}x{height}", file.display(), rgba.width(), rgba.height()));
    }
    let size = [usize::try_from(width).map_err(|error| error.to_string())?, usize::try_from(height).map_err(|error| error.to_string())?];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    Ok(Some((Arc::new(rgba), color)))
}

/// Executes one mutating job on the worker and scans the inventory right after it.
fn run_mutation(job: CleanJob) -> CleanEvent {
    let (result, ctx) = match job {
        CleanJob::Attach { path, page_idx, page_size, remove_source, model, ctx } => (run_attach(&path, page_idx, page_size, remove_source, &ctx.paths, &model), ctx),
        CleanJob::Unlink { page_idx, model, ctx } => {
            let result = run_unlink(page_idx, &ctx, &model).and_then(|report| {
                if report.leftovers.is_empty() { Ok(()) } else { Err(CleanOpError::UnlinkLeftovers(report.leftovers.join("; "))) }
            });
            (result, ctx)
        }
        CleanJob::Bind { file_name, page_idx, model, ctx } => (run_bind(&file_name, page_idx, &ctx, &model), ctx),
        CleanJob::DeleteUnassigned { file_name, ctx } => {
            (clean_assign::delete_unassigned_clean(&ctx.paths, &ctx.pages, &file_name).map_err(|error| CleanOpError::Failed(error.to_string())), ctx)
        }
        CleanJob::Scan { epoch, paths, pages } => return CleanEvent::Scanned { epoch, inventory: clean_assign::scan_clean_inventory(&paths, &pages) },
        CleanJob::ProbeAttach { path, page_idx, page_size } => return probe_attach(path, page_idx, page_size),
    };
    if let Err(error) = &result {
        runtime_log::log_warn(format!("[page-manager::clean] clean operation did not fully apply: {error:?}"));
    }
    CleanEvent::Finished { epoch: ctx.epoch, result, inventory: clean_assign::scan_clean_inventory(&ctx.paths, &ctx.pages) }
}

/// Header-only probe of a replacement clean (fast enough for a picker follow-up, still off the GUI).
fn probe_attach(path: PathBuf, page_idx: usize, page_size: [u32; 2]) -> CleanEvent {
    let outcome = match image::image_dimensions(&path) {
        Ok((width, height)) => match clean_assign::attach_fit([width, height], page_size) {
            Some(fit) => Ok(fit),
            None => Err(ProbeAttachError::Incompatible { size: [width, height] }),
        },
        Err(err) => Err(ProbeAttachError::Unreadable(err.to_string())),
    };
    CleanEvent::AttachProbed { path, page_idx, page_size, outcome }
}

/// Single-worker runtime; serializing clean operations makes their disk/model
/// ordering deterministic and lets the UI expose one in-flight flag.
pub(super) struct CleanRuntime {
    tx: mpsc::Sender<CleanJob>,
    rx: mpsc::Receiver<CleanEvent>,
}

impl Default for CleanRuntime {
    fn default() -> Self {
        let (tx, jobs) = mpsc::channel();
        let (events, rx) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(job) = jobs.recv() {
                if events.send(run_mutation(job)).is_err() { break; }
            }
        });
        Self { tx, rx }
    }
}

impl CleanRuntime {
    /// Queues a job; channel failure means the worker exited and is reported to the UI.
    pub(super) fn send(&self, job: CleanJob) -> Result<(), String> {
        self.tx.send(job).map_err(|_| "clean worker stopped unexpectedly".to_string())
    }

    /// Drains all completed events without blocking the GUI thread.
    pub(super) fn poll(&self) -> Vec<CleanEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() { events.push(event); }
        events
    }
}

impl PageManagerTabState {
    /// True while a scan for the CURRENT epoch has been submitted but its result
    /// has not arrived yet (drives the spinner and the repaint keep-alive).
    pub(super) fn clean_scan_in_flight(&self) -> bool {
        self.clean_scan_requested_epoch == Some(self.clean_scan_epoch)
            && self.clean_scan_done_epoch != Some(self.clean_scan_epoch)
    }

    /// True while the grid must show its loading placeholder instead of rows: no inventory of the
    /// current pages is installed yet (a fresh tab, or right after `notify_pages_changed`) and a scan
    /// for it is on the way. Row heights depend on the links the inventory yields, so a grid laid
    /// out before it would be too short and the `ScrollArea` would clamp the remembered offset
    /// away. When no scan could be submitted (worker gone, or a clean job holds the scan back) the
    /// grid is drawn with what is known instead of waiting forever.
    #[must_use]
    pub(super) fn grid_awaits_clean_inventory(&self) -> bool {
        self.clean_inventory.is_none() && self.clean_scan_in_flight()
    }

    /// Starts a scan once per epoch (bumped by `notify_pages_changed`, the refresh
    /// button, and every finished clean operation). The pages/paths snapshots make
    /// the worker independent from the live project object.
    pub(super) fn request_clean_scan_if_needed(&mut self, project: &ms_project::ProjectData) {
        if self.clean_op_in_flight || self.clean_scan_requested_epoch == Some(self.clean_scan_epoch) {
            return;
        }
        self.clean_scan_requested_epoch = Some(self.clean_scan_epoch);
        if let Err(error) = self.clean_runtime.send(CleanJob::Scan {
            epoch: self.clean_scan_epoch,
            paths: project.paths.clone(),
            pages: project.pages.clone(),
        }) {
            self.clean_scan_requested_epoch = None;
            self.error_message = Some(tf!("page_manager.clean_operation_failed", error = error));
        }
    }

    /// Applies worker replies on the GUI thread. A finished mutating job carries the inventory
    /// scanned right after it; it is installed when its epoch is still current, otherwise a fresh
    /// scan is requested (the epoch moved while the job ran, e.g. `request_clean_rescan`).
    pub(super) fn absorb_clean_events(&mut self, project: &ms_project::ProjectData) {
        for event in self.clean_runtime.poll() {
            match event {
                CleanEvent::Scanned { epoch, inventory } => {
                    // A result from a superseded epoch describes pages that may have
                    // been renumbered or reloaded; only the current epoch is accepted.
                    if epoch != self.clean_scan_epoch {
                        continue;
                    }
                    self.install_clean_inventory(inventory, false);
                    self.clean_scan_done_epoch = Some(epoch);
                }
                CleanEvent::AttachProbed { path, page_idx, page_size, outcome } => {
                    self.clean_op_in_flight = false;
                    match outcome {
                        Ok(fit) => {
                            self.clean_dialog = Some(CleanDialog::Attach {
                                path,
                                page_idx,
                                page_size,
                                fit,
                                remove_source: false,
                            });
                        }
                        Err(ProbeAttachError::Incompatible { size }) => {
                            self.error_message = Some(tf!(
                                "page_manager.clean_replace_incompatible",
                                clean_width = size[0],
                                clean_height = size[1],
                                page_width = page_size[0],
                                page_height = page_size[1]
                            ));
                        }
                        Err(ProbeAttachError::Unreadable(error)) => {
                            self.error_message = Some(tf!("page_manager.clean_operation_failed", error = error));
                        }
                    }
                }
                CleanEvent::Finished { epoch, result, inventory } => {
                    self.clean_op_in_flight = false;
                    if let Err(error) = result {
                        self.error_message = Some(clean_op_error_message(error));
                    }
                    if epoch == self.clean_scan_epoch {
                        // The worker scanned right after the job: install it as this epoch's scan.
                        self.install_clean_inventory(inventory, true);
                        self.clean_scan_requested_epoch = Some(epoch);
                        self.clean_scan_done_epoch = Some(epoch);
                    } else {
                        // Renamed files keep their mtime: drop their thumbnails even though this
                        // inventory is not installed.
                        self.forget_clean_file_thumbs(&inventory);
                        self.request_clean_scan_if_needed(project);
                    }
                }
            }
        }
    }

    /// Installs a scanned inventory: invalidates the link cache,
    /// and makes thumbnails of clean files revalidate. After a MUTATING job (`after_mutation`) the
    /// thumbnails of every clean path of the old and new inventories are dropped outright,
    /// because a rename puts different content under a path with an unchanged mtime.
    fn install_clean_inventory(&mut self, inventory: CleanInventory, after_mutation: bool) {
        if after_mutation {
            if let Some(old) = self.clean_inventory.take() {
                self.forget_clean_file_thumbs(&old);
            }
            self.forget_clean_file_thumbs(&inventory);
        }
        // Save-merges and autosaves rewrite files in place: force (path, mtime) revalidation.
        self.generation = self.generation.wrapping_add(1);
        self.clean_inventory = Some(inventory);
        self.clean_inventory_epoch = self.clean_inventory_epoch.wrapping_add(1);
    }

    /// Drops the cached thumbnails of every clean file path in `inventory` (both trees).
    fn forget_clean_file_thumbs(&mut self, inventory: &CleanInventory) {
        let pages = inventory.pages.iter().flat_map(|entry| [&entry.committed, &entry.staged]);
        let unassigned = inventory.unassigned.iter().flat_map(|item| [&item.committed, &item.staged]);
        for probe in pages.chain(unassigned).flatten() {
            self.thumbs.forget_path_thumb(&probe.path);
        }
    }

    /// Polls the native replacement picker without blocking the UI. A picked file
    /// is first probed on the clean worker (header dimensions -> real `AttachFit`),
    /// so the confirmation dialog can warn about scaling or reject an incompatible
    /// image instead of always claiming an exact fit.
    pub(super) fn poll_clean_picker(&mut self, project: &ms_project::ProjectData, page_infos: &std::collections::HashMap<usize, PageImageInfo>) {
        let Some(rx) = self.clean_picker_rx.as_ref() else { return; };
        match rx.try_recv() {
            Ok(Some(path)) => {
                self.clean_picker_rx = None;
                // The picker only produces a path; header decode remains in the clean worker.
                let Some(page_idx) = self.selection.iter().next().copied() else { return; };
                let sizes = page_sizes(project, page_infos, &self.thumbs);
                let Some((_, page_size)) = sizes.into_iter().find(|(idx, _)| *idx == page_idx) else { self.error_message = Some(t!("page_manager.clean_size_unknown").to_string()); return; };
                match self.clean_runtime.send(CleanJob::ProbeAttach { path, page_idx, page_size }) {
                    // The probe occupies the serial worker; treat it as in-flight so
                    // mutating buttons stay disabled and frames keep coming.
                    Ok(()) => self.clean_op_in_flight = true,
                    Err(error) => self.error_message = Some(tf!("page_manager.clean_operation_failed", error = error)),
                }
            }
            Ok(None) => self.clean_picker_rx = None,
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => { self.clean_picker_rx = None; self.error_message = Some(t!("page_manager.clean_picker_failed").to_string()); }
        }
    }

    /// Shows operation confirmations and submits the accepted worker job.
    /// [`Self::clean_mutation_blocked`] disables confirmation.
    pub(super) fn draw_clean_dialog(&mut self, ctx: &egui::Context, project: &ms_project::ProjectData) {
        let Some(mut dialog) = self.clean_dialog.take() else { return; };
        let mut keep = true;
        let mut cancel = false;
        let mut confirm = false;
        let title = match dialog {
            CleanDialog::Attach { .. } => t!("page_manager.clean_attach_title"),
            CleanDialog::DeleteUnassigned { .. } => t!("page_manager.clean_delete_title"),
            CleanDialog::Detach { .. } => t!("page_manager.clean_detach_title"),
            CleanDialog::BindMismatch { .. } => t!("page_manager.clean_bind_mismatch_title"),
        };
        egui::Window::new(title).id(egui::Id::new("page_manager_clean_confirm")).collapsible(false).resizable(false).open(&mut keep).show(ctx, |ui| {
            match &mut dialog {
                CleanDialog::Attach { fit, remove_source, .. } => {
                    ui.label(if matches!(fit, AttachFit::ScaleSameAspect) { t!("page_manager.clean_attach_resize_warning") } else { t!("page_manager.clean_attach_message") });
                    ui.checkbox(remove_source, t!("page_manager.clean_remove_source_checkbox"));
                    ui.label(t!("page_manager.clean_trash_note"));
                }
                CleanDialog::DeleteUnassigned { .. } => { ui.label(t!("page_manager.clean_delete_message")); }
                CleanDialog::Detach { .. } => { ui.label(t!("page_manager.clean_detach_message")); }
                CleanDialog::BindMismatch { page_idx, fit, .. } => { ui.label(bind_mismatch_message(*page_idx, fit.as_ref())); }
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !self.clean_mutation_blocked(),
                        egui::Button::new(t!("page_manager.clean_confirm_button")),
                    )
                    .clicked()
                {
                    confirm = true;
                }
                if ui.button(t!("page_manager.dialog.cancel_button")).clicked() {
                    cancel = true;
                }
            });
        });
        if confirm {
            match dialog {
                CleanDialog::Attach { path, page_idx, page_size, remove_source, .. } => {
                    let model = self.overlays_model.clone();
                    self.submit_clean_mutation(project, |ctx| model.map(|model| CleanJob::Attach { path, page_idx, page_size, remove_source, model, ctx }));
                }
                CleanDialog::DeleteUnassigned { file_name } => self.request_delete_unassigned(project, &file_name),
                CleanDialog::Detach { page_idx } => self.request_unlink(project, page_idx),
                CleanDialog::BindMismatch { file_name, page_idx, .. } => self.request_bind(project, &file_name, page_idx),
            }
            return;
        }
        if keep && !cancel { self.clean_dialog = Some(dialog); }
    }

    /// Recomputes the per-page link state when the model revision, the installed inventory, or
    /// the page count changed ([`clean_link::page_clean_link`] per page; no I/O, no lock).
    pub(super) fn refresh_clean_links(&mut self, project: &ms_project::ProjectData) {
        let key = (self.overlays_revision_seen, self.clean_inventory_epoch, project.pages.len());
        if self.clean_links_key == Some(key) {
            return;
        }
        self.clean_links_key = Some(key);
        let entries = self.clean_inventory.as_ref().map(|inventory| inventory.pages.as_slice()).unwrap_or_default();
        self.clean_links = project
            .pages
            .iter()
            .enumerate()
            .map(|(idx, page)| {
                // The inventory lists pages in the order it was scanned with; guard against a
                // stale one by also matching the page's own index.
                let entry = entries.get(idx).filter(|entry| entry.page_idx == page.idx);
                let model = self.model_page_cleans.get(idx).copied().unwrap_or_default();
                let canonical = clean_assign::clean_overlay_file_name(clean_assign::writer_clean_stem(&page.path));
                clean_link::page_clean_link(entry, model, &canonical)
            })
            .collect();
    }

    /// Link state of page `page_idx` as of this frame, `None` for an out-of-range page.
    pub(crate) fn page_clean_link(&self, page_idx: usize) -> Option<&clean_link::PageCleanLink> {
        self.clean_links.get(page_idx)
    }

    /// Number of pages with a clean card (an OK or problem link), the same source the grid draws
    /// clean cards from. 0 until the first inventory lands.
    #[must_use]
    pub(crate) fn linked_clean_count(&self) -> usize {
        self.clean_links.iter().filter(|link| link.has_clean()).count()
    }

    /// The «Клин без страницы» list: clean files named after no page, merged across both trees
    /// (staged shadows committed), sorted by file name. Empty until the first scan lands.
    pub(crate) fn unassigned_cleans(&self) -> &[UnassignedClean] {
        self.clean_inventory.as_ref().map(|inventory| inventory.unassigned.as_slice()).unwrap_or_default()
    }

    /// The two groups of the "bind to …" menu (pages without a clean / pages whose clean will be
    /// replaced), from this frame's links.
    pub(crate) fn clean_bind_targets(&self) -> clean_link::BindTargets {
        clean_link::bind_targets(&self.clean_links)
    }

    /// The exact-size fit of the unassigned clean `file_name` against page `page_idx`, from the
    /// installed inventory; anything but `Matches` warrants the "bind as-is" warning. `None` when
    /// either is unknown (no scan yet, stale name or page).
    pub(crate) fn clean_bind_fit(&self, file_name: &OsStr, page_idx: usize) -> Option<CleanPageFit> {
        let inventory = self.clean_inventory.as_ref()?;
        let source = inventory.unassigned.iter().find(|item| item.file_name == file_name)?;
        clean_link::bind_fit(source, inventory.pages.get(page_idx)?)
    }

    /// True while no clean mutation may start: a clean job/probe is in flight, a structural op or
    /// save is running (`op_in_progress` of the current frame), or the app's overlay loader is
    /// still delivering (`set_overlays_loading`) — a late `load_prepared_overlay` would overwrite
    /// what the mutation put into the model.
    #[must_use]
    pub(crate) fn clean_mutation_blocked(&self) -> bool {
        self.clean_op_in_flight || self.op_in_progress || self.overlays_loading
    }

    /// Builds a mutating job from a fresh [`CleanJobContext`] and queues it, setting the in-flight
    /// flag. Refused (no-op) while [`Self::clean_mutation_blocked`]; `build` returning `None`
    /// means the overlays model is not wired, reported as a localized error.
    fn submit_clean_mutation(&mut self, project: &ms_project::ProjectData, build: impl FnOnce(CleanJobContext) -> Option<CleanJob>) {
        if self.clean_mutation_blocked() {
            return;
        }
        let ctx = CleanJobContext { epoch: self.clean_scan_epoch, paths: project.paths.clone(), pages: project.pages.clone() };
        match build(ctx) {
            Some(job) => match self.clean_runtime.send(job) {
                Ok(()) => {
                    self.error_message = None;
                    self.clean_op_in_flight = true;
                }
                Err(error) => self.error_message = Some(tf!("page_manager.clean_operation_failed", error = error)),
            },
            None => self.error_message = Some(t!("page_manager.clean_model_unavailable").to_string()),
        }
    }

    /// Detaches page `page_idx`'s clean into an unassigned `<stem>[_<n>]_detached.png` holding
    /// its CURRENT pixels (unsaved edits included). Immediate; no confirmation is shown here.
    /// No-op while [`Self::clean_mutation_blocked`] or for an out-of-range page.
    pub(crate) fn request_unlink(&mut self, project: &ms_project::ProjectData, page_idx: usize) {
        if page_idx >= project.pages.len() {
            return;
        }
        let model = self.overlays_model.clone();
        self.submit_clean_mutation(project, |ctx| model.map(|model| CleanJob::Unlink { page_idx, model, ctx }));
    }

    /// Binds the unassigned clean named `file_name` (an [`UnassignedClean::file_name`]) to page
    /// `page_idx` by renaming it to the page's canonical name. A clean the page already has is
    /// detached first (kept as an unassigned file). A size mismatch is bound as-is (the caller
    /// warns first, see [`Self::clean_bind_fit`]). No-op while blocked or for an out-of-range page.
    ///
    /// [`UnassignedClean::file_name`]: ms_models::clean_assign::UnassignedClean::file_name
    pub(crate) fn request_bind(&mut self, project: &ms_project::ProjectData, file_name: &OsStr, page_idx: usize) {
        if page_idx >= project.pages.len() {
            return;
        }
        let model = self.overlays_model.clone();
        let file_name = file_name.to_os_string();
        self.submit_clean_mutation(project, |ctx| model.map(|model| CleanJob::Bind { file_name, page_idx, model, ctx }));
    }

    /// Deletes the unassigned clean named `file_name` from BOTH trees (staged removed, committed
    /// to `.pageop_trash`). No-op while [`Self::clean_mutation_blocked`].
    pub(crate) fn request_delete_unassigned(&mut self, project: &ms_project::ProjectData, file_name: &OsStr) {
        let file_name = file_name.to_os_string();
        self.submit_clean_mutation(project, |ctx| Some(CleanJob::DeleteUnassigned { file_name, ctx }));
    }

    /// Opens the asynchronous native picker for a selected page's replacement clean.
    /// Blocked while any clean op / probe / structural op is in flight, and while a
    /// previous picker is still open (its receiver must not be overwritten).
    pub(super) fn start_replace_clean_picker(&mut self) {
        if !self.clean_mutation_blocked() && self.clean_picker_rx.is_none() {
            self.clean_picker_rx = Some(spawn_clean_picker());
        }
    }

    /// Opens the unlink confirmation for page `page_idx` (the context-menu path; the in-gap
    /// control of `clean_cards.rs` confirms inline). Refused while clean mutations are blocked.
    pub(super) fn start_detach_clean(&mut self, page_idx: usize) {
        if !self.clean_mutation_blocked() {
            self.clean_dialog = Some(CleanDialog::Detach { page_idx });
        }
    }
}

fn page_sizes(project: &ms_project::ProjectData, page_infos: &std::collections::HashMap<usize, PageImageInfo>, thumbs: &super::thumbs::ThumbRuntime) -> Vec<(usize, [u32; 2])> {
    project.pages.iter().enumerate().filter_map(|(idx, page)| page_infos.get(&idx).map(|info| [info.width_px, info.height_px]).filter(|size| size[0] > 0 && size[1] > 0).or_else(|| thumbs.cache.peek(&page.path).and_then(|entry| entry.full_size).map(|(width, height)| [width, height])).map(|size| (idx, size))).collect()
}

/// The localized message for a failed or partially applied clean operation.
fn clean_op_error_message(error: CleanOpError) -> String {
    match error {
        CleanOpError::Failed(error) => tf!("page_manager.clean_operation_failed", error = error),
        CleanOpError::AttachSourceCleanupFailed(error) => tf!("page_manager.clean_attach_partial", error = error),
        CleanOpError::AttachPersistFailed(error) => tf!("page_manager.clean_attach_persist_failed", error = error),
        CleanOpError::UnlinkLeftovers(error) => tf!("page_manager.clean_detach_partial", error = error),
        CleanOpError::BindFailedAfterUnlink { detached, error } => {
            // Persistence identifier (a file name), surfaced only through the placeholder.
            let detached = detached.as_deref().and_then(Path::file_name).map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| "-".to_string());
            tf!("page_manager.clean_bind_failed_after_unlink", detached = detached, error = error)
        }
        CleanOpError::BindSourceCleanupFailed(error) => tf!("page_manager.clean_bind_source_cleanup_failed", error = error),
        CleanOpError::BindModelLoadFailed(error) => tf!("page_manager.clean_bind_model_load_failed", error = error),
    }
}

/// The body of the "bind at a different size" warning for page `page_idx` (0-based): both sizes
/// for a mismatch, otherwise why the size could not be checked. `None` means the fit is unknown
/// (no scan of the file or page yet).
fn bind_mismatch_message(page_idx: usize, fit: Option<&CleanPageFit>) -> String {
    let page = page_idx + 1;
    match fit {
        Some(CleanPageFit::SizeMismatch { clean, page: page_size }) => tf!(
            "page_manager.clean_bind_mismatch_message",
            page = page,
            clean_width = clean[0],
            clean_height = clean[1],
            page_width = page_size[0],
            page_height = page_size[1]
        ),
        Some(CleanPageFit::CleanUnreadable(error) | CleanPageFit::PageUnreadable { error, .. }) => tf!("page_manager.clean_bind_unchecked_message", page = page, error = error),
        // An exact match never reaches this dialog; treat it like an unknown fit if it does.
        Some(CleanPageFit::Matches { .. }) | None => tf!("page_manager.clean_bind_unchecked_message", page = page, error = t!("page_manager.card.size_unknown")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Project paths with only the two clean trees set (everything else empty).
    fn clean_only_paths(clean: PathBuf, unsaved_clean: PathBuf) -> ProjectPaths {
        ProjectPaths {
            project_dir: PathBuf::new(), title_dir: PathBuf::new(), notes_file: PathBuf::new(), char_favorites_file: PathBuf::new(), color_presets_file: PathBuf::new(), bubbles_file: PathBuf::new(), src_dir: PathBuf::new(), clean_layers_dir: clean, cleaned_dir: PathBuf::new(), alt_vers_dir: PathBuf::new(), saved_dir: PathBuf::new(), image_bubbles_dir: PathBuf::new(), text_images_dir: PathBuf::new(), layers_dir: PathBuf::new(), text_detection_dir: PathBuf::new(), characters_dir: PathBuf::new(), terms_file: PathBuf::new(), settings_file: PathBuf::new(), unsaved_dir: PathBuf::new(), unsaved_bubbles_file: PathBuf::new(), unsaved_clean_layers_dir: unsaved_clean, unsaved_image_bubbles_dir: PathBuf::new(), unsaved_text_images_dir: PathBuf::new(), unsaved_layers_dir: PathBuf::new(),
        }
    }

    /// A fresh, empty directory under the system temp dir, unique per test and process.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ms_page_manager_clean_{tag}_{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("clear stale test dir");
        }
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// Writes an opaque 4x4 PNG to `path` (a valid attach source for a 4x4 page).
    fn write_clean_png(path: &Path) {
        image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255])).save(path).expect("write source png");
    }

    fn one_page_model() -> Mutex<CleanOverlaysModel> {
        Mutex::new(CleanOverlaysModel::new_from_pages(&[PathBuf::from("001.png")]))
    }

    /// Attach with `remove_source` writes the page to `_unsaved/clean_layers` BEFORE trashing the
    /// staged source, and leaves the page dirty so the gate-driven autosave rewrites current pixels.
    #[test]
    fn attach_persists_the_page_before_removing_the_staged_source() {
        let dir = unique_temp_dir("attach_persist");
        let unsaved = dir.join("unsaved_clean");
        std::fs::create_dir_all(&unsaved).expect("create unsaved dir");
        let source = unsaved.join("orphan.png");
        write_clean_png(&source);
        let paths = clean_only_paths(dir.join("clean"), unsaved.clone());
        let model = one_page_model();
        assert!(run_attach(&source, 0, [4, 4], true, &paths, &model).is_ok());
        let written = image::open(unsaved.join("001.png")).expect("attached page written").to_rgba8();
        assert_eq!(written.get_pixel(0, 0), &image::Rgba([10, 20, 30, 255]));
        assert!(!source.exists(), "the staged source must be removed after the write");
        assert!(model.lock().expect("model lock").has_unsaved_overlay_changes(), "the page stays dirty for the autosave");
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    /// A failed write keeps the source (the clean must never exist only in memory) and reports the
    /// distinct persist-failure outcome.
    #[test]
    fn attach_keeps_the_source_when_the_write_fails() {
        let dir = unique_temp_dir("attach_write_fails");
        let unsaved = dir.join("unsaved_clean");
        std::fs::create_dir_all(&unsaved).expect("create unsaved dir");
        let source = unsaved.join("orphan.png");
        write_clean_png(&source);
        // The target "directory" is a regular file, so creating it fails.
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, b"not a dir").expect("write blocker file");
        let paths = clean_only_paths(dir.join("clean"), blocked.join("clean_layers"));
        let model = one_page_model();
        let result = run_attach(&source, 0, [4, 4], true, &paths, &model);
        assert!(matches!(result, Err(CleanOpError::AttachPersistFailed(_))));
        assert!(source.exists(), "the source must be kept when the attached page was not written");
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    /// A staged clean attached to its own page is the file just written: it must not be trashed.
    #[test]
    fn attach_does_not_trash_a_source_that_is_the_written_file() {
        let dir = unique_temp_dir("attach_in_place");
        let unsaved = dir.join("unsaved_clean");
        std::fs::create_dir_all(&unsaved).expect("create unsaved dir");
        let source = unsaved.join("001.png");
        write_clean_png(&source);
        let paths = clean_only_paths(dir.join("clean"), unsaved);
        let model = one_page_model();
        assert!(run_attach(&source, 0, [4, 4], true, &paths, &model).is_ok());
        assert!(source.exists(), "the in-place source is the persisted clean");
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    /// A throwaway chapter: `(dir, paths, pages)` with one 4x4 page `001.png` and both clean trees
    /// created. `title_dir` is real so committed files can be trashed into `.pageop_trash`.
    fn chapter(tag: &str) -> (PathBuf, ProjectPaths, Vec<Page>) {
        let dir = unique_temp_dir(tag);
        let mut paths = clean_only_paths(dir.join("chapter/clean_layers"), dir.join("chapter_unsaved/clean_layers"));
        paths.title_dir = dir.clone();
        paths.project_dir = dir.join("chapter");
        paths.src_dir = dir.join("chapter/src");
        for tree in [&paths.src_dir, &paths.clean_layers_dir, &paths.unsaved_clean_layers_dir] {
            std::fs::create_dir_all(tree).expect("create tree");
        }
        let page = paths.src_dir.join("001.png");
        write_png(&page, 4, [0, 0, 0, 255]);
        (dir, paths, vec![Page { idx: 0, path: page }])
    }

    fn write_png(path: &Path, side: u32, pixel: [u8; 4]) {
        image::RgbaImage::from_pixel(side, side, image::Rgba(pixel)).save(path).expect("write png");
    }

    fn context(paths: &ProjectPaths, pages: &[Page]) -> CleanJobContext {
        CleanJobContext { epoch: 7, paths: paths.clone(), pages: pages.to_vec() }
    }

    fn model_for(pages: &[Page]) -> Mutex<CleanOverlaysModel> {
        Mutex::new(CleanOverlaysModel::new_from_pages(&pages.iter().map(|page| page.path.clone()).collect::<Vec<_>>()))
    }

    fn pixel(path: &Path) -> image::Rgba<u8> {
        *image::open(path).expect("decode").to_rgba8().get_pixel(0, 0)
    }

    #[test]
    fn unlink_materialized_persists_current_pixels_and_trashes_both_canonical_files() {
        let (dir, paths, pages) = chapter("unlink_materialized");
        write_png(&paths.clean_layers_dir.join("001.png"), 4, [1, 1, 1, 255]);
        write_png(&paths.unsaved_clean_layers_dir.join("001.png"), 4, [2, 2, 2, 255]);
        let model = model_for(&pages);
        model.lock().expect("lock").replace_from_rgba(0, image::RgbaImage::from_pixel(4, 4, image::Rgba([9, 8, 7, 255])));
        let report = run_unlink(0, &context(&paths, &pages), &model).expect("unlink");
        let detached = paths.clean_layers_dir.join("001_detached.png");
        assert_eq!(report.detached.as_deref(), Some(detached.as_path()));
        assert!(report.leftovers.is_empty());
        assert_eq!(pixel(&detached), image::Rgba([9, 8, 7, 255]), "the model's unsaved pixels are kept");
        assert!(!paths.clean_layers_dir.join("001.png").exists());
        assert!(!paths.unsaved_clean_layers_dir.join("001.png").exists());
        let guard = model.lock().expect("lock");
        assert!(guard.is_overlay_virtual_absent(0));
        assert!(!guard.has_unsaved_overlay_changes());
        drop(guard);
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    /// The user paints the page in the cleaning tab and the autosave writes it to the staged
    /// `001.png` AFTER step 1 detached the page but BEFORE step 3 removed that staged file. The
    /// new strokes are the page's new clean: they must be queued for the next autosave again, not
    /// left un-dirty with no file on disk.
    #[test]
    fn unlink_requeues_a_page_edited_between_detach_and_cleanup() {
        let (dir, paths, pages) = chapter("unlink_interleaved_edit");
        let model = model_for(&pages);
        model.lock().expect("lock").replace_from_rgba(0, image::RgbaImage::from_pixel(4, 4, image::Rgba([9, 8, 7, 255])));
        let staged = paths.unsaved_clean_layers_dir.join("001.png");
        let report = run_unlink_with(0, &context(&paths, &pages), &model, || {
            let snapshots = {
                let mut guard = model.lock().expect("lock");
                guard.replace_from_rgba(0, image::RgbaImage::from_pixel(4, 4, image::Rgba([5, 5, 5, 255])));
                guard.take_dirty_save_snapshots()
            };
            save_overlay_snapshots_guarded(&paths.unsaved_clean_layers_dir, &snapshots, &model).expect("autosave");
            assert_eq!(pixel(&staged), image::Rgba([5, 5, 5, 255]), "the autosave wrote the new strokes");
            assert!(!model.lock().expect("lock").has_unsaved_overlay_changes(), "…and took the page's dirty mark");
        })
        .expect("unlink");
        assert_eq!(pixel(&paths.clean_layers_dir.join("001_detached.png")), image::Rgba([9, 8, 7, 255]), "the old clean is detached");
        assert!(report.leftovers.is_empty());
        let snapshots = model.lock().expect("lock").take_dirty_save_snapshots();
        assert_eq!(snapshots.len(), 1, "the new strokes are queued for the next autosave again");
        assert_eq!(*snapshots[0].image.get_pixel(0, 0), image::Rgba([5, 5, 5, 255]));
        save_overlay_snapshots_guarded(&paths.unsaved_clean_layers_dir, &snapshots, &model).expect("next autosave");
        assert_eq!(pixel(&staged), image::Rgba([5, 5, 5, 255]), "the next autosave restores the staged file");
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    #[test]
    fn unlink_virtual_moves_the_loader_file_byte_exact() {
        for (tag, staged) in [("unlink_virtual_committed", false), ("unlink_virtual_staged", true)] {
            let (dir, paths, pages) = chapter(tag);
            let committed = paths.clean_layers_dir.join("001.png");
            std::fs::write(&committed, b"committed bytes").expect("write");
            if staged {
                std::fs::write(paths.unsaved_clean_layers_dir.join("001.png"), b"staged bytes").expect("write");
            }
            let report = run_unlink(0, &context(&paths, &pages), &model_for(&pages)).expect("unlink");
            let expected: &[u8] = if staged { b"staged bytes" } else { b"committed bytes" };
            assert_eq!(std::fs::read(report.detached.expect("detached")).expect("read"), expected, "{tag}");
            assert!(!committed.exists() && !paths.unsaved_clean_layers_dir.join("001.png").exists(), "{tag}");
            std::fs::remove_dir_all(&dir).expect("remove test dir");
        }
    }

    #[test]
    fn unlink_detaches_a_problem_file_too_and_refuses_nothing() {
        let (dir, paths, pages) = chapter("unlink_problem");
        let model = model_for(&pages);
        assert!(matches!(run_unlink(0, &context(&paths, &pages), &model), Err(CleanOpError::Failed(_))));
        write_png(&paths.clean_layers_dir.join("001.png"), 2, [5, 5, 5, 255]);
        write_png(&paths.clean_layers_dir.join("001_detached.png"), 2, [6, 6, 6, 255]);
        let report = run_unlink(0, &context(&paths, &pages), &model).expect("unlink");
        assert_eq!(report.detached, Some(paths.clean_layers_dir.join("001_1_detached.png")), "collision-safe name");
        assert_eq!(pixel(&paths.clean_layers_dir.join("001_1_detached.png")), image::Rgba([5, 5, 5, 255]));
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    #[test]
    fn unlink_write_failure_restores_the_model_as_dirty() {
        let (dir, mut paths, pages) = chapter("unlink_restore");
        // A committed "tree" that is a regular file: listing it fails, so no name can be allocated.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").expect("write blocker");
        paths.clean_layers_dir = blocker;
        let model = model_for(&pages);
        let pixels = image::RgbaImage::from_pixel(4, 4, image::Rgba([3, 4, 5, 255]));
        model.lock().expect("lock").replace_from_rgba(0, pixels.clone());
        assert!(matches!(run_unlink(0, &context(&paths, &pages), &model), Err(CleanOpError::Failed(_))));
        let guard = model.lock().expect("lock");
        assert!(guard.has_unsaved_overlay_changes(), "restored pixels are dirty so the autosave rewrites them");
        assert_eq!(guard.overlay_rgba(0).as_deref(), Some(&pixels));
        drop(guard);
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    #[test]
    fn bind_exact_size_loads_the_model_without_dirtying_it() {
        let (dir, paths, pages) = chapter("bind_exact");
        write_png(&paths.clean_layers_dir.join("x_detached.png"), 4, [7, 7, 7, 255]);
        let model = model_for(&pages);
        run_bind(OsStr::new("x_detached.png"), 0, &context(&paths, &pages), &model).expect("bind");
        assert_eq!(pixel(&paths.clean_layers_dir.join("001.png")), image::Rgba([7, 7, 7, 255]));
        assert!(!paths.clean_layers_dir.join("x_detached.png").exists());
        let guard = model.lock().expect("lock");
        assert!(!guard.is_overlay_virtual_absent(0));
        assert!(!guard.has_unsaved_overlay_changes());
        drop(guard);
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    #[test]
    fn bind_mismatch_renames_as_is_and_leaves_the_model_virtual() {
        let (dir, paths, pages) = chapter("bind_mismatch");
        write_png(&paths.clean_layers_dir.join("small.png"), 2, [7, 7, 7, 255]);
        let model = model_for(&pages);
        run_bind(OsStr::new("small.png"), 0, &context(&paths, &pages), &model).expect("bind");
        assert_eq!(image::image_dimensions(paths.clean_layers_dir.join("001.png")).expect("dims"), (2, 2));
        assert!(model.lock().expect("lock").is_overlay_virtual_absent(0));
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    #[test]
    fn bind_replace_detaches_the_old_clean_first() {
        let (dir, paths, pages) = chapter("bind_replace");
        std::fs::write(paths.clean_layers_dir.join("001.png"), b"old clean").expect("write");
        std::fs::write(paths.clean_layers_dir.join("new.png"), b"new clean").expect("write");
        run_bind(OsStr::new("new.png"), 0, &context(&paths, &pages), &model_for(&pages)).expect("bind");
        assert_eq!(std::fs::read(paths.clean_layers_dir.join("001_detached.png")).expect("read"), b"old clean");
        assert_eq!(std::fs::read(paths.clean_layers_dir.join("001.png")).expect("read"), b"new clean");
        assert!(!paths.clean_layers_dir.join("new.png").exists());
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    #[test]
    fn bind_staged_source_wins_and_its_committed_twin_is_trashed() {
        let (dir, paths, pages) = chapter("bind_staged");
        std::fs::write(paths.clean_layers_dir.join("z.png"), b"committed twin").expect("write");
        std::fs::write(paths.unsaved_clean_layers_dir.join("z.png"), b"staged copy").expect("write");
        run_bind(OsStr::new("z.png"), 0, &context(&paths, &pages), &model_for(&pages)).expect("bind");
        assert_eq!(std::fs::read(paths.clean_layers_dir.join("001.png")).expect("read"), b"staged copy");
        assert!(!paths.clean_layers_dir.join("z.png").exists() && !paths.unsaved_clean_layers_dir.join("z.png").exists());
        assert!(matches!(run_bind(OsStr::new("001.png"), 0, &context(&paths, &pages), &model_for(&pages)), Err(CleanOpError::Failed(_))), "a bound name is not a source");
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    #[test]
    fn delete_unassigned_finishes_with_a_fresh_inventory() {
        let (dir, paths, pages) = chapter("delete_unassigned");
        std::fs::write(paths.clean_layers_dir.join("gone.png"), b"c").expect("write");
        std::fs::write(paths.unsaved_clean_layers_dir.join("gone.png"), b"s").expect("write");
        std::fs::write(paths.clean_layers_dir.join("kept.png"), b"k").expect("write");
        let event = run_mutation(CleanJob::DeleteUnassigned { file_name: OsString::from("gone.png"), ctx: context(&paths, &pages) });
        let CleanEvent::Finished { epoch, result, inventory } = event else { panic!("a mutating job must finish") };
        assert_eq!(epoch, 7);
        assert!(result.is_ok());
        let names: Vec<_> = inventory.unassigned.iter().map(|item| item.file_name.clone()).collect();
        assert_eq!(names, vec![OsString::from("kept.png")]);
        assert!(!paths.unsaved_clean_layers_dir.join("gone.png").exists());
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    #[test]
    fn mutations_are_refused_while_blocked() {
        let (dir, paths, pages) = chapter("blocked");
        let project = ms_project::ProjectData {
            project_dir: paths.project_dir.clone(),
            image_dir: paths.src_dir.clone(),
            pages,
            bubbles: Arc::new(Vec::new()),
            paths,
            comic_type: None,
            canvas_settings: ms_project::CanvasSettings::default(),
            settings_data: Default::default(),
            session: Default::default(),
        };
        let mut state = PageManagerTabState::default();
        state.set_overlays_model(Arc::new(model_for(&project.pages)));
        state.set_overlays_loading(true);
        state.request_bind(&project, OsStr::new("x.png"), 0);
        state.request_delete_unassigned(&project, OsStr::new("x.png"));
        state.request_unlink(&project, 0);
        assert!(!state.clean_op_in_flight(), "nothing may be queued while the overlay loader runs");
        assert!(state.unassigned_cleans().is_empty() && state.page_clean_link(0).is_none());
        assert_eq!(state.clean_bind_fit(OsStr::new("x.png"), 0), None);
        assert_eq!(state.clean_bind_targets(), clean_link::BindTargets::default());
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    #[test]
    fn grid_waits_for_the_first_inventory_of_the_current_pages() {
        let (dir, paths, pages) = chapter("grid_waits");
        let project = ms_project::ProjectData {
            project_dir: paths.project_dir.clone(),
            image_dir: paths.src_dir.clone(),
            pages,
            bubbles: Arc::new(Vec::new()),
            paths,
            comic_type: None,
            canvas_settings: ms_project::CanvasSettings::default(),
            settings_data: Default::default(),
            session: Default::default(),
        };
        let mut state = PageManagerTabState::default();
        // Nothing requested yet: nothing to wait for.
        assert!(!state.grid_awaits_clean_inventory());
        state.request_clean_scan_if_needed(&project);
        assert!(state.grid_awaits_clean_inventory());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while state.grid_awaits_clean_inventory() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
            state.absorb_clean_events(&project);
        }
        assert!(!state.grid_awaits_clean_inventory(), "the scan result ends the wait");
        // A rescan of an installed inventory never brings the placeholder back.
        state.request_clean_rescan();
        state.request_clean_scan_if_needed(&project);
        assert!(state.clean_scan_in_flight() && !state.grid_awaits_clean_inventory());
        // New pages: the old inventory is dropped and the grid waits again.
        state.notify_pages_changed();
        state.request_clean_scan_if_needed(&project);
        assert!(state.grid_awaits_clean_inventory());
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }
}
