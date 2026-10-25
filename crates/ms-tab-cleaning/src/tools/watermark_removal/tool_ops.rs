/*
File: tools/watermark_removal/tool_ops.rs

Purpose:
The inherent behaviour of `WatermarkRemovalTool`: settings load/save, status, the library
panel, selection diversion, library capture, chapter job start / poll / apply and model unload.

Key items:
- `impl WatermarkRemovalTool`

Notes:
The struct and its `Default` are declared in `mod.rs`.
*/
use crate::tools::mask_generation::{poll_watermark_status, spawn_watermark_status_query};
use crate::tools::watermark_library::{list_entries, save_entry};
use crate::tools::watermark_library_window::{
    LibraryArm, LibraryCaptureRefusal, LibraryPanelRequest, remove_scratch_crop,
};
use ms_canvas::{CanvasView, OverlayRectPx};
use ms_project::ProjectData;
use ms_thread as thread;
use std::sync::mpsc::{self, TryRecvError};
use super::{
    CHAPTER_MAX_TEMPLATE_SIDE, ChapterApplyBatch, ChapterApplyReport, ChapterApplyRequest,
    ChapterEvent, ChapterJobLabel, ChapterMatchRequest, ChapterPageTask, ChapterRequest,
    ChapterSampleRequest, ChapterScanRequest, LibraryCaptureRequest, WatermarkMode,
    WatermarkRemovalTool,
};
use super::backend::{load_watermark_settings, save_watermark_settings};
use super::chapter_jobs::{
    build_library_request, chapter_source_ref, run_chapter_apply, run_chapter_library_load,
    run_chapter_sample, run_chapter_scan, run_chapter_use_match,
};
use super::chapter_state::{next_library_arm, page_rect_to_overlay};
use super::library_capture::run_library_capture;
use super::settings::effective_watermark_mode;


impl WatermarkRemovalTool {
    /// The mode this session runs in; see `effective_watermark_mode`.
    pub(super) fn effective_mode(&self) -> WatermarkMode {
        effective_watermark_mode(&self.settings.mode, self.series_watermark_library)
    }

