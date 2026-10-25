/*
File: crates/ms-tab-translation/src/tab/detection_storage.rs

Purpose:
The text detector's event drain and the per-chapter `text_detection` disk storage: lazy load,
Save, their worker jobs and results (the file format itself: `text_detector/storage.rs`).

Key functions:
- TranslationTabState::poll_text_detector_events: detector controller events (pages, progress,
  region detections, errors).
- ensure_text_detection_storage_loaded / start_text_detection_storage_load /
  start_text_detection_storage_save / poll_text_detection_storage_events: the storage jobs.
- clear_detector_result_stores / apply_loaded_detection_pages: result-store resets and fills.
- load_text_detection_storage / save_text_detection_storage: worker-side directory policy
  (which pages load, stale-file removal, protected pages, logging). The FORMAT — file names,
  the page reader and writer — is owned by `text_detector::storage`.

Notes:
- A page whose stored files exist but cannot be read is reported, never treated as empty, and
  Save keeps it untouched (`load_failed_detection_pages`).
*/

use ms_canvas::CanvasView;
use ms_project::ProjectData;
use crate::panels::text_detector::TextDetectorAlgorithm;
use crate::text_detector::{
    SURYA_DETECTOR_SPECS, TextDetectorControllerEvent, TextDetectorPageResult, read_text_detection_page,
    stored_detection_file_page, text_detection_blocks_file_path, write_text_detection_page,
};
use eframe::egui;
use ms_theme::Severity;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::mpsc;
use ms_thread as thread;
use super::{DetectionLoad, TextDetectionStorageEvent, TranslationTabState};
use super::detector_region;

impl TranslationTabState {
    pub(super) fn poll_text_detector_events(&mut self, ctx: &egui::Context, canvas: &mut CanvasView, project: &ProjectData) {
        // Regions held back by a storage job that has ended since.
        self.replay_pending_region_detections(ctx);
        for event in self.text_detector_controller.poll_events() {
            // Pages and the batch end follow a finished (or stopped) download.
            if !matches!(event, TextDetectorControllerEvent::ModelDownloadStarted | TextDetectorControllerEvent::ModelDownloadProgress(_)) {
                self.text_detector_model_download.clear();
            }
            match event {
                TextDetectorControllerEvent::ModelDownloadStarted => {
                    self.set_text_detector_status(
                        t!("translation.common.downloading_model_status"),
                        Severity::Info,
                    );
                }
                TextDetectorControllerEvent::DetectStarted { total, replace } => {
                    if replace {
                        self.clear_detector_result_stores();
                    }
                    self.text_detector_progress = Some((0, total));
                    self.set_text_detector_status(
                        self.text_detector_running_status(),
                        Severity::Info,
                    );
                }
                TextDetectorControllerEvent::PageDetected { page_idx, result, texts } => {
                    // NeuroLate "recognize text right away": the block texts of this page become
                    // bubbles exactly as block OCR makes them.
                    if let Some(texts) = texts {
                        self.create_bubbles_from_page_texts(canvas, project, page_idx, &result, &texts);
                    }
                    self.text_detector_results.insert(page_idx, result);
                    // The page's blocks were replaced: a line selection or drag indexes the old ones.
                    self.drop_line_edit_state_on_page(page_idx);
                    // A full detection is the whole page: Save may now replace its unreadable files.
                    self.load_failed_detection_pages.remove(&page_idx);
                    self.text_detector_mask_textures.remove(&page_idx);
                    self.sync_text_mask_page_from_result(page_idx);
                }
                TextDetectorControllerEvent::PageFailed { page_idx, error } => {
                    self.push_toast(
                        ctx,
                        tf!("translation.tab.detector_page_skipped_status", page_idx = page_idx, error = error),
                        Severity::Warning,
                        2.8,
                    );
                }
                TextDetectorControllerEvent::DetectProgress { done, total } => {
                    self.text_detector_progress = Some((done, total));
                }
                TextDetectorControllerEvent::DetectFinished {
                    total_blocks,
                    failed_pages,
                } => {
                    self.text_detector_progress = None;
                    let status = if failed_pages == 0 {
                        tf!("translation.tab.detector_done_blocks_status", total_blocks = total_blocks)
                    } else {
                        tf!("translation.tab.detector_done_blocks_errors_status", total_blocks = total_blocks, failed_pages = failed_pages)
                    };
                    let severity = if failed_pages == 0 {
                        Severity::Success
                    } else {
                        Severity::Warning
                    };
                    self.set_text_detector_status(status, severity);
                }
                TextDetectorControllerEvent::DetectFailed { error } => {
                    self.text_detector_progress = None;
                    // A Surya batch fails up front when its checkpoint is missing: re-probe
                    // so the panel draws the button that error names.
                    if self.text_detector_options.algorithm == TextDetectorAlgorithm::Surya {
                        self.ocr_model_downloads.request_bundle_status(&SURYA_DETECTOR_SPECS);
                    }
                    self.set_text_detector_status(
                        tf!("translation.tab.detector_error", error = error),
                        Severity::Error,
                    );
                }
                // `ModelDownloadStarted` already set the status; the panel draws the event.
                TextDetectorControllerEvent::ModelDownloadProgress(event) => {
                    self.text_detector_model_download.record(event);
                    ctx.request_repaint();
                }
                TextDetectorControllerEvent::DetectCancelled => {
                    self.text_detector_progress = None;
                    self.set_text_detector_status(t!("translation.tab.detector_cancelled_status"), Severity::Info);
                }
                TextDetectorControllerEvent::RegionDetected(region) => {
                    self.receive_region_detection(ctx, region);
                }
                TextDetectorControllerEvent::RegionFailed { page_idx, engine, error } => {
                    ms_log::runtime_log::log_error(format!("[text-detector] region detection on page {page_idx} failed: {error}"));
                    self.region_detection_failed(ctx, engine, error);
                }
            }
        }
    }

