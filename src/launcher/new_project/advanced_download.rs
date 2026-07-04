/*
File: src/launcher/new_project/advanced_download.rs

Purpose:
Background bridge for the advanced browser downloader in the New Project launcher.

Main responsibilities:
- keep all browser-profile work outside the GUI thread via a Python helper daemon;
- open a selected Selenium browser or the CloakBrowser persistent profile;
- compare the helper startup version with the Rust Studio version;
- fetch image URLs from the active Selenium page, download them, convert them into ribbon pages,
  and run canvas snapshot/capture flows via the same helper protocol.

Key structures:
- AdvancedDownloadController
- AdvancedDownloadEvent
- AdvancedDownloadSuccess

Notes:
The actual Selenium/CloakBrowser runtime stays in Python because the project already ships
browser/profile helpers there. Rust owns the UI state, process lifecycle, progress streaming, and
ribbon conversion.
*/

use crate::backend_ipc::{self, CallError};
use crate::launcher::new_project::ribbon::{ImportedImage, RibbonPage, build_ribbon_pages};
use egui::ColorImage;
use image::{DynamicImage, GenericImageView};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use ms_thread as thread;
use web_time::Duration;

/// Per-frame timeout for a browser IPC command: if no progress frame or terminal
/// arrives within this window the call is abandoned (treated as a dead backend).
/// Generous because some stages (page load, canvas save) are silent for a while.
const BROWSER_COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
/// How long to wait for the backend process + socket to come up when a browser
/// command is issued while the backend is not yet running.
const BACKEND_START_WAIT: Duration = Duration::from_secs(20);

const DEFAULT_LINK_PREFIX: &str = "https://page-edge.kakao.com/sdownload/resource*";

#[derive(Debug)]
struct PendingAdvancedDownload {
    blocks_ui: bool,
    rx: Receiver<AdvancedDownloadWorkerEvent>,
    cancel_file: Option<PathBuf>,
}

pub struct AdvancedDownloadController {
    daemon: Arc<Mutex<Option<PythonDaemon>>>,
    pending: Option<PendingAdvancedDownload>,
    available_browsers: Vec<String>,
    backend: AdvancedBrowserBackend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvancedBrowserBackend {
    Selenium,
    Cloak,
}

impl AdvancedBrowserBackend {
    pub const ALL: [Self; 2] = [Self::Selenium, Self::Cloak];

    pub fn label(self) -> &'static str {
        match self {
            Self::Selenium => "Selenium",
            Self::Cloak => "CloakBrowser",
        }
    }

    /// Backend identifier sent to the unified backend's `browser.set_backend`.
    fn ipc_backend_name(self) -> &'static str {
        match self {
            Self::Selenium => "selenium",
            Self::Cloak => "cloak",
        }
    }

    fn browser_name(self, selected_browser: &str) -> String {
        match self {
            Self::Selenium => selected_browser.to_string(),
            Self::Cloak => "CloakBrowser".to_string(),
        }
    }
}

pub struct AdvancedDownloadSuccess {
    pub source_url: String,
    pub pages: Vec<RibbonPage>,
    pub downloaded_images: usize,
}

pub struct AdvancedAutoCandidateSet {
    pub source_url: String,
    pub items: Vec<AdvancedAutoCandidate>,
    pub groups: Vec<AdvancedAutoGroup>,
}

pub struct AdvancedAutoCandidate {
    pub id: usize,
    pub order_index: usize,
    pub group_id: usize,
    pub url: String,
    pub name: String,
    pub width: usize,
    pub height: usize,
    pub image: DynamicImage,
    pub thumbnail: ColorImage,
    /// Set by the helper when the candidate is a size-outlier (icon, sprite, UI
    /// chrome) rather than a manga page; the review UI deselects these by default.
    pub probable_junk: bool,
}

pub struct AdvancedAutoGroup {
    pub id: usize,
    pub signature: String,
    pub item_ids: Vec<usize>,
}

/// Live breakdown of what the (deep) intercept has collected so far. `total` is the
/// raw captured-payload count; `canvases` counts canvas-element captures and `images`
/// counts everything else (plain `<img>`, network bytes, blob/descramble exports).
/// Plain canvas intercept only fills `total`.
#[derive(Debug, Clone, Copy, Default)]
pub struct InterceptCounts {
    pub total: usize,
    pub canvases: usize,
    pub images: usize,
}

pub enum AdvancedDownloadEvent {
    VersionMismatch {
        studio_version: String,
        downloader_version: String,
    },
    Progress {
        stage: String,
        current: usize,
        total: usize,
    },
    BrowserOpened {
        current_url: String,
    },
    LinkCollectStarted {
        current_url: String,
    },
    LinkCollectCountUpdated {
        found_links: usize,
    },
    InterceptStarted {
        current_url: String,
    },
    InterceptCountUpdated {
        counts: InterceptCounts,
    },
    Loaded(AdvancedDownloadSuccess),
    AutoCandidatesReady(AdvancedAutoCandidateSet),
    Failed {
        user_message: String,
        log_message: String,
    },
    WorkerDisconnected,
}

enum AdvancedDownloadWorkerEvent {
    VersionMismatch {
        studio_version: String,
        downloader_version: String,
    },
    Progress {
        stage: &'static str,
        current: usize,
        total: usize,
    },
    InterceptCountUpdated {
        counts: InterceptCounts,
    },
    LinkCollectCountUpdated {
        found_links: usize,
    },
    Finished(Result<AdvancedWorkerOutcome, AdvancedDownloadError>),
}

enum AdvancedWorkerOutcome {
    BrowserOpened { current_url: String },
    LinkCollectStarted { current_url: String },
    LinkCollectCountUpdated { found_links: usize },
    InterceptStarted { current_url: String },
    InterceptCountUpdated { counts: InterceptCounts },
    Loaded(LoadedAdvancedDownload),
    AutoCandidatesReady(AdvancedAutoCandidateSet),
}

struct LoadedAdvancedDownload {
    source_url: String,
    pages: Vec<RibbonPage>,
    downloaded_images: usize,
}

#[derive(Debug)]
struct AutoDownloadedItem {
    order_index: usize,
    url: String,
    file_name: String,
    probable_junk: bool,
}

#[derive(Debug)]
struct AdvancedDownloadError {
    user_message: String,
    log_message: String,
}

/// Adapter that speaks the legacy line-JSON daemon protocol (`write_command` +
/// `read_payload`) on top of the unified AI backend's framed IPC. The browser
/// session now lives inside `ai_backend.py` (method `browser.command`); this type
/// keeps the per-command read loops in this file unchanged by translating each
/// command into one streaming IPC call whose progress frames and terminal event
/// are surfaced as the same JSON payloads the old stdio daemon produced.
struct PythonDaemon {
    backend: AdvancedBrowserBackend,
    client: backend_ipc::BackendClient,
    /// Frame stream for the in-flight command: zero or more `progress` payloads
    /// followed by exactly one terminal payload (`result` / `opened` / ... / `error`).
    frames: Option<Receiver<Value>>,
}

enum AdvancedCommand {
    OpenUrl {
        browser: String,
        url: String,
    },
    Fetch {
        browser: String,
        pattern: String,
        max_parallel: usize,
    },
    FetchAuto {
        browser: String,
        max_parallel: usize,
        cancel_file: PathBuf,
    },
    StartLinkCollect {
        browser: String,
        pattern: String,
        max_parallel: usize,
    },
    StartAutoLinkCollect {
        browser: String,
        max_parallel: usize,
    },
    QueryLinkCollectCount {
        browser: String,
    },
    StopLinkCollect {
        browser: String,
    },
    StopAutoLinkCollect {
        browser: String,
        cancel_file: PathBuf,
    },
    FetchCanvas {
        browser: String,
    },
    StartCanvasIntercept {
        browser: String,
    },
    QueryCanvasInterceptCount {
        browser: String,
    },
    StopCanvasIntercept {
        browser: String,
    },
    StartDeepIntercept {
        browser: String,
    },
    QueryDeepInterceptCount {
        browser: String,
    },
    StopDeepIntercept {
        browser: String,
        cancel_file: PathBuf,
    },
}

