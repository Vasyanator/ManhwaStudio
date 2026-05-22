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
*/

use crate::launcher::pages::base::{self, PageNavAction};
use crate::launcher::state::OpenProjectSelection;
use crate::launcher::theme;
use crate::runtime_log;
use crate::widgets::EditableComboBox;
use egui::{Align, Layout, RichText, Ui};
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

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
    queued_open: Option<OpenProjectSelection>,
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
                .with_hint_text("Введите или выберите тайтл")
                .with_desired_text_width(432.0)
                .with_popup_style(theme::combo_popup_style()),
            pending_titles: None,
            pending_metadata: None,
            pending_import: None,
            queued_open: None,
        };
        state.start_titles_refresh();
        state
    }

    pub fn show(&mut self, ui: &mut Ui) -> Option<PageNavAction> {
        self.poll_titles();
        self.poll_metadata();
        self.poll_import();

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
                        ui.label(RichText::new("Импорт главы").size(24.0).strong());
                        ui.add_space(14.0);

                        ui.label(theme::status("Файл главы (.mschapter):", theme::TEXT_MUTED));
                        ui.horizontal(|ui| {
                            let mut file_label = self
                                .archive_path
                                .as_ref()
                                .map(|path| path.display().to_string())
                                .unwrap_or_else(|| "Файл не выбран".to_string());
                            ui.add_enabled(
                                false,
                                egui::TextEdit::singleline(&mut file_label).desired_width(360.0),
                            );
                            if theme::launcher_button(
                                ui,
                                "Обзор...",
                                egui::vec2(108.0, 34.0),
                                !self.is_busy(),
                            )
                            .clicked()
                            {
                                self.pick_archive_file();
                            }
                        });

                        ui.add_space(10.0);
                        ui.label(theme::status(
                            "Тайтл (существующий или новый):",
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
                        ui.label(theme::status("Название главы:", theme::TEXT_MUTED));
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
                                "Импортировать и открыть",
                                egui::vec2(198.0, 36.0),
                                can_import,
                            )
                            .clicked()
                            {
                                self.start_import(true);
                            }
                            if theme::launcher_button(
                                ui,
                                "Импортировать",
                                egui::vec2(132.0, 36.0),
                                can_import,
                            )
                            .clicked()
                            {
                                self.start_import(false);
                            }
                            if theme::launcher_button(
                                ui,
                                "Обновить",
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
        self.queued_open = None;
        self.start_titles_refresh();
    }

    fn pick_archive_file(&mut self) {
        let start_dir = self
            .archive_path
            .as_ref()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| self.projects_root.clone());
        let Some(path) = FileDialog::new()
            .set_directory(start_dir)
            .add_filter("Файлы глав", &["mschapter"])
            .pick_file()
        else {
            return;
        };

        self.archive_path = Some(path.clone());
        clear_status_if_success(&mut self.status);
        self.start_metadata_load(path);
    }

    fn start_titles_refresh(&mut self) {
        self.status = ImportPageStatus::LoadingTitles;
        let projects_root = self.projects_root.clone();
        let (tx, rx) = mpsc::channel();
        self.pending_titles = Some(rx);
        let spawn_result = thread::Builder::new()
            .name("launcher-import-refresh".to_string())
            .spawn(move || {
                let result = match crate::list_titles(&projects_root) {
                    Ok(titles) => TitleRefreshResult {
                        titles,
                        error_message: None,
                    },
                    Err(err) => TitleRefreshResult {
                        titles: Vec::new(),
                        error_message: Some(format!(
                            "Не удалось прочитать папку проектов '{}': {}",
                            projects_root.display(),
                            err
                        )),
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
                "Не удалось запустить обновление списка тайтлов.".to_string(),
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
                        user_message: Some("Файл главы прочитан.".to_string()),
                        log_message: None,
                    },
                    Ok(None) => ArchiveMetadataResult {
                        chapter_name: None,
                        user_message: Some("Архив не содержит папки главы.".to_string()),
                        log_message: Some(format!(
                            "[launcher-import] archive '{}' contained no root folder",
                            path.display()
                        )),
                    },
                    Err(err) => ArchiveMetadataResult {
                        chapter_name: None,
                        user_message: Some(format!(
                            "Не удалось прочитать информацию из файла главы: {err}"
                        )),
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
                ImportPageStatus::Error("Не удалось запустить чтение файла главы.".to_string());
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
                ImportPageStatus::Error("Укажите файл главы, тайтл и название главы.".to_string());
            return;
        }

        let title_dir = self.projects_root.join(&title);
        let chapter_dir = title_dir.join(&chapter);
        if chapter_dir.is_dir() {
            let description = format!(
                "Глава «{}» в тайтле «{}» уже существует.\nПри импорте она будет полностью заменена.\n\nПродолжить?",
                chapter, title
            );
            let should_continue = MessageDialog::new()
                .set_title("ManhwaStudio")
                .set_description(&description)
                .set_buttons(MessageButtons::YesNo)
                .set_level(MessageLevel::Warning)
                .show()
                == MessageDialogResult::Yes;
            if !should_continue {
                return;
            }
        }

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
                        user_message: format!(
                            "Глава успешно импортирована в:\n{}",
                            projects_root.join(&title).join(&chapter).display()
                        ),
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
                        user_message: format!("Не удалось импортировать главу: {err}"),
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
            self.status = ImportPageStatus::Error("Не удалось запустить импорт главы.".to_string());
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
                        "Обновление списка тайтлов завершилось ошибкой.".to_string(),
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
                        "Чтение файла главы завершилось ошибкой.".to_string(),
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
                        ImportPageStatus::Error("Импорт главы завершился ошибкой.".to_string());
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
    }
}

fn show_status(ui: &mut Ui, status: &ImportPageStatus) {
    match status {
        ImportPageStatus::LoadingTitles => {
            ui.label(theme::status(
                "Считываем список тайтлов...",
                theme::TEXT_MUTED,
            ));
        }
        ImportPageStatus::Ready => {
            ui.label(theme::status("Готово к импорту.", theme::TEXT_MUTED));
        }
        ImportPageStatus::LoadingMetadata => {
            ui.label(theme::status(
                "Читаем содержимое файла главы...",
                theme::TEXT_MUTED,
            ));
        }
        ImportPageStatus::Importing => {
            ui.label(theme::status("Импортируем главу...", theme::TEXT_MUTED));
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
        .map_err(|err| format!("не удалось открыть '{}': {err}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|err| format!("не удалось открыть zstd-поток: {err}"))?;
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|err| format!("не удалось прочитать tar-архив: {err}"))?;

    for entry_result in entries {
        let entry =
            entry_result.map_err(|err| format!("не удалось прочитать запись архива: {err}"))?;
        let path = entry
            .path()
            .map_err(|err| format!("не удалось прочитать путь записи архива: {err}"))?;
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
        format!(
            "не удалось создать папку тайтла '{}': {err}",
            title_dir.display()
        )
    })?;
    let chapter_dir = title_dir.join(chapter);
    let temp_dir = title_dir.join(format!(
        ".{}_importing_{}",
        sanitize_for_temp_name(chapter),
        unique_suffix()
    ));
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir).map_err(|err| {
            format!(
                "не удалось очистить временную папку '{}': {err}",
                temp_dir.display()
            )
        })?;
    }
    fs::create_dir_all(&temp_dir).map_err(|err| {
        format!(
            "не удалось создать временную папку '{}': {err}",
            temp_dir.display()
        )
    })?;

    let import_result = (|| -> Result<(), String> {
        let file = File::open(archive_path)
            .map_err(|err| format!("не удалось открыть '{}': {err}", archive_path.display()))?;
        let decoder = zstd::stream::read::Decoder::new(file)
            .map_err(|err| format!("не удалось открыть zstd-поток: {err}"))?;
        let mut archive = tar::Archive::new(decoder);
        let entries = archive
            .entries()
            .map_err(|err| format!("не удалось прочитать tar-архив: {err}"))?;

        let mut expected_root: Option<OsString> = None;
        let mut wrote_any = false;

        for entry_result in entries {
            let mut entry =
                entry_result.map_err(|err| format!("не удалось прочитать запись архива: {err}"))?;
            let archive_path = entry
                .path()
                .map_err(|err| format!("не удалось прочитать путь записи архива: {err}"))?;
            let Some(relative) = strip_archive_root(&archive_path, &mut expected_root)? else {
                continue;
            };
            let destination = temp_dir.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|err| {
                    format!(
                        "не удалось создать папку '{}' при импорте: {err}",
                        parent.display()
                    )
                })?;
            }
            entry.unpack(&destination).map_err(|err| {
                format!("не удалось распаковать '{}' : {err}", destination.display())
            })?;
            wrote_any = true;
        }

        if !wrote_any {
            return Err("архив не содержит файлов главы".to_string());
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
            format!(
                "не удалось удалить старую главу '{}': {err}",
                chapter_dir.display()
            )
        })?;
    }
    fs::rename(&temp_dir, &chapter_dir).map_err(|err| {
        format!(
            "не удалось переместить импортированную главу в '{}': {err}",
            chapter_dir.display()
        )
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
                return Err("архив содержит небезопасный путь".to_string());
            }
            None => return Ok(None),
        }
    };

    if let Some(expected) = expected_root.as_ref() {
        if expected != &root {
            return Err("архив содержит несколько корневых папок".to_string());
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
                return Err("архив содержит небезопасный путь".to_string());
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