    pub(super) fn ensure_text_detection_storage_loaded(&mut self, project: &ProjectData) {
        if !self.session_caps.chapter_storage {
            return;
        }
        let project_dir = project.paths.project_dir.clone();
        if self
            .text_detection_storage_loaded_for
            .as_ref()
            .is_some_and(|loaded| *loaded == project_dir)
        {
            return;
        }
        if self.text_detection_storage_busy {
            return;
        }
        self.start_text_detection_storage_load(project);
    }

    fn start_text_detection_storage_load(&mut self, project: &ProjectData) {
        if self.text_detection_storage_busy {
            return;
        }
        let project_dir = project.paths.project_dir.clone();
        let storage_dir = project.paths.text_detection_dir.clone();
        let page_indices = project
            .pages
            .iter()
            .map(|page| page.idx)
            .collect::<Vec<_>>();
        let (tx, rx) = mpsc::channel::<TextDetectionStorageEvent>();
        self.text_detection_storage_busy = true;
        self.text_detection_storage_rx = Some(rx);
        self.set_text_detector_status(t!("translation.tab.loading_saved_mask_status"), Severity::Info);

        thread::spawn(move || {
            let load = load_text_detection_storage(&storage_dir, &page_indices);
            let _ = tx.send(TextDetectionStorageEvent::Loaded { project_dir, load });
        });
    }

