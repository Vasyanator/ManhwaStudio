/*
File: src/launcher/pages/export_page.rs

Purpose:
Dedicated Rust launcher page for exporting project chapters into `.mschapter` archives.

Main responsibilities:
- render the export form with title/chapter selection and compression preset;
- refresh project lists in the background;
- stream chapter contents into `tar + zstd` archives on a worker thread.

Key structures:
- `ExportPageState`
- `ExportPageStatus`
- `ExportWorkerResult`

Notes:
Filesystem scanning and archive creation stay off the GUI thread to keep launcher interactions responsive.
The destination comes from the non-blocking `ms_platform::file_dialog` seam (`SaveFile`): the
open dialog is polled each frame and keeps the page busy; the archive worker writes the
complete file to `SaveTarget::write_path` and only then calls `SaveTarget::commit`
(`run_export_job` owns that order), so a commit failure is reported like an export failure.
*/

use crate::pages::base::{self, PageNavAction};
use crate::theme;
use ms_log::runtime_log;
use ms_widgets::WheelComboBox;
use egui::{Align, Layout, RichText, Ui};
// Save dialog seam. On web the export step is replaced with an "unavailable on web" status
// (Phase 5 adds download), so the dialog types are native-only here.
#[cfg(not(target_arch = "wasm32"))]
use ms_platform::file_dialog::{self, DialogKind, FileDialogError, FileDialogRequest, PendingDialog, SaveTarget};
// Used only by the native archive writer, which is compiled out on web.
#[cfg(not(target_arch = "wasm32"))]
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use ms_thread as thread;

// Each entry pairs an i18n catalog KEY (not the text) with its zstd level. The key is
// resolved to the active-locale label at display time via `compression_preset_label`,
// because `t!` is not `const`.
const COMPRESSION_PRESETS: [(&str, i32); 5] = [
    ("launcher.export_page.compression_very_fast", 1),
    ("launcher.export_page.compression_fast", 3),
    ("launcher.export_page.compression_balanced", 6),
    ("launcher.export_page.compression_strong", 10),
    ("launcher.export_page.compression_max", 15),
];

/// Yellow accent for the format warning shown above the export form. Kept local
/// to this page because the launcher theme exposes only success/error accents.
const STATUS_WARNING: egui::Color32 = egui::Color32::from_rgb(232, 186, 66);