    /// Reads the settings file on a worker thread (never on the GUI thread).
    pub(super) fn request_settings_load(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.settings_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(load_watermark_settings());
        });
    }

    /// Applies a finished settings load. A disconnected channel keeps the
    /// in-memory defaults and unblocks saving.
    pub(super) fn poll_settings_load(&mut self) {
        let Some(rx) = self.settings_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(settings) => {
                self.settings = settings;
                self.settings_loaded = true;
                self.settings_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.settings_loaded = true;
                self.settings_rx = None;
            }
        }
    }

    /// Writes dirty settings on a worker thread, at most one save in flight, and
    /// never before the initial load finished (which would clobber the file).
    pub(super) fn poll_and_maybe_save(&mut self) {
        if let Some(rx) = self.save_rx.as_ref() {
            match rx.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => self.save_rx = None,
                Err(TryRecvError::Empty) => return,
            }
        }
        if !self.dirty || !self.settings_loaded {
            return;
        }
        self.dirty = false;
        let settings = self.settings.clone();
        let (tx, rx) = mpsc::channel();
        self.save_rx = Some(rx);
        thread::spawn(move || {
            if let Err(err) = save_watermark_settings(&settings) {
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] failed to save watermark removal settings: {err}"
                ));
            }
            let _ = tx.send(());
        });
    }

    /// Polls the catalog query and arms a new one when it is wanted and nothing
    /// else is running. The query is never issued while a run is in flight, so
    /// the backend is not asked mid-download.
    pub(super) fn poll_and_maybe_query_status(&mut self) {
        poll_watermark_status(&mut self.status, &mut self.status_rx);
        if self.status_wanted
            && self.ai_backend_available
            && self.status_rx.is_none()
            && self.session.run_rx.is_none()
        {
            self.status_wanted = false;
            self.status_rx = Some(spawn_watermark_status_query());
        }
    }

    /// Snapshot of every page of the chapter, taken on the GUI thread before the editor
    /// borrows the canvas: path, index and the overlay size the canvas already knows.
    fn chapter_page_tasks(&self, canvas: &CanvasView, project: &ProjectData) -> Vec<ChapterPageTask> {
        project
            .pages
            .iter()
            .map(|page| ChapterPageTask {
                page_idx: page.idx,
                path: page.path.clone(),
                overlay_size: canvas.overlay_size(page.idx),
            })
            .collect()
    }

    /// Ids of the library entries the open chapter is currently searching for.
    ///
    /// Membership is the CATALOG, not `pinned_entry`: an entry is being hunted exactly when a
    /// mark of its own was loaded from it. `ChapterCatalog::index_of` keys on the kind id, and
    /// a kind loaded from the library carries the entry id, but a chapter-DISCOVERED mark is
    /// `mark-{n}`; `library_entry` is what tells the two apart, so it is what is checked here
    /// and in `unload_library_entry`.
    pub(super) fn chapter_library_selection(&self) -> Vec<String> {
        self.chapter
            .catalog
            .kinds
            .iter()
            .zip(self.chapter.catalog.marks.iter())
            .filter(|(kind, mark)| mark.library_entry.as_deref() == Some(kind.id()))
            .map(|(kind, _)| kind.id().to_string())
            .collect()
    }

    /// Ids of the library entries a chapter-DISCOVERED mark was written to.
    ///
    /// «В библиотеку» stamps the new entry id onto a mark whose kind id stays `mark-{n}`, so
    /// such an entry is already in the chapter under a different name: loading it would give
    /// the catalog a SECOND kind for one physical mark, and «Убрать» would then drop only the
    /// loaded copy. It is therefore neither selectable nor unselectable — the card says it is
    /// already here, and `ChapterRequest::LoadLibrary` refuses it for the same reason.
    /// Index of the catalog mark that ALREADY represents `entry_id`, however it got there.
    ///
    /// Two things can put an entry in the chapter: loading it (the kind takes the entry's id)
    /// and saving a discovered mark to it (the kind keeps `mark-{n}` and only the mark names
    /// the entry). Both count, because either way a second load would give the catalog two
    /// kinds for one physical mark and everything keyed on that id would resolve to whichever
    /// came first.
    pub(super) fn chapter_entry_index(&self, entry_id: &str) -> Option<usize> {
        self.chapter.catalog.index_of(entry_id).or_else(|| {
            self.chapter
                .catalog
                .marks
                .iter()
                .position(|mark| mark.library_entry.as_deref() == Some(entry_id))
        })
    }

    pub(super) fn chapter_saved_entries(&self) -> Vec<String> {
        self.chapter
            .catalog
            .kinds
            .iter()
            .zip(self.chapter.catalog.marks.iter())
            .filter_map(|(kind, mark)| {
                mark.library_entry
                    .as_deref()
                    .filter(|entry_id| *entry_id != kind.id())
                    .map(str::to_string)
            })
            .collect()
    }

    /// Runs what the library panel asked for while it drew.
    ///
    /// Reached from `draw_overlay_ui`, i.e. after the dock body of the same frame, because
    /// every branch here touches state the dock body may not: the chapter catalog, the
    /// canvas, or a worker.
    pub(super) fn run_library_panel_request(
        &mut self,
        request: LibraryPanelRequest,
        canvas: &CanvasView,
        project: &ProjectData,
    ) {
        self.library_arm = next_library_arm(self.library_arm.take(), &request);
        self.sync_selection_diversion();
        match request {
            LibraryPanelRequest::SelectEntry(entry_id) => {
                self.start_chapter_request(ChapterRequest::LoadLibrary(entry_id), canvas, project);
            }
            LibraryPanelRequest::UnselectEntry(entry_id) => self.unload_library_entry(&entry_id),
            // The reservation is the WHOLE effect of these two; it was applied above.
            LibraryPanelRequest::Arm(_) | LibraryPanelRequest::Disarm => {}
        }
    }

    /// Drops the chapter mark that was loaded from `entry_id`, and only that one.
    ///
    /// A mark the chapter DISCOVERED is never touched, even when it sits under a kind id that
    /// happens to match: removing it would throw away measurements that cost a scan, and it
    /// was never "the library entry the user selected" in the first place.
    pub(super) fn unload_library_entry(&mut self, entry_id: &str) {
        let Some(index) = self
            .chapter
            .catalog
            .index_of(entry_id)
            .filter(|index| {
                self.chapter
                    .catalog
                    .marks
                    .get(*index)
                    .and_then(|mark| mark.library_entry.as_deref())
                    == Some(entry_id)
            })
        else {
            return;
        };
        // Drop the mark's preview textures with it, or the map would keep one entry per mark
        // ever created in this session.
        self.chapter.textures.remove(entry_id);
        self.chapter.catalog.remove(index);
        self.chapter.selected = self
            .chapter
            .selected
            .min(self.chapter.catalog.len().saturating_sub(1));
        self.chapter.invalidate_scan();
        self.chapter.status = Some(tf!(
            "cleaning.tools.watermark.chapter.library_unloaded_status",
            id = entry_id
        ));
    }

    /// Keeps the region base's selection DIVERSION in step with the live reservation.
    ///
    /// The two are one fact: while a reservation is in force the next completed selection is a
    /// library crop, and must not run the ordinary region editor — no loader job, no modal
    /// spinner, no editor window left standing over the canvas with its zoom and undo shortcuts
    /// disabled. Call this after every mutation of `library_arm`, including the ones that DROP
    /// it, because dropping the diversion is what discards an unclaimed diverted rect.
    pub(super) fn sync_selection_diversion(&mut self) {
        self.region_base
            .set_selection_diverted(self.library_arm.is_some());
    }

    /// Drops a reservation whose screen is no longer drawn, SAYING SO.
    ///
    /// A reservation may only outlive the surface that made it for as long as that surface is
    /// the one the user is looking at: the red «Отменить выделение» is the ONLY way to call it
    /// off, so a reservation drawn on no screen is one the user cannot cancel. The panel
    /// decides (it owns the screen); this owns the reservation and drops it.
    ///
    /// Dropping it in silence was the defect: the next drag would then open the ordinary
    /// region editor instead of adding a sample, with nothing anywhere having said the
    /// reservation was gone. The reason lands on the panel — the surface that drew the
    /// reservation, and the one the user comes back to — and in the log, because a reservation
    /// dying for a reason the user did not intend is a diagnosable event.
    pub(super) fn drop_reservation_without_a_screen(&mut self) {
        let Some(arm) = self.library_arm.as_ref() else {
            return;
        };
        if self.library_window.arm_survives(arm) {
            return;
        }
        ms_log::runtime_log::log_warn(format!(
            "[cleaning] watermark library reservation {arm:?} dropped: its screen is no longer drawn"
        ));
        self.library_arm = None;
        self.sync_selection_diversion();
        self.library_window
            .set_status(t!("cleaning.tools.watermark.chapter.library_arm_lost_status").to_string());
    }

    /// Takes the reservation a parked rect belongs to, REPORTING the rect when none is live.
    ///
    /// `None` is the defensive branch: every path that clears `library_arm` also calls
    /// `sync_selection_diversion`, which drops an unclaimed parked rect with it, so a rect
    /// reaching here unclaimed means one of those paths stopped keeping that promise. It costs
    /// a line to say so instead of dropping the user's gesture without a word — which on
    /// screen is indistinguishable from a drag that did nothing at all.
    #[must_use]
    pub(super) fn take_reservation_for_diverted(
        &mut self,
        page_idx: usize,
        rect: OverlayRectPx,
    ) -> Option<LibraryArm> {
        let arm = self.library_arm.take();
        if arm.is_none() {
            ms_log::runtime_log::log_warn(format!(
                "[cleaning] diverted selection on page {page_idx} ({}x{} at {},{}) arrived with no reservation",
                rect.w, rect.h, rect.x, rect.y
            ));
            self.library_window.set_status(
                t!("cleaning.tools.watermark.chapter.library_arm_lost_status").to_string(),
            );
        }
        arm
    }

    /// Repeats the region base's "that drag produced no rectangle" reason on the library panel.
    ///
    /// The base keeps the same reason in its own dock body, but a user who armed a reservation
    /// is reading the LIBRARY panel, two tabs away. Drained unconditionally so the one-shot can
    /// never go stale; shown only while a reservation is live, because otherwise the base's own
    /// body is already the surface the user is looking at.
    pub(super) fn report_selection_refusal(&mut self) {
        let refusal = self.region_base.take_selection_refusal();
        if let Some(refusal) = refusal
            && self.library_arm.is_some()
        {
            self.library_window.set_status(refusal);
        }
    }

    /// Cuts the armed canvas selection out of its page on a worker thread.
    ///
    /// The reservation is consumed by the caller before this runs, so a capture that fails
    /// does not leave the next selection silently spoken for.
    ///
    /// EVERY early return here is answered the same way a refused intake is: a red row on the
    /// entry the drag was aimed at (`WatermarkLibraryWindow::reject_capture`) plus a line in the
    /// runtime log. Neither is optional. The row is what the user sees — the panel's status line
    /// is nowhere near the list they are watching — and the log line is what makes the failure
    /// diagnosable from outside, which it was not: this lane used to report two of its refusals
    /// to the status line alone and nothing at all to the log.
    pub(super) fn start_library_capture(
        &mut self,
        arm: LibraryArm,
        page_idx: usize,
        rect: OverlayRectPx,
        canvas: &CanvasView,
        project: &ProjectData,
    ) {
        if self.library_capture_rx.is_some() {
            self.library_window.reject_capture(
                &arm,
                &LibraryCaptureRefusal::untagged(
                    t!("cleaning.mask_editor.processing_already_running_status").to_string(),
                ),
            );
            return;
        }
        let Some(page) = self
            .chapter_page_tasks(canvas, project)
            .into_iter()
            .find(|task| task.page_idx == page_idx)
        else {
            self.library_window.reject_capture(
                &arm,
                &LibraryCaptureRefusal::untagged(tf!(
                    "cleaning.tools.watermark.chapter.library_capture_page_error",
                    page = page_idx + 1
                )),
            );
            return;
        };
        let request = LibraryCaptureRequest {
            page,
            rect,
            sample_params: self.settings.normalized().chapter_sample_params(),
            max_side: CHAPTER_MAX_TEMPLATE_SIDE,
        };
        let (tx, rx) = mpsc::channel();
        self.library_capture_rx = Some(rx);
        self.library_capture_arm = Some(arm);
        thread::spawn(move || {
            let _ = tx.send(run_library_capture(request));
        });
    }

    /// Folds a finished canvas capture into the library panel.
    ///
    /// A refusal lands on the entry the drag was aimed at, exactly as an accepted crop does —
    /// see `start_library_capture` for why the status line alone was not enough.
    pub(super) fn poll_library_capture(&mut self) {
        let Some(rx) = self.library_capture_rx.as_ref() else {
            return;
        };
        let captured = match rx.try_recv() {
            Ok(captured) => captured,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(LibraryCaptureRefusal::untagged(
                t!("cleaning.mask_editor.processing_thread_crashed_error").to_string(),
            )),
        };
        self.library_capture_rx = None;
        let arm = self.library_capture_arm.take();
        match (captured, arm) {
            (Ok(crop), Some(arm)) => {
                let sample_params = self.settings.normalized().chapter_sample_params();
                self.library_window.accept_captured_crop(
                    crop,
                    &arm,
                    sample_params,
                    CHAPTER_MAX_TEMPLATE_SIDE,
                );
            }
            // The crop is on disk but nothing claims it any more: no screen can show it and
            // no screen can discard it, so the scratch file goes with the reservation that
            // was its only owner.
            (Ok(crop), None) => {
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] watermark library capture {} arrived with no reservation",
                    crop.path.display()
                ));
                remove_scratch_crop(&crop.path);
            }
            (Err(refusal), Some(arm)) => self.library_window.reject_capture(&arm, &refusal),
            // No reservation left to hang the row on, so the panel-wide line is the only
            // surface there is — and it is still said out loud rather than dropped.
            (Err(refusal), None) => {
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] watermark library capture refused with no reservation ({:?}): {}",
                    refusal.refusal, refusal.message
                ));
                self.library_window.set_status(refusal.message);
            }
        }
    }

    /// Drains the chapter worker channel and folds every finished step into the state.
    ///
    /// `canvas` is needed because a finished removal batch is applied here, on the GUI
    /// thread, through `CanvasView` — the tool never writes the overlay model itself.
    pub(super) fn poll_chapter_job(&mut self, canvas: &mut CanvasView) {
        loop {
            let event = {
                let Some(rx) = self.chapter.rx.as_ref() else {
                    return;
                };
                rx.try_recv()
            };
            match event {
                Ok(ChapterEvent::Progress { done, total, label }) => {
                    self.chapter.progress = Some((done, total, label));
                }
                Ok(ChapterEvent::SampleDone(outcome)) => {
                    let outcome = *outcome;
                    self.chapter.catalog = outcome.catalog;
                    self.chapter.selected = outcome.selected;
                    self.chapter.next_mark_number = self
                        .chapter
                        .next_mark_number
                        .max(self.chapter.catalog.len())
                        .saturating_add(1);
                    self.chapter.status = Some(outcome.status);
                    // The catalog changed, so last scan's hits no longer describe it.
                    self.chapter.invalidate_scan();
                    self.finish_chapter_job();
                }
                Ok(ChapterEvent::ScanDone(outcome)) => {
                    let outcome = *outcome;
                    self.chapter.catalog = outcome.catalog;
                    self.chapter.hits = outcome.hits;
                    self.chapter.scan_report = Some(outcome.report);
                    self.chapter.apply_report = None;
                    self.chapter.status = Some(tf!(
                        "cleaning.tools.watermark.chapter.scan_done_status",
                        found = outcome.report.found,
                        pages = outcome.report.pages,
                        unverified = outcome.report.unverified
                    ));
                    self.finish_chapter_job();
                }
                Ok(ChapterEvent::ApplyBatch(batch)) => {
                    let failed = self.apply_chapter_batch(canvas, &batch);
                    // Accumulate as the pages land so the report grows live; the final
                    // event replaces the totals with the worker's own count.
                    let report = self
                        .chapter
                        .apply_report
                        .get_or_insert_with(ChapterApplyReport::default);
                    report.pages += 1;
                    report.removed += batch.removed;
                    report.refused += batch.refused;
                    report.residual.merge(&batch.residual);
                    report.failed_patches += failed;
                }
                Ok(ChapterEvent::ApplyDone(report)) => {
                    let failed = self
                        .chapter
                        .apply_report
                        .map_or(0, |partial| partial.failed_patches);
                    let mut report = *report;
                    report.failed_patches = failed;
                    self.chapter.status = Some(tf!(
                        "cleaning.tools.watermark.chapter.apply_done_status",
                        pages = report.pages,
                        removed = report.removed,
                        refused = report.refused
                    ));
                    self.chapter.apply_report = Some(report);
                    self.finish_chapter_job();
                }
                Ok(ChapterEvent::LibraryList(entries)) => {
                    self.chapter.library = entries;
                    self.chapter.library_loaded = true;
                    self.finish_chapter_job();
                }
                Ok(ChapterEvent::LibrarySaved { mark_id, entry_id }) => {
                    if let Some(index) = self.chapter.catalog.index_of(&mark_id) {
                        self.chapter.catalog.marks[index].library_entry = Some(entry_id.clone());
                    }
                    self.chapter.status = Some(tf!(
                        "cleaning.tools.watermark.chapter.library_saved_status",
                        id = entry_id
                    ));
                    // The library gained (or changed) an entry, so the picker is stale.
                    self.chapter.library_requested = true;
                    self.finish_chapter_job();
                }
                Ok(ChapterEvent::LibraryLoaded(outcome)) => {
                    let outcome = *outcome;
                    self.chapter.catalog.push(outcome.kind, outcome.mark);
                    self.chapter.selected = self.chapter.catalog.len().saturating_sub(1);
                    self.chapter.status = Some(outcome.status);
                    self.chapter.invalidate_scan();
                    self.finish_chapter_job();
                }
                Ok(ChapterEvent::Failed(err)) => {
                    self.chapter.status = Some(tf!("cleaning.mask_editor.processing_error", err = err));
                    self.finish_chapter_job();
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.chapter.status =
                        Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
                    self.finish_chapter_job();
                    return;
                }
            }
        }
    }

    /// Clears the in-flight markers once a job reported its last event.
    fn finish_chapter_job(&mut self) {
        self.chapter.rx = None;
        self.chapter.job = None;
        self.chapter.progress = None;
    }

    /// Writes one page's recovered patches into the clean overlay, returning how many the
    /// canvas refused (a page whose overlay could not be opened at all).
    fn apply_chapter_batch(&mut self, canvas: &mut CanvasView, batch: &ChapterApplyBatch) -> usize {
        let overlay_size = batch
            .overlay_size
            .or_else(|| canvas.overlay_size(batch.page_idx))
            .unwrap_or(batch.page_size);
        let mut failed = 0usize;
        for patch in &batch.patches {
            let Some(target) = page_rect_to_overlay(patch.rect, batch.page_size, overlay_size)
            else {
                failed += 1;
                continue;
            };
            if !canvas.replace_overlay_region_px(batch.page_idx, target, &patch.image) {
                failed += 1;
            }
        }
        if failed > 0 {
            ms_log::runtime_log::log_warn(format!(
                "[cleaning] chapter watermark removal: {failed} patch(es) of page {} were refused by the canvas",
                batch.page_idx
            ));
        }
        failed
    }

    /// Starts the worker one chapter button asked for.
    ///
    /// Every job takes a COPY of the catalog: the worker owns it for the duration and hands
    /// back the updated one, so the GUI keeps a consistent catalog to draw and the engine
    /// never runs under a lock.
    pub(super) fn start_chapter_request(
        &mut self,
        request: ChapterRequest,
        canvas: &CanvasView,
        project: &ProjectData,
    ) {
        // The chapter mode is not offered in a single-image session (`effective_watermark_mode`);
        // this refuses a request that reached here anyway, because the sample path would file
        // a library entry under the scratch directory's name.
        if !self.series_watermark_library {
            ms_log::runtime_log::log_warn(
                "[cleaning.watermark] refused a chapter-mode request in a single-image session; \
                 the chapter mode is not offered there",
            );
            return;
        }
        // The page snapshot is built HERE rather than once per frame: it clones a path per
        // page of the chapter and only a job start ever needs it.
        let pages = self.chapter_page_tasks(canvas, project);
        match request {
            ChapterRequest::Sample {
                page_idx,
                rect,
                target,
            } => {
                let Some(page) = pages.iter().find(|task| task.page_idx == page_idx).cloned()
                else {
                    self.chapter.status = Some(t!("cleaning.tab.no_pages_error").to_string());
                    return;
                };
                let sample_request = ChapterSampleRequest {
                    catalog: self.chapter.catalog.clone(),
                    target,
                    page,
                    rect,
                    settings: self.settings.normalized(),
                    next_number: self.chapter.next_mark_number.max(1),
                };
                self.chapter.start_job(ChapterJobLabel::Sample, move |tx| {
                    let _ = tx.send(match run_chapter_sample(sample_request) {
                        Ok(outcome) => ChapterEvent::SampleDone(Box::new(outcome)),
                        Err(err) => ChapterEvent::Failed(err),
                    });
                });
            }
            ChapterRequest::Scan => {
                let scan_request = ChapterScanRequest {
                    catalog: self.chapter.catalog.clone(),
                    pages,
                    settings: self.settings.normalized(),
                };
                self.chapter.start_job(ChapterJobLabel::Scan, move |tx| {
                    let _ = tx.send(match run_chapter_scan(scan_request, tx) {
                        Ok(outcome) => ChapterEvent::ScanDone(Box::new(outcome)),
                        Err(err) => ChapterEvent::Failed(err),
                    });
                });
            }
            ChapterRequest::Apply => {
                if self.chapter.hits.is_empty() {
                    self.chapter.status =
                        Some(t!("cleaning.tools.watermark.chapter.no_hits_error").to_string());
                    return;
                }
                let apply_request = ChapterApplyRequest {
                    catalog: self.chapter.catalog.clone(),
                    pages,
                    hits: self.chapter.hits.clone(),
                };
                self.chapter.apply_report = None;
                self.chapter.start_job(ChapterJobLabel::Apply, move |tx| {
                    let _ = tx.send(match run_chapter_apply(apply_request, tx) {
                        Ok(report) => ChapterEvent::ApplyDone(Box::new(report)),
                        Err(err) => ChapterEvent::Failed(err),
                    });
                });
            }
            ChapterRequest::SaveLibrary(index) => {
                let (Some(mark), Some(kind)) = (
                    self.chapter.catalog.marks.get(index),
                    self.chapter.catalog.kinds.get(index),
                ) else {
                    return;
                };
                let mark_id = kind.id().to_string();
                let source = chapter_source_ref(
                    project,
                    mark.page_width,
                    kind.id(),
                    kind.template().anchor_key(),
                );
                let save_request = build_library_request(mark, kind, Some(source));
                self.chapter.start_job(ChapterJobLabel::Library, move |tx| {
                    let _ = tx.send(match save_entry(&save_request) {
                        Ok(entry_id) => ChapterEvent::LibrarySaved { mark_id, entry_id },
                        Err(err) => ChapterEvent::Failed(err),
                    });
                });
            }
            ChapterRequest::LoadLibrary(entry_id) => {
                // Loading the same entry twice would give the catalog two kinds sharing one
                // id, and everything keyed on that id (patches, previews, the save target)
                // would resolve to whichever came first. A mark the chapter DISCOVERED and
                // then saved to the library is the same physical mark under a `mark-{n}` kind
                // id, so it has to be recognised by `library_entry` as well.
                if let Some(index) = self.chapter_entry_index(&entry_id) {
                    self.chapter.selected = index;
                    self.chapter.status = Some(
                        t!("cleaning.tools.watermark.chapter.library_already_loaded_status")
                            .to_string(),
                    );
                    return;
                }
                self.chapter.start_job(ChapterJobLabel::Library, move |tx| {
                    let _ = tx.send(match run_chapter_library_load(&entry_id) {
                        Ok(outcome) => ChapterEvent::LibraryLoaded(Box::new(outcome)),
                        Err(err) => ChapterEvent::Failed(err),
                    });
                });
            }
            ChapterRequest::RefreshLibrary => self.request_chapter_library(),
            ChapterRequest::UseLibraryMatch { index, entry } => {
                let match_request = ChapterMatchRequest {
                    catalog: self.chapter.catalog.clone(),
                    index,
                    entry,
                };
                self.chapter.start_job(ChapterJobLabel::Library, move |tx| {
                    let _ = tx.send(match run_chapter_use_match(match_request) {
                        Ok(outcome) => ChapterEvent::SampleDone(Box::new(outcome)),
                        Err(err) => ChapterEvent::Failed(err),
                    });
                });
            }
            ChapterRequest::ToggleLibraryPanel => self.library_window.toggle(),
        }
    }

    /// Asks the library for its entry list, at most one query at a time.
    pub(super) fn request_chapter_library(&mut self) {
        self.chapter.library_requested = false;
        self.chapter.start_job(ChapterJobLabel::Library, |tx| {
            let _ = tx.send(ChapterEvent::LibraryList(list_entries()));
        });
    }

    /// Polls the background unload call and reports its outcome in the params
    /// section.
    pub(super) fn poll_unload(&mut self) {
        let Some(rx) = self.unload_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.unload_rx = None;
                self.unload_status =
                    Some(t!("cleaning.tools.watermark.unload_done_status").to_string());
            }
            Ok(Err(err)) => {
                self.unload_rx = None;
                self.unload_status = Some(tf!("cleaning.inpaint.unload_error", err = err));
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.unload_rx = None;
            }
        }
    }
}