    pub(super) fn start_text_detection_storage_save(&mut self, project: &ProjectData) {
        // The Save button is not drawn without storage; this refuses an action that got here
        // anyway rather than writing results into a scratch chapter deleted on exit.
        if !self.session_caps.chapter_storage || self.text_detection_storage_busy {
            return;
        }
        // Save what the user sees: the shared model holds the shown masks (brush edits
        // included), the tab's store the blocks. Only the masks to write are copied under the
        // lock; the files are written on a worker.
        let pages = match self.text_mask_model.as_ref().map(|model| model.lock()) {
            Some(Ok(model)) => detector_region::detection_pages_for_save(&self.text_detector_results, Some(&model)),
            Some(Err(error)) => {
                ms_log::runtime_log::log_error(format!("[text-detector] text mask model lock poisoned, saving detector results without shown masks: {error}"));
                detector_region::detection_pages_for_save(&self.text_detector_results, None)
            }
            None => detector_region::detection_pages_for_save(&self.text_detector_results, None),
        };
        if pages.is_empty() {
            self.set_text_detector_status(
                t!("translation.tab.no_results_to_save_status"),
                Severity::Error,
            );
            return;
        }
        // A page whose stored files could not be read is neither deleted nor rewritten: what the
        // session holds for it (a region merged onto an empty page) is not the whole page.
        let protected = self.load_failed_detection_pages.clone();
        let pages: Vec<(usize, TextDetectorPageResult)> = pages.into_iter().filter(|(page_idx, _)| !protected.contains(page_idx)).collect();
        if !protected.is_empty() {
            let mut sorted: Vec<usize> = protected.iter().copied().collect();
            sorted.sort_unstable();
            ms_log::runtime_log::log_warn(format!(
                "[text-detector] save keeps the unreadable stored detection of page(s) {sorted:?} as it is (not deleted, not rewritten)"
            ));
        }
        if pages.is_empty() {
            let status = if protected.is_empty() {
                t!("translation.tab.no_results_to_save_status").to_string()
            } else {
                tf!("translation.tab.unreadable_pages_kept_status", count = protected.len())
            };
            self.set_text_detector_status(status, Severity::Error);
            return;
        }
        let project_dir = project.paths.project_dir.clone();
        let storage_dir = project.paths.text_detection_dir.clone();
        let (tx, rx) = mpsc::channel::<TextDetectionStorageEvent>();
        self.text_detection_storage_busy = true;
        self.text_detection_storage_rx = Some(rx);
        self.set_text_detector_status(t!("translation.tab.saving_mask_status"), Severity::Info);

        thread::spawn(move || {
            let event = match save_text_detection_storage(&storage_dir, &pages, &protected) {
                Ok((saved, failed)) => TextDetectionStorageEvent::Saved {
                    project_dir,
                    saved,
                    failed,
                    kept_unreadable: protected.len(),
                },
                Err(error) => TextDetectionStorageEvent::Failed { project_dir, error },
            };
            let _ = tx.send(event);
        });
    }

    pub(super) fn poll_text_detection_storage_events(&mut self, project: &ProjectData) {
        let Some(rx) = self.text_detection_storage_rx.as_ref() else {
            return;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.text_detection_storage_rx = None;
                self.text_detection_storage_busy = false;
                // `loaded_for` stays unset, so the next region dispatch or detector panel draw
                // runs the load again; held regions wait for it (`region_merge_must_wait`).
                if !self.pending_region_detections.is_empty() {
                    ms_log::runtime_log::log_warn(format!(
                        "[text-detector] stored detection job died; {} region result(s) wait for the stored detection to load",
                        self.pending_region_detections.len()
                    ));
                }
                self.set_text_detector_status(
                    t!("translation.tab.text_detection_aborted_status"),
                    Severity::Error,
                );
                return;
            }
        };
        self.text_detection_storage_rx = None;
        self.text_detection_storage_busy = false;

        let current_project_dir = project.paths.project_dir.clone();
        match event {
            TextDetectionStorageEvent::Loaded { project_dir, load } => {
                if project_dir != current_project_dir {
                    return;
                }
                let loaded = load.pages.len();
                let failed = load.failed_pages.len();
                self.load_failed_detection_pages = load.failed_pages.into_iter().collect();
                self.apply_loaded_detection_pages(load.pages);
                self.text_detection_storage_loaded_for = Some(project_dir);
                let status = if failed == 0 {
                    tf!("translation.tab.loaded_saved_pages_status", loaded = loaded)
                } else {
                    tf!("translation.tab.load_progress_status", loaded = loaded, failed = failed)
                };
                let severity = if failed == 0 {
                    Severity::Success
                } else {
                    Severity::Warning
                };
                self.set_text_detector_status(status, severity);
            }
            TextDetectionStorageEvent::Saved {
                project_dir,
                saved,
                failed,
                kept_unreadable,
            } => {
                if project_dir != current_project_dir {
                    return;
                }
                self.text_detection_storage_loaded_for = Some(project_dir);
                let mut status = if failed == 0 {
                    tf!("translation.tab.saved_pages_status", saved = saved)
                } else {
                    tf!("translation.tab.save_progress_status", saved = saved, failed = failed)
                };
                if kept_unreadable > 0 {
                    status = format!("{status} {}", tf!("translation.tab.unreadable_pages_kept_status", count = kept_unreadable));
                }
                let severity = if failed == 0 && kept_unreadable == 0 {
                    Severity::Success
                } else {
                    Severity::Warning
                };
                self.set_text_detector_status(status, severity);
            }
            TextDetectionStorageEvent::Failed { project_dir, error } => {
                if project_dir != current_project_dir {
                    return;
                }
                self.text_detection_storage_loaded_for = Some(project_dir);
                self.set_text_detector_status(
                    tf!("translation.tab.text_detection_error", error = error),
                    Severity::Error,
                );
            }
        }
    }

    /// Empties both detector stores, their mask textures and the region results still waiting
    /// for a storage job (they belong to the results being cleared and must not come back).
    pub(super) fn clear_detector_result_stores(&mut self) {
        self.text_detector_results.clear();
        self.text_detector_mask_textures.clear();
        self.clear_text_mask_model();
        self.pending_region_detections.clear();
        self.pending_ocr_regions.clear();
    }

    /// Applies the chapter's stored detection: fills only pages that hold no result yet (a page
    /// detected in this session wins) and mirrors each into the shared model.
    pub(super) fn apply_loaded_detection_pages(&mut self, pages: Vec<(usize, TextDetectorPageResult)>) {
        for (page_idx, result) in pages {
            if let std::collections::hash_map::Entry::Vacant(entry) = self.text_detector_results.entry(page_idx) {
                entry.insert(result);
                self.text_detector_mask_textures.remove(&page_idx);
                self.sync_text_mask_page_from_result(page_idx);
            }
        }
    }
}

