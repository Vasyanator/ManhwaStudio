/*
File: src/launcher/pages/open_page.rs

Purpose:
Working "Open chapter" launcher page backed by on-disk project discovery.

Main responsibilities:
- list titles and chapters from the configured projects root;
- validate the selected chapter in a background thread before opening;
- remember the last open-page selection in `user_config.json`.

Notes:
All filesystem scans and validation run in worker threads so the launcher UI remains responsive.
*/

use crate::config;
use crate::launcher::pages::base::{self, PageNavAction};
use crate::launcher::state::OpenProjectSelection;
use crate::launcher::theme;
use crate::runtime_log;
use crate::widgets::WheelComboBox;
use egui::{Align, Layout, RichText, Ui};
use serde_json::Value;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

const LAST_OPEN_TITLE_KEY: &str = "open_page_last_title";
const LAST_OPEN_CHAPTER_KEY: &str = "open_page_last_chapter";
const NO_PROJECTS_MESSAGE: &str = "Проектов нет. Чтобы создать проект, перейдите в \"Новая глава\"";

#[derive(Debug)]
pub struct OpenPageState {
    projects_root: PathBuf,
    titles: Vec<String>,
    chapters: Vec<String>,
    selected_title: Option<String>,
    selected_chapter: Option<String>,
    /// Chapter name for which an `_unsaved` folder was detected (if any).
    unsaved_chapter: Option<String>,
    status: OpenPageStatus,
    pending_refresh: Option<Receiver<OpenPageRefreshResult>>,
    pending_validation: Option<Receiver<OpenPageValidationResult>>,
    pending_open: Option<Receiver<Result<OpenProjectSelection, String>>>,
    preferred_title: String,
    preferred_chapter: String,
}

#[derive(Debug)]
enum OpenPageStatus {
    Loading,
    RefreshError(String),
    Empty(String),
    Validating,
    Opening,
    Ready { image_count: usize },
    Invalid(String),
}

#[derive(Debug)]
struct OpenPageRefreshResult {
    titles: Vec<String>,
    selected_title: Option<String>,
    chapters: Vec<String>,
    selected_chapter: Option<String>,
    error_message: Option<String>,
}

#[derive(Debug)]
struct OpenPageValidationResult {
    project_dir: PathBuf,
    state: crate::ProjectValidationState,
}

impl OpenPageState {
    pub fn new(projects_root: PathBuf, user_settings: &Value) -> Self {
        let (preferred_title, preferred_chapter) = read_last_open_selection(user_settings);
        let mut state = Self {
            projects_root,
            titles: Vec::new(),
            chapters: Vec::new(),
            selected_title: None,
            selected_chapter: None,
            unsaved_chapter: None,
            status: OpenPageStatus::Loading,
            pending_refresh: None,
            pending_validation: None,
            pending_open: None,
            preferred_title,
            preferred_chapter,
        };
        state.start_refresh(None);
        state
    }

