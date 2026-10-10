/*
File: src/launcher/pages/import_page.rs

Purpose:
Dedicated Rust launcher page for importing `.mschapter` archives.

Main responsibilities:
- render the import chapter form with archive selection and editable title input;
- read archive metadata and perform import work in background threads;
- optionally open the imported chapter through launcher navigation once the import completes.

Key structures:
- `ImportPageState`
- `ImportPageStatus`
- `ImportWorkerResult`

Notes:
All archive reads, zstd decode, tar traversal, and filesystem writes run off the GUI thread.
The archive picker is a non-blocking `ms_platform::file_dialog` request polled every frame
(Browse is disabled while it is open), and replacing an existing chapter is confirmed through
the in-app `ConfirmModal`; neither blocks the GUI thread.
*/

use crate::confirm_modal::{ConfirmAnswer, ConfirmModal};
use crate::pages::base::{self, PageNavAction};
use crate::state::OpenProjectSelection;
use crate::theme;
use ms_log::runtime_log;
use ms_widgets::EditableComboBox;
use egui::{Align, Layout, RichText, Ui};
use ms_platform::file_dialog::{self, DialogKind, FileDialogRequest, PendingDialog};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use ms_thread as thread;
use web_time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct ImportPageState {
    projects_root: PathBuf,
    title_options: Vec<String>,
    title_input: String,
    chapter_input: String,
    chapter_edited: bool,
    archive_path: Option<PathBuf>,
    status: ImportPageStatus,
    title_combo: EditableComboBox,
    pending_titles: Option<Receiver<TitleRefreshResult>>,
    pending_metadata: Option<Receiver<ArchiveMetadataResult>>,
    pending_import: Option<Receiver<ImportWorkerResult>>,
    /// The open `.mschapter` picker, polled every frame.
    pending_pick: Option<PendingDialog>,
    /// "Replace the existing chapter?" question and the import it would start.
    pending_replace: Option<PendingReplace>,
    queued_open: Option<OpenProjectSelection>,
}

/// An import waiting for the user to confirm replacing an existing chapter.
#[derive(Debug)]
struct PendingReplace {
    modal: ConfirmModal,
    request: ImportRequest,
}

/// Everything one import worker needs, captured when the import was requested.
#[derive(Debug)]
struct ImportRequest {
    archive_path: PathBuf,
    title: String,
    chapter: String,
    open_after: bool,
}

#[derive(Debug)]
enum ImportPageStatus {
    LoadingTitles,
    Ready,
    LoadingMetadata,
    Importing,
    Success(String),
    Error(String),
}

#[derive(Debug)]
struct TitleRefreshResult {
    titles: Vec<String>,
    error_message: Option<String>,
}

#[derive(Debug)]
struct ArchiveMetadataResult {
    chapter_name: Option<String>,
    user_message: Option<String>,
    log_message: Option<String>,
}

#[derive(Debug)]
struct ImportWorkerResult {
    selection: Option<OpenProjectSelection>,
    user_message: String,
    log_message: Option<String>,
    success: bool,
}

impl ImportPageState {
    pub fn new(projects_root: PathBuf) -> Self {
        let mut state = Self {
            projects_root,
            title_options: Vec::new(),
            title_input: String::new(),
            chapter_input: String::new(),
            chapter_edited: false,
            archive_path: None,
            status: ImportPageStatus::LoadingTitles,
            title_combo: EditableComboBox::new("launcher_import_title")
                .with_hint_text(t!("launcher.import_page.title_input_hint"))
                .with_desired_text_width(432.0)
                .with_popup_style(theme::combo_popup_style()),
            pending_titles: None,
            pending_metadata: None,
            pending_import: None,
            pending_pick: None,
            pending_replace: None,
            queued_open: None,
        };
        state.start_titles_refresh();
        state
    }

