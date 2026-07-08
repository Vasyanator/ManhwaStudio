/*
File: src/launcher/new_project/open_source.rs

Purpose:
Source picking and background import pipeline for the New Project launcher window.

Main responsibilities:
- isolate folder/file pickers from the UI layer;
- import images from folders, HTML saves, archives, and standalone image files;
- parse saved webpage HTML and turn imported images into ribbon pages on a worker thread.

Key structures:
- OpenSourceKind
- SourceImportController
- SourceImportOptions
- SourceLoadEvent

Notes:
The GUI polls this module for completed import events. Long-running file I/O stays off the
main thread to keep the launcher responsive.
*/

use crate::launcher::new_project::ribbon::{ImportedImage, RibbonPage, build_ribbon_pages};
use image::{DynamicImage, ImageFormat, ImageReader};
use ms_thread as thread;
#[cfg(not(target_arch = "wasm32"))]
use rfd::FileDialog;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use web_time::{SystemTime, UNIX_EPOCH};

const IMAGE_SIGNATURE_BYTES: usize = 32;
// Picker filter lists are only consumed by the native file dialog (`rfd`); gated out
// of the web build, which has no dialog to filter.
#[cfg(not(target_arch = "wasm32"))]
const IMAGE_DIALOG_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "bmp", "webp", "tif", "tiff"];
#[cfg(not(target_arch = "wasm32"))]
const HTML_DIALOG_EXTENSIONS: &[&str] = &["html", "htm"];
#[cfg(not(target_arch = "wasm32"))]
const ARCHIVE_DIALOG_EXTENSIONS: &[&str] = &["zip", "rar", "7z", "tar", "tgz", "gz"];
#[cfg(not(target_arch = "wasm32"))]
const SUPPORTED_DIALOG_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "bmp", "webp", "tif", "tiff", "html", "htm", "zip", "rar", "7z", "tar",
    "tgz", "gz",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenSourceKind {
    Folder,
    File,
}

pub struct SourceImportOptions {
    pub filter_same_width: bool,
    pub extra_name_patterns: String,
}

#[derive(Debug)]
struct PendingSourceLoad {
    rx: Receiver<SourceLoadWorkerEvent>,
}

pub struct SourceImportController {
    pending: Option<PendingSourceLoad>,
}

pub struct SourceLoadSuccess {
    pub source_path: PathBuf,
    pub pages: Vec<RibbonPage>,
    pub imported_images: usize,
    pub skipped_files: usize,
    pub filtered_out: usize,
    pub filter_bounds: Option<(usize, usize, usize)>,
}

pub enum SourceLoadEvent {
    Progress {
        stage: String,
        current: usize,
        total: usize,
    },
    Loaded(SourceLoadSuccess),
    Failed {
        user_message: String,
        log_message: String,
    },
    WorkerDisconnected,
}

struct LoadedSource {
    source_path: PathBuf,
    pages: Vec<RibbonPage>,
    imported_images: usize,
    skipped_files: usize,
    filtered_out: usize,
    filter_bounds: Option<(usize, usize, usize)>,
}

enum SourceLoadWorkerEvent {
    Progress {
        stage: &'static str,
        current: usize,
        total: usize,
    },
    Finished(Result<LoadedSource, SourceLoadError>),
}

#[derive(Debug)]
struct SourceLoadError {
    user_message: String,
    log_message: String,
}

struct ImportedImageBatch {
    source_path: PathBuf,
    images: Vec<ImportedImage>,
    skipped_files: usize,
}

