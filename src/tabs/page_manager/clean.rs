/*
File: tabs/page_manager/clean.rs

Purpose:
Owns the page-manager clean-layer worker protocol (scan / probe / attach /
delete / detach) and the pure attachment candidate/path helpers used by the
tab UI.

Notes:
- Orphan scans are epoch-tagged; results from a superseded epoch are dropped.
- A replacement clean picked from disk is probed on the worker (header
  dimensions -> real `AttachFit`) before its confirmation dialog is shown.
- Mutating operations report partial success distinctly (`CleanOpError`), and a
  rescan is scheduled after EVERY finished operation.

Threading:
All filesystem and image work is performed by the dedicated worker. Model
locks are taken only after decoding has completed. Detach mutates the model
BEFORE trashing files so the detach generation invalidates in-flight autosave
snapshots (see clean_overlays_model.rs).
*/

use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

use ms_thread as thread;
use eframe::egui;

use crate::models::clean_assign::{self, AttachFit, CleanFileLocation, OrphanClean, OrphanReason};
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::project::{Page, ProjectPaths};
use crate::app::PageImageInfo;

use super::dialogs::spawn_clean_picker;
use super::PageManagerTabState;

/// A pending user confirmation for a content operation.
pub(super) enum CleanDialog {
    Attach { path: PathBuf, page_idx: usize, page_size: [u32; 2], fit: AttachFit, remove_source: bool },
    Delete { path: PathBuf },
    Detach { page_idx: usize },
}

/// A page to which an orphan clean can be attached, ordered for presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CleanCandidate {
    pub(super) page_idx: usize,
    pub(super) fit: AttachFit,
    pub(super) preferred: bool,
}

/// Builds the two persistence paths which may back a page clean layer.
#[must_use]
pub(super) fn clean_paths_for_page(paths: &ProjectPaths, page: &Page) -> Option<[PathBuf; 2]> {
    let stem = page.path.file_stem()?;
    let mut name = stem.to_os_string();
    name.push(".png");
    Some([
        paths.clean_layers_dir.join(&name),
        paths.unsaved_clean_layers_dir.join(name),
    ])
}

/// Returns compatible pages with exact fits first; a mismatched stem page is
/// retained as the preferred candidate when its aspect ratio is compatible.
#[must_use]
pub(super) fn attachment_candidates(
    orphan: &OrphanClean,
    page_sizes: &[(usize, [u32; 2])],
) -> Vec<CleanCandidate> {
    let preferred_idx = match orphan.reason {
        OrphanReason::SizeMismatch { page_idx, .. } => Some(page_idx),
        OrphanReason::NoMatchingPage | OrphanReason::Unreadable { .. } => None,
    };
    let mut candidates: Vec<_> = page_sizes
        .iter()
        .filter_map(|(page_idx, page_size)| {
            clean_assign::attach_fit(orphan.size, *page_size).map(|fit| CleanCandidate {
                page_idx: *page_idx,
                fit,
                preferred: preferred_idx == Some(*page_idx),
            })
        })
        .collect();
    candidates.sort_by_key(|candidate| {
        (
            matches!(candidate.fit, AttachFit::ScaleSameAspect),
            !candidate.preferred,
            candidate.page_idx,
        )
    });
    candidates
}

/// Work accepted by the dedicated clean worker.
pub(super) enum CleanJob {
    /// Rescan orphans; `epoch` is echoed back so stale results are dropped.
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
        paths: ProjectPaths,
        model: Arc<Mutex<CleanOverlaysModel>>,
    },
    Delete { path: PathBuf, paths: ProjectPaths },
    Detach {
        page_idx: usize,
        files: [PathBuf; 2],
        paths: ProjectPaths,
        model: Arc<Mutex<CleanOverlaysModel>>,
    },
}