impl AdvancedDownloadController {
    pub fn new() -> Self {
        Self {
            daemon: Arc::new(Mutex::new(None)),
            pending: None,
            available_browsers: detect_available_browsers(),
            backend: AdvancedBrowserBackend::Selenium,
        }
    }

    pub fn default_link_prefix() -> &'static str {
        DEFAULT_LINK_PREFIX
    }

    pub fn available_browsers(&self) -> &[String] {
        &self.available_browsers
    }

    pub fn backend(&self) -> AdvancedBrowserBackend {
        self.backend
    }

    pub fn set_backend(&mut self, backend: AdvancedBrowserBackend) {
        if self.backend == backend {
            return;
        }
        self.shutdown_daemon();
        self.backend = backend;
    }

    pub fn browser_name_for_backend(&self, selected_browser: &str) -> String {
        self.backend.browser_name(selected_browser)
    }

    pub fn is_loading(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.blocks_ui)
    }

    pub fn has_pending_command(&self) -> bool {
        self.pending.is_some()
    }

    pub fn can_cancel_current_auto_fetch(&self) -> bool {
        self.pending
            .as_ref()
            .and_then(|pending| pending.cancel_file.as_ref())
            .is_some_and(|path| !path.is_file())
    }

    pub fn request_cancel_current_auto_fetch(&self) -> Result<(), String> {
        let cancel_file = self
            .pending
            .as_ref()
            .and_then(|pending| pending.cancel_file.as_ref())
            .ok_or_else(|| "auto fetch cancellation is not available".to_string())?;
        fs::write(cancel_file, b"cancel")
            .map_err(|err| format!("write cancel marker '{}': {err}", cancel_file.display()))
    }

    fn shutdown_daemon(&mut self) {
        let lock = self.daemon.lock();
        let mut guard = match lock {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        // The backend process is app-global and stays running; we only close the
        // live browser session it holds (so a backend switch starts a fresh one).
        if let Some(mut daemon) = guard.take()
            && let Err(err) = daemon.write_command(&json!({ "command": "close" }))
        {
            crate::runtime_log::log_warn(format!(
                "[new-project] failed to close advanced downloader browser session: {err}"
            ));
        }
    }

    pub fn begin_open(&mut self, browser: String, url: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            cancel_file: None,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::OpenUrl { browser, url },
            ),
        });
    }

    pub fn begin_fetch(&mut self, browser: String, pattern: String, max_parallel: usize) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            cancel_file: None,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::Fetch {
                    browser,
                    pattern,
                    max_parallel,
                },
            ),
        });
    }

    pub fn begin_fetch_auto(&mut self, browser: String, max_parallel: usize) {
        let cancel_file = advanced_cancel_file_path();
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            cancel_file: Some(cancel_file.clone()),
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::FetchAuto {
                    browser,
                    max_parallel,
                    cancel_file,
                },
            ),
        });
    }

    pub fn begin_start_link_collect(
        &mut self,
        browser: String,
        pattern: String,
        max_parallel: usize,
    ) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            cancel_file: None,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::StartLinkCollect {
                    browser,
                    pattern,
                    max_parallel,
                },
            ),
        });
    }

    pub fn begin_start_auto_link_collect(&mut self, browser: String, max_parallel: usize) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            cancel_file: None,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::StartAutoLinkCollect {
                    browser,
                    max_parallel,
                },
            ),
        });
    }

    pub fn begin_query_link_collect_count(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: false,
            cancel_file: None,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::QueryLinkCollectCount { browser },
            ),
        });
    }

    pub fn begin_stop_link_collect(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            cancel_file: None,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::StopLinkCollect { browser },
            ),
        });
    }

    pub fn begin_stop_auto_link_collect(&mut self, browser: String) {
        let cancel_file = advanced_cancel_file_path();
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            cancel_file: Some(cancel_file.clone()),
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::StopAutoLinkCollect {
                    browser,
                    cancel_file,
                },
            ),
        });
    }

    pub fn begin_fetch_canvas(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            cancel_file: None,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::FetchCanvas { browser },
            ),
        });
    }

    pub fn begin_start_canvas_intercept(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            cancel_file: None,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::StartCanvasIntercept { browser },
            ),
        });
    }

    pub fn begin_stop_canvas_intercept(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            cancel_file: None,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::StopCanvasIntercept { browser },
            ),
        });
    }

    pub fn begin_query_canvas_intercept_count(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: false,
            cancel_file: None,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::QueryCanvasInterceptCount { browser },
            ),
        });
    }

    pub fn begin_start_deep_intercept(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            cancel_file: None,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::StartDeepIntercept { browser },
            ),
        });
    }

    pub fn begin_query_deep_intercept_count(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: false,
            cancel_file: None,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::QueryDeepInterceptCount { browser },
            ),
        });
    }

    pub fn begin_stop_deep_intercept(&mut self, browser: String) {
        let cancel_file = advanced_cancel_file_path();
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            cancel_file: Some(cancel_file.clone()),
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                self.backend,
                AdvancedCommand::StopDeepIntercept {
                    browser,
                    cancel_file,
                },
            ),
        });
    }

    pub fn poll(&mut self, ctx: &egui::Context) -> Option<AdvancedDownloadEvent> {
        let pending = self.pending.take()?;
        let mut last_progress = None;
        loop {
            match pending.rx.try_recv() {
                Ok(AdvancedDownloadWorkerEvent::VersionMismatch {
                    studio_version,
                    downloader_version,
                }) => {
                    self.pending = Some(pending);
                    ctx.request_repaint();
                    return Some(AdvancedDownloadEvent::VersionMismatch {
                        studio_version,
                        downloader_version,
                    });
                }
                Ok(AdvancedDownloadWorkerEvent::Progress {
                    stage,
                    current,
                    total,
                }) => {
                    ctx.request_repaint();
                    last_progress = Some(AdvancedDownloadEvent::Progress {
                        stage: stage.to_string(),
                        current,
                        total,
                    });
                }
                Ok(AdvancedDownloadWorkerEvent::InterceptCountUpdated { counts }) => {
                    ctx.request_repaint();
                    last_progress =
                        Some(AdvancedDownloadEvent::InterceptCountUpdated { counts });
                }
                Ok(AdvancedDownloadWorkerEvent::LinkCollectCountUpdated { found_links }) => {
                    ctx.request_repaint();
                    last_progress =
                        Some(AdvancedDownloadEvent::LinkCollectCountUpdated { found_links });
                }
                Ok(AdvancedDownloadWorkerEvent::Finished(result)) => {
                    cleanup_pending_cancel_file(pending.cancel_file.as_deref());
                    match result {
                        Ok(AdvancedWorkerOutcome::BrowserOpened { current_url }) => {
                            ctx.request_repaint();
                            return Some(AdvancedDownloadEvent::BrowserOpened { current_url });
                        }
                        Ok(AdvancedWorkerOutcome::LinkCollectStarted { current_url }) => {
                            ctx.request_repaint();
                            return Some(AdvancedDownloadEvent::LinkCollectStarted { current_url });
                        }
                        Ok(AdvancedWorkerOutcome::LinkCollectCountUpdated { found_links }) => {
                            ctx.request_repaint();
                            return Some(AdvancedDownloadEvent::LinkCollectCountUpdated {
                                found_links,
                            });
                        }
                        Ok(AdvancedWorkerOutcome::InterceptStarted { current_url }) => {
                            ctx.request_repaint();
                            return Some(AdvancedDownloadEvent::InterceptStarted { current_url });
                        }
                        Ok(AdvancedWorkerOutcome::InterceptCountUpdated { counts }) => {
                            ctx.request_repaint();
                            return Some(AdvancedDownloadEvent::InterceptCountUpdated { counts });
                        }
                        Ok(AdvancedWorkerOutcome::Loaded(success)) => {
                            ctx.request_repaint();
                            return Some(AdvancedDownloadEvent::Loaded(AdvancedDownloadSuccess {
                                source_url: success.source_url,
                                pages: success.pages,
                                downloaded_images: success.downloaded_images,
                            }));
                        }
                        Ok(AdvancedWorkerOutcome::AutoCandidatesReady(candidates)) => {
                            ctx.request_repaint();
                            return Some(AdvancedDownloadEvent::AutoCandidatesReady(candidates));
                        }
                        Err(err) => {
                            return Some(AdvancedDownloadEvent::Failed {
                                user_message: err.user_message,
                                log_message: err.log_message,
                            });
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    self.pending = Some(pending);
                    return last_progress;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Some(AdvancedDownloadEvent::WorkerDisconnected);
                }
            }
        }
    }
}

impl Drop for AdvancedDownloadController {
    fn drop(&mut self) {
        self.shutdown_daemon();
    }
}

fn spawn_advanced_command(
    daemon: Arc<Mutex<Option<PythonDaemon>>>,
    backend: AdvancedBrowserBackend,
    command: AdvancedCommand,
) -> Receiver<AdvancedDownloadWorkerEvent> {
    let (tx, rx) = mpsc::channel();
    let tx_worker = tx.clone();
    let spawn_result = thread::Builder::new()
        .name("new-project-advanced-download".to_string())
        .spawn(move || {
            let result = run_advanced_command(&daemon, backend, &command, &tx_worker);
            let send_result = tx_worker.send(AdvancedDownloadWorkerEvent::Finished(result));
            if send_result.is_err() {
                crate::runtime_log::log_warn(
                    "[new-project] failed to send advanced download result to UI",
                );
            }
        });

    if let Err(err) = spawn_result {
        crate::runtime_log::log_error(format!(
            "[new-project] failed to spawn advanced downloader worker: {err}"
        ));
        let send_result = tx.send(AdvancedDownloadWorkerEvent::Finished(Err(
            AdvancedDownloadError {
                user_message: "Не удалось запустить продвинутый выкачиватель.".to_string(),
                log_message: format!("failed to spawn advanced downloader worker: {err}"),
            },
        )));
        if send_result.is_err() {
            crate::runtime_log::log_warn(
                "[new-project] failed to deliver advanced downloader spawn error",
            );
        }
    }

    rx
}

fn run_advanced_command(
    daemon: &Arc<Mutex<Option<PythonDaemon>>>,
    backend: AdvancedBrowserBackend,
    command: &AdvancedCommand,
    progress_tx: &Sender<AdvancedDownloadWorkerEvent>,
) -> Result<AdvancedWorkerOutcome, AdvancedDownloadError> {
    let lock = daemon.lock();
    let mut guard = match lock {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let daemon = ensure_python_daemon(&mut guard, backend, progress_tx)?;

    match command {
        AdvancedCommand::OpenUrl { browser, url } => {
            let normalized = normalize_http_url(url).map_err(|err| AdvancedDownloadError {
                user_message: "Ссылка для браузера выглядит некорректной.".to_string(),
                log_message: format!("invalid advanced downloader url '{url}': {err}"),
            })?;
            daemon
                .write_command(&json!({
                    "command": "open_url",
                    "browser": browser,
                    "url": normalized,
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось отправить команду браузеру.".to_string(),
                    log_message: format!("failed to write open_url command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => {
                        send_progress(progress_tx, stage, current, total);
                    }
                    DaemonEvent::Opened { current_url } => {
                        return Ok(AdvancedWorkerOutcome::BrowserOpened { current_url });
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
                    DaemonEvent::LinkCollectStarted { current_url: _ } => {}
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::InterceptStarted { current_url: _ } => {}
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        send_intercept_count(progress_tx, counts);
                    }
                    DaemonEvent::Result {
                        page_url,
                        output_dir,
                        downloaded_images,
                    } => {
                        let pages = load_ribbon_pages_from_dir(&output_dir)?;
                        cleanup_temp_dir(&output_dir);
                        return Ok(AdvancedWorkerOutcome::Loaded(LoadedAdvancedDownload {
                            source_url: page_url,
                            pages,
                            downloaded_images,
                        }));
                    }
                    DaemonEvent::AutoResult {
                        page_url: _,
                        output_dir: _,
                        items: _,
                    } => {}
                }
            }
        }
        AdvancedCommand::Fetch {
            browser,
            pattern,
            max_parallel,
        } => {
            daemon
                .write_command(&json!({
                    "command": "fetch",
                    "browser": browser,
                    "pattern": pattern,
                    "max_parallel": max_parallel,
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось запустить выкачивание из браузера.".to_string(),
                    log_message: format!("failed to write fetch command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => {
                        send_progress(progress_tx, stage, current, total);
                    }
                    DaemonEvent::Result {
                        page_url,
                        output_dir,
                        downloaded_images,
                    } => {
                        let pages = load_ribbon_pages_from_dir(&output_dir)?;
                        cleanup_temp_dir(&output_dir);
                        return Ok(AdvancedWorkerOutcome::Loaded(LoadedAdvancedDownload {
                            source_url: page_url,
                            pages,
                            downloaded_images,
                        }));
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::LinkCollectStarted { current_url: _ } => {}
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::InterceptStarted { current_url: _ } => {}
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        send_intercept_count(progress_tx, counts);
                    }
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
                    DaemonEvent::AutoResult {
                        page_url: _,
                        output_dir: _,
                        items: _,
                    } => {}
                }
            }
        }
        AdvancedCommand::FetchAuto {
            browser,
            max_parallel,
            cancel_file,
        } => {
            daemon
                .write_command(&json!({
                    "command": "fetch_auto_links",
                    "browser": browser,
                    "max_parallel": max_parallel,
                    "cancel_file": cancel_file.display().to_string(),
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось запустить автоподбор ссылок.".to_string(),
                    log_message: format!("failed to write fetch_auto_links command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => {
                        send_progress(progress_tx, stage, current, total);
                    }
                    DaemonEvent::AutoResult {
                        page_url,
                        output_dir,
                        items,
                    } => {
                        let candidates =
                            load_auto_candidate_set_from_dir(page_url, &output_dir, items)?;
                        cleanup_temp_dir(&output_dir);
                        return Ok(AdvancedWorkerOutcome::AutoCandidatesReady(candidates));
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::LinkCollectStarted { current_url: _ } => {}
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::InterceptStarted { current_url: _ } => {}
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        send_intercept_count(progress_tx, counts);
                    }
                    DaemonEvent::Result {
                        page_url: _,
                        output_dir: _,
                        downloaded_images: _,
                    } => {}
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
                }
            }
        }
        AdvancedCommand::StartLinkCollect {
            browser,
            pattern,
            max_parallel,
        } => {
            daemon
                .write_command(&json!({
                    "command": "start_link_collect",
                    "browser": browser,
                    "pattern": pattern,
                    "max_parallel": max_parallel,
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось запустить фоновый сбор ссылок.".to_string(),
                    log_message: format!("failed to write start_link_collect command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => {
                        send_progress(progress_tx, stage, current, total);
                    }
                    DaemonEvent::LinkCollectStarted { current_url } => {
                        return Ok(AdvancedWorkerOutcome::LinkCollectStarted { current_url });
                    }
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::InterceptStarted { current_url: _ } => {}
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        send_intercept_count(progress_tx, counts);
                    }
                    DaemonEvent::Result {
                        page_url: _,
                        output_dir: _,
                        downloaded_images: _,
                    } => {}
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
                    DaemonEvent::AutoResult {
                        page_url: _,
                        output_dir: _,
                        items: _,
                    } => {}
                }
            }
        }
        AdvancedCommand::StartAutoLinkCollect {
            browser,
            max_parallel,
        } => {
            daemon
                .write_command(&json!({
                    "command": "start_auto_link_collect",
                    "browser": browser,
                    "max_parallel": max_parallel,
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось запустить фоновый автосбор ссылок.".to_string(),
                    log_message: format!("failed to write start_auto_link_collect command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => {
                        send_progress(progress_tx, stage, current, total);
                    }
                    DaemonEvent::LinkCollectStarted { current_url } => {
                        return Ok(AdvancedWorkerOutcome::LinkCollectStarted { current_url });
                    }
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::InterceptStarted { current_url: _ } => {}
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        send_intercept_count(progress_tx, counts);
                    }
                    DaemonEvent::Result {
                        page_url: _,
                        output_dir: _,
                        downloaded_images: _,
                    } => {}
                    DaemonEvent::AutoResult {
                        page_url: _,
                        output_dir: _,
                        items: _,
                    } => {}
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
                }
            }
        }
        AdvancedCommand::QueryLinkCollectCount { browser } => {
            daemon
                .write_command(&json!({
                    "command": "link_collect_status",
                    "browser": browser,
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось получить статус сбора ссылок.".to_string(),
                    log_message: format!("failed to write link_collect_status command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        return Ok(AdvancedWorkerOutcome::LinkCollectCountUpdated { found_links });
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => {
                        send_progress(progress_tx, stage, current, total);
                    }
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::LinkCollectStarted { current_url: _ } => {}
                    DaemonEvent::InterceptStarted { current_url: _ } => {}
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        send_intercept_count(progress_tx, counts);
                    }
                    DaemonEvent::Result {
                        page_url: _,
                        output_dir: _,
                        downloaded_images: _,
                    } => {}
                    DaemonEvent::AutoResult {
                        page_url: _,
                        output_dir: _,
                        items: _,
                    } => {}
                }
            }
        }
        AdvancedCommand::StopLinkCollect { browser } => {
            daemon
                .write_command(&json!({
                    "command": "stop_link_collect",
                    "browser": browser,
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось завершить сбор ссылок.".to_string(),
                    log_message: format!("failed to write stop_link_collect command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => {
                        send_progress(progress_tx, stage, current, total);
                    }
                    DaemonEvent::Result {
                        page_url,
                        output_dir,
                        downloaded_images,
                    } => {
                        let pages = load_ribbon_pages_from_dir(&output_dir)?;
                        cleanup_temp_dir(&output_dir);
                        return Ok(AdvancedWorkerOutcome::Loaded(LoadedAdvancedDownload {
                            source_url: page_url,
                            pages,
                            downloaded_images,
                        }));
                    }
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::LinkCollectStarted { current_url: _ } => {}
                    DaemonEvent::InterceptStarted { current_url: _ } => {}
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        send_intercept_count(progress_tx, counts);
                    }
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
                    DaemonEvent::AutoResult {
                        page_url: _,
                        output_dir: _,
                        items: _,
                    } => {}
                }
            }
        }
        AdvancedCommand::StopAutoLinkCollect {
            browser,
            cancel_file,
        } => {
            daemon
                .write_command(&json!({
                    "command": "stop_auto_link_collect",
                    "browser": browser,
                    "cancel_file": cancel_file.display().to_string(),
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось завершить автосбор ссылок.".to_string(),
                    log_message: format!("failed to write stop_auto_link_collect command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => {
                        send_progress(progress_tx, stage, current, total);
                    }
                    DaemonEvent::AutoResult {
                        page_url,
                        output_dir,
                        items,
                    } => {
                        let candidates =
                            load_auto_candidate_set_from_dir(page_url, &output_dir, items)?;
                        cleanup_temp_dir(&output_dir);
                        return Ok(AdvancedWorkerOutcome::AutoCandidatesReady(candidates));
                    }
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::LinkCollectStarted { current_url: _ } => {}
                    DaemonEvent::InterceptStarted { current_url: _ } => {}
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        send_intercept_count(progress_tx, counts);
                    }
                    DaemonEvent::Result {
                        page_url: _,
                        output_dir: _,
                        downloaded_images: _,
                    } => {}
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
                }
            }
        }
        AdvancedCommand::FetchCanvas { browser } => {
            daemon
                .write_command(&json!({
                    "command": "fetch_canvas",
                    "browser": browser,
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось запустить скачивание canvas.".to_string(),
                    log_message: format!("failed to write fetch_canvas command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => {
                        send_progress(progress_tx, stage, current, total);
                    }
                    DaemonEvent::Result {
                        page_url,
                        output_dir,
                        downloaded_images,
                    } => {
                        let pages = load_ribbon_pages_from_dir(&output_dir)?;
                        cleanup_temp_dir(&output_dir);
                        return Ok(AdvancedWorkerOutcome::Loaded(LoadedAdvancedDownload {
                            source_url: page_url,
                            pages,
                            downloaded_images,
                        }));
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::LinkCollectStarted { current_url: _ } => {}
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::InterceptStarted { current_url: _ } => {}
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        send_intercept_count(progress_tx, counts);
                    }
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
                    DaemonEvent::AutoResult {
                        page_url: _,
                        output_dir: _,
                        items: _,
                    } => {}
                }
            }
        }
        AdvancedCommand::StartCanvasIntercept { browser } => {
            daemon
                .write_command(&json!({
                    "command": "start_intercept",
                    "browser": browser,
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось запустить перехват в браузере.".to_string(),
                    log_message: format!("failed to write start_intercept command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => {
                        send_progress(progress_tx, stage, current, total);
                    }
                    DaemonEvent::InterceptStarted { current_url } => {
                        return Ok(AdvancedWorkerOutcome::InterceptStarted { current_url });
                    }
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        send_intercept_count(progress_tx, counts);
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::LinkCollectStarted { current_url: _ } => {}
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::Result {
                        page_url: _,
                        output_dir: _,
                        downloaded_images: _,
                    } => {}
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
                    DaemonEvent::AutoResult {
                        page_url: _,
                        output_dir: _,
                        items: _,
                    } => {}
                }
            }
        }
        AdvancedCommand::QueryCanvasInterceptCount { browser } => {
            daemon
                .write_command(&json!({
                    "command": "intercept_status",
                    "browser": browser,
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось получить статус перехвата Canvas.".to_string(),
                    log_message: format!("failed to write intercept_status command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        return Ok(AdvancedWorkerOutcome::InterceptCountUpdated { counts });
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => {
                        send_progress(progress_tx, stage, current, total);
                    }
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::LinkCollectStarted { current_url: _ } => {}
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::InterceptStarted { current_url: _ } => {}
                    DaemonEvent::Result {
                        page_url: _,
                        output_dir: _,
                        downloaded_images: _,
                    } => {}
                    DaemonEvent::AutoResult {
                        page_url: _,
                        output_dir: _,
                        items: _,
                    } => {}
                }
            }
        }
        AdvancedCommand::StopCanvasIntercept { browser } => {
            daemon
                .write_command(&json!({
                    "command": "stop_intercept",
                    "browser": browser,
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось завершить перехват в браузере.".to_string(),
                    log_message: format!("failed to write stop_intercept command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => {
                        send_progress(progress_tx, stage, current, total);
                    }
                    DaemonEvent::Result {
                        page_url,
                        output_dir,
                        downloaded_images,
                    } => {
                        let pages = load_ribbon_pages_from_dir(&output_dir)?;
                        cleanup_temp_dir(&output_dir);
                        return Ok(AdvancedWorkerOutcome::Loaded(LoadedAdvancedDownload {
                            source_url: page_url,
                            pages,
                            downloaded_images,
                        }));
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::LinkCollectStarted { current_url: _ } => {}
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::InterceptStarted { current_url: _ } => {}
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        send_intercept_count(progress_tx, counts);
                    }
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
                    DaemonEvent::AutoResult {
                        page_url: _,
                        output_dir: _,
                        items: _,
                    } => {}
                }
            }
        }
        AdvancedCommand::StartDeepIntercept { browser } => {
            if backend != AdvancedBrowserBackend::Cloak {
                return Err(AdvancedDownloadError {
                    user_message: "Глубокий перехват доступен только для CloakBrowser.".to_string(),
                    log_message: "deep intercept requested for non-Cloak backend".to_string(),
                });
            }
            daemon
                .write_command(&json!({
                    "command": "start_deep_intercept",
                    "browser": browser,
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось запустить глубокий перехват.".to_string(),
                    log_message: format!("failed to write start_deep_intercept command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => send_progress(progress_tx, stage, current, total),
                    DaemonEvent::InterceptStarted { current_url } => {
                        return Ok(AdvancedWorkerOutcome::InterceptStarted { current_url });
                    }
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        send_intercept_count(progress_tx, counts);
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::LinkCollectStarted { current_url: _ } => {}
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::Result {
                        page_url: _,
                        output_dir: _,
                        downloaded_images: _,
                    } => {}
                    DaemonEvent::AutoResult {
                        page_url: _,
                        output_dir: _,
                        items: _,
                    } => {}
                    DaemonEvent::Log { level, message } => log_daemon_line(&level, &message),
                }
            }
        }
        AdvancedCommand::QueryDeepInterceptCount { browser } => {
            daemon
                .write_command(&json!({
                    "command": "deep_intercept_status",
                    "browser": browser,
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось получить статус глубокого перехвата.".to_string(),
                    log_message: format!("failed to write deep_intercept_status command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        return Ok(AdvancedWorkerOutcome::InterceptCountUpdated { counts });
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => send_progress(progress_tx, stage, current, total),
                    DaemonEvent::Log { level, message } => log_daemon_line(&level, &message),
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::LinkCollectStarted { current_url: _ } => {}
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::InterceptStarted { current_url: _ } => {}
                    DaemonEvent::Result {
                        page_url: _,
                        output_dir: _,
                        downloaded_images: _,
                    } => {}
                    DaemonEvent::AutoResult {
                        page_url: _,
                        output_dir: _,
                        items: _,
                    } => {}
                }
            }
        }
        AdvancedCommand::StopDeepIntercept {
            browser,
            cancel_file,
        } => {
            daemon
                .write_command(&json!({
                    "command": "stop_deep_intercept",
                    "browser": browser,
                    "cancel_file": cancel_file.display().to_string(),
                }))
                .map_err(|err| AdvancedDownloadError {
                    user_message: "Не удалось завершить глубокий перехват.".to_string(),
                    log_message: format!("failed to write stop_deep_intercept command: {err}"),
                })?;

            loop {
                match read_daemon_event(daemon)? {
                    DaemonEvent::Progress {
                        stage,
                        current,
                        total,
                    } => send_progress(progress_tx, stage, current, total),
                    DaemonEvent::AutoResult {
                        page_url,
                        output_dir,
                        items,
                    } => {
                        let candidates =
                            load_auto_candidate_set_from_dir(page_url, &output_dir, items)?;
                        cleanup_temp_dir(&output_dir);
                        return Ok(AdvancedWorkerOutcome::AutoCandidatesReady(candidates));
                    }
                    DaemonEvent::InterceptCountUpdated { counts } => {
                        send_intercept_count(progress_tx, counts);
                    }
                    DaemonEvent::Error {
                        user_message,
                        log_message,
                    } => {
                        return Err(AdvancedDownloadError {
                            user_message,
                            log_message,
                        });
                    }
                    DaemonEvent::Opened { current_url: _ } => {}
                    DaemonEvent::LinkCollectStarted { current_url: _ } => {}
                    DaemonEvent::LinkCollectCountUpdated { found_links } => {
                        send_link_count(progress_tx, found_links);
                    }
                    DaemonEvent::InterceptStarted { current_url: _ } => {}
                    DaemonEvent::Result {
                        page_url: _,
                        output_dir: _,
                        downloaded_images: _,
                    } => {}
                    DaemonEvent::Log { level, message } => log_daemon_line(&level, &message),
                }
            }
        }
    }
}

fn send_progress(
    tx: &Sender<AdvancedDownloadWorkerEvent>,
    stage: &'static str,
    current: usize,
    total: usize,
) {
    let send_result = tx.send(AdvancedDownloadWorkerEvent::Progress {
        stage,
        current,
        total,
    });
    if send_result.is_err() {
        crate::runtime_log::log_warn("[new-project] UI dropped advanced downloader progress event");
    }
}

fn send_intercept_count(tx: &Sender<AdvancedDownloadWorkerEvent>, counts: InterceptCounts) {
    let send_result = tx.send(AdvancedDownloadWorkerEvent::InterceptCountUpdated { counts });
    if send_result.is_err() {
        crate::runtime_log::log_warn(
            "[new-project] UI dropped advanced downloader intercept count event",
        );
    }
}

fn send_link_count(tx: &Sender<AdvancedDownloadWorkerEvent>, found_links: usize) {
    let send_result = tx.send(AdvancedDownloadWorkerEvent::LinkCollectCountUpdated { found_links });
    if send_result.is_err() {
        crate::runtime_log::log_warn(
            "[new-project] UI dropped advanced downloader link count event",
        );
    }
}

fn ensure_python_daemon<'daemon>(
    slot: &'daemon mut Option<PythonDaemon>,
    backend: AdvancedBrowserBackend,
    progress_tx: &Sender<AdvancedDownloadWorkerEvent>,
) -> Result<&'daemon mut PythonDaemon, AdvancedDownloadError> {
    let needs_restart = match slot.as_ref() {
        Some(daemon) if daemon.backend != backend => {
            crate::runtime_log::log_info(format!(
                "[new-project] switching advanced downloader to {} backend",
                backend.label()
            ));
            true
        }
        // The backend process is app-global; a dead client means it was stopped or
        // crashed, so we rebuild (which re-starts the backend if needed).
        Some(daemon) => !daemon.client.is_alive(),
        None => true,
    };

    if needs_restart {
        if let Some(mut daemon) = slot.take() {
            let _ = daemon.write_command(&json!({ "command": "close" }));
        }
        *slot = Some(start_python_daemon(backend, progress_tx)?);
    }

    slot.as_mut().ok_or_else(|| AdvancedDownloadError {
        user_message: "Не удалось подключиться к browser helper.".to_string(),
        log_message: "python daemon slot remained empty after start".to_string(),
    })
}

/// Connects to the app-global AI backend (the browser session now lives there).
/// If the backend is not running, asks the supervisor to start it and waits for
/// the socket to come up before returning a usable client.
fn ensure_backend_client() -> Result<backend_ipc::BackendClient, AdvancedDownloadError> {
    if let Ok(client) = backend_ipc::shared_client()
        && backend_is_ready(&client)
    {
        return Ok(client);
    }

    if let Some(handle) = crate::ai_backend_supervisor::global_handle() {
        handle.send_process(crate::ai_backend_supervisor::AiBackendProcessCommand::Start);
    }

    let deadline = web_time::Instant::now() + BACKEND_START_WAIT;
    loop {
        if let Ok(client) = backend_ipc::shared_client()
            && backend_is_ready(&client)
        {
            return Ok(client);
        }
        if web_time::Instant::now() >= deadline {
            return Err(AdvancedDownloadError {
                user_message: "ИИ бэкенд не запущен. Запустите его в настройках, чтобы \
                    пользоваться браузерным выкачивателем."
                    .to_string(),
                log_message: "browser command requested but AI backend is not connectable"
                    .to_string(),
            });
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Crate-internal entry for other launcher code (e.g. the batch executor) to get a
/// ready backend client, starting the backend if needed. Flattens the error to a
/// single string for callers that do not use [`AdvancedDownloadError`].
pub(crate) fn connect_browser_backend() -> Result<backend_ipc::BackendClient, String> {
    ensure_backend_client().map_err(|err| err.log_message)
}

/// Cheap readiness ping: a successful `health` call means the backend socket is up.
fn backend_is_ready(client: &backend_ipc::BackendClient) -> bool {
    client
        .call(
            backend_ipc::protocol::METHOD_HEALTH,
            json!({}),
            &[],
            Duration::from_millis(700),
        )
        .is_ok()
}

fn start_python_daemon(
    backend: AdvancedBrowserBackend,
    progress_tx: &Sender<AdvancedDownloadWorkerEvent>,
) -> Result<PythonDaemon, AdvancedDownloadError> {
    let client = ensure_backend_client()?;
    crate::runtime_log::log_info(format!(
        "[new-project] using unified backend browser session ({} backend)",
        backend.label()
    ));
    let mut daemon = PythonDaemon {
        backend,
        client,
        frames: None,
    };
    // Select the browser backend in the shared session. A failure here resurfaces
    // on the first real command with a clear message, so it is best-effort.
    if let Err(err) = daemon.write_command(&json!({
        "command": "set_backend",
        "backend": backend.ipc_backend_name(),
    })) {
        crate::runtime_log::log_warn(format!(
            "[new-project] failed to select browser backend over IPC: {err}"
        ));
    } else {
        let _ = daemon.read_payload(); // consume the `backend_set` terminal
    }
    fetch_and_warn_downloader_version(&mut daemon, progress_tx);
    Ok(daemon)
}

impl PythonDaemon {
    /// Starts one `browser.command` IPC call on a worker thread. Progress frames
    /// and the terminal event are forwarded into `self.frames`, which the existing
    /// per-command read loops drain via [`read_payload`](Self::read_payload).
    fn write_command(&mut self, command: &Value) -> Result<(), String> {
        let (tx, rx) = mpsc::channel::<Value>();
        let client = self.client.clone();
        let payload = command.clone();
        thread::Builder::new()
            .name("advanced-download-ipc".to_string())
            .spawn(move || {
                let progress_tx = tx.clone();
                let on_progress = move |header: &Value, _blob: &[u8]| {
                    let _ = progress_tx.send(progress_frame_from_header(header));
                };
                let terminal = match client.call_streaming(
                    backend_ipc::protocol::METHOD_BROWSER_COMMAND,
                    json!({ "payload": payload }),
                    &[],
                    on_progress,
                    BROWSER_COMMAND_TIMEOUT,
                ) {
                    Ok((header, _blob)) => header,
                    Err(err) => browser_error_frame(err),
                };
                let _ = tx.send(terminal);
            })
            .map_err(|err| format!("spawn browser ipc worker failed: {err}"))?;
        self.frames = Some(rx);
        Ok(())
    }

    /// Blocks for the next payload of the in-flight command (a `progress` frame or
    /// the single terminal event), mirroring the old line-per-payload stdio read.
    fn read_payload(&mut self) -> Result<Value, AdvancedDownloadError> {
        let rx = self.frames.as_ref().ok_or_else(|| AdvancedDownloadError {
            user_message: "Браузерный выкачиватель не запущен.".to_string(),
            log_message: "read_payload called with no in-flight browser command".to_string(),
        })?;
        rx.recv().map_err(|_| AdvancedDownloadError {
            user_message: "Браузерный выкачиватель неожиданно завершился.".to_string(),
            log_message: "browser ipc frame channel closed before a terminal event".to_string(),
        })
    }
}

/// Maps an IPC `progress` frame header (`stage`/`current`/`total`) into the legacy
/// daemon `{"event":"progress", ...}` payload the command loops expect.
fn progress_frame_from_header(header: &Value) -> Value {
    json!({
        "event": "progress",
        "stage": header.get("stage").and_then(Value::as_str).unwrap_or("collect"),
        "current": header.get("current").and_then(Value::as_u64).unwrap_or(0),
        "total": header.get("total").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// Maps an IPC call error into the legacy `{"event":"error", ...}` terminal payload.
fn browser_error_frame(err: CallError) -> Value {
    let (user_message, log_message) = match err {
        CallError::Error(msg) => (msg.clone(), msg),
        CallError::Interrupted(msg) => ("Операция отменена.".to_string(), msg),
        CallError::Transport(msg) => (
            "ИИ бэкенд недоступен. Запустите его в настройках.".to_string(),
            msg,
        ),
    };
    json!({ "event": "error", "user_message": user_message, "log_message": log_message })
}

enum DaemonEvent {
    Progress {
        stage: &'static str,
        current: usize,
        total: usize,
    },
    Opened {
        current_url: String,
    },
    LinkCollectStarted {
        current_url: String,
    },
    LinkCollectCountUpdated {
        found_links: usize,
    },
    InterceptStarted {
        current_url: String,
    },
    InterceptCountUpdated {
        counts: InterceptCounts,
    },
    Result {
        page_url: String,
        output_dir: PathBuf,
        downloaded_images: usize,
    },
    AutoResult {
        page_url: String,
        output_dir: PathBuf,
        items: Vec<AutoDownloadedItem>,
    },
    Error {
        user_message: String,
        log_message: String,
    },
    Log {
        level: String,
        message: String,
    },
}

fn parse_daemon_event(payload: Value) -> Result<DaemonEvent, AdvancedDownloadError> {
    let event_name = payload
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match event_name {
        "progress" => {
            let stage_name = payload
                .get("stage")
                .and_then(Value::as_str)
                .unwrap_or("collect");
            Ok(DaemonEvent::Progress {
                stage: stage_name_to_static(stage_name),
                current: payload
                    .get("current")
                    .and_then(Value::as_u64)
                    .map_or(0, u64_to_usize),
                total: payload
                    .get("total")
                    .and_then(Value::as_u64)
                    .map_or(0, u64_to_usize),
            })
        }
        "opened" => Ok(DaemonEvent::Opened {
            current_url: payload
                .get("current_url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "link_collect_started" => Ok(DaemonEvent::LinkCollectStarted {
            current_url: payload
                .get("current_url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "link_collect_count" => Ok(DaemonEvent::LinkCollectCountUpdated {
            found_links: payload
                .get("found_links")
                .and_then(Value::as_u64)
                .map_or(0, u64_to_usize),
        }),
        "intercept_started" => Ok(DaemonEvent::InterceptStarted {
            current_url: payload
                .get("current_url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "intercept_count" => {
            let count_field = |key: &str| {
                payload
                    .get(key)
                    .and_then(Value::as_u64)
                    .map_or(0, u64_to_usize)
            };
            Ok(DaemonEvent::InterceptCountUpdated {
                counts: InterceptCounts {
                    total: count_field("found_pages"),
                    canvases: count_field("found_canvases"),
                    images: count_field("found_images"),
                },
            })
        }
        "result" => {
            let output_dir = payload
                .get("output_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .ok_or_else(|| AdvancedDownloadError {
                    user_message: "Python helper не вернул папку с изображениями.".to_string(),
                    log_message: format!("missing output_dir in result payload: {payload}"),
                })?;
            Ok(DaemonEvent::Result {
                page_url: payload
                    .get("page_url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                output_dir,
                downloaded_images: payload
                    .get("downloaded_images")
                    .and_then(Value::as_u64)
                    .map_or(0, u64_to_usize),
            })
        }
        "auto_result" => {
            let output_dir = payload
                .get("output_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .ok_or_else(|| AdvancedDownloadError {
                    user_message: "Python helper не вернул папку с автокандидатами.".to_string(),
                    log_message: format!("missing output_dir in auto_result payload: {payload}"),
                })?;
            Ok(DaemonEvent::AutoResult {
                page_url: payload
                    .get("page_url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                output_dir,
                items: parse_auto_downloaded_items(&payload)?,
            })
        }
        "error" => Ok(DaemonEvent::Error {
            user_message: payload
                .get("user_message")
                .and_then(Value::as_str)
                .unwrap_or("Продвинутый выкачиватель завершился с ошибкой.")
                .to_string(),
            log_message: payload
                .get("log_message")
                .and_then(Value::as_str)
                .unwrap_or("advanced downloader daemon returned error")
                .to_string(),
        }),
        "log" => Ok(DaemonEvent::Log {
            level: payload
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or("info")
                .to_string(),
            message: payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        _ => Err(AdvancedDownloadError {
            user_message: "Python helper вернул неизвестное событие.".to_string(),
            log_message: format!("unknown daemon event payload: {payload}"),
        }),
    }
}

fn read_daemon_event(daemon: &mut PythonDaemon) -> Result<DaemonEvent, AdvancedDownloadError> {
    let payload = daemon.read_payload()?;
    parse_daemon_event(payload)
}

fn parse_auto_downloaded_items(
    payload: &Value,
) -> Result<Vec<AutoDownloadedItem>, AdvancedDownloadError> {
    let Some(items) = payload.get("items").and_then(Value::as_array) else {
        return Err(AdvancedDownloadError {
            user_message: "Python helper не вернул список автокандидатов.".to_string(),
            log_message: format!("missing items in auto_result payload: {payload}"),
        });
    };

    let mut parsed = Vec::with_capacity(items.len());
    for item in items {
        let order_index = item
            .get("order")
            .and_then(Value::as_u64)
            .map_or(0, u64_to_usize);
        let url = item
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let file_name = item
            .get("file_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if url.is_empty() || file_name.is_empty() {
            return Err(AdvancedDownloadError {
                user_message: "Python helper вернул неполные данные автокандидата.".to_string(),
                log_message: format!("invalid auto candidate item: {item}"),
            });
        }
        let probable_junk = item
            .get("probable_junk")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        parsed.push(AutoDownloadedItem {
            order_index,
            url,
            file_name,
            probable_junk,
        });
    }
    Ok(parsed)
}

/// Asks the backend for the downloader version (`browser.version`) and warns the
/// UI if it differs from the studio build. Best-effort: a failure is logged and
/// skipped (the real commands still work).
fn fetch_and_warn_downloader_version(
    daemon: &mut PythonDaemon,
    progress_tx: &Sender<AdvancedDownloadWorkerEvent>,
) {
    if daemon.write_command(&json!({ "command": "version" })).is_err() {
        return;
    }
    let Ok(payload) = daemon.read_payload() else {
        return;
    };
    let downloader_version = payload
        .get("downloader_version")
        .or_else(|| payload.get("version"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(downloader_version) = downloader_version {
        emit_downloader_version_warning_if_needed(progress_tx, downloader_version);
    }
}

fn emit_downloader_version_warning_if_needed(
    progress_tx: &Sender<AdvancedDownloadWorkerEvent>,
    downloader_version: &str,
) {
    let studio_version = env!("CARGO_PKG_VERSION").trim().to_string();
    if downloader_version == studio_version {
        return;
    }

    crate::runtime_log::log_warn(format!(
        "[new-project] Python downloader version mismatch: studio={studio_version} downloader={downloader_version}"
    ));
    if progress_tx
        .send(AdvancedDownloadWorkerEvent::VersionMismatch {
            studio_version,
            downloader_version: downloader_version.to_string(),
        })
        .is_err()
    {
        crate::runtime_log::log_warn(
            "[new-project] UI dropped advanced downloader version mismatch event",
        );
    }
}

pub fn advanced_downloader_version_warning_message(
    studio_version: &str,
    downloader_version: &str,
) -> String {
    format!(
        "Версии студии и Python-выкачивателя не соответствуют: {studio_version}/{downloader_version}. Возможна некорректная работа."
    )
}

fn stage_name_to_static(stage: &str) -> &'static str {
    match stage {
        "browser" => "browser",
        "collect" => "collect",
        "collect_canvas" => "collect_canvas",
        "download" => "download",
        "save_canvas" => "save_canvas",
        other => {
            crate::runtime_log::log_warn(format!(
                "[new-project] unknown advanced downloader stage '{other}', falling back to collect"
            ));
            "collect"
        }
    }
}

fn u64_to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn log_daemon_line(level: &str, message: &str) {
    match level {
        "warn" => crate::runtime_log::log_warn(format!("[new-project][advanced-python] {message}")),
        "error" => {
            crate::runtime_log::log_error(format!("[new-project][advanced-python] {message}"))
        }
        _ => crate::runtime_log::log_info(format!("[new-project][advanced-python] {message}")),
    }
}

fn load_ribbon_pages_from_dir(dir: &Path) -> Result<Vec<RibbonPage>, AdvancedDownloadError> {
    let mut files = fs::read_dir(dir)
        .map_err(|err| AdvancedDownloadError {
            user_message: "Не удалось прочитать результаты выкачивания.".to_string(),
            log_message: format!(
                "failed to read advanced downloader output dir '{}': {err}",
                dir.display()
            ),
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_image_file(path))
        .collect::<Vec<_>>();
    files.sort();

    let mut images = Vec::with_capacity(files.len());
    for path in files {
        let image = image::open(&path).map_err(|err| AdvancedDownloadError {
            user_message: "Не удалось открыть одну из скачанных картинок.".to_string(),
            log_message: format!(
                "failed to decode advanced downloader image '{}': {err}",
                path.display()
            ),
        })?;
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("page.png")
            .to_string();
        images.push(ImportedImage { name, image });
    }

    if images.is_empty() {
        return Err(AdvancedDownloadError {
            user_message: "Продвинутый выкачиватель не нашёл подходящих изображений.".to_string(),
            log_message: format!(
                "advanced downloader output dir '{}' contained no images",
                dir.display()
            ),
        });
    }

    Ok(build_ribbon_pages(images))
}

fn load_auto_candidate_set_from_dir(
    source_url: String,
    dir: &Path,
    downloaded_items: Vec<AutoDownloadedItem>,
) -> Result<AdvancedAutoCandidateSet, AdvancedDownloadError> {
    if downloaded_items.is_empty() {
        return Err(AdvancedDownloadError {
            user_message: "Автоподбор не нашёл изображений для проверки.".to_string(),
            log_message: format!(
                "auto candidate result for '{source_url}' contained no downloaded items"
            ),
        });
    }

    let mut items = Vec::with_capacity(downloaded_items.len());
    for (id, item) in downloaded_items.into_iter().enumerate() {
        let path = dir.join(&item.file_name);
        let image = image::open(&path).map_err(|err| AdvancedDownloadError {
            user_message: "Не удалось открыть одну из картинок автоподбора.".to_string(),
            log_message: format!(
                "failed to decode auto candidate '{}': {err}",
                path.display()
            ),
        })?;
        let (width, height) = image.dimensions();
        let width = usize::try_from(width).unwrap_or(usize::MAX);
        let height = usize::try_from(height).unwrap_or(usize::MAX);
        let thumbnail = build_candidate_thumbnail(&image);
        items.push(AdvancedAutoCandidate {
            id,
            order_index: item.order_index,
            group_id: 0,
            url: item.url,
            name: item.file_name,
            width,
            height,
            image,
            thumbnail,
            probable_junk: item.probable_junk,
        });
    }
    assign_auto_candidate_groups(&mut items);
    let groups = build_auto_candidate_groups(&items);

    Ok(AdvancedAutoCandidateSet {
        source_url,
        items,
        groups,
    })
}

fn build_candidate_thumbnail(image: &DynamicImage) -> ColorImage {
    let thumbnail = image.thumbnail(180, 180).to_rgba8();
    let width = usize::try_from(thumbnail.width()).unwrap_or(1).max(1);
    let height = usize::try_from(thumbnail.height()).unwrap_or(1).max(1);
    ColorImage::from_rgba_unmultiplied([width, height], thumbnail.as_raw())
}

fn assign_auto_candidate_groups(items: &mut [AdvancedAutoCandidate]) {
    let mut signature_to_group = HashMap::new();
    let mut next_group_id = 0usize;
    for item in items {
        let signature = auto_url_group_signature(&item.url);
        let group_id = *signature_to_group.entry(signature).or_insert_with(|| {
            let id = next_group_id;
            next_group_id = next_group_id.saturating_add(1);
            id
        });
        item.group_id = group_id;
    }
}

fn build_auto_candidate_groups(items: &[AdvancedAutoCandidate]) -> Vec<AdvancedAutoGroup> {
    let mut by_group: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    let mut signatures: HashMap<usize, String> = HashMap::new();
    for item in items {
        by_group.entry(item.group_id).or_default().push(item.id);
        signatures
            .entry(item.group_id)
            .or_insert_with(|| auto_url_group_signature(&item.url));
    }

    by_group
        .into_iter()
        .map(|(id, item_ids)| AdvancedAutoGroup {
            id,
            signature: signatures
                .remove(&id)
                .unwrap_or_else(|| "unknown".to_string()),
            item_ids,
        })
        .collect()
}

fn auto_url_group_signature(url: &str) -> String {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let (without_query, query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, ""), |(left, right)| (left, right));
    let without_scheme = without_query
        .split_once("://")
        .map_or(without_query, |(_, rest)| rest);
    let (host, path) = without_scheme
        .split_once('/')
        .map_or((without_scheme, ""), |(left, right)| (left, right));

    let mut parts = Vec::new();
    parts.push(format!("h:{}", host_signature(host)));
    parts.push(format!("p:{}", path_signature(path)));
    if !query.is_empty() {
        parts.push(format!("q:{}", query_signature(query)));
    }
    parts.join("|")
}

fn host_signature(host: &str) -> String {
    let labels = host
        .split('.')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if labels.len() <= 2 {
        return labels
            .iter()
            .map(|label| label.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(".");
    }

    labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            if index + 2 >= labels.len() {
                label.to_ascii_lowercase()
            } else {
                token_signature(label)
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn path_signature(path: &str) -> String {
    path.split('/')
        .filter(|part| !part.is_empty())
        .map(path_segment_signature)
        .collect::<Vec<_>>()
        .join("/")
}

fn path_segment_signature(segment: &str) -> String {
    let lower = segment.to_ascii_lowercase();
    if let Some((stem, ext)) = lower.rsplit_once('.')
        && !stem.is_empty()
        && !ext.is_empty()
        && ext.len() <= 5
    {
        return format!("{}.{}", token_signature(stem), ext);
    }
    token_signature(&lower)
}

fn query_signature(query: &str) -> String {
    let mut pairs = query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            format!("{}={}", key.to_ascii_lowercase(), token_signature(value))
        })
        .collect::<Vec<_>>();
    pairs.sort();
    pairs.join("&")
}

fn token_signature(token: &str) -> String {
    let value = token.trim().to_ascii_lowercase();
    if value.is_empty() {
        return "{}".to_string();
    }
    if value.chars().all(|ch| ch.is_ascii_digit()) {
        return "{num}".to_string();
    }
    if value.len() >= 8 && value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return "{hex}".to_string();
    }
    if is_uuid_like(&value) {
        return "{uuid}".to_string();
    }
    let has_ascii_digit = value.chars().any(|ch| ch.is_ascii_digit());
    let has_ascii_alpha = value.chars().any(|ch| ch.is_ascii_alphabetic());
    if has_ascii_digit && has_ascii_alpha {
        return "{id}".to_string();
    }
    if has_ascii_alpha && value.len() <= 3 {
        return "{short-alpha}".to_string();
    }
    if value.len() >= 16 && value.chars().all(is_url_safe_token_char) {
        return "{token}".to_string();
    }
    value
}

fn is_uuid_like(value: &str) -> bool {
    let parts = value.split('-').map(str::len).collect::<Vec<_>>();
    parts == [8, 4, 4, 4, 12]
}

fn is_url_safe_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '~')
}

pub fn build_pages_from_auto_candidates(
    candidates: &AdvancedAutoCandidateSet,
    removed_items: &HashSet<usize>,
    removed_groups: &HashSet<usize>,
) -> Result<Vec<RibbonPage>, String> {
    let mut retained = candidates
        .items
        .iter()
        .filter(|item| {
            !removed_items.contains(&item.id) && !removed_groups.contains(&item.group_id)
        })
        .collect::<Vec<_>>();
    retained.sort_by_key(|item| item.order_index);

    let images = retained
        .into_iter()
        .map(|item| ImportedImage {
            name: item.name.clone(),
            image: item.image.clone(),
        })
        .collect::<Vec<_>>();
    if images.is_empty() {
        return Err("Все картинки автоподбора удалены.".to_string());
    }
    Ok(build_ribbon_pages(images))
}

fn cleanup_temp_dir(dir: &Path) {
    let remove_result = fs::remove_dir_all(dir);
    if let Err(err) = remove_result {
        crate::runtime_log::log_warn(format!(
            "[new-project] failed to remove advanced downloader temp dir '{}': {err}",
            dir.display()
        ));
    }
}

fn advanced_cancel_file_path() -> PathBuf {
    let nanos = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    std::env::temp_dir().join(format!(
        "mangafucker_adv_cancel_{}_{}.flag",
        std::process::id(),
        nanos
    ))
}

fn cleanup_pending_cancel_file(path: Option<&Path>) {
    let Some(path) = path else {
        return;
    };
    if let Err(err) = fs::remove_file(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        crate::runtime_log::log_warn(format!(
            "[new-project] failed to remove advanced downloader cancel marker '{}': {err}",
            path.display()
        ));
    }
}

fn is_image_file(path: &Path) -> bool {
    match path.extension().and_then(|value| value.to_str()) {
        Some(ext) => matches!(
            ext.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "webp" | "bmp" | "tif" | "tiff"
        ),
        None => false,
    }
}


fn detect_available_browsers() -> Vec<String> {
    let mut browsers = Vec::new();
    if firefox_binary().is_some() {
        browsers.push("Firefox".to_string());
    }
    if chrome_binary().is_some() {
        browsers.push("Chrome".to_string());
    }
    if edge_binary().is_some() {
        browsers.push("Edge".to_string());
    }
    if safari_available() {
        browsers.push("Safari".to_string());
    }
    browsers
}

fn firefox_binary() -> Option<PathBuf> {
    if let Some(path) = env_file("FIREFOX_BIN") {
        return Some(path);
    }
    find_in_path(&["firefox", "firefox-esr"]).or_else(|| {
        find_existing_path(&[
            "/usr/bin/firefox",
            "/usr/bin/firefox-esr",
            "/snap/bin/firefox",
            "/opt/firefox/firefox",
            r"C:\Program Files\Mozilla Firefox\firefox.exe",
            r"C:\Program Files (x86)\Mozilla Firefox\firefox.exe",
            "/Applications/Firefox.app/Contents/MacOS/firefox",
        ])
    })
}

fn chrome_binary() -> Option<PathBuf> {
    for env_name in ["CHROME_BIN", "GOOGLE_CHROME_BIN"] {
        if let Some(path) = env_file(env_name) {
            return Some(path);
        }
    }
    find_in_path(&["google-chrome", "chrome", "chromium", "chromium-browser"]).or_else(|| {
        find_existing_path(&[
            "/usr/bin/google-chrome",
            "/usr/bin/chromium",
            "/snap/bin/chromium",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ])
    })
}

fn edge_binary() -> Option<PathBuf> {
    if let Some(path) = env_file("EDGE_BIN") {
        return Some(path);
    }
    find_in_path(&["microsoft-edge", "microsoft-edge-stable"]).or_else(|| {
        find_existing_path(&[
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ])
    })
}

fn safari_available() -> bool {
    cfg!(target_os = "macos") && find_in_path(&["safaridriver"]).is_some()
}

fn env_file(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    let path = PathBuf::from(value);
    if path.is_file() { Some(path) } else { None }
}

fn find_in_path(names: &[&str]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
            #[cfg(target_os = "windows")]
            {
                let candidate_exe = dir.join(format!("{name}.exe"));
                if candidate_exe.is_file() {
                    return Some(candidate_exe);
                }
            }
        }
    }
    None
}

fn find_existing_path(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn normalize_http_url(raw: &str) -> Result<String, String> {
    let trimmed = raw
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>()
        .trim()
        .replace('\\', "/");
    if trimmed.is_empty() {
        return Err("empty url".to_string());
    }
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("file://")
    {
        return Ok(trimmed);
    }
    if trimmed.starts_with("www.") || looks_like_domain(&trimmed) {
        return Ok(format!("https://{trimmed}"));
    }
    Err("supported schemes are http(s) and file://".to_string())
}

fn looks_like_domain(value: &str) -> bool {
    let mut parts = value.split('/');
    let host = parts.next().unwrap_or_default();
    if host.is_empty() || !host.contains('.') {
        return false;
    }
    host.chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::{advanced_downloader_version_warning_message, auto_url_group_signature};

    #[test]
    fn formats_advanced_downloader_version_warning() {
        assert_eq!(
            advanced_downloader_version_warning_message("3.4.0", "3.3.0"),
            "Версии студии и Python-выкачивателя не соответствуют: 3.4.0/3.3.0. Возможна некорректная работа."
        );
    }

    #[test]
    fn auto_group_signature_keeps_cdn_page_sequence_together() {
        let first = auto_url_group_signature("https://a1.manga.com/chapter/12387123/1");
        let second = auto_url_group_signature("https://b3.manga.com/chapter/12387123/39");

        assert_eq!(first, second);
    }

    #[test]
    fn auto_group_signature_separates_different_static_paths() {
        let page = auto_url_group_signature("https://a1.manga.com/chapter/12387123/1");
        let icon = auto_url_group_signature("https://manga.com/icos/ru.jpg");

        assert_ne!(page, icon);
    }

    #[test]
    fn auto_group_signature_groups_short_locale_icons() {
        let ru = auto_url_group_signature("https://manga.com/icos/ru.jpg");
        let en = auto_url_group_signature("https://manga.com/icos/en.jpg");

        assert_eq!(ru, en);
    }
}
