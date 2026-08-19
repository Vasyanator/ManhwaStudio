/*
File: src/launcher/new_project/quick_download/controller.rs

Purpose:
UI-facing controller and worker thread of the quick downloader: it starts one background
download per URL, streams progress back to the launcher window, and converts the decoded
images into ribbon pages.

Main responsibilities:
- own the worker channel and expose a non-blocking `begin_download` / `poll` pair;
- run URL normalization, plan building and image download off the GUI thread;
- download images in parallel while preserving the plan order.

Key structures:
- QuickDownloadController
- QuickDownloadEvent
- QuickDownloadSuccess

Key functions:
- spawn_quick_download()
- load_quick_download()
- download_images_ordered()

Notes:
Nothing here knows about a specific site; host handling lives in `plan.rs` and `sites/`.
*/

use super::http::{fetch_bytes, install_on_download_pool};
use super::plan::{QuickDownloadError, SiteDownloadPlan, build_site_download_plan};
use super::url_util::normalize_http_url;
use crate::launcher::new_project::ribbon::{ImportedImage, RibbonPage, build_ribbon_pages};
use image::DynamicImage;
use rayon::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use ms_thread as thread;

/// Handle of the single in-flight download: the receiving end of the worker channel.
#[derive(Debug)]
struct PendingQuickDownload {
    rx: Receiver<QuickDownloadWorkerEvent>,
}

/// Launcher-side state of the quick downloader. Holds at most one running download and
/// never blocks the GUI thread.
pub struct QuickDownloadController {
    pending: Option<PendingQuickDownload>,
}

/// Result of a finished download: the normalized source URL and the built ribbon pages.
pub struct QuickDownloadSuccess {
    pub source_url: String,
    pub pages: Vec<RibbonPage>,
    pub downloaded_images: usize,
}

/// UI-facing event drained by `QuickDownloadController::poll`.
pub enum QuickDownloadEvent {
    Progress {
        stage: String,
        current: usize,
        total: usize,
    },
    Loaded(QuickDownloadSuccess),
    Failed {
        user_message: String,
        log_message: String,
    },
    WorkerDisconnected,
}

/// Internal worker-to-UI message; converted into `QuickDownloadEvent` while polling.
enum QuickDownloadWorkerEvent {
    Progress {
        stage: &'static str,
        current: usize,
        total: usize,
    },
    Finished(Result<LoadedQuickDownload, QuickDownloadError>),
}

/// Worker-side payload of a successful download, mirrored into `QuickDownloadSuccess`.
struct LoadedQuickDownload {
    source_url: String,
    pages: Vec<RibbonPage>,
    downloaded_images: usize,
}

impl QuickDownloadController {
    /// Creates an idle controller with no pending download.
    pub fn new() -> Self {
        Self { pending: None }
    }

    /// Returns `true` while a download worker is running.
    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    /// Starts a download for `url`, replacing any previously tracked worker handle.
    pub fn begin_download(&mut self, url: String) {
        self.pending = Some(PendingQuickDownload {
            rx: spawn_quick_download(url),
        });
    }