/// Failure of a mutating clean operation, distinguishing partial success so the
/// UI can report exactly what was and was not applied. Error strings are raw
/// worker diagnostics; the UI wraps them in localized messages.
pub(super) enum CleanOpError {
    /// Nothing was applied.
    Failed(String),
    /// The overlay WAS replaced in the model, but the source file could not be removed.
    AttachSourceCleanupFailed(String),
    /// The model overlay WAS detached, but at least one backing file was not trashed.
    DetachFilesIncomplete(String),
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
    Scanned { epoch: u64, orphans: Vec<OrphanClean> },
    AttachProbed {
        path: PathBuf,
        page_idx: usize,
        page_size: [u32; 2],
        outcome: Result<AttachFit, ProbeAttachError>,
    },
    Finished(Result<(), CleanOpError>),
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
                let event = match job {
                    CleanJob::Scan { epoch, paths, pages } => CleanEvent::Scanned {
                        epoch,
                        orphans: clean_assign::scan_orphan_cleans(&paths, &pages),
                    },
                    CleanJob::ProbeAttach { path, page_idx, page_size } => {
                        // Header-only read: fast enough for a picker follow-up, still off the GUI.
                        let outcome = match image::image_dimensions(&path) {
                            Ok((width, height)) => match clean_assign::attach_fit([width, height], page_size) {
                                Some(fit) => Ok(fit),
                                None => Err(ProbeAttachError::Incompatible { size: [width, height] }),
                            },
                            Err(err) => Err(ProbeAttachError::Unreadable(err.to_string())),
                        };
                        CleanEvent::AttachProbed { path, page_idx, page_size, outcome }
                    }
                    CleanJob::Attach { path, page_idx, page_size, remove_source, paths, model } => {
                        let result = match clean_assign::load_clean_for_attach(&path, page_size) {
                            Err(error) => Err(CleanOpError::Failed(error)),
                            Ok(image) => match model.lock() {
                                Err(_) => Err(CleanOpError::Failed("clean overlay model is unavailable".to_string())),
                                Ok(mut guard) => {
                                    guard.replace_from_rgba(page_idx, image);
                                    drop(guard);
                                    if remove_source {
                                        // The overlay is already applied; a failed source
                                        // removal is a PARTIAL success, not a plain failure.
                                        clean_assign::trash_clean_file(&paths, &path).map_err(|error| {
                                            CleanOpError::AttachSourceCleanupFailed(error.to_string())
                                        })
                                    } else {
                                        Ok(())
                                    }
                                }
                            },
                        };
                        CleanEvent::Finished(result)
                    }
                    CleanJob::Delete { path, paths } => CleanEvent::Finished(
                        clean_assign::trash_clean_file(&paths, &path)
                            .map_err(|error| CleanOpError::Failed(error.to_string())),
                    ),
                    CleanJob::Detach { page_idx, files, paths, model } => {
                        let result = match model.lock() {
                            Err(_) => Err(CleanOpError::Failed("clean overlay model is unavailable".to_string())),
                            Ok(mut guard) => {
                                // The model detach must run FIRST: it bumps the page's detach
                                // generation, invalidating in-flight autosave snapshots before
                                // the files are moved (see clean_overlays_model.rs).
                                guard.detach_page_overlay(page_idx);
                                drop(guard);
                                let errors: Vec<String> = files
                                    .iter()
                                    .filter(|file| file.exists())
                                    .filter_map(|file| {
                                        clean_assign::trash_clean_file(&paths, file)
                                            .err()
                                            .map(|error| error.to_string())
                                    })
                                    .collect();
                                if errors.is_empty() {
                                    Ok(())
                                } else {
                                    // The model was already detached: report as partial.
                                    Err(CleanOpError::DetachFilesIncomplete(errors.join("; ")))
                                }
                            }
                        };
                        CleanEvent::Finished(result)
                    }
                };
                if events.send(event).is_err() { break; }
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