/// Reads the stored detection of `page_indices` from `storage_dir` (worker thread). A page with
/// no blocks file has none; a page whose files exist but cannot be read is logged and listed in
/// [`DetectionLoad::failed_pages`] (never treated as empty).
pub(super) fn load_text_detection_storage(storage_dir: &Path, page_indices: &[usize]) -> DetectionLoad {
    let mut load = DetectionLoad::default();
    if !storage_dir.exists() {
        return load;
    }
    for page_idx in page_indices {
        let blocks_path = text_detection_blocks_file_path(storage_dir, *page_idx);
        if !blocks_path.exists() {
            continue;
        }
        match read_text_detection_page(storage_dir, *page_idx) {
            Ok(Some(result)) => load.pages.push((*page_idx, result)),
            Ok(None) => {}
            Err(error) => {
                ms_log::runtime_log::log_error(format!(
                    "[text-detector] stored detection of page {page_idx} could not be read; Save will keep its files untouched: {error}"
                ));
                load.failed_pages.push(*page_idx);
            }
        }
    }
    load
}

/// Replaces the chapter's stored detection with `pages` (worker thread): every stored
/// `*_blocks.json` / `*_mask.png` is removed first, except the files of the `protected` pages
/// (their stored detection could not be read at load), then each page is written.
///
/// Returns `(saved, failed)`: `failed` counts the pages that could not be written plus the old
/// files that could not be removed (each logged). `Err` when the directory cannot be created or
/// listed — nothing was removed or written then.
pub(super) fn save_text_detection_storage(
    storage_dir: &Path,
    pages: &[(usize, TextDetectorPageResult)],
    protected: &HashSet<usize>,
) -> Result<(usize, usize), String> {
    fs::create_dir_all(storage_dir).map_err(|err| format!("{}: {err}", storage_dir.display()))?;
    let entries = fs::read_dir(storage_dir).map_err(|err| format!("{}: {err}", storage_dir.display()))?;

    let mut saved = 0usize;
    let mut failed = 0usize;

    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(err) => {
                ms_log::runtime_log::log_warn(format!("[text-detector] save: unreadable entry in {}: {err}", storage_dir.display()));
                continue;
            }
        };
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !(name.ends_with("_blocks.json") || name.ends_with("_mask.png")) {
            continue;
        }
        if stored_detection_file_page(name).is_some_and(|page_idx| protected.contains(&page_idx)) {
            continue;
        }
        if let Err(err) = fs::remove_file(&path) {
            // A stale file left behind would be loaded again next time: report it as a failure.
            ms_log::runtime_log::log_error(format!("[text-detector] save: could not remove the old {}: {err}", path.display()));
            failed = failed.saturating_add(1);
        }
    }

    let mut sorted = pages.to_vec();
    sorted.sort_by_key(|(page_idx, _)| *page_idx);
    for (page_idx, result) in sorted {
        match write_text_detection_page(storage_dir, page_idx, &result) {
            Ok(()) => {
                saved = saved.saturating_add(1);
            }
            Err(error) => {
                ms_log::runtime_log::log_error(format!("[text-detector] save: page {page_idx} not written: {error}"));
                failed = failed.saturating_add(1);
            }
        }
    }

    Ok((saved, failed))
}