    /// Drains the worker channel without blocking; returns the terminal event, or the
    /// last progress update seen in this frame, or `None` when nothing changed.
    pub fn poll(&mut self, ctx: &egui::Context) -> Option<QuickDownloadEvent> {
        let pending = self.pending.take()?;
        let mut last_progress = None;
        loop {
            match pending.rx.try_recv() {
                Ok(QuickDownloadWorkerEvent::Progress {
                    stage,
                    current,
                    total,
                }) => {
                    ctx.request_repaint();
                    last_progress = Some(QuickDownloadEvent::Progress {
                        stage: stage.to_string(),
                        current,
                        total,
                    });
                }
                Ok(QuickDownloadWorkerEvent::Finished(result)) => match result {
                    Ok(success) => {
                        ctx.request_repaint();
                        return Some(QuickDownloadEvent::Loaded(QuickDownloadSuccess {
                            source_url: success.source_url,
                            pages: success.pages,
                            downloaded_images: success.downloaded_images,
                        }));
                    }
                    Err(err) => {
                        return Some(QuickDownloadEvent::Failed {
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
                    return Some(QuickDownloadEvent::WorkerDisconnected);
                }
            }
        }
    }
}

/// Spawns the download worker thread and returns the receiving end of its event channel.
/// A spawn failure is reported through the same channel instead of panicking.
fn spawn_quick_download(url: String) -> Receiver<QuickDownloadWorkerEvent> {
    let (tx, rx) = mpsc::channel();
    let tx_worker = tx.clone();
    let url_for_thread = url.clone();
    match thread::Builder::new()
        .name("new-project-quick-download".to_string())
        .spawn(move || {
            let result = load_quick_download(&url_for_thread, &tx_worker);
            if tx_worker
                .send(QuickDownloadWorkerEvent::Finished(result))
                .is_err()
            {
                crate::runtime_log::log_warn(
                    "[new-project] failed to send quick download result to UI",
                );
            }
        }) {
        Ok(_) => {}
        Err(err) => {
            crate::runtime_log::log_error(format!(
                "[new-project] failed to spawn quick downloader for '{url}': {err}"
            ));
            if tx
                .send(QuickDownloadWorkerEvent::Finished(Err(
                    QuickDownloadError {
                        user_message: t!("launcher.new_project.quick_dl.start_error").to_string(),
                        log_message: format!("failed to spawn quick downloader for '{url}': {err}"),
                    },
                )))
                .is_err()
            {
                crate::runtime_log::log_warn(
                    "[new-project] failed to deliver quick downloader spawn error",
                );
            }
        }
    }
    rx
}

/// Worker body: normalizes the URL, resolves the site plan, downloads every image and
/// builds ribbon pages.
///
/// # Errors
/// Returns `QuickDownloadError` for an invalid URL, an unsupported/failed site plan, an
/// empty image list, or any download/decode failure.
fn load_quick_download(
    url: &str,
    progress_tx: &Sender<QuickDownloadWorkerEvent>,
) -> Result<LoadedQuickDownload, QuickDownloadError> {
    let normalized = normalize_http_url(url).map_err(|err| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.invalid_url_error").to_string(),
        log_message: format!("invalid quick download url '{url}': {err}"),
    })?;
    let plan = build_site_download_plan(&normalized)?;
    if plan.image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!("quick downloader found zero images for '{normalized}'"),
        });
    }
    let images = download_images_ordered(&plan, progress_tx)?;
    let pages = build_ribbon_pages(images);
    Ok(LoadedQuickDownload {
        source_url: normalized,
        downloaded_images: pages.len(),
        pages,
    })
}

/// Downloads every image of `plan` in parallel and returns them in plan order.
///
/// The work runs on the downloader's own thread pool (`install_on_download_pool`), so the
/// concurrency is the network fan-out rather than the core count and the global rayon pool
/// stays free for compute work. Progress is streamed through `progress_tx` as images
/// complete, so the reported order is arbitrary while the returned vector is sorted back by
/// index.
///
/// # Errors
/// Returns the first download or decode error, or a pool-creation failure; no partial
/// result is produced.
fn download_images_ordered(
    plan: &SiteDownloadPlan,
    progress_tx: &Sender<QuickDownloadWorkerEvent>,
) -> Result<Vec<ImportedImage>, QuickDownloadError> {
    let total = plan.image_urls.len();
    let downloaded = Arc::new(AtomicUsize::new(0));
    let referer = plan.referer.clone();
    let progress_tx = progress_tx.clone();

    // One image per rayon task (`with_max_len(1)`): the default chunking would hand several
    // URLs to one worker and leave the rest of the pool idle on a short chapter.
    let downloads = install_on_download_pool(|| {
        plan.image_urls
            .par_iter()
            .enumerate()
            .with_max_len(1)
            .map(|(index, url)| {
                let image = download_image(url, referer.as_deref())?;
                let current = downloaded.fetch_add(1, Ordering::Relaxed) + 1;
                let _ = progress_tx.send(QuickDownloadWorkerEvent::Progress {
                    stage: "download",
                    current,
                    total,
                });
                Ok::<(usize, ImportedImage), QuickDownloadError>((
                    index,
                    ImportedImage {
                        name: format!("{:04}.png", index + 1),
                        image,
                    },
                ))
            })
            .collect::<Result<Vec<_>, _>>()
    })?;

    let mut indexed = downloads?;
    indexed.sort_by_key(|(index, _)| *index);
    Ok(indexed.into_iter().map(|(_, image)| image).collect())
}

/// Fetches one image and decodes it from its bytes (never from the URL extension).
///
/// # Errors
/// Returns `QuickDownloadError` on a transport/HTTP failure or when the bytes are not a
/// decodable image.
fn download_image(url: &str, referer: Option<&str>) -> Result<DynamicImage, QuickDownloadError> {
    let bytes = fetch_bytes(url, referer)?;
    image::load_from_memory(&bytes).map_err(|err| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.decode_image_error").to_string(),
        log_message: format!("failed to decode downloaded image '{url}': {err}"),
    })
}