    /// Starts a scan once per epoch (bumped by `notify_pages_changed`, the refresh
    /// button, and every finished clean operation). The pages/paths snapshots make
    /// the worker independent from the live project object.
    pub(super) fn request_clean_scan_if_needed(&mut self, project: &crate::project::ProjectData) {
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

    /// Applies worker replies on the GUI thread and schedules the required rescan.
    pub(super) fn absorb_clean_events(&mut self, project: &crate::project::ProjectData) {
        for event in self.clean_runtime.poll() {
            match event {
                CleanEvent::Scanned { epoch, orphans } => {
                    // A result from a superseded epoch describes pages that may have
                    // been renumbered or reloaded; only the current epoch is accepted.
                    if epoch != self.clean_scan_epoch {
                        continue;
                    }
                    self.orphan_cleans = orphans;
                    self.selected_orphan = self.selected_orphan.filter(|idx| *idx < self.orphan_cleans.len());
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
                CleanEvent::Finished(result) => {
                    self.clean_op_in_flight = false;
                    match result {
                        Ok(()) => {}
                        Err(CleanOpError::Failed(error)) => {
                            self.error_message = Some(tf!("page_manager.clean_operation_failed", error = error));
                        }
                        Err(CleanOpError::AttachSourceCleanupFailed(error)) => {
                            self.error_message = Some(tf!("page_manager.clean_attach_partial", error = error));
                        }
                        Err(CleanOpError::DetachFilesIncomplete(error)) => {
                            self.error_message = Some(tf!("page_manager.clean_detach_partial", error = error));
                        }
                    }
                    // Rescan in EVERY outcome (success, failure, partial) so the list
                    // reflects what actually happened on disk.
                    self.clean_scan_epoch = self.clean_scan_epoch.wrapping_add(1);
                    self.request_clean_scan_if_needed(project);
                }
            }
        }
    }

    /// Renders the collapsible orphan-clean section and candidate actions.
    /// `op_in_progress` (structural op / save in flight) disables every mutating button.
    pub(super) fn draw_orphan_cleans(
        &mut self,
        ui: &mut egui::Ui,
        project: &crate::project::ProjectData,
        page_infos: &std::collections::HashMap<usize, PageImageInfo>,
        op_in_progress: bool,
    ) {
        let mutation_blocked = self.clean_op_in_flight || op_in_progress;
        let title = tf!("page_manager.clean_orphans_header", count = self.orphan_cleans.len());
        egui::CollapsingHeader::new(title)
            .id_salt("page_manager_clean_orphans")
            .show(ui, |ui| {
                if ui.button(t!("page_manager.clean_refresh_button")).clicked() {
                    // A new epoch both forces a rescan and invalidates any stale
                    // in-flight scan result.
                    self.clean_scan_epoch = self.clean_scan_epoch.wrapping_add(1);
                }
                if self.clean_scan_in_flight() { ui.spinner(); }
                for (orphan_idx, orphan) in self.orphan_cleans.iter().enumerate() {
                    let selected = self.selected_orphan == Some(orphan_idx);
                    let name = orphan.path.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
                    self.thumbs.request_thumb_if_needed(&orphan.path, self.generation);
                    let preview = self.thumbs.cache.touch_and_get(&orphan.path).and_then(|entry| match &entry.visual {
                        super::thumbs::ThumbVisual::Ready(texture) => Some((texture.id(), texture.size_vec2())),
                        super::thumbs::ThumbVisual::Failed => None,
                    });
                    ui.horizontal(|ui| {
                        if let Some((texture, size)) = preview {
                            ui.add(egui::Image::new((texture, size)).fit_to_exact_size(egui::vec2(40.0, 40.0)));
                        }
                        if ui.selectable_label(selected, tf!("page_manager.clean_orphan_entry", name = name, reason = orphan_reason_label(orphan))).clicked() {
                            self.selected_orphan = Some(orphan_idx);
                        }
                    });
                }
                let Some(orphan_idx) = self.selected_orphan else { return; };
                let Some(orphan) = self.orphan_cleans.get(orphan_idx).cloned() else { return; };
                ui.separator();
                ui.label(tf!("page_manager.clean_orphan_dimensions", width = orphan.size[0], height = orphan.size[1]));
                ui.label(match orphan.location { CleanFileLocation::Committed => t!("page_manager.clean_location_committed"), CleanFileLocation::Unsaved => t!("page_manager.clean_location_unsaved") });
                let sizes = page_sizes(project, page_infos, &self.thumbs);
                for candidate in attachment_candidates(&orphan, &sizes) {
                    let mut label = tf!("page_manager.clean_attach_page_button", page = candidate.page_idx + 1);
                    if matches!(candidate.fit, AttachFit::ScaleSameAspect) { label.push_str(t!("page_manager.clean_scaled_suffix")); }
                    if candidate.preferred { label.push_str(t!("page_manager.clean_preferred_suffix")); }
                    if ui.add_enabled(!mutation_blocked, egui::Button::new(label)).clicked() {
                        let page_size = sizes.iter().find_map(|(idx, size)| (*idx == candidate.page_idx).then_some(*size));
                    if let Some(page_size) = page_size { self.clean_dialog = Some(CleanDialog::Attach { path: orphan.path.clone(), page_idx: candidate.page_idx, page_size, fit: candidate.fit, remove_source: true }); }
                    }
                }
                if ui.add_enabled(!mutation_blocked, egui::Button::new(t!("page_manager.clean_delete_button"))).clicked() {
                    self.clean_dialog = Some(CleanDialog::Delete { path: orphan.path });
                }
            });
    }

    /// Polls the native replacement picker without blocking the UI. A picked file
    /// is first probed on the clean worker (header dimensions -> real `AttachFit`),
    /// so the confirmation dialog can warn about scaling or reject an incompatible
    /// image instead of always claiming an exact fit.
    pub(super) fn poll_clean_picker(&mut self, project: &crate::project::ProjectData, page_infos: &std::collections::HashMap<usize, PageImageInfo>) {
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
    /// `op_in_progress` (structural op / save in flight) disables confirmation.
    pub(super) fn draw_clean_dialog(&mut self, ctx: &egui::Context, project: &crate::project::ProjectData, page_infos: &std::collections::HashMap<usize, PageImageInfo>, op_in_progress: bool) {
        let Some(mut dialog) = self.clean_dialog.take() else { return; };
        let mut keep = true;
        let mut cancel = false;
        let mut confirm = false;
        let title = match dialog { CleanDialog::Attach { .. } => t!("page_manager.clean_attach_title"), CleanDialog::Delete { .. } => t!("page_manager.clean_delete_title"), CleanDialog::Detach { .. } => t!("page_manager.clean_detach_title") };
        egui::Window::new(title).id(egui::Id::new("page_manager_clean_confirm")).collapsible(false).resizable(false).open(&mut keep).show(ctx, |ui| {
            match &mut dialog {
                CleanDialog::Attach { fit, remove_source, .. } => {
                    ui.label(if matches!(fit, AttachFit::ScaleSameAspect) { t!("page_manager.clean_attach_resize_warning") } else { t!("page_manager.clean_attach_message") });
                    ui.checkbox(remove_source, t!("page_manager.clean_remove_source_checkbox"));
                    ui.label(t!("page_manager.clean_trash_note"));
                }
                CleanDialog::Delete { .. } => { ui.label(t!("page_manager.clean_delete_message")); }
                CleanDialog::Detach { .. } => { ui.label(t!("page_manager.clean_detach_message")); }
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !self.clean_op_in_flight && !op_in_progress,
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
            let job = match dialog {
                CleanDialog::Attach { path, page_idx, page_size, remove_source, .. } => self.overlays_model.as_ref().map(|model| CleanJob::Attach { path, page_idx, page_size, remove_source, paths: project.paths.clone(), model: Arc::clone(model) }),
                CleanDialog::Delete { path } => Some(CleanJob::Delete { path, paths: project.paths.clone() }),
                CleanDialog::Detach { page_idx } => clean_paths_for_page(&project.paths, &project.pages[page_idx]).and_then(|files| self.overlays_model.as_ref().map(|model| CleanJob::Detach { page_idx, files, paths: project.paths.clone(), model: Arc::clone(model) })),
            };
            match job { Some(job) => match self.clean_runtime.send(job) { Ok(()) => self.clean_op_in_flight = true, Err(error) => self.error_message = Some(tf!("page_manager.clean_operation_failed", error = error)) }, None => self.error_message = Some(t!("page_manager.clean_model_unavailable").to_string()) }
            return;
        }
        if keep && !cancel { self.clean_dialog = Some(dialog); }
        let _ = page_infos;
    }

    /// Opens the asynchronous native picker for a selected page's replacement clean.
    /// Blocked while any clean op / probe / structural op is in flight, and while a
    /// previous picker is still open (its receiver must not be overwritten).
    pub(super) fn start_replace_clean_picker(&mut self, op_in_progress: bool) {
        if !self.clean_op_in_flight && !op_in_progress && self.clean_picker_rx.is_none() {
            self.clean_picker_rx = Some(spawn_clean_picker());
        }
    }

    /// Opens detach confirmation for a page with a materialized overlay.
    pub(super) fn start_detach_clean(&mut self, page_idx: usize, op_in_progress: bool) {
        if !self.clean_op_in_flight && !op_in_progress {
            self.clean_dialog = Some(CleanDialog::Detach { page_idx });
        }
    }
}

fn page_sizes(project: &crate::project::ProjectData, page_infos: &std::collections::HashMap<usize, PageImageInfo>, thumbs: &super::thumbs::ThumbRuntime) -> Vec<(usize, [u32; 2])> {
    project.pages.iter().enumerate().filter_map(|(idx, page)| page_infos.get(&idx).map(|info| [info.width_px, info.height_px]).filter(|size| size[0] > 0 && size[1] > 0).or_else(|| thumbs.cache.peek(&page.path).and_then(|entry| entry.full_size).map(|(width, height)| [width, height])).map(|size| (idx, size))).collect()
}

fn orphan_reason_label(orphan: &OrphanClean) -> String {
    match &orphan.reason {
        OrphanReason::NoMatchingPage => t!("page_manager.clean_reason_no_matching_page").to_string(),
        OrphanReason::SizeMismatch { page_idx, page_size } => tf!("page_manager.clean_reason_size_mismatch", page = page_idx + 1, clean_width = orphan.size[0], clean_height = orphan.size[1], page_width = page_size[0], page_height = page_size[1]),
        OrphanReason::Unreadable { error } => tf!("page_manager.clean_reason_unreadable", error = error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::clean_assign::{CleanFileLocation, OrphanReason};

    fn orphan(reason: OrphanReason) -> OrphanClean {
        OrphanClean { path: PathBuf::from("orphan.png"), location: CleanFileLocation::Committed, size: [100, 200], reason }
    }

    #[test]
    fn candidates_sort_exact_before_scaling_and_prefer_matching_page() {
        let candidates = attachment_candidates(
            &orphan(OrphanReason::SizeMismatch { page_idx: 2, page_size: [101, 202] }),
            &[(1, [101, 202]), (2, [100, 200]), (3, [100, 200])],
        );
        assert_eq!(candidates.iter().map(|item| item.page_idx).collect::<Vec<_>>(), vec![2, 3, 1]);
        assert!(candidates[0].preferred);
        assert_eq!(candidates[2].fit, AttachFit::ScaleSameAspect);
    }

    #[test]
    fn clean_paths_use_page_stem_in_both_trees() {
        let paths = ProjectPaths {
            project_dir: PathBuf::new(), title_dir: PathBuf::new(), notes_file: PathBuf::new(), bubbles_file: PathBuf::new(), src_dir: PathBuf::new(), clean_layers_dir: PathBuf::from("clean"), cleaned_dir: PathBuf::new(), alt_vers_dir: PathBuf::new(), saved_dir: PathBuf::new(), image_bubbles_dir: PathBuf::new(), text_images_dir: PathBuf::new(), layers_dir: PathBuf::new(), text_detection_dir: PathBuf::new(), characters_dir: PathBuf::new(), terms_file: PathBuf::new(), settings_file: PathBuf::new(), unsaved_dir: PathBuf::new(), unsaved_bubbles_file: PathBuf::new(), unsaved_clean_layers_dir: PathBuf::from("unsaved_clean"), unsaved_image_bubbles_dir: PathBuf::new(), unsaved_text_images_dir: PathBuf::new(), unsaved_layers_dir: PathBuf::new(),
        };
        let page = Page { idx: 0, path: PathBuf::from("src/001.jpg") };
        assert_eq!(clean_paths_for_page(&paths, &page), Some([PathBuf::from("clean/001.png"), PathBuf::from("unsaved_clean/001.png")]));
    }
}
