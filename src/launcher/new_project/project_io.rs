/*
File: src/launcher/new_project/project_io.rs

Purpose:
Reusable project catalog and save workers for the New Project launcher window.

Main responsibilities:
- scan the configured projects root for titles and chapters on a background thread;
- save ribbon pages as zero-padded PNG files into project `src`, `alt_vers`, or any folder;
- keep filesystem work off the GUI thread and stream structured progress back to the UI.

Key structures:
- ProjectCatalogController
- ProjectCatalogSnapshot
- ProjectSaveController
- ProjectSaveRequest
- ProjectSaveTarget

Notes:
This module is shared infrastructure for the new-project flow and is intended to be reused by
other launcher features that need project discovery or image export.
*/

use crate::config;
use crate::launcher::state::OpenProjectSelection;
use image::{DynamicImage, ImageFormat, RgbaImage};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;

#[derive(Clone)]
pub struct ProjectCatalogSnapshot {
    pub titles: Vec<String>,
    pub chapters_by_title: HashMap<String, Vec<String>>,
}

#[derive(Debug)]
struct PendingProjectCatalog {
    rx: Receiver<Result<ProjectCatalogSnapshot, ProjectCatalogError>>,
}

pub struct ProjectCatalogController {
    projects_root: PathBuf,
    pending: Option<PendingProjectCatalog>,
}

pub enum ProjectCatalogEvent {
    Loaded(ProjectCatalogSnapshot),
    Failed {
        user_message: String,
        log_message: String,
    },
    WorkerDisconnected,
}

#[derive(Debug)]
struct ProjectCatalogError {
    user_message: String,
    log_message: String,
}

#[derive(Clone)]
pub struct ProjectSaveImage {
    pub image: RgbaImage,
}

pub enum ProjectSaveTarget {
    ProjectSource {
        title: String,
        chapter: String,
    },
    AltVersion {
        title: String,
        chapter: String,
        alt_name: String,
    },
    Folder {
        folder: PathBuf,
    },
}

pub struct ProjectSaveRequest {
    pub target: ProjectSaveTarget,
    pub images: Vec<ProjectSaveImage>,
}

#[derive(Debug)]
struct PendingProjectSave {
    rx: Receiver<ProjectSaveWorkerEvent>,
}

pub struct ProjectSaveController {
    projects_root: PathBuf,
    pending: Option<PendingProjectSave>,
}

pub struct ProjectSaveSuccess {
    pub target_dir: PathBuf,
    pub saved_images: usize,
    pub open_selection: Option<OpenProjectSelection>,
}

pub enum ProjectSaveEvent {
    Progress {
        stage: String,
        current: usize,
        total: usize,
    },
    Completed(ProjectSaveSuccess),
    Failed {
        user_message: String,
        log_message: String,
    },
    WorkerDisconnected,
}

enum ProjectSaveWorkerEvent {
    Progress {
        stage: &'static str,
        current: usize,
        total: usize,
    },
    Finished(Result<ProjectSaveSuccess, ProjectSaveError>),
}

#[derive(Debug)]
struct ProjectSaveError {
    user_message: String,
    log_message: String,
}

struct SaveResolvedTarget {
    output_dir: PathBuf,
    open_selection: Option<OpenProjectSelection>,
}

impl ProjectCatalogController {
    pub fn new(projects_root: PathBuf) -> Self {
        Self {
            projects_root,
            pending: None,
        }
    }