    pub fn show(&mut self, ui: &mut Ui) -> Option<PageNavAction> {
        self.poll_titles();
        self.poll_metadata();
        self.poll_import();
        self.poll_pick();
        self.show_replace_confirm(ui.ctx());

        if let Some(selection) = self.queued_open.take() {
            return Some(PageNavAction::OpenProject(selection));
        }

        let mut action = None;
        if let Some(back_action) = base::show_page_shell(ui, |ui| {
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                ui.add_space((ui.available_height() * 0.08).max(12.0));
                theme::card_frame().show(ui, |ui| {
                    ui.set_width(520.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(t!("launcher.import_page.heading")).size(24.0).strong());
                        ui.add_space(14.0);

                        ui.label(theme::status(t!("launcher.import_page.chapter_file_label"), theme::TEXT_MUTED));
                        ui.horizontal(|ui| {
                            let mut file_label = self
                                .archive_path
                                .as_ref()
                                .map(|path| path.display().to_string())
                                .unwrap_or_else(|| t!("launcher.import_page.no_file_selected").to_string());
                            ui.add_enabled(
                                false,
                                egui::TextEdit::singleline(&mut file_label).desired_width(360.0),
                            );
                            if theme::launcher_button(
                                ui,
                                t!("launcher.import_page.browse_button"),
                                egui::vec2(108.0, 34.0),
                                !self.is_busy(),
                            )
                            .clicked()
                            {
                                self.pick_archive_file(ui.ctx());
                            }
                        });

                        ui.add_space(10.0);
                        ui.label(theme::status(
                            t!("launcher.import_page.title_label"),
                            theme::TEXT_MUTED,
                        ));
                        let title_response = ui
                            .scope(|ui| {
                                ui.set_style(theme::combo_box_style(ui.style().as_ref()));
                                self.title_combo.draw(
                                    ui,
                                    &mut self.title_input,
                                    &self.title_options,
                                )
                            })
                            .inner;
                        if title_response.changed {
                            clear_status_if_success(&mut self.status);
                        }

                        ui.add_space(10.0);
                        ui.label(theme::status(t!("launcher.import_page.chapter_name_label"), theme::TEXT_MUTED));
                        let chapter_response = ui.add_sized(
                            [432.0, ui.spacing().interact_size.y.max(34.0)],
                            egui::TextEdit::singleline(&mut self.chapter_input),
                        );
                        if chapter_response.changed() {
                            self.chapter_edited = true;
                            clear_status_if_success(&mut self.status);
                        }

                        if let Some(path) = &self.archive_path {
                            ui.add_space(8.0);
                            ui.label(theme::footer(&path.display().to_string()));
                        }

                        ui.add_space(8.0);
                        show_status(ui, &self.status);

                        ui.add_space(18.0);
                        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                            let can_import = self.can_import();
                            if theme::launcher_button(
                                ui,
                                t!("launcher.import_page.import_and_open_button"),
                                egui::vec2(198.0, 36.0),
                                can_import,
                            )
                            .clicked()
                            {
                                self.start_import(true);
                            }
                            if theme::launcher_button(
                                ui,
                                t!("launcher.import_page.import_button"),
                                egui::vec2(132.0, 36.0),
                                can_import,
                            )
                            .clicked()
                            {
                                self.start_import(false);
                            }
                            if theme::launcher_button(
                                ui,
                                t!("launcher.common.refresh_button"),
                                egui::vec2(118.0, 36.0),
                                !self.is_busy(),
                            )
                            .clicked()
                            {
                                self.start_titles_refresh();
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
        self.title_options.clear();
        self.pending_metadata = None;
        self.pending_import = None;
        self.pending_titles = None;
        // The question names a chapter under the OLD root; it no longer describes the import.
        self.pending_replace = None;
        self.queued_open = None;
        self.start_titles_refresh();
    }

    /// Opens the file picker for a `.mschapter` archive without blocking; `poll_pick` takes
    /// the answer and starts the metadata load. The picker starts next to the previously chosen
    /// archive, else in the projects root (desktop only; other platforms ignore it).
    fn pick_archive_file(&mut self, ctx: &egui::Context) {
        if self.pending_pick.is_some() {
            return;
        }
        let start_dir = self
            .archive_path
            .as_ref()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| self.projects_root.clone());
        let request = FileDialogRequest::new(DialogKind::OpenFile)
            .start_dir(&start_dir)
            .filter(t!("launcher.common.chapter_files_filter"), &["mschapter"]);
        let repaint_ctx = ctx.clone();
        self.pending_pick = Some(file_dialog::request(request, Some(Arc::new(move || repaint_ctx.request_repaint()))));
    }

    /// Applies the picker's answer once it arrives. A cancel changes nothing; a dialog failure
    /// (busy, unavailable on this platform, backend error) becomes the page's error status.
    fn poll_pick(&mut self) {
        let Some(pending) = self.pending_pick.as_mut() else {
            return;
        };
        let Some(result) = pending.poll() else {
            return;
        };
        self.pending_pick = None;
        match result {
            Ok(Some(picked)) => {
                let Some(file) = picked.into_file() else {
                    // `OpenFile` always answers with `Picked::Files` (ms-platform contract).
                    runtime_log::log_error("[launcher-import] archive picker returned a non-file result");
                    self.status = ImportPageStatus::Error(file_dialog::FileDialogError::WorkerLost.user_message());
                    return;
                };
                self.archive_path = Some(file.path.clone());
                clear_status_if_success(&mut self.status);
                self.start_metadata_load(file.path);
            }
            Ok(None) => {}
            Err(err) => {
                runtime_log::log_warn(format!("[launcher-import] archive picker failed: {err:?}"));
                self.status = ImportPageStatus::Error(err.user_message());
            }
        }
    }

    /// Draws the pending "replace the existing chapter?" question; "Yes" starts the import it
    /// was asked for, "No" drops it.
    fn show_replace_confirm(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending_replace else {
            return;
        };
        match pending.modal.show(ctx) {
            ConfirmAnswer::Pending => {}
            ConfirmAnswer::No => self.pending_replace = None,
            ConfirmAnswer::Yes => {
                if let Some(pending) = self.pending_replace.take() {
                    self.spawn_import(pending.request);
                }
            }
        }
    }

    fn start_titles_refresh(&mut self) {
        self.status = ImportPageStatus::LoadingTitles;
        let projects_root = self.projects_root.clone();
        let (tx, rx) = mpsc::channel();
        self.pending_titles = Some(rx);
        let spawn_result = thread::Builder::new()
            .name("launcher-import-refresh".to_string())
            .spawn(move || {
                let result = match ms_project::project_scan::list_titles(&projects_root) {
                    Ok(titles) => TitleRefreshResult {
                        titles,
                        error_message: None,
                    },
                    Err(err) => TitleRefreshResult {
                        titles: Vec::new(),
                        error_message: Some(tf!("launcher.common.read_projects_folder_error", projects_root = projects_root.display(), err = err)),
                    },
                };
                if tx.send(result).is_err() {
                    runtime_log::log_warn(
                        "[launcher-import] title refresh receiver dropped before result delivery",
                    );
                }
            });
        if let Err(err) = spawn_result {
            self.pending_titles = None;
            self.status = ImportPageStatus::Error(
                t!("launcher.import_page.start_refresh_error").to_string(),
            );
            runtime_log::log_error(format!(
                "[launcher-import] failed to spawn title refresh worker: {err}"
            ));
        }
    }

    fn start_metadata_load(&mut self, path: PathBuf) {
        self.status = ImportPageStatus::LoadingMetadata;
        let (tx, rx) = mpsc::channel();
        self.pending_metadata = Some(rx);
        let spawn_result = thread::Builder::new()
            .name("launcher-import-metadata".to_string())
            .spawn(move || {
                let result = match read_archive_root_name(&path) {
                    Ok(Some(chapter_name)) => ArchiveMetadataResult {
                        chapter_name: Some(chapter_name),
                        user_message: Some(t!("launcher.import_page.chapter_file_read_status").to_string()),
                        log_message: None,
                    },
                    Ok(None) => ArchiveMetadataResult {
                        chapter_name: None,
                        user_message: Some(t!("launcher.import_page.archive_no_chapter_folder").to_string()),
                        log_message: Some(format!(
                            "[launcher-import] archive '{}' contained no root folder",
                            path.display()
                        )),
                    },
                    Err(err) => ArchiveMetadataResult {
                        chapter_name: None,
                        user_message: Some(tf!("launcher.import_page.read_chapter_info_error", err = err)),
                        log_message: Some(format!(
                            "[launcher-import] failed to read archive metadata '{}': {err}",
                            path.display()
                        )),
                    },
                };
                if tx.send(result).is_err() {
                    runtime_log::log_warn(
                        "[launcher-import] metadata receiver dropped before result delivery",
                    );
                }
            });
        if let Err(err) = spawn_result {
            self.pending_metadata = None;
            self.status =
                ImportPageStatus::Error(t!("launcher.import_page.start_read_chapter_error").to_string());
            runtime_log::log_error(format!(
                "[launcher-import] failed to spawn metadata worker: {err}"
            ));
        }
    }

    fn start_import(&mut self, open_after: bool) {
        let Some(archive_path) = self.archive_path.clone() else {
            return;
        };
        let title = self.title_input.trim().to_string();
        let chapter = self.chapter_input.trim().to_string();
        if title.is_empty() || chapter.is_empty() {
            self.status =
                ImportPageStatus::Error(t!("launcher.import_page.specify_file_title_chapter_error").to_string());
            return;
        }

        let request = ImportRequest { archive_path, title, chapter, open_after };
        let chapter_dir = self.projects_root.join(&request.title).join(&request.chapter);
        if chapter_dir.is_dir() {
            // Replacing a chapter is destructive: ask first, start the import on "Yes".
            let text = tf!("launcher.import_page.chapter_exists_prompt", chapter = request.chapter, title = request.title);
            self.pending_replace = Some(PendingReplace {
                modal: ConfirmModal::new(
                    "launcher.import_page.replace_chapter_title",
                    t!("launcher.import_page.replace_chapter_title"),
                    text,
                ),
                request,
            });
            return;
        }
        self.spawn_import(request);
    }

    /// Starts the import worker for an already validated (and, if needed, confirmed) request.
    fn spawn_import(&mut self, request: ImportRequest) {
        let ImportRequest { archive_path, title, chapter, open_after } = request;
        self.status = ImportPageStatus::Importing;
        let projects_root = self.projects_root.clone();
        let (tx, rx) = mpsc::channel();
        self.pending_import = Some(rx);
        let spawn_result = thread::Builder::new()
            .name("launcher-import-worker".to_string())
            .spawn(move || {
                let result = match import_archive_into_projects(
                    &projects_root,
                    &archive_path,
                    &title,
                    &chapter,
                    open_after,
                ) {
                    Ok(selection) => ImportWorkerResult {
                        selection,
                        user_message: tf!("launcher.import_page.import_success_status", projects_root = projects_root.join(&title).join(&chapter).display()),
                        log_message: Some(format!(
                            "[launcher-import] imported '{}' into '{}/{}'",
                            archive_path.display(),
                            title,
                            chapter
                        )),
                        success: true,
                    },
                    Err(err) => ImportWorkerResult {
                        selection: None,
                        user_message: tf!("launcher.import_page.import_error", err = err),
                        log_message: Some(format!(
                            "[launcher-import] failed to import '{}' into '{}/{}': {err}",
                            archive_path.display(),
                            title,
                            chapter
                        )),
                        success: false,
                    },
                };
                if tx.send(result).is_err() {
                    runtime_log::log_warn(
                        "[launcher-import] import receiver dropped before result delivery",
                    );
                }
            });
        if let Err(err) = spawn_result {
            self.pending_import = None;
            self.status = ImportPageStatus::Error(t!("launcher.import_page.start_import_error").to_string());
            runtime_log::log_error(format!(
                "[launcher-import] failed to spawn import worker: {err}"
            ));
        }
    }

    fn poll_titles(&mut self) {
        let mut clear = false;
        if let Some(rx) = &self.pending_titles {
            match rx.try_recv() {
                Ok(result) => {
                    clear = true;
                    self.title_options = result.titles;
                    if self.title_input.trim().is_empty()
                        && let Some(first) = self.title_options.first()
                    {
                        self.title_input = first.clone();
                    }
                    self.status = if let Some(message) = result.error_message {
                        ImportPageStatus::Error(message)
                    } else {
                        ImportPageStatus::Ready
                    };
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    clear = true;
                    self.status = ImportPageStatus::Error(
                        t!("launcher.import_page.refresh_failed").to_string(),
                    );
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if clear {
            self.pending_titles = None;
        }
    }

    fn poll_metadata(&mut self) {
        let mut clear = false;
        if let Some(rx) = &self.pending_metadata {
            match rx.try_recv() {
                Ok(result) => {
                    clear = true;
                    if let Some(message) = result.log_message {
                        runtime_log::log_warn(message);
                    }
                    if let Some(chapter_name) = result.chapter_name
                        && (!self.chapter_edited || self.chapter_input.trim().is_empty())
                    {
                        self.chapter_input = chapter_name;
                        self.chapter_edited = false;
                    }
                    self.status = result
                        .user_message
                        .map(ImportPageStatus::Success)
                        .unwrap_or(ImportPageStatus::Ready);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    clear = true;
                    self.status = ImportPageStatus::Error(
                        t!("launcher.import_page.read_chapter_failed").to_string(),
                    );
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if clear {
            self.pending_metadata = None;
        }
    }

    fn poll_import(&mut self) {
        let mut clear = false;
        if let Some(rx) = &self.pending_import {
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
                    if result.success {
                        self.status = ImportPageStatus::Success(result.user_message);
                        self.queued_open = result.selection;
                    } else {
                        self.status = ImportPageStatus::Error(result.user_message);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    clear = true;
                    self.status =
                        ImportPageStatus::Error(t!("launcher.import_page.import_failed").to_string());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if clear {
            self.pending_import = None;
        }
    }

    fn can_import(&self) -> bool {
        !self.is_busy()
            && self.archive_path.is_some()
            && !self.title_input.trim().is_empty()
            && !self.chapter_input.trim().is_empty()
    }

    fn is_busy(&self) -> bool {
        self.pending_titles.is_some()
            || self.pending_metadata.is_some()
            || self.pending_import.is_some()
            || self.pending_pick.is_some()
            || self.pending_replace.is_some()
    }
}

fn show_status(ui: &mut Ui, status: &ImportPageStatus) {
    match status {
        ImportPageStatus::LoadingTitles => {
            ui.label(theme::status(
                t!("launcher.common.reading_titles_status"),
                theme::TEXT_MUTED,
            ));
        }
        ImportPageStatus::Ready => {
            ui.label(theme::status(t!("launcher.import_page.ready_to_import"), theme::TEXT_MUTED));
        }
        ImportPageStatus::LoadingMetadata => {
            ui.label(theme::status(
                t!("launcher.import_page.reading_chapter_status"),
                theme::TEXT_MUTED,
            ));
        }
        ImportPageStatus::Importing => {
            ui.label(theme::status(t!("launcher.import_page.importing_status"), theme::TEXT_MUTED));
        }
        ImportPageStatus::Success(message) => {
            ui.label(theme::status(message, theme::STATUS_SUCCESS));
        }
        ImportPageStatus::Error(message) => {
            ui.label(theme::status(
                message,
                egui::Color32::from_rgb(214, 104, 104),
            ));
        }
    }
}

fn clear_status_if_success(status: &mut ImportPageStatus) {
    if matches!(status, ImportPageStatus::Success(_)) {
        *status = ImportPageStatus::Ready;
    }
}

fn read_archive_root_name(path: &Path) -> Result<Option<String>, String> {
    let file = File::open(path)
        .map_err(|err| tf!("launcher.import_page.open_file_error", path = path.display(), err = err))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|err| tf!("launcher.import_page.open_zstd_stream_error", err = err))?;
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|err| tf!("launcher.import_page.read_tar_error", err = err))?;

    for entry_result in entries {
        let entry =
            entry_result.map_err(|err| tf!("launcher.import_page.read_archive_entry_error", err = err))?;
        let path = entry
            .path()
            .map_err(|err| tf!("launcher.import_page.read_archive_entry_path_error", err = err))?;
        if let Some(root) = first_normal_component(&path) {
            return Ok(Some(root.to_string_lossy().into_owned()));
        }
    }

    Ok(None)
}

fn import_archive_into_projects(
    projects_root: &Path,
    archive_path: &Path,
    title: &str,
    chapter: &str,
    open_after: bool,
) -> Result<Option<OpenProjectSelection>, String> {
    let title_dir = projects_root.join(title);
    fs::create_dir_all(&title_dir).map_err(|err| {
        tf!("launcher.import_page.create_title_folder_error", title_dir = title_dir.display(), err = err)
    })?;
    let chapter_dir = title_dir.join(chapter);
    let temp_dir = title_dir.join(format!(
        ".{}_importing_{}",
        sanitize_for_temp_name(chapter),
        unique_suffix()
    ));
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir).map_err(|err| {
            tf!("launcher.import_page.clear_temp_folder_error", temp_dir = temp_dir.display(), err = err)
        })?;
    }
    fs::create_dir_all(&temp_dir).map_err(|err| {
        tf!("launcher.import_page.create_temp_folder_error", temp_dir = temp_dir.display(), err = err)
    })?;

    let import_result = (|| -> Result<(), String> {
        let file = File::open(archive_path)
            .map_err(|err| tf!("launcher.import_page.open_archive_error", archive_path = archive_path.display(), err = err))?;
        let decoder = zstd::stream::read::Decoder::new(file)
            .map_err(|err| tf!("launcher.import_page.open_zstd_stream_error", err = err))?;
        let mut archive = tar::Archive::new(decoder);
        let entries = archive
            .entries()
            .map_err(|err| tf!("launcher.import_page.read_tar_error", err = err))?;

        let mut expected_root: Option<OsString> = None;
        let mut wrote_any = false;

        for entry_result in entries {
            let mut entry =
                entry_result.map_err(|err| tf!("launcher.import_page.read_archive_entry_error", err = err))?;
            let archive_path = entry
                .path()
                .map_err(|err| tf!("launcher.import_page.read_archive_entry_path_error", err = err))?;
            let Some(relative) = strip_archive_root(&archive_path, &mut expected_root)? else {
                continue;
            };
            let destination = temp_dir.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|err| {
                    tf!("launcher.import_page.create_folder_on_import_error", parent = parent.display(), err = err)
                })?;
            }
            entry.unpack(&destination).map_err(|err| {
                tf!("launcher.import_page.extract_error", destination = destination.display(), err = err)
            })?;
            wrote_any = true;
        }

        if !wrote_any {
            return Err(t!("launcher.import_page.archive_no_chapter_files").to_string());
        }

        Ok(())
    })();

    if import_result.is_err() {
        let cleanup_result = fs::remove_dir_all(&temp_dir);
        if let Err(cleanup_err) = cleanup_result {
            runtime_log::log_warn(format!(
                "[launcher-import] failed to remove temporary directory '{}': {}",
                temp_dir.display(),
                cleanup_err
            ));
        }
        return import_result.map(|_| None);
    }

    if chapter_dir.exists() {
        fs::remove_dir_all(&chapter_dir).map_err(|err| {
            tf!("launcher.import_page.delete_old_chapter_error", chapter_dir = chapter_dir.display(), err = err)
        })?;
    }
    fs::rename(&temp_dir, &chapter_dir).map_err(|err| {
        tf!("launcher.import_page.move_imported_chapter_error", chapter_dir = chapter_dir.display(), err = err)
    })?;

    Ok(open_after.then(|| OpenProjectSelection {
        project_dir: chapter_dir,
        title: title.to_string(),
        chapter: chapter.to_string(),
        resume_unsaved: false,
    }))
}

fn first_normal_component(path: &Path) -> Option<&OsStr> {
    for component in path.components() {
        if let Component::Normal(value) = component {
            return Some(value);
        }
    }
    None
}

fn strip_archive_root(
    path: &Path,
    expected_root: &mut Option<OsString>,
) -> Result<Option<PathBuf>, String> {
    let mut components = path.components();
    let root = loop {
        match components.next() {
            Some(Component::Normal(value)) => break value.to_os_string(),
            Some(Component::CurDir) => continue,
            Some(Component::ParentDir | Component::RootDir | Component::Prefix(_)) => {
                return Err(t!("launcher.import_page.archive_unsafe_path").to_string());
            }
            None => return Ok(None),
        }
    };

    if let Some(expected) = expected_root.as_ref() {
        if expected != &root {
            return Err(t!("launcher.import_page.archive_multiple_roots").to_string());
        }
    } else {
        *expected_root = Some(root);
    }

    let mut relative = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(value) => relative.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(t!("launcher.import_page.archive_unsafe_path").to_string());
            }
        }
    }

    if relative.as_os_str().is_empty() {
        Ok(None)
    } else {
        Ok(Some(relative))
    }
}

fn sanitize_for_temp_name(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        "chapter".to_string()
    } else {
        trimmed.chars().take(48).collect()
    }
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}