impl SourceImportController {
    pub fn new() -> Self {
        Self { pending: None }
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn begin_pick(&mut self, kind: OpenSourceKind, options: SourceImportOptions) -> bool {
        let Some(path) = pick_source(kind) else {
            return false;
        };
        self.begin_load(kind, path, options);
        true
    }

    pub fn begin_load(
        &mut self,
        kind: OpenSourceKind,
        path: PathBuf,
        options: SourceImportOptions,
    ) {
        self.pending = Some(PendingSourceLoad {
            rx: spawn_source_loader(kind, path, options),
        });
    }

    pub fn poll(&mut self, ctx: &egui::Context) -> Option<SourceLoadEvent> {
        let pending = self.pending.take()?;
        let mut last_progress = None;
        loop {
            match pending.rx.try_recv() {
                Ok(SourceLoadWorkerEvent::Progress {
                    stage,
                    current,
                    total,
                }) => {
                    ctx.request_repaint();
                    last_progress = Some(SourceLoadEvent::Progress {
                        stage: stage.to_string(),
                        current,
                        total,
                    });
                }
                Ok(SourceLoadWorkerEvent::Finished(result)) => match result {
                    Ok(result) => {
                        ctx.request_repaint();
                        return Some(SourceLoadEvent::Loaded(SourceLoadSuccess {
                            source_path: result.source_path,
                            pages: result.pages,
                            imported_images: result.imported_images,
                            skipped_files: result.skipped_files,
                            filtered_out: result.filtered_out,
                            filter_bounds: result.filter_bounds,
                        }));
                    }
                    Err(err) => {
                        return Some(SourceLoadEvent::Failed {
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
                    return Some(SourceLoadEvent::WorkerDisconnected);
                }
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn pick_source(kind: OpenSourceKind) -> Option<PathBuf> {
    match kind {
        OpenSourceKind::Folder => FileDialog::new().pick_folder(),
        OpenSourceKind::File => FileDialog::new()
            .add_filter("Поддерживаемые файлы", SUPPORTED_DIALOG_EXTENSIONS)
            .add_filter("Изображения", IMAGE_DIALOG_EXTENSIONS)
            .add_filter("HTML", HTML_DIALOG_EXTENSIONS)
            .add_filter("Архивы", ARCHIVE_DIALOG_EXTENSIONS)
            .pick_file(),
    }
}

/// Web stub: native file/folder pickers (`rfd`) have no browser equivalent. Returns
/// `None` (as a cancelled pick) and logs the dropped capability.
#[cfg(target_arch = "wasm32")]
fn pick_source(_kind: OpenSourceKind) -> Option<PathBuf> {
    crate::runtime_log::log_warn("source file picker unavailable on web build");
    None
}

fn spawn_source_loader(
    kind: OpenSourceKind,
    path: PathBuf,
    options: SourceImportOptions,
) -> Receiver<SourceLoadWorkerEvent> {
    let (tx, rx) = mpsc::channel();
    let tx_worker = tx.clone();
    let path_for_thread = path.clone();
    match thread::Builder::new()
        .name("new-project-source-loader".to_string())
        .spawn(move || {
            let result = load_source(kind, &path_for_thread, &options, &tx_worker);
            if tx_worker
                .send(SourceLoadWorkerEvent::Finished(result))
                .is_err()
            {
                crate::runtime_log::log_warn(
                    "[new-project] failed to send source import result to UI",
                );
            }
        }) {
        Ok(_) => {}
        Err(err) => {
            crate::runtime_log::log_error(format!(
                "[new-project] failed to spawn source loader for '{}': {err}",
                path.display()
            ));
            if tx
                .send(SourceLoadWorkerEvent::Finished(Err(SourceLoadError {
                    user_message: "Не удалось запустить импорт источника.".to_string(),
                    log_message: format!(
                        "failed to spawn source loader for '{}': {err}",
                        path.display()
                    ),
                })))
                .is_err()
            {
                crate::runtime_log::log_warn(
                    "[new-project] failed to deliver source spawn error to UI",
                );
            }
        }
    }
    rx
}

fn load_source(
    kind: OpenSourceKind,
    path: &Path,
    options: &SourceImportOptions,
    progress_tx: &Sender<SourceLoadWorkerEvent>,
) -> Result<LoadedSource, SourceLoadError> {
    let _ = progress_tx.send(SourceLoadWorkerEvent::Progress {
        stage: "scan",
        current: 0,
        total: 1,
    });
    let mut batch = match kind {
        OpenSourceKind::Folder => load_from_folder(path, options, progress_tx)?,
        OpenSourceKind::File => load_from_file(path, progress_tx)?,
    };

    if batch.images.is_empty() {
        return Err(SourceLoadError {
            user_message: "Не удалось получить изображения из выбранного источника.".to_string(),
            log_message: format!("source '{}' produced zero images", path.display()),
        });
    }

    let (filtered_out, filter_bounds) = if options.filter_same_width {
        let _ = progress_tx.send(SourceLoadWorkerEvent::Progress {
            stage: "filter",
            current: 0,
            total: batch.images.len().max(1),
        });
        apply_width_filter_with_fallback(&mut batch.images)
    } else {
        (0, None)
    };
    let imported_images = batch.images.len();
    let _ = progress_tx.send(SourceLoadWorkerEvent::Progress {
        stage: "preview",
        current: 0,
        total: imported_images.max(1),
    });
    let pages = build_ribbon_pages(batch.images);
    Ok(LoadedSource {
        source_path: batch.source_path,
        pages,
        imported_images,
        skipped_files: batch.skipped_files,
        filtered_out,
        filter_bounds,
    })
}

fn load_from_folder(
    folder: &Path,
    options: &SourceImportOptions,
    progress_tx: &Sender<SourceLoadWorkerEvent>,
) -> Result<ImportedImageBatch, SourceLoadError> {
    if !folder.is_dir() {
        return Err(SourceLoadError {
            user_message: "Выбранный путь не является папкой.".to_string(),
            log_message: format!("'{}' is not a directory", folder.display()),
        });
    }

    if let Some((html_path, resources_folder)) = check_saved_webpage(folder) {
        crate::runtime_log::log_info(format!(
            "[new-project] detected saved webpage folder '{}' -> '{}'",
            folder.display(),
            html_path.display()
        ));
        let images = load_images_from_html(&html_path, &resources_folder, progress_tx)?;
        if images.is_empty() {
            return Err(SourceLoadError {
                user_message: "Не удалось открыть изображения из сохранённой веб-страницы."
                    .to_string(),
                log_message: format!(
                    "saved webpage '{}' did not produce decodable images",
                    html_path.display()
                ),
            });
        }
        return Ok(ImportedImageBatch {
            source_path: folder.to_path_buf(),
            images,
            skipped_files: 0,
        });
    }

    let mut candidates = collect_folder_candidates(folder, &options.extra_name_patterns)?;
    if candidates.is_empty() {
        candidates = list_resource_like_files(folder)?;
    }
    if candidates.is_empty() {
        return Err(SourceLoadError {
            user_message: "Не удалось открыть изображения из папки.".to_string(),
            log_message: format!("folder '{}' has no matching files", folder.display()),
        });
    }

    let (images, skipped_files) = decode_images_from_paths(candidates, progress_tx);
    if images.is_empty() {
        return Err(SourceLoadError {
            user_message: "Не удалось открыть изображения из папки.".to_string(),
            log_message: format!("folder '{}' has no decodable images", folder.display()),
        });
    }

    Ok(ImportedImageBatch {
        source_path: folder.to_path_buf(),
        images,
        skipped_files,
    })
}

fn load_from_file(
    path: &Path,
    progress_tx: &Sender<SourceLoadWorkerEvent>,
) -> Result<ImportedImageBatch, SourceLoadError> {
    if !path.is_file() {
        return Err(SourceLoadError {
            user_message: "Выбранный путь не является файлом.".to_string(),
            log_message: format!("'{}' is not a file", path.display()),
        });
    }

    if is_decodable_image_file(path) {
        let _ = progress_tx.send(SourceLoadWorkerEvent::Progress {
            stage: "decode",
            current: 0,
            total: 1,
        });
        let image = decode_image_path(path).map_err(|err| SourceLoadError {
            user_message: "Не удалось открыть изображение.".to_string(),
            log_message: format!("failed to decode image '{}': {err}", path.display()),
        })?;
        return Ok(ImportedImageBatch {
            source_path: path.to_path_buf(),
            images: vec![ImportedImage {
                name: file_name_or_display(path),
                image,
            }],
            skipped_files: 0,
        });
    }

    let ext = lowercase_path(path);
    if ext.ends_with(".html") || ext.ends_with(".htm") {
        let resources_folder = resources_folder_for_html(path)
            .unwrap_or_else(|| path.parent().unwrap_or(path).to_path_buf());
        let images = load_images_from_html(path, &resources_folder, progress_tx)?;
        if images.is_empty() {
            return Err(SourceLoadError {
                user_message: "Не удалось открыть изображения из HTML.".to_string(),
                log_message: format!("html '{}' produced zero images", path.display()),
            });
        }
        return Ok(ImportedImageBatch {
            source_path: path.to_path_buf(),
            images,
            skipped_files: 0,
        });
    }

    if archive_kind(path).is_some() {
        let images = load_images_from_archive(path, progress_tx)?;
        if images.is_empty() {
            return Err(SourceLoadError {
                user_message: "Не удалось найти изображения в архиве.".to_string(),
                log_message: format!("archive '{}' produced zero images", path.display()),
            });
        }
        return Ok(ImportedImageBatch {
            source_path: path.to_path_buf(),
            images,
            skipped_files: 0,
        });
    }

    Err(SourceLoadError {
        user_message: "Этот формат файла пока не поддерживается.".to_string(),
        log_message: format!("unsupported file format '{}'", path.display()),
    })
}

fn collect_folder_candidates(
    folder: &Path,
    extra_patterns: &str,
) -> Result<Vec<PathBuf>, SourceLoadError> {
    let entries = fs::read_dir(folder).map_err(|err| SourceLoadError {
        user_message: "Не удалось открыть папку с изображениями.".to_string(),
        log_message: format!("read_dir failed for '{}': {err}", folder.display()),
    })?;

    let patterns = parse_pattern_list(extra_patterns);
    let mut by_signature = Vec::new();
    let mut extra = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                crate::runtime_log::log_warn(format!(
                    "[new-project] failed to read entry in '{}': {err}",
                    folder.display()
                ));
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if is_decodable_image_file(&path) {
            by_signature.push(path.clone());
        }
        if !patterns.is_empty() && wildcard_matches_any(file_name, &patterns) {
            extra.push(path);
        }
    }

    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for path in by_signature.into_iter().chain(extra) {
        let key = path.to_string_lossy().to_string();
        if seen.insert(key) {
            unique.push(path);
        }
    }
    unique.sort_by(|left, right| compare_import_paths(left, right));
    Ok(unique)
}

fn list_resource_like_files(folder: &Path) -> Result<Vec<PathBuf>, SourceLoadError> {
    let entries = fs::read_dir(folder).map_err(|err| SourceLoadError {
        user_message: "Не удалось открыть папку с изображениями.".to_string(),
        log_message: format!("read_dir failed for '{}': {err}", folder.display()),
    })?;
    let mut matches = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                crate::runtime_log::log_warn(format!(
                    "[new-project] failed to read entry in '{}': {err}",
                    folder.display()
                ));
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        if let Some(index) = parse_resource_like_name(stem) {
            matches.push((index, path));
        }
    }
    matches.sort_by_key(|(index, _)| *index);
    Ok(matches.into_iter().map(|(_, path)| path).collect())
}

fn check_saved_webpage(folder: &Path) -> Option<(PathBuf, PathBuf)> {
    let folder_name = folder.file_name()?.to_str()?;
    let parent_dir = folder.parent()?;
    for suffix in ["_files", "_data"] {
        if let Some(page_name) = folder_name.strip_suffix(suffix) {
            let html_path = parent_dir.join(format!("{page_name}.html"));
            if html_path.is_file() {
                return Some((html_path, folder.to_path_buf()));
            }
        }
    }
    None
}

fn resources_folder_for_html(html_path: &Path) -> Option<PathBuf> {
    let stem = html_path.file_stem()?.to_str()?;
    let parent = html_path.parent()?;
    for suffix in ["_files", "_data"] {
        let candidate = parent.join(format!("{stem}{suffix}"));
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

fn load_images_from_html(
    html_path: &Path,
    resources_folder: &Path,
    progress_tx: &Sender<SourceLoadWorkerEvent>,
) -> Result<Vec<ImportedImage>, SourceLoadError> {
    let _ = progress_tx.send(SourceLoadWorkerEvent::Progress {
        stage: "parse_html",
        current: 0,
        total: 1,
    });
    let paths = parse_saved_webpage_images(html_path, resources_folder)?;
    let (images, _skipped) = decode_images_from_paths(paths, progress_tx);
    Ok(images)
}

fn parse_saved_webpage_images(
    html_path: &Path,
    resources_folder: &Path,
) -> Result<Vec<PathBuf>, SourceLoadError> {
    let html_bytes = fs::read(html_path).map_err(|err| SourceLoadError {
        user_message: "Не удалось прочитать HTML файл.".to_string(),
        log_message: format!("failed to read html '{}': {err}", html_path.display()),
    })?;
    let html = String::from_utf8_lossy(&html_bytes);
    let html_dir = html_path.parent().unwrap_or(resources_folder);
    let resources_name = resources_folder
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();

    let mut results = Vec::new();
    let mut seen = HashSet::new();
    let mut in_picture_depth = 0usize;
    for tag in extract_html_tags(&html) {
        let lower_name = tag.name.to_ascii_lowercase();
        if !tag.is_end {
            if lower_name == "picture" {
                in_picture_depth = in_picture_depth.saturating_add(1);
            }
            if lower_name == "img" || (lower_name == "source" && in_picture_depth > 0) {
                for attr_name in ["src", "data-src", "srcset"] {
                    if let Some(value) = get_html_attr(tag.attrs, attr_name) {
                        if attr_name == "srcset" {
                            for item in value.split(',') {
                                let candidate = item.split_whitespace().next().unwrap_or_default();
                                process_html_image_src(
                                    candidate,
                                    html_dir,
                                    resources_folder,
                                    resources_name,
                                    &mut seen,
                                    &mut results,
                                );
                            }
                        } else {
                            process_html_image_src(
                                value,
                                html_dir,
                                resources_folder,
                                resources_name,
                                &mut seen,
                                &mut results,
                            );
                        }
                    }
                }
            }
        } else if lower_name == "picture" {
            in_picture_depth = in_picture_depth.saturating_sub(1);
        }
    }
    Ok(results)
}

fn process_html_image_src(
    raw_src: &str,
    html_dir: &Path,
    resources_folder: &Path,
    resources_name: &str,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) {
    let decoded = percent_decode(raw_src.trim());
    if decoded.is_empty() {
        return;
    }
    let clean_src = decoded.trim_start_matches("./");
    let source_path = if clean_src.starts_with(&format!("{resources_name}/"))
        || clean_src.starts_with(&format!("{resources_name}\\"))
    {
        html_dir.join(clean_src)
    } else if !clean_src.contains('/') && !clean_src.contains('\\') {
        resources_folder.join(clean_src)
    } else {
        html_dir.join(clean_src)
    };
    if !source_path.is_file() || !is_decodable_image_file(&source_path) {
        return;
    }
    let absolute = source_path.canonicalize().unwrap_or(source_path);
    if seen.insert(absolute.clone()) {
        out.push(absolute);
    }
}

fn decode_images_from_paths(
    paths: Vec<PathBuf>,
    progress_tx: &Sender<SourceLoadWorkerEvent>,
) -> (Vec<ImportedImage>, usize) {
    let mut images = Vec::new();
    let mut skipped = 0usize;
    let total = paths.len();
    for (index, path) in paths.into_iter().enumerate() {
        let _ = progress_tx.send(SourceLoadWorkerEvent::Progress {
            stage: "decode",
            current: index,
            total,
        });
        match decode_image_path(&path) {
            Ok(image) => images.push(ImportedImage {
                name: file_name_or_display(&path),
                image,
            }),
            Err(err) => {
                skipped += 1;
                crate::runtime_log::log_warn(format!(
                    "[new-project] failed to decode '{}': {err}",
                    path.display()
                ));
            }
        }
    }
    let _ = progress_tx.send(SourceLoadWorkerEvent::Progress {
        stage: "decode",
        current: total,
        total,
    });
    (images, skipped)
}

fn decode_image_path(path: &Path) -> Result<DynamicImage, String> {
    let format = guess_image_format_from_path(path)?.ok_or_else(|| {
        format!(
            "unsupported or unknown image signature '{}'",
            path.display()
        )
    })?;
    let file =
        File::open(path).map_err(|err| format!("failed to open '{}': {err}", path.display()))?;
    ImageReader::with_format(BufReader::new(file), format)
        .decode()
        .map_err(|err| format!("failed to decode '{}': {err}", path.display()))
}

fn guess_image_format_from_path(path: &Path) -> Result<Option<ImageFormat>, String> {
    let mut file =
        File::open(path).map_err(|err| format!("failed to open '{}': {err}", path.display()))?;
    let mut prefix = [0u8; IMAGE_SIGNATURE_BYTES];
    let bytes_read = file
        .read(&mut prefix)
        .map_err(|err| format!("failed to read '{}': {err}", path.display()))?;
    Ok(guess_image_format_from_prefix(&prefix[..bytes_read]))
}

fn guess_image_format_from_prefix(prefix: &[u8]) -> Option<ImageFormat> {
    image::guess_format(prefix).ok()
}

fn is_decodable_image_file(path: &Path) -> bool {
    matches!(guess_image_format_from_path(path), Ok(Some(_)))
}

fn load_images_from_archive(
    path: &Path,
    progress_tx: &Sender<SourceLoadWorkerEvent>,
) -> Result<Vec<ImportedImage>, SourceLoadError> {
    let _ = progress_tx.send(SourceLoadWorkerEvent::Progress {
        stage: "archive",
        current: 0,
        total: 1,
    });
    match archive_kind(path).as_deref() {
        Some("zip") => load_images_from_zip(path, progress_tx),
        Some("tar") => load_images_from_tar(path, progress_tx),
        Some("rar") => load_images_from_external_archive(
            path,
            &["rar", "unrar", "unar", "7z", "7za"],
            progress_tx,
        ),
        Some("7z") => load_images_from_external_archive(path, &["7z", "7za"], progress_tx),
        _ => Ok(Vec::new()),
    }
}

fn load_images_from_zip(
    path: &Path,
    progress_tx: &Sender<SourceLoadWorkerEvent>,
) -> Result<Vec<ImportedImage>, SourceLoadError> {
    let file = File::open(path).map_err(|err| SourceLoadError {
        user_message: "Не удалось открыть ZIP архив.".to_string(),
        log_message: format!("failed to open zip '{}': {err}", path.display()),
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|err| SourceLoadError {
        user_message: "Не удалось прочитать ZIP архив.".to_string(),
        log_message: format!("failed to read zip '{}': {err}", path.display()),
    })?;

    let mut names = Vec::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(|err| SourceLoadError {
            user_message: "Не удалось прочитать ZIP архив.".to_string(),
            log_message: format!(
                "failed to access zip entry {index} in '{}': {err}",
                path.display()
            ),
        })?;
        if file.name().ends_with('/') {
            continue;
        }
        let name = file.name().to_string();
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_ok() && guess_image_format_from_prefix(&bytes).is_some()
        {
            names.push(name);
        }
    }
    let picked = pick_archive_images(names);
    let total = picked.len();
    let mut images = Vec::new();
    for (index, name) in picked.into_iter().enumerate() {
        let _ = progress_tx.send(SourceLoadWorkerEvent::Progress {
            stage: "decode",
            current: index,
            total,
        });
        let mut file = archive.by_name(&name).map_err(|err| SourceLoadError {
            user_message: "Не удалось прочитать ZIP архив.".to_string(),
            log_message: format!(
                "failed to open zip entry '{}' in '{}': {err}",
                name,
                path.display()
            ),
        })?;
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_ok()
            && let Some(image) = decode_image_bytes(&bytes, &name)
        {
            images.push(image);
        }
    }
    let _ = progress_tx.send(SourceLoadWorkerEvent::Progress {
        stage: "decode",
        current: total,
        total,
    });
    Ok(images)
}

fn load_images_from_tar(
    path: &Path,
    progress_tx: &Sender<SourceLoadWorkerEvent>,
) -> Result<Vec<ImportedImage>, SourceLoadError> {
    let file = File::open(path).map_err(|err| SourceLoadError {
        user_message: "Не удалось открыть TAR архив.".to_string(),
        log_message: format!("failed to open tar '{}': {err}", path.display()),
    })?;
    let ext = lowercase_path(path);
    let archive_reader: Box<dyn Read> = if ext.ends_with(".tar.gz") || ext.ends_with(".tgz") {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = tar::Archive::new(archive_reader);
    let entries = archive.entries().map_err(|err| SourceLoadError {
        user_message: "Не удалось прочитать TAR архив.".to_string(),
        log_message: format!("failed to list tar entries in '{}': {err}", path.display()),
    })?;

    let mut records = Vec::new();
    for entry in entries {
        let mut entry = entry.map_err(|err| SourceLoadError {
            user_message: "Не удалось прочитать TAR архив.".to_string(),
            log_message: format!("failed to read tar entry in '{}': {err}", path.display()),
        })?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let entry_path = entry.path().map_err(|err| SourceLoadError {
            user_message: "Не удалось прочитать TAR архив.".to_string(),
            log_message: format!("failed to get tar path in '{}': {err}", path.display()),
        })?;
        let name = entry_path.to_string_lossy().to_string();
        let mut bytes = Vec::new();
        if entry.read_to_end(&mut bytes).is_ok() && guess_image_format_from_prefix(&bytes).is_some()
        {
            records.push((name, bytes));
        }
    }

    let picked = pick_archive_images(records.iter().map(|(name, _)| name.clone()).collect());
    let picked_set: HashSet<_> = picked.into_iter().collect();
    let mut images = Vec::new();
    let total = picked_set.len();
    let mut decoded = 0usize;
    for (name, bytes) in records {
        if picked_set.contains(&name) {
            let _ = progress_tx.send(SourceLoadWorkerEvent::Progress {
                stage: "decode",
                current: decoded,
                total,
            });
            if let Some(image) = decode_image_bytes(&bytes, &name) {
                images.push(image);
            }
            decoded += 1;
        }
    }
    let _ = progress_tx.send(SourceLoadWorkerEvent::Progress {
        stage: "decode",
        current: decoded,
        total,
    });
    images.sort_by(|left, right| compare_import_names(&left.name, &right.name));
    Ok(images)
}

fn load_images_from_external_archive(
    path: &Path,
    commands: &[&str],
    progress_tx: &Sender<SourceLoadWorkerEvent>,
) -> Result<Vec<ImportedImage>, SourceLoadError> {
    let temp_dir = create_temp_extract_dir()?;
    let extract_result = extract_archive_with_commands(path, &temp_dir, commands);
    let _ = progress_tx.send(SourceLoadWorkerEvent::Progress {
        stage: "archive",
        current: 1,
        total: 1,
    });
    let images = match extract_result {
        Ok(()) => load_images_from_extracted_dir(&temp_dir, progress_tx),
        Err(err) => {
            let _ = fs::remove_dir_all(&temp_dir);
            return Err(err);
        }
    };
    if let Err(err) = fs::remove_dir_all(&temp_dir) {
        crate::runtime_log::log_warn(format!(
            "[new-project] failed to remove temp dir '{}': {err}",
            temp_dir.display()
        ));
    }
    images
}

fn create_temp_extract_dir() -> Result<PathBuf, SourceLoadError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!(
        "mangafucker_new_project_{}_{}",
        std::process::id(),
        timestamp
    ));
    fs::create_dir_all(&dir).map_err(|err| SourceLoadError {
        user_message: "Не удалось подготовить временную папку для архива.".to_string(),
        log_message: format!("failed to create temp dir '{}': {err}", dir.display()),
    })?;
    Ok(dir)
}

/// Web stub: archive extraction shells out to external tools (`rar`/`7z`/…) via
/// `std::process::Command`, which cannot run in the browser. Returns a clear error.
#[cfg(target_arch = "wasm32")]
fn extract_archive_with_commands(
    path: &Path,
    _output_dir: &Path,
    _commands: &[&str],
) -> Result<(), SourceLoadError> {
    Err(SourceLoadError {
        user_message: "Распаковка архивов недоступна в веб-версии.".to_string(),
        log_message: format!(
            "archive extraction via external tools is unavailable on the web build for '{}'",
            path.display()
        ),
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn extract_archive_with_commands(
    path: &Path,
    output_dir: &Path,
    commands: &[&str],
) -> Result<(), SourceLoadError> {
    for command in commands {
        let status = if *command == "rar" || *command == "unrar" {
            Command::new(command)
                .arg("x")
                .arg("-o+")
                .arg(path)
                .arg(output_dir)
                .status()
        } else if *command == "unar" {
            Command::new(command)
                .arg("-q")
                .arg("-f")
                .arg("-no-recursion")
                .arg("-output-directory")
                .arg(output_dir)
                .arg(path)
                .status()
        } else {
            Command::new(command)
                .arg("x")
                .arg("-y")
                .arg(format!("-o{}", output_dir.display()))
                .arg(path)
                .status()
        };
        match status {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => {
                crate::runtime_log::log_warn(format!(
                    "[new-project] extractor '{}' exited with status {} for '{}'",
                    command,
                    status,
                    path.display()
                ));
            }
            Err(err) => {
                crate::runtime_log::log_warn(format!(
                    "[new-project] failed to run extractor '{}' for '{}': {err}",
                    command,
                    path.display()
                ));
            }
        }
    }

    Err(SourceLoadError {
        user_message:
            "Не удалось распаковать архив. Нужен совместимый `rar`, `unrar`, `unar`, `7z` или `7za`."
                .to_string(),
        log_message: format!("no extractor succeeded for '{}'", path.display()),
    })
}

fn load_images_from_extracted_dir(
    root_dir: &Path,
    progress_tx: &Sender<SourceLoadWorkerEvent>,
) -> Result<Vec<ImportedImage>, SourceLoadError> {
    let mut images = list_images_sorted(root_dir)?;
    if images.is_empty() {
        let mut subdirs = fs::read_dir(root_dir)
            .map_err(|err| SourceLoadError {
                user_message: "Не удалось прочитать распакованный архив.".to_string(),
                log_message: format!("read_dir failed for '{}': {err}", root_dir.display()),
            })?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        subdirs.sort();
        if subdirs.len() > 1 {
            crate::runtime_log::log_warn(format!(
                "[new-project] extracted archive '{}' appears to contain multiple chapters",
                root_dir.display()
            ));
        }
        if let Some(first) = subdirs.first() {
            images = list_images_sorted(first)?;
            if images.is_empty() {
                images = collect_images_recursive(first)?;
            }
        }
    }
    let (decoded, _skipped) = decode_images_from_paths(images, progress_tx);
    Ok(decoded)
}

fn collect_images_recursive(root: &Path) -> Result<Vec<PathBuf>, SourceLoadError> {
    let mut results = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let entries = fs::read_dir(&path).map_err(|err| SourceLoadError {
            user_message: "Не удалось прочитать распакованный архив.".to_string(),
            log_message: format!("read_dir failed for '{}': {err}", path.display()),
        })?;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    crate::runtime_log::log_warn(format!(
                        "[new-project] failed to read entry in '{}': {err}",
                        path.display()
                    ));
                    continue;
                }
            };
            let entry_path = entry.path();
            if entry_path.is_dir() {
                stack.push(entry_path);
            } else if is_decodable_image_file(&entry_path) {
                results.push(entry_path);
            }
        }
    }
    results.sort_by(|left, right| compare_import_paths(left, right));
    Ok(results)
}

fn list_images_sorted(folder: &Path) -> Result<Vec<PathBuf>, SourceLoadError> {
    let entries = fs::read_dir(folder).map_err(|err| SourceLoadError {
        user_message: "Не удалось прочитать папку с изображениями.".to_string(),
        log_message: format!("read_dir failed for '{}': {err}", folder.display()),
    })?;
    let mut images = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                crate::runtime_log::log_warn(format!(
                    "[new-project] failed to read entry in '{}': {err}",
                    folder.display()
                ));
                continue;
            }
        };
        let path = entry.path();
        if path.is_file() && is_decodable_image_file(&path) {
            images.push(path);
        }
    }
    images.sort_by(|left, right| compare_import_paths(left, right));
    Ok(images)
}

fn decode_image_bytes(bytes: &[u8], name: &str) -> Option<ImportedImage> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let image = reader.decode().ok()?;
    Some(ImportedImage {
        name: Path::new(name)
            .file_name()
            .and_then(|file_name| file_name.to_str())
            .unwrap_or(name)
            .to_string(),
        image,
    })
}

fn pick_archive_images(paths: Vec<String>) -> Vec<String> {
    let mut cleaned = paths
        .into_iter()
        .map(|path| path.replace('\\', "/").trim_start_matches("./").to_string())
        .collect::<Vec<_>>();
    cleaned.retain(|path| {
        Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| !name.is_empty())
    });
    if cleaned.is_empty() {
        return Vec::new();
    }

    let mut top_dirs = cleaned
        .iter()
        .filter_map(|path| path.split_once('/').map(|(prefix, _)| prefix.to_string()))
        .collect::<Vec<_>>();
    top_dirs.sort();
    top_dirs.dedup();
    if top_dirs.len() > 1 {
        crate::runtime_log::log_warn(
            "[new-project] archive appears to contain multiple top-level chapters",
        );
    }

    let root_images = cleaned
        .iter()
        .filter(|path| !path.contains('/'))
        .cloned()
        .collect::<Vec<_>>();
    if !root_images.is_empty() {
        return sort_archive_paths(root_images);
    }

    if let Some(base) = top_dirs.first() {
        let mut direct = cleaned
            .iter()
            .filter(|path| path.starts_with(&format!("{base}/")) && path.matches('/').count() == 1)
            .cloned()
            .collect::<Vec<_>>();
        if direct.is_empty() {
            direct = cleaned
                .iter()
                .filter(|path| path.starts_with(&format!("{base}/")))
                .cloned()
                .collect::<Vec<_>>();
        }
        return sort_archive_paths(direct);
    }

    sort_archive_paths(cleaned)
}

fn sort_archive_paths(mut paths: Vec<String>) -> Vec<String> {
    paths.sort_by(|left, right| compare_import_names(left, right));
    paths
}

fn apply_width_filter_with_fallback(
    images: &mut Vec<ImportedImage>,
) -> (usize, Option<(usize, usize, usize)>) {
    if images.len() < 3 {
        return (0, None);
    }
    let original = std::mem::take(images);
    let original_len = original.len();
    let mut widths = original
        .iter()
        .map(|image| image.image.width() as usize)
        .collect::<Vec<_>>();
    widths.sort_unstable();
    let median = widths[widths.len() / 2];
    let min_width = median / 2;
    let max_width = median + (median / 2);
    let mut filtered = Vec::with_capacity(original_len);
    let mut removed_images = Vec::new();
    for image in original {
        let width = image.image.width() as usize;
        if width >= min_width && width <= max_width {
            filtered.push(image);
        } else {
            removed_images.push(image);
        }
    }
    if filtered.is_empty() {
        *images = removed_images;
        return (0, None);
    }
    let removed = original_len.saturating_sub(filtered.len());
    *images = filtered;
    (removed, Some((median, min_width, max_width)))
}

fn archive_kind(path: &Path) -> Option<String> {
    let lowercase = lowercase_path(path);
    if lowercase.ends_with(".tar.gz") || lowercase.ends_with(".tgz") || lowercase.ends_with(".tar")
    {
        return Some("tar".to_string());
    }
    if lowercase.ends_with(".zip") {
        return Some("zip".to_string());
    }
    if lowercase.ends_with(".rar") {
        return Some("rar".to_string());
    }
    if lowercase.ends_with(".7z") {
        return Some("7z".to_string());
    }
    None
}

fn file_name_or_display(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn lowercase_path(path: &Path) -> String {
    path.to_string_lossy().to_ascii_lowercase()
}

fn parse_resource_like_name(stem: &str) -> Option<usize> {
    let lowercase = stem.to_ascii_lowercase();
    let rest = if let Some(rest) = lowercase.strip_prefix("resource") {
        rest
    } else {
        // Tolerate the common "resouce" misspelling; `?` returns None when neither prefix matches.
        lowercase.strip_prefix("resouce")?
    };
    if rest.is_empty() {
        return Some(0);
    }
    if rest.starts_with('(') && rest.ends_with(')') {
        return rest[1..rest.len() - 1].parse::<usize>().ok();
    }
    None
}

fn compare_import_paths(left: &Path, right: &Path) -> Ordering {
    let left_name = left
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let right_name = right
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    compare_import_names(left_name, right_name)
}

fn compare_import_names(left: &str, right: &str) -> Ordering {
    match (python_sort_key(left), python_sort_key(right)) {
        (Some(left_key), Some(right_key)) => left_key.cmp(&right_key),
        _ => compare_natural_strings(left, right),
    }
}

fn python_sort_key(path: &str) -> Option<(u64, u8, i64, usize, usize)> {
    let stem = Path::new(path).file_stem()?.to_str()?;
    let (left, right) = stem.split_once('_').unwrap_or((stem, ""));
    let (x_value, x_zeros) = parse_numeric_part(left)?;
    if right.is_empty() {
        return Some((x_value, 0, -1, x_zeros, 0));
    }
    let (y_value, y_zeros) = parse_numeric_part(right)?;
    Some((x_value, 1, y_value as i64, x_zeros, y_zeros))
}

fn parse_numeric_part(input: &str) -> Option<(u64, usize)> {
    let value = input.parse::<u64>().ok()?;
    let stripped = input.trim_start_matches('0');
    let zeros = if stripped.is_empty() {
        input.len().saturating_sub(1)
    } else {
        input.len().saturating_sub(stripped.len())
    };
    Some((value, zeros))
}

fn compare_natural_strings(left: &str, right: &str) -> Ordering {
    let left_parts = tokenize_for_natural_sort(left);
    let right_parts = tokenize_for_natural_sort(right);
    for (left_part, right_part) in left_parts.iter().zip(right_parts.iter()) {
        let order = match (left_part, right_part) {
            (SortToken::Number(left_number), SortToken::Number(right_number)) => {
                left_number.cmp(right_number)
            }
            (SortToken::Text(left_text), SortToken::Text(right_text)) => left_text.cmp(right_text),
            (SortToken::Number(_), SortToken::Text(_)) => Ordering::Less,
            (SortToken::Text(_), SortToken::Number(_)) => Ordering::Greater,
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    left_parts.len().cmp(&right_parts.len())
}

#[derive(Debug, PartialEq, Eq)]
enum SortToken {
    Number(u64),
    Text(String),
}

fn tokenize_for_natural_sort(input: &str) -> Vec<SortToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_is_digit = None;
    for character in input.chars() {
        let is_digit = character.is_ascii_digit();
        match current_is_digit {
            Some(flag) if flag == is_digit => current.push(character.to_ascii_lowercase()),
            Some(flag) => {
                push_sort_token(&mut tokens, &current, flag);
                current.clear();
                current.push(character.to_ascii_lowercase());
                current_is_digit = Some(is_digit);
            }
            None => {
                current.push(character.to_ascii_lowercase());
                current_is_digit = Some(is_digit);
            }
        }
    }
    if let Some(flag) = current_is_digit {
        push_sort_token(&mut tokens, &current, flag);
    }
    tokens
}

fn push_sort_token(tokens: &mut Vec<SortToken>, current: &str, is_digit: bool) {
    if is_digit {
        match current.parse::<u64>() {
            Ok(value) => tokens.push(SortToken::Number(value)),
            Err(_) => tokens.push(SortToken::Text(current.to_string())),
        }
    } else {
        tokens.push(SortToken::Text(current.to_string()));
    }
}

fn parse_pattern_list(patterns: &str) -> Vec<String> {
    patterns
        .split(|character: char| character == ',' || character == '|' || character.is_whitespace())
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn wildcard_matches_any(file_name: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| wildcard_match(pattern, file_name))
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    wildcard_match_bytes(pattern.as_bytes(), value.as_bytes())
}

fn wildcard_match_bytes(pattern: &[u8], value: &[u8]) -> bool {
    let mut pattern_index = 0usize;
    let mut value_index = 0usize;
    let mut star_backtrack = None;
    while value_index < value.len() {
        if pattern_index < pattern.len() {
            match pattern[pattern_index] {
                b'?' => {
                    pattern_index += 1;
                    value_index += 1;
                    continue;
                }
                b'*' => {
                    star_backtrack = Some((pattern_index, value_index));
                    pattern_index += 1;
                    continue;
                }
                b'[' => {
                    if let Some((matched, consumed)) =
                        match_char_class(&pattern[pattern_index..], value[value_index] as char)
                        && matched
                    {
                        pattern_index += consumed;
                        value_index += 1;
                        continue;
                    }
                }
                byte if (byte as char).eq_ignore_ascii_case(&(value[value_index] as char)) => {
                    pattern_index += 1;
                    value_index += 1;
                    continue;
                }
                _ => {}
            }
        }
        if let Some((star_index, matched_value_index)) = star_backtrack {
            pattern_index = star_index + 1;
            value_index = matched_value_index + 1;
            star_backtrack = Some((star_index, matched_value_index + 1));
            continue;
        }
        return false;
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn match_char_class(pattern: &[u8], character: char) -> Option<(bool, usize)> {
    if pattern.first().copied()? != b'[' {
        return None;
    }
    let mut index = 1usize;
    let mut matched = false;
    let mut negated = false;
    if pattern.get(index).copied() == Some(b'!') || pattern.get(index).copied() == Some(b'^') {
        negated = true;
        index += 1;
    }
    let character = character.to_ascii_lowercase();
    while index < pattern.len() {
        match pattern[index] {
            b']' => return Some(((matched && !negated) || (!matched && negated), index + 1)),
            start
                if index + 2 < pattern.len()
                    && pattern[index + 1] == b'-'
                    && pattern[index + 2] != b']' =>
            {
                let start = (start as char).to_ascii_lowercase();
                let end = (pattern[index + 2] as char).to_ascii_lowercase();
                if start <= character && character <= end {
                    matched = true;
                }
                index += 3;
            }
            byte => {
                if (byte as char).to_ascii_lowercase() == character {
                    matched = true;
                }
                index += 1;
            }
        }
    }
    None
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            output.push((high << 4) | low);
            index += 3;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

struct HtmlTag<'a> {
    name: &'a str,
    attrs: &'a str,
    is_end: bool,
}

fn extract_html_tags(html: &str) -> Vec<HtmlTag<'_>> {
    let mut tags = Vec::new();
    let mut cursor = 0usize;
    while let Some(start_offset) = html[cursor..].find('<') {
        let start = cursor + start_offset;
        let Some(end_offset) = html[start..].find('>') else {
            break;
        };
        let end = start + end_offset;
        let raw = &html[start + 1..end];
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('!') || trimmed.starts_with('?') {
            cursor = end + 1;
            continue;
        }
        let is_end = trimmed.starts_with('/');
        let content = if is_end {
            trimmed[1..].trim_start()
        } else {
            trimmed
        };
        let mut parts = content.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or_default();
        let attrs = parts.next().unwrap_or_default();
        if !name.is_empty() {
            tags.push(HtmlTag {
                name,
                attrs,
                is_end,
            });
        }
        cursor = end + 1;
    }
    tags
}

fn get_html_attr<'a>(attrs: &'a str, attr_name: &str) -> Option<&'a str> {
    let bytes = attrs.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let name_start = index;
        while index < bytes.len() && !bytes[index].is_ascii_whitespace() && bytes[index] != b'=' {
            index += 1;
        }
        let name = &attrs[name_start..index];
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let value = if index < bytes.len() && (bytes[index] == b'"' || bytes[index] == b'\'') {
            let quote = bytes[index];
            index += 1;
            let value_start = index;
            while index < bytes.len() && bytes[index] != quote {
                index += 1;
            }
            let value = &attrs[value_start..index];
            if index < bytes.len() {
                index += 1;
            }
            value
        } else {
            let value_start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            &attrs[value_start..index]
        };
        if name.eq_ignore_ascii_case(attr_name) {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    use std::error::Error;

    #[test]
    fn decode_image_path_uses_signature_when_extension_is_wrong() -> Result<(), Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!(
            "manhwastudio_signature_decode_{}.bin",
            std::process::id()
        ));
        let image = image::RgbaImage::from_pixel(1, 1, Rgba([255u8, 0, 0, 255]));
        image.save_with_format(&path, ImageFormat::Png)?;

        let decode_result = decode_image_path(&path);
        fs::remove_file(&path)?;
        let decoded = decode_result.map_err(std::io::Error::other)?;

        assert_eq!(decoded.width(), 1);
        assert_eq!(decoded.height(), 1);
        Ok(())
    }

    #[test]
    fn load_from_file_prefers_image_signature_over_archive_extension() -> Result<(), Box<dyn Error>>
    {
        let path = std::env::temp_dir().join(format!(
            "manhwastudio_signature_source_{}.zip",
            std::process::id()
        ));
        let image = image::RgbaImage::from_pixel(1, 1, Rgba([0u8, 255, 0, 255]));
        image.save_with_format(&path, ImageFormat::Png)?;
        let (tx, _rx) = mpsc::channel();

        let load_result = load_from_file(&path, &tx);
        fs::remove_file(&path)?;
        let batch = load_result.map_err(|err| std::io::Error::other(err.log_message))?;

        assert_eq!(batch.images.len(), 1);
        assert_eq!(batch.images[0].image.width(), 1);
        Ok(())
    }

    #[test]
    fn non_image_prefix_has_no_image_format() {
        assert_eq!(guess_image_format_from_prefix(b"not an image"), None);
    }
}