    pub fn show(&mut self, ui: &mut Ui) -> Option<PageNavAction> {
        self.poll_refresh();
        self.poll_validation();
        let pending_open_action = self.poll_open();
        let mut ui_action = None;

        if let Some(back_action) = base::show_page_shell(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.10).max(12.0));
                ui.allocate_ui_with_layout(
                    egui::vec2(528.0, 0.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        theme::card_frame().show(ui, |ui| {
                            ui.set_max_width(480.0);
                            ui.vertical(|ui| {
                                ui.label(RichText::new("Открыть главу").size(24.0).strong());
                                ui.add_space(14.0);

                                ui.label(theme::status("Тайтл:", theme::TEXT_MUTED));
                                let mut title_changed = false;
                                ui.scope(|ui| {
                                    ui.set_style(theme::combo_box_style(ui.style().as_ref()));
                                    WheelComboBox::from_id_salt("launcher_open_title")
                                        .width(432.0)
                                        .selected_text(
                                            self.selected_title.as_deref().unwrap_or("—"),
                                        )
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
                                    self.refresh_unsaved_detection();
                                    self.start_refresh(self.selected_title.clone());
                                }

                                ui.add_space(10.0);
                                ui.label(theme::status("Глава:", theme::TEXT_MUTED));
                                let mut chapter_changed = false;
                                ui.scope(|ui| {
                                    ui.set_style(theme::combo_box_style(ui.style().as_ref()));
                                    WheelComboBox::from_id_salt("launcher_open_chapter")
                                        .width(432.0)
                                        .selected_text(
                                            self.selected_chapter.as_deref().unwrap_or("—"),
                                        )
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
                                                    chapter_changed = true;
                                                }
                                            }
                                        });
                                });
                                if chapter_changed {
                                    self.start_validation_for_current_selection();
                                }

                                // Recovery banner: shown when an _unsaved folder is detected.
                                if let Some(ref unsaved_name) = self.unsaved_chapter.clone() {
                                    ui.add_space(10.0);
                                    egui::Frame::default()
                                        .fill(egui::Color32::from_rgb(72, 58, 0))
                                        .inner_margin(egui::Margin::symmetric(10, 8))
                                        .corner_radius(egui::CornerRadius::same(6))
                                        .show(ui, |ui| {
                                            ui.set_width(432.0);
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "Незавершённая сессия: «{unsaved_name}»"
                                                    ))
                                                    .color(egui::Color32::from_rgb(255, 210, 40)),
                                                );
                                                ui.with_layout(
                                                    Layout::right_to_left(Align::Center),
                                                    |ui| {
                                                        if theme::launcher_button(
                                                            ui,
                                                            "Восстановить",
                                                            egui::vec2(130.0, 26.0),
                                                            true,
                                                        )
                                                        .clicked()
                                                        {
                                                            let title = self
                                                                .selected_title
                                                                .clone()
                                                                .unwrap_or_default();
                                                            let selection = OpenProjectSelection {
                                                                project_dir: self
                                                                    .projects_root
                                                                    .join(&title)
                                                                    .join(unsaved_name),
                                                                title,
                                                                chapter: unsaved_name.clone(),
                                                                resume_unsaved: true,
                                                            };
                                                            ui_action =
                                                                Some(PageNavAction::OpenProject(
                                                                    selection,
                                                                ));
                                                        }
                                                    },
                                                );
                                            });
                                        });
                                }

                                ui.add_space(12.0);
                                if let Some(project_dir) = self.selected_project_dir() {
                                    ui.label(theme::footer(&project_dir.display().to_string()));
                                } else {
                                    ui.label(theme::footer("Выберите тайтл и главу."));
                                }

                                ui.add_space(8.0);
                                show_status(ui, &self.status);

                                ui.add_space(18.0);
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    let can_open = self.can_open();
                                    if theme::launcher_button(
                                        ui,
                                        "Открыть",
                                        egui::vec2(118.0, 36.0),
                                        can_open,
                                    )
                                    .clicked()
                                    {
                                        ui_action = self.start_open_current_selection();
                                    }
                                    if theme::launcher_button(
                                        ui,
                                        "Обновить",
                                        egui::vec2(118.0, 36.0),
                                        true,
                                    )
                                    .clicked()
                                    {
                                        self.start_refresh(self.selected_title.clone());
                                    }
                                });
                            });
                        });
                    },
                );
            });
        }) {
            ui_action = Some(back_action);
        }

        pending_open_action.or(ui_action)
    }

    pub fn persist_last_selection(&self) -> anyhow::Result<()> {
        let Some(title) = self.selected_title.as_ref() else {
            return Ok(());
        };
        let Some(chapter) = self.selected_chapter.as_ref() else {
            return Ok(());
        };

        persist_last_selection_values(title, chapter)
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
        self.unsaved_chapter = None;
        self.pending_validation = None;
        self.pending_open = None;
        self.start_refresh(None);
    }

    fn current_selection(&self) -> Option<OpenProjectSelection> {
        let title = self.selected_title.clone()?;
        let chapter = self.selected_chapter.clone()?;
        Some(OpenProjectSelection {
            project_dir: self.projects_root.join(&title).join(&chapter),
            title,
            chapter,
            resume_unsaved: false,
        })
    }

    fn refresh_unsaved_detection(&mut self) {
        self.unsaved_chapter = self
            .selected_title
            .as_deref()
            .and_then(|title| crate::find_unsaved_chapter(&self.projects_root, title));
    }

    fn can_open(&self) -> bool {
        self.pending_open.is_none()
            && matches!(self.status, OpenPageStatus::Ready { .. })
            && self.selected_project_dir().is_some()
    }

    fn selected_project_dir(&self) -> Option<PathBuf> {
        self.current_selection()
            .map(|selection| selection.project_dir)
    }

    fn current_unsaved_dir(&self) -> Option<PathBuf> {
        let title = self.selected_title.as_ref()?;
        let unsaved_chapter = self.unsaved_chapter.as_ref()?;
        Some(
            self.projects_root
                .join(title)
                .join(format!("{unsaved_chapter}_unsaved")),
        )
    }

    fn start_refresh(&mut self, preferred_title_override: Option<String>) {
        self.status = OpenPageStatus::Loading;
        self.pending_validation = None;

        let projects_root = self.projects_root.clone();
        let preferred_title = preferred_title_override
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| self.preferred_title.clone());
        let preferred_chapter = self.preferred_chapter.clone();

        let (tx, rx) = mpsc::channel();
        self.pending_refresh = Some(rx);
        let spawn_result = thread::Builder::new()
            .name("launcher-open-refresh".to_string())
            .spawn(move || {
                let result =
                    build_refresh_result(&projects_root, &preferred_title, &preferred_chapter);
                if let Err(err) = tx.send(result) {
                    runtime_log::log_warn(format!(
                        "[launcher-open] failed to send refresh result: {}",
                        err
                    ));
                }
            });

        if let Err(err) = spawn_result {
            self.pending_refresh = None;
            self.status = OpenPageStatus::RefreshError(format!(
                "Не удалось запустить обновление списка проектов: {}",
                err
            ));
        }
    }

    fn poll_refresh(&mut self) {
        let mut should_clear = false;
        if let Some(rx) = &self.pending_refresh {
            match rx.try_recv() {
                Ok(result) => {
                    should_clear = true;
                    self.titles = result.titles;
                    self.selected_title = result.selected_title;
                    self.chapters = result.chapters;
                    self.selected_chapter = result.selected_chapter;

                    self.status = if let Some(error_message) = result.error_message {
                        OpenPageStatus::RefreshError(error_message)
                    } else if self.titles.is_empty() {
                        OpenPageStatus::Empty(NO_PROJECTS_MESSAGE.to_string())
                    } else if self.chapters.is_empty() {
                        OpenPageStatus::Empty("Для выбранного тайтла не найдено глав.".to_string())
                    } else {
                        OpenPageStatus::Validating
                    };

                    self.refresh_unsaved_detection();

                    if self.selected_project_dir().is_some() {
                        self.start_validation_for_current_selection();
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    should_clear = true;
                    self.status = OpenPageStatus::RefreshError(
                        "Проверка списка проектов завершилась ошибкой.".to_string(),
                    );
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if should_clear {
            self.pending_refresh = None;
        }
    }

    fn start_validation_for_current_selection(&mut self) {
        let Some(selection) = self.current_selection() else {
            self.status = OpenPageStatus::Empty("Выберите тайтл и главу.".to_string());
            return;
        };

        self.status = OpenPageStatus::Validating;
        let project_dir = selection.project_dir.clone();
        let (tx, rx) = mpsc::channel();
        self.pending_validation = Some(rx);
        let spawn_result = thread::Builder::new()
            .name("launcher-open-validate".to_string())
            .spawn(move || {
                let state = crate::validate_project_dir_for_startup(&project_dir);
                if let Err(err) = tx.send(OpenPageValidationResult { project_dir, state }) {
                    runtime_log::log_warn(format!(
                        "[launcher-open] failed to send validation result: {}",
                        err
                    ));
                }
            });

        if let Err(err) = spawn_result {
            self.pending_validation = None;
            self.status =
                OpenPageStatus::Invalid(format!("Не удалось запустить проверку главы: {}", err));
        }
    }

    fn start_open_current_selection(&mut self) -> Option<PageNavAction> {
        let Some(selection) = self.current_selection() else {
            self.status = OpenPageStatus::Empty("Выберите тайтл и главу.".to_string());
            return None;
        };

        let Some(unsaved_dir) = self.current_unsaved_dir() else {
            return Some(PageNavAction::OpenProject(selection));
        };

        self.status = OpenPageStatus::Opening;
        let (tx, rx) = mpsc::channel();
        self.pending_open = Some(rx);
        let spawn_result = thread::Builder::new()
            .name("launcher-open-cleanup-unsaved".to_string())
            .spawn(move || {
                let cleanup_result = if unsaved_dir.exists() {
                    fs::remove_dir_all(&unsaved_dir).map_err(|err| {
                        format!(
                            "Не удалось удалить временную главу {}: {err}",
                            unsaved_dir.display()
                        )
                    })
                } else {
                    Ok(())
                };

                match cleanup_result {
                    Ok(()) => {
                        runtime_log::log_info(format!(
                            "[launcher-open] deleted stale unsaved chapter '{}'",
                            unsaved_dir.display()
                        ));
                        if let Err(err) = tx.send(Ok(selection)) {
                            runtime_log::log_warn(format!(
                                "[launcher-open] failed to send open result: {}",
                                err
                            ));
                        }
                    }
                    Err(message) => {
                        runtime_log::log_error(format!(
                            "[launcher-open] failed to delete stale unsaved chapter '{}': {}",
                            unsaved_dir.display(),
                            message
                        ));
                        if let Err(err) = tx.send(Err(message)) {
                            runtime_log::log_warn(format!(
                                "[launcher-open] failed to send open error: {}",
                                err
                            ));
                        }
                    }
                }
            });

        if let Err(err) = spawn_result {
            self.pending_open = None;
            self.status = OpenPageStatus::Invalid(format!(
                "Не удалось запустить очистку временной главы: {}",
                err
            ));
        }

        None
    }

    fn poll_validation(&mut self) {
        let mut should_clear = false;
        if let Some(rx) = &self.pending_validation {
            match rx.try_recv() {
                Ok(result) => {
                    should_clear = true;
                    if self.selected_project_dir().as_ref() == Some(&result.project_dir) {
                        self.status = match result.state {
                            crate::ProjectValidationState::Valid { image_count } => {
                                OpenPageStatus::Ready { image_count }
                            }
                            crate::ProjectValidationState::Invalid { message } => {
                                OpenPageStatus::Invalid(message)
                            }
                        };
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    should_clear = true;
                    self.status = OpenPageStatus::Invalid(
                        "Проверка выбранной главы завершилась ошибкой.".to_string(),
                    );
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if should_clear {
            self.pending_validation = None;
        }
    }

    fn poll_open(&mut self) -> Option<PageNavAction> {
        let mut should_clear = false;
        let mut action = None;
        if let Some(rx) = &self.pending_open {
            match rx.try_recv() {
                Ok(Ok(selection)) => {
                    should_clear = true;
                    action = Some(PageNavAction::OpenProject(selection));
                }
                Ok(Err(message)) => {
                    should_clear = true;
                    self.status = OpenPageStatus::Invalid(message);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    should_clear = true;
                    self.status =
                        OpenPageStatus::Invalid("Открытие главы завершилось ошибкой.".to_string());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        if should_clear {
            self.pending_open = None;
        }

        action
    }
}

pub fn persist_last_selection_values(title: &str, chapter: &str) -> anyhow::Result<()> {
    let mut cfg = config::load_user_config()?;
    cfg.set_path(
        &["General", LAST_OPEN_TITLE_KEY],
        Value::String(title.to_string()),
    )?;
    cfg.set_path(
        &["General", LAST_OPEN_CHAPTER_KEY],
        Value::String(chapter.to_string()),
    )?;
    Ok(())
}

fn build_refresh_result(
    projects_root: &std::path::Path,
    preferred_title: &str,
    preferred_chapter: &str,
) -> OpenPageRefreshResult {
    let titles = match crate::list_titles(projects_root) {
        Ok(titles) => titles,
        Err(err) if err.kind() == ErrorKind::NotFound => Vec::new(),
        Err(err) => {
            return OpenPageRefreshResult {
                titles: Vec::new(),
                selected_title: None,
                chapters: Vec::new(),
                selected_chapter: None,
                error_message: Some(format!(
                    "Не удалось прочитать папку проектов '{}': {}",
                    projects_root.display(),
                    err
                )),
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
        .map(|title| crate::list_chapters(projects_root, title).unwrap_or_default())
        .unwrap_or_default();

    let selected_chapter = if preferred_chapter.is_empty() {
        chapters.first().cloned()
    } else if chapters.iter().any(|chapter| chapter == preferred_chapter) {
        Some(preferred_chapter.to_string())
    } else {
        chapters.first().cloned()
    };

    OpenPageRefreshResult {
        titles,
        selected_title,
        chapters,
        selected_chapter,
        error_message: None,
    }
}

fn read_last_open_selection(user_settings: &Value) -> (String, String) {
    let general = user_settings.get("General").and_then(Value::as_object);
    let title = general
        .and_then(|general| general.get(LAST_OPEN_TITLE_KEY))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string();
    let chapter = general
        .and_then(|general| general.get(LAST_OPEN_CHAPTER_KEY))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string();
    (title, chapter)
}

fn show_status(ui: &mut Ui, status: &OpenPageStatus) {
    match status {
        OpenPageStatus::Loading => {
            ui.label(theme::status(
                "Загрузка списка тайтлов...",
                theme::TEXT_MUTED,
            ));
        }
        OpenPageStatus::RefreshError(message) | OpenPageStatus::Invalid(message) => {
            ui.label(theme::status(
                message,
                egui::Color32::from_rgb(220, 120, 120),
            ));
        }
        OpenPageStatus::Empty(message) => {
            ui.label(theme::status(message, theme::TEXT_MUTED));
        }
        OpenPageStatus::Validating => {
            ui.label(theme::status(
                "Проверка структуры папки и изображений...",
                theme::TEXT_MUTED,
            ));
        }
        OpenPageStatus::Opening => {
            ui.label(theme::status(
                "Очищаем найденную временную главу перед открытием...",
                theme::TEXT_MUTED,
            ));
        }
        OpenPageStatus::Ready { image_count } => {
            ui.label(theme::status(
                &format!(
                    "Готово к открытию: найдено {} изображений в src.",
                    image_count
                ),
                theme::STATUS_SUCCESS,
            ));
        }
    }
}