/// Resolves a compression-preset catalog key to its localized label.
fn compression_preset_label(key: &'static str) -> &'static str {
    ms_i18n::lookup(key).unwrap_or(key)
}

#[derive(Debug)]
pub struct ExportPageState {
    projects_root: PathBuf,
    titles: Vec<String>,
    chapters: Vec<String>,
    selected_title: Option<String>,
    selected_chapter: Option<String>,
    selected_compression: usize,
    status: ExportPageStatus,
    pending_refresh: Option<Receiver<ExportRefreshResult>>,
    pending_export: Option<Receiver<ExportWorkerResult>>,
    /// The open save dialog plus the export it was opened for; `Some` until `poll_save_dialog`
    /// takes its result.
    #[cfg(not(target_arch = "wasm32"))]
    pending_save_dialog: Option<PendingSaveDialog>,
}

/// An export waiting for its destination: what to archive, snapshotted when Export was
/// clicked, and the dialog choosing where.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
struct PendingSaveDialog {
    project_dir: PathBuf,
    chapter_name: String,
    compression_level: i32,
    dialog: PendingDialog,
}

#[derive(Debug)]
enum ExportPageStatus {
    Loading,
    Ready,
    Exporting,
    Success(String),
    Error(String),
}

#[derive(Debug)]
struct ExportRefreshResult {
    titles: Vec<String>,
    selected_title: Option<String>,
    chapters: Vec<String>,
    selected_chapter: Option<String>,
    error_message: Option<String>,
}

#[derive(Debug)]
struct ExportWorkerResult {
    user_message: String,
    log_message: Option<String>,
    success: bool,
}

impl ExportPageState {
    pub fn new(projects_root: PathBuf) -> Self {
        let mut state = Self {
            projects_root,
            titles: Vec::new(),
            chapters: Vec::new(),
            selected_title: None,
            selected_chapter: None,
            selected_compression: 2,
            status: ExportPageStatus::Loading,
            pending_refresh: None,
            pending_export: None,
            #[cfg(not(target_arch = "wasm32"))]
            pending_save_dialog: None,
        };
        state.start_refresh(None, None);
        state
    }

    pub fn show(&mut self, ui: &mut Ui) -> Option<PageNavAction> {
        self.poll_refresh();
        #[cfg(not(target_arch = "wasm32"))]
        self.poll_save_dialog();
        self.poll_export();
        let ctx = ui.ctx().clone();

        let mut action = None;
        if let Some(back_action) = base::show_page_shell(ui, |ui| {
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                ui.add_space((ui.available_height() * 0.08).max(12.0));
                theme::card_frame().show(ui, |ui| {
                    ui.set_width(500.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(t!("launcher.export_page.heading")).size(24.0).strong());
                        ui.add_space(14.0);

                        // The page exports the editable chapter into the app's own
                        // `.mschapter` container, which users regularly confuse with a
                        // PNG/PSD render export; the warning points at the real path.
                        ui.add(
                            egui::Label::new(theme::status(
                                t!("launcher.export_page.format_warning"),
                                STATUS_WARNING,
                            ))
                            .wrap(),
                        );
                        ui.add_space(10.0);

                        ui.label(theme::status(t!("launcher.export_page.title_label"), theme::TEXT_MUTED));
                        let mut title_changed = false;
                        ui.scope(|ui| {
                            ui.set_style(theme::combo_box_style(ui.style().as_ref()));
                            WheelComboBox::from_id_salt("launcher_export_title")
                                .width(432.0)
                                .selected_text(self.selected_title.as_deref().unwrap_or("—"))
                                .popup_style(theme::combo_popup_style())
                                .show_ui(ui, |ui| {
                                    for title in &self.titles {
                                        if ui
                                            .selectable_value(
                                                &mut self.selected_title,
                                                Some(title.clone()),
                                                title,
                                            )
                                            .changed()
                                        {
                                            title_changed = true;
                                        }
                                    }
                                });
                        });
                        if title_changed {
                            self.start_refresh(self.selected_title.clone(), None);
                            clear_status_if_success(&mut self.status);
                        }

                        ui.add_space(10.0);
                        ui.label(theme::status(t!("launcher.export_page.chapter_label"), theme::TEXT_MUTED));
                        ui.scope(|ui| {
                            ui.set_style(theme::combo_box_style(ui.style().as_ref()));
                            WheelComboBox::from_id_salt("launcher_export_chapter")
                                .width(432.0)
                                .selected_text(self.selected_chapter.as_deref().unwrap_or("—"))
                                .popup_style(theme::combo_popup_style())
                                .show_ui(ui, |ui| {
                                    for chapter in &self.chapters {
                                        if ui
                                            .selectable_value(
                                                &mut self.selected_chapter,
                                                Some(chapter.clone()),
                                                chapter,
                                            )
                                            .changed()
                                        {
                                            clear_status_if_success(&mut self.status);
                                        }
                                    }
                                });
                        });

                        ui.add_space(10.0);
                        ui.label(theme::status(t!("launcher.export_page.compression_level_label"), theme::TEXT_MUTED));
                        ui.scope(|ui| {
                            ui.set_style(theme::combo_box_style(ui.style().as_ref()));
                            egui::ComboBox::from_id_salt("launcher_export_compression")
                                .selected_text(compression_label(self.selected_compression))
                                .width(432.0)
                                .show_ui(ui, |ui| {
                                    for (index, (label_key, level)) in
                                        COMPRESSION_PRESETS.iter().enumerate()
                                    {
                                        let label = compression_preset_label(label_key);
                                        ui.selectable_value(
                                            &mut self.selected_compression,
                                            index,
                                            format!("{label} ({level})"),
                                        );
                                    }
                                });
                        });

                        if let Some(project_dir) = self.selected_project_dir() {
                            ui.add_space(8.0);
                            ui.label(theme::footer(&project_dir.display().to_string()));
                        }

                        ui.add_space(8.0);
                        show_status(ui, &self.status);

                        ui.add_space(18.0);
                        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                            if theme::launcher_button(
                                ui,
                                t!("launcher.export_page.export_button"),
                                egui::vec2(148.0, 36.0),
                                self.can_export(),
                            )
                            .clicked()
                            {
                                self.start_export(&ctx);
                            }
                            if theme::launcher_button(
                                ui,
                                t!("launcher.common.refresh_button"),
                                egui::vec2(118.0, 36.0),
                                !self.is_busy(),
                            )
                            .clicked()
                            {
                                self.start_refresh(
                                    self.selected_title.clone(),
                                    self.selected_chapter.clone(),
                                );
                            }
                        });
                    });
                });
            });
        }) {
            action = Some(back_action);
        }

        action
    }

    pub fn set_projects_root(&mut self, projects_root: PathBuf) {
        if self.projects_root == projects_root {
            return;
        }

        self.projects_root = projects_root;
        self.titles.clear();
        self.chapters.clear();
        self.selected_title = None;
        self.selected_chapter = None;
        self.pending_export = None;
        self.pending_refresh = None;
        // A dialog opened for a chapter of the old root must not export into the new one.
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.pending_save_dialog = None;
        }
        self.start_refresh(None, None);
    }

    fn start_refresh(
        &mut self,
        preferred_title_override: Option<String>,
        preferred_chapter_override: Option<String>,
    ) {
        self.status = ExportPageStatus::Loading;
        let projects_root = self.projects_root.clone();
        let preferred_title = preferred_title_override
            .unwrap_or_else(|| self.selected_title.clone().unwrap_or_default());
        let preferred_chapter = preferred_chapter_override
            .unwrap_or_else(|| self.selected_chapter.clone().unwrap_or_default());
        let (tx, rx) = mpsc::channel();
        self.pending_refresh = Some(rx);
        let spawn_result = thread::Builder::new()
            .name("launcher-export-refresh".to_string())
            .spawn(move || {
                let result =
                    build_refresh_result(&projects_root, &preferred_title, &preferred_chapter);
                if tx.send(result).is_err() {
                    runtime_log::log_warn(
                        "[launcher-export] refresh receiver dropped before result delivery",
                    );
                }
            });
        if let Err(err) = spawn_result {
            self.pending_refresh = None;
            self.status = ExportPageStatus::Error(
                t!("launcher.export_page.start_refresh_error").to_string(),
            );
            runtime_log::log_error(format!(
                "[launcher-export] failed to spawn refresh worker: {err}"
            ));
        }
    }

    /// Starts a `.mschapter` export: opens the save dialog without blocking; once it resolves,
    /// `poll_save_dialog` runs `tar + zstd` archiving on a worker. The selection and compression
    /// level are snapshotted now, so changing them while the dialog is open does not matter.
    ///
    /// Native only. On web there is no save dialog and archive download is a
    /// Phase 5 web-glue feature, so the web twin reports the operation as
    /// unavailable instead of exporting.
    #[cfg(not(target_arch = "wasm32"))]
    fn start_export(&mut self, ctx: &egui::Context) {
        let Some(selection) = self.current_selection() else {
            self.status = ExportPageStatus::Error(t!("launcher.common.select_title_chapter_error").to_string());
            return;
        };

        let request = FileDialogRequest::new(DialogKind::SaveFile)
            .start_dir(&self.projects_root)
            .suggested_name(format!("{} {}.mschapter", selection.title, selection.chapter))
            .filter(t!("launcher.common.chapter_files_filter"), &["mschapter"]);
        let waker_ctx = ctx.clone();
        let dialog = file_dialog::request(request, Some(std::sync::Arc::new(move || waker_ctx.request_repaint())));
        let compression_level = COMPRESSION_PRESETS
            .get(self.selected_compression)
            .map(|(_, level)| *level)
            .unwrap_or(10);
        self.pending_save_dialog = Some(PendingSaveDialog {
            project_dir: selection.project_dir,
            chapter_name: selection.chapter,
            compression_level,
            dialog,
        });
    }

    /// Takes the save dialog's result, if it resolved: a destination starts the archive worker,
    /// a cancel changes nothing, a dialog failure becomes the error status (details in the log).
    #[cfg(not(target_arch = "wasm32"))]
    fn poll_save_dialog(&mut self) {
        let Some(result) = self.pending_save_dialog.as_mut().and_then(|pending| pending.dialog.poll()) else {
            return;
        };
        let Some(pending) = self.pending_save_dialog.take() else {
            return;
        };
        match result {
            Ok(None) => {}
            Ok(Some(picked)) => match picked.into_save_target() {
                Some(target) => self.spawn_export_worker(pending, target),
                // `SaveFile` always yields `Picked::SaveTarget` (the seam's contract).
                None => {
                    runtime_log::log_error("[launcher-export] save dialog returned a result without a save target");
                    self.status = ExportPageStatus::Error(t!("launcher.export_page.export_failed").to_string());
                }
            },
            Err(err) => {
                runtime_log::log_warn(format!("[launcher-export] save dialog failed: {err:?}"));
                self.status = ExportPageStatus::Error(err.user_message());
            }
        }
    }

    /// Runs the archive write and the target commit (`run_export_job`) on a worker.
    #[cfg(not(target_arch = "wasm32"))]
    fn spawn_export_worker(&mut self, pending: PendingSaveDialog, target: SaveTarget) {
        let PendingSaveDialog { project_dir, chapter_name, compression_level, dialog: _ } = pending;
        self.status = ExportPageStatus::Exporting;
        let (tx, rx) = mpsc::channel();
        self.pending_export = Some(rx);
        let spawn_result = thread::Builder::new()
            .name("launcher-export-worker".to_string())
            .spawn(move || {
                let save_path = ensure_mschapter_extension(target.write_path().to_path_buf());
                let result = run_export_job(&project_dir, &save_path, |save_path| export_chapter_archive(&project_dir, &chapter_name, save_path, compression_level), || target.commit());
                if tx.send(result).is_err() {
                    runtime_log::log_warn(
                        "[launcher-export] export receiver dropped before result delivery",
                    );
                }
            });
        if let Err(err) = spawn_result {
            self.pending_export = None;
            self.status =
                ExportPageStatus::Error(t!("launcher.export_page.start_export_error").to_string());
            runtime_log::log_error(format!(
                "[launcher-export] failed to spawn export worker: {err}"
            ));
        }
    }

    /// Web twin of `start_export`: chapter export is not wired on the web build
    /// yet, so it surfaces a clear "unavailable on web" status.
    #[cfg(target_arch = "wasm32")]
    fn start_export(&mut self, _ctx: &egui::Context) {
        self.status =
            ExportPageStatus::Error(t!("launcher.export_page.export_web_unsupported").to_string());
    }

    fn poll_refresh(&mut self) {
        let mut clear = false;
        if let Some(rx) = &self.pending_refresh {
            match rx.try_recv() {
                Ok(result) => {
                    clear = true;
                    self.titles = result.titles;
                    self.selected_title = result.selected_title;
                    self.chapters = result.chapters;
                    self.selected_chapter = result.selected_chapter;
                    self.status = if let Some(message) = result.error_message {
                        ExportPageStatus::Error(message)
                    } else {
                        ExportPageStatus::Ready
                    };
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    clear = true;
                    self.status = ExportPageStatus::Error(
                        t!("launcher.export_page.refresh_failed").to_string(),
                    );
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if clear {
            self.pending_refresh = None;
        }
    }

    fn poll_export(&mut self) {
        let mut clear = false;
        if let Some(rx) = &self.pending_export {
            match rx.try_recv() {
                Ok(result) => {
                    clear = true;
                    if let Some(message) = result.log_message {
                        if result.success {
                            runtime_log::log_info(message);
                        } else {
                            runtime_log::log_error(message);
                        }
                    }
                    self.status = if result.success {
                        ExportPageStatus::Success(result.user_message)
                    } else {
                        ExportPageStatus::Error(result.user_message)
                    };
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    clear = true;
                    self.status =
                        ExportPageStatus::Error(t!("launcher.export_page.export_failed").to_string());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if clear {
            self.pending_export = None;
        }
    }

    fn current_selection(&self) -> Option<crate::state::OpenProjectSelection> {
        let title = self.selected_title.clone()?;
        let chapter = self.selected_chapter.clone()?;
        Some(crate::state::OpenProjectSelection {
            project_dir: self.projects_root.join(&title).join(&chapter),
            title,
            chapter,
        })
    }

    fn selected_project_dir(&self) -> Option<PathBuf> {
        self.current_selection()
            .map(|selection| selection.project_dir)
    }

    fn can_export(&self) -> bool {
        !self.is_busy() && self.current_selection().is_some()
    }

    fn is_busy(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        let save_dialog_open = self.pending_save_dialog.is_some();
        #[cfg(target_arch = "wasm32")]
        let save_dialog_open = false;
        self.pending_refresh.is_some() || self.pending_export.is_some() || save_dialog_open
    }
}

fn build_refresh_result(
    projects_root: &Path,
    preferred_title: &str,
    preferred_chapter: &str,
) -> ExportRefreshResult {
    let titles = match ms_project::project_scan::list_titles(projects_root) {
        Ok(titles) => titles,
        Err(err) => {
            return ExportRefreshResult {
                titles: Vec::new(),
                selected_title: None,
                chapters: Vec::new(),
                selected_chapter: None,
                error_message: Some(tf!("launcher.common.read_projects_folder_error", projects_root = projects_root.display(), err = err)),
            };
        }
    };

    let selected_title = if preferred_title.is_empty() {
        titles.first().cloned()
    } else if titles.iter().any(|title| title == preferred_title) {
        Some(preferred_title.to_string())
    } else {
        titles.first().cloned()
    };

    let chapters = selected_title
        .as_ref()
        .map(|title| ms_project::project_scan::list_chapters(projects_root, title).unwrap_or_default())
        .unwrap_or_default();

    let selected_chapter = if preferred_chapter.is_empty() {
        chapters.first().cloned()
    } else if chapters.iter().any(|chapter| chapter == preferred_chapter) {
        Some(preferred_chapter.to_string())
    } else {
        chapters.first().cloned()
    };

    ExportRefreshResult {
        titles,
        selected_title,
        chapters,
        selected_chapter,
        error_message: None,
    }
}

/// One export job, in its fixed order: `write_archive` writes the COMPLETE archive to
/// `save_path`, and only after it succeeded `commit` publishes the save target (exactly once; a
/// failed write never commits a partial file). Either failure becomes an error result with the
/// shared export error status. Blocking: runs on the export worker.
#[cfg(not(target_arch = "wasm32"))]
fn run_export_job(
    project_dir: &Path,
    save_path: &Path,
    write_archive: impl FnOnce(&Path) -> Result<(), String>,
    commit: impl FnOnce() -> Result<(), FileDialogError>,
) -> ExportWorkerResult {
    let outcome = write_archive(save_path).and_then(|()| {
        commit().map_err(|err| {
            runtime_log::log_warn(format!("[launcher-export] committing '{}' failed: {err:?}", save_path.display()));
            err.user_message()
        })
    });
    match outcome {
        Ok(()) => ExportWorkerResult {
            user_message: tf!("launcher.export_page.export_success_status", save_path = save_path.display()),
            log_message: Some(format!(
                "[launcher-export] exported '{}' to '{}'",
                project_dir.display(),
                save_path.display()
            )),
            success: true,
        },
        Err(err) => ExportWorkerResult {
            user_message: tf!("launcher.export_page.export_error", err = err),
            log_message: Some(format!(
                "[launcher-export] failed to export '{}' to '{}': {err}",
                project_dir.display(),
                save_path.display()
            )),
            success: false,
        },
    }
}

// Only reached through the native export worker; compiled out on web where
// no export path is wired yet.
#[cfg(not(target_arch = "wasm32"))]
fn export_chapter_archive(
    chapter_dir: &Path,
    chapter_name: &str,
    save_path: &Path,
    compression_level: i32,
) -> Result<(), String> {
    if !chapter_dir.is_dir() {
        return Err(tf!("launcher.export_page.chapter_dir_not_found", chapter_dir = chapter_dir.display()));
    }

    let file = File::create(save_path)
        .map_err(|err| tf!("launcher.export_page.create_file_error", save_path = save_path.display(), err = err))?;
    let mut encoder = zstd::stream::write::Encoder::new(file, compression_level)
        .map_err(|err| tf!("launcher.export_page.open_zstd_encoder_error", err = err))?;
    let mut builder = tar::Builder::new(&mut encoder);
    builder
        .append_dir_all(chapter_name, chapter_dir)
        .map_err(|err| tf!("launcher.export_page.pack_chapter_error", err = err))?;
    builder
        .finish()
        .map_err(|err| tf!("launcher.export_page.finish_tar_error", err = err))?;
    drop(builder);
    encoder
        .finish()
        .map_err(|err| tf!("launcher.export_page.finish_zstd_error", err = err))?;
    Ok(())
}

/// Appends `.mschapter` when the user typed a name without it (GTK does not add the filter's
/// extension). Desktop-shaped: the path is the destination itself. Native only.
#[cfg(not(target_arch = "wasm32"))]
fn ensure_mschapter_extension(path: PathBuf) -> PathBuf {
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mschapter"))
    {
        path
    } else {
        path.with_extension("mschapter")
    }
}

fn compression_label(index: usize) -> String {
    COMPRESSION_PRESETS
        .get(index)
        .map(|(label_key, level)| format!("{} ({level})", compression_preset_label(label_key)))
        .unwrap_or_else(|| t!("launcher.export_page.compression_balanced_default").to_string())
}

fn show_status(ui: &mut Ui, status: &ExportPageStatus) {
    match status {
        ExportPageStatus::Loading => {
            ui.label(theme::status(
                t!("launcher.export_page.reading_projects_status"),
                theme::TEXT_MUTED,
            ));
        }
        ExportPageStatus::Ready => {
            ui.label(theme::status(t!("launcher.export_page.ready_to_export"), theme::TEXT_MUTED));
        }
        ExportPageStatus::Exporting => {
            ui.label(theme::status(t!("launcher.export_page.exporting_status"), theme::TEXT_MUTED));
        }
        ExportPageStatus::Success(message) => {
            ui.label(theme::status(message, theme::STATUS_SUCCESS));
        }
        ExportPageStatus::Error(message) => {
            ui.label(theme::status(
                message,
                egui::Color32::from_rgb(214, 104, 104),
            ));
        }
    }
}

fn clear_status_if_success(status: &mut ExportPageStatus) {
    if matches!(status, ExportPageStatus::Success(_)) {
        *status = ExportPageStatus::Ready;
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{export_chapter_archive, run_export_job};
    use ms_platform::file_dialog::{FileDialogError, SaveTarget};
    use std::cell::RefCell;
    use std::path::Path;
    use std::sync::MutexGuard;

    /// Installs a catalog with the two messages the error tests read, holding the process-wide
    /// locale lock for the test's duration (the catalog slot is global to the test binary).
    fn install_error_catalog() -> MutexGuard<'static, ()> {
        let guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let catalog = ms_i18n::Catalog::from_json_str(
            &ms_i18n::LocaleTag::parse("en").expect("en tag"),
            r#"{ "launcher.export_page.export_error": "Export failed: {err}", "platform.file_dialog.commit_error": "Could not write the destination: {detail}" }"#,
        )
        .expect("test catalog parses");
        ms_i18n::install(catalog);
        guard
    }

    #[test]
    fn commit_runs_once_after_a_successful_write() {
        let calls = RefCell::new(Vec::new());
        let result = run_export_job(
            Path::new("/home/u/title/chapter"),
            Path::new("/home/u/out.mschapter"),
            |_| {
                calls.borrow_mut().push("write");
                Ok(())
            },
            || {
                calls.borrow_mut().push("commit");
                Ok(())
            },
        );
        assert!(result.success);
        assert_eq!(calls.into_inner(), vec!["write", "commit"]);
    }

    #[test]
    fn a_failed_write_never_commits() {
        let _locale = install_error_catalog();
        let committed = RefCell::new(false);
        let result = run_export_job(
            Path::new("/home/u/title/chapter"),
            Path::new("/home/u/out.mschapter"),
            |_| Err("disk full".to_owned()),
            || {
                *committed.borrow_mut() = true;
                Ok(())
            },
        );
        assert!(!result.success);
        assert!(!committed.into_inner());
        assert!(result.user_message.contains("disk full"));
    }

    #[test]
    fn a_commit_failure_surfaces_as_the_export_error_status() {
        let _locale = install_error_catalog();
        let commit_error = FileDialogError::Commit { detail: "uri revoked".to_owned() };
        let expected = commit_error.user_message();
        let result = run_export_job(Path::new("/home/u/title/chapter"), Path::new("/home/u/out.mschapter"), |_| Ok(()), || Err(commit_error));
        assert!(!result.success);
        assert_eq!(expected, "Could not write the destination: uri revoked");
        assert!(result.user_message.contains(&expected), "{} does not carry {expected}", result.user_message);
    }

    #[test]
    fn archive_lands_at_the_save_target_write_path() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let chapter_dir = dir.path().join("title").join("chapter");
        std::fs::create_dir_all(&chapter_dir).expect("create chapter dir");
        std::fs::write(chapter_dir.join("page.png"), b"page").expect("write page");
        let target = SaveTarget::for_test(dir.path().join("out.mschapter"));
        let save_path = target.write_path().to_path_buf();
        let result = run_export_job(&chapter_dir, &save_path, |path| export_chapter_archive(&chapter_dir, "chapter", path, 1), || target.commit());
        assert!(result.success, "{}", result.user_message);
        assert!(std::fs::metadata(&save_path).is_ok_and(|meta| meta.len() > 0));
    }
}