    pub fn projects_root(&self) -> &Path {
        &self.projects_root
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn refresh(&mut self) {
        let projects_root = self.projects_root.clone();
        let (tx, rx) = mpsc::channel();
        self.pending = Some(PendingProjectCatalog { rx });
        match thread::Builder::new()
            .name("new-project-project-catalog".to_string())
            .spawn(move || {
                let result = load_project_catalog(&projects_root);
                if tx.send(result).is_err() {
                    crate::runtime_log::log_warn(
                        "[new-project] failed to send project catalog result to UI",
                    );
                }
            }) {
            Ok(_) => {}
            Err(err) => {
                crate::runtime_log::log_error(format!(
                    "[new-project] failed to spawn project catalog worker: {err}"
                ));
                let (fallback_tx, fallback_rx) = mpsc::channel();
                self.pending = Some(PendingProjectCatalog { rx: fallback_rx });
                if fallback_tx
                    .send(Err(ProjectCatalogError {
                        user_message: "Не удалось запустить чтение списка проектов.".to_string(),
                        log_message: format!("failed to spawn project catalog worker: {err}"),
                    }))
                    .is_err()
                {
                    crate::runtime_log::log_warn(
                        "[new-project] failed to deliver project catalog spawn error",
                    );
                }
            }
        }
    }

    pub fn poll(&mut self, ctx: &egui::Context) -> Option<ProjectCatalogEvent> {
        let pending = self.pending.take()?;
        match pending.rx.try_recv() {
            Ok(result) => {
                ctx.request_repaint();
                match result {
                    Ok(snapshot) => Some(ProjectCatalogEvent::Loaded(snapshot)),
                    Err(err) => Some(ProjectCatalogEvent::Failed {
                        user_message: err.user_message,
                        log_message: err.log_message,
                    }),
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.pending = Some(pending);
                None
            }
            Err(mpsc::TryRecvError::Disconnected) => Some(ProjectCatalogEvent::WorkerDisconnected),
        }
    }
}

impl ProjectSaveController {
    pub fn new(projects_root: PathBuf) -> Self {
        Self {
            projects_root,
            pending: None,
        }
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn begin(&mut self, request: ProjectSaveRequest) {
        let projects_root = self.projects_root.clone();
        let (tx, rx) = mpsc::channel();
        self.pending = Some(PendingProjectSave { rx });
        match thread::Builder::new()
            .name("new-project-save".to_string())
            .spawn(move || {
                let result = run_save_request(&projects_root, request, tx.clone());
                if tx.send(ProjectSaveWorkerEvent::Finished(result)).is_err() {
                    crate::runtime_log::log_warn("[new-project] failed to send save result to UI");
                }
            }) {
            Ok(_) => {}
            Err(err) => {
                crate::runtime_log::log_error(format!(
                    "[new-project] failed to spawn save worker: {err}"
                ));
                let (fallback_tx, fallback_rx) = mpsc::channel();
                self.pending = Some(PendingProjectSave { rx: fallback_rx });
                if fallback_tx
                    .send(ProjectSaveWorkerEvent::Finished(Err(ProjectSaveError {
                        user_message: "Не удалось запустить сохранение.".to_string(),
                        log_message: format!("failed to spawn save worker: {err}"),
                    })))
                    .is_err()
                {
                    crate::runtime_log::log_warn(
                        "[new-project] failed to deliver save spawn error",
                    );
                }
            }
        }
    }

    pub fn poll(&mut self, ctx: &egui::Context) -> Option<ProjectSaveEvent> {
        let pending = self.pending.take()?;
        let mut last_progress = None;
        loop {
            match pending.rx.try_recv() {
                Ok(ProjectSaveWorkerEvent::Progress {
                    stage,
                    current,
                    total,
                }) => {
                    ctx.request_repaint();
                    last_progress = Some(ProjectSaveEvent::Progress {
                        stage: stage.to_string(),
                        current,
                        total,
                    });
                }
                Ok(ProjectSaveWorkerEvent::Finished(result)) => match result {
                    Ok(success) => {
                        ctx.request_repaint();
                        return Some(ProjectSaveEvent::Completed(success));
                    }
                    Err(err) => {
                        return Some(ProjectSaveEvent::Failed {
                            user_message: err.user_message,
                            log_message: err.log_message,
                        });
                    }
                },
                Err(mpsc::TryRecvError::Empty) => {
                    self.pending = Some(pending);
                    return last_progress;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Some(ProjectSaveEvent::WorkerDisconnected);
                }
            }
        }
    }
}

pub fn chapters_for_title<'a>(snapshot: &'a ProjectCatalogSnapshot, title: &str) -> &'a [String] {
    snapshot
        .chapters_by_title
        .get(title)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

pub fn dir_has_entries(path: &Path) -> Result<bool, std::io::Error> {
    let mut entries = fs::read_dir(path)?;
    Ok(entries.next().is_some())
}

fn clear_dir_contents(path: &Path) -> Result<(), ProjectSaveError> {
    if !path.exists() {
        return Ok(());
    }
    let entries = fs::read_dir(path).map_err(|err| ProjectSaveError {
        user_message: "Не удалось подготовить папку для сохранения.".to_string(),
        log_message: format!(
            "failed to read dir '{}' before cleaning: {err}",
            path.display()
        ),
    })?;
    for entry in entries {
        let entry = entry.map_err(|err| ProjectSaveError {
            user_message: "Не удалось подготовить папку для сохранения.".to_string(),
            log_message: format!(
                "failed to enumerate dir '{}' before cleaning: {err}",
                path.display()
            ),
        })?;
        let entry_path = entry.path();
        let result = if entry_path.is_dir() {
            fs::remove_dir_all(&entry_path)
        } else {
            fs::remove_file(&entry_path)
        };
        result.map_err(|err| ProjectSaveError {
            user_message: "Не удалось очистить папку перед сохранением.".to_string(),
            log_message: format!(
                "failed to remove '{}' while cleaning: {err}",
                entry_path.display()
            ),
        })?;
    }
    Ok(())
}

fn load_project_catalog(
    projects_root: &Path,
) -> Result<ProjectCatalogSnapshot, ProjectCatalogError> {
    let titles = crate::list_titles(projects_root).map_err(|err| ProjectCatalogError {
        user_message: "Не удалось прочитать список тайтлов.".to_string(),
        log_message: format!(
            "failed to list titles in '{}': {err}",
            projects_root.display()
        ),
    })?;
    let mut chapters_by_title = HashMap::with_capacity(titles.len());
    for title in &titles {
        let chapters =
            crate::list_chapters(projects_root, title).map_err(|err| ProjectCatalogError {
                user_message: format!("Не удалось прочитать главы тайтла '{title}'."),
                log_message: format!(
                    "failed to list chapters for title '{}' in '{}': {err}",
                    title,
                    projects_root.display()
                ),
            })?;
        chapters_by_title.insert(title.clone(), chapters);
    }
    Ok(ProjectCatalogSnapshot {
        titles,
        chapters_by_title,
    })
}

fn run_save_request(
    projects_root: &Path,
    request: ProjectSaveRequest,
    tx: mpsc::Sender<ProjectSaveWorkerEvent>,
) -> Result<ProjectSaveSuccess, ProjectSaveError> {
    if request.images.is_empty() {
        return Err(ProjectSaveError {
            user_message: "На холсте нет изображений для сохранения.".to_string(),
            log_message: "save request received without images".to_string(),
        });
    }

    send_progress(&tx, "prepare", 0, 0);
    let resolved = resolve_target(projects_root, &request.target)?;
    fs::create_dir_all(&resolved.output_dir).map_err(|err| ProjectSaveError {
        user_message: "Не удалось создать папку для сохранения.".to_string(),
        log_message: format!(
            "failed to create save dir '{}': {err}",
            resolved.output_dir.display()
        ),
    })?;

    send_progress(&tx, "clean", 0, 0);
    clear_dir_contents(&resolved.output_dir)?;

    let saved_images = save_png_images_parallel(&request.images, &resolved.output_dir, &tx)?;
    if saved_images == 0 {
        return Err(ProjectSaveError {
            user_message: "Не удалось сохранить ни одного изображения.".to_string(),
            log_message: format!(
                "save target '{}' produced zero files",
                resolved.output_dir.display()
            ),
        });
    }

    Ok(ProjectSaveSuccess {
        target_dir: resolved.output_dir,
        saved_images,
        open_selection: resolved.open_selection,
    })
}

fn resolve_target(
    projects_root: &Path,
    target: &ProjectSaveTarget,
) -> Result<SaveResolvedTarget, ProjectSaveError> {
    match target {
        ProjectSaveTarget::ProjectSource { title, chapter } => {
            let title = title.trim();
            let chapter = chapter.trim();
            if title.is_empty() || chapter.is_empty() {
                return Err(ProjectSaveError {
                    user_message: "Укажите тайтл и название главы.".to_string(),
                    log_message: format!(
                        "project save target missing title/chapter: title='{}', chapter='{}'",
                        title, chapter
                    ),
                });
            }
            let title_dir = projects_root.join(title);
            fs::create_dir_all(&title_dir).map_err(|err| ProjectSaveError {
                user_message: "Не удалось подготовить папку тайтла.".to_string(),
                log_message: format!(
                    "failed to create title dir '{}': {err}",
                    title_dir.display()
                ),
            })?;
            let notes_path = title_dir.join(config::NOTES_FILE);
            if !notes_path.exists() {
                fs::write(&notes_path, b"").map_err(|err| ProjectSaveError {
                    user_message: "Не удалось подготовить файл заметок тайтла.".to_string(),
                    log_message: format!(
                        "failed to create notes file '{}': {err}",
                        notes_path.display()
                    ),
                })?;
            }
            let project_dir = title_dir.join(chapter);
            let output_dir = project_dir.join(config::SRC_DIR);
            Ok(SaveResolvedTarget {
                output_dir,
                open_selection: Some(OpenProjectSelection {
                    project_dir,
                    title: title.to_string(),
                    chapter: chapter.to_string(),
                    resume_unsaved: false,
                }),
            })
        }
        ProjectSaveTarget::AltVersion {
            title,
            chapter,
            alt_name,
        } => {
            let title = title.trim();
            let chapter = chapter.trim();
            let alt_name = alt_name.trim();
            if title.is_empty() || chapter.is_empty() || alt_name.is_empty() {
                return Err(ProjectSaveError {
                    user_message: "Укажите тайтл, главу и название альтер-версии.".to_string(),
                    log_message: format!(
                        "alt save target missing fields: title='{}', chapter='{}', alt='{}'",
                        title, chapter, alt_name
                    ),
                });
            }
            Ok(SaveResolvedTarget {
                output_dir: projects_root
                    .join(title)
                    .join(chapter)
                    .join(config::ALT_VERS_DIR)
                    .join(alt_name),
                open_selection: None,
            })
        }
        ProjectSaveTarget::Folder { folder } => Ok(SaveResolvedTarget {
            output_dir: folder.clone(),
            open_selection: None,
        }),
    }
}

fn save_png_images_parallel(
    images: &[ProjectSaveImage],
    output_dir: &Path,
    tx: &mpsc::Sender<ProjectSaveWorkerEvent>,
) -> Result<usize, ProjectSaveError> {
    let total = images.len();
    let pad = total.max(1).to_string().len().max(3);
    let progress = Arc::new(AtomicUsize::new(0));
    let output_dir = output_dir.to_path_buf();
    let tx = tx.clone();

    images.par_iter().enumerate().try_for_each(
        |(index, image)| -> Result<(), ProjectSaveError> {
            let file_name = format!("{:0width$}.png", index + 1, width = pad);
            let path = output_dir.join(&file_name);
            DynamicImage::ImageRgba8(image.image.clone())
                .save_with_format(&path, ImageFormat::Png)
                .map_err(|err| ProjectSaveError {
                    user_message: "Не удалось сохранить изображения в PNG.".to_string(),
                    log_message: format!("failed to save png '{}': {err}", path.display()),
                })?;
            let current = progress.fetch_add(1, Ordering::Relaxed) + 1;
            send_progress(&tx, "write", current, total);
            Ok(())
        },
    )?;

    Ok(total)
}

fn send_progress(
    tx: &mpsc::Sender<ProjectSaveWorkerEvent>,
    stage: &'static str,
    current: usize,
    total: usize,
) {
    if tx
        .send(ProjectSaveWorkerEvent::Progress {
            stage,
            current,
            total,
        })
        .is_err()
    {
        crate::runtime_log::log_warn("[new-project] failed to send save progress to UI");
    }
}
