/*
File: src/launcher/new_project/advanced_download.rs

Purpose:
Background bridge for the advanced Selenium-based downloader in the New Project launcher.

Main responsibilities:
- keep all Selenium and browser-profile work outside the GUI thread via a Python helper daemon;
- open a selected browser with the persistent profile from `modules/browser_profiles`;
- compare the helper-reported Python package version with the Rust Studio version;
- fetch image URLs from the active Selenium page, download them, convert them into ribbon pages,
  and run canvas snapshot/capture flows via the same Selenium helper.

Key structures:
- AdvancedDownloadController
- AdvancedDownloadEvent
- AdvancedDownloadSuccess

Notes:
The actual Selenium runtime stays in Python because the project already ships browser/profile
helpers there. Rust owns the UI state, process lifecycle, progress streaming, and ribbon conversion.
*/

use crate::config;
use crate::launcher::new_project::ribbon::{ImportedImage, RibbonPage, build_ribbon_pages};
use crate::python_manager;
use crate::python_manager::ManagedPythonChild;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdin, ChildStdout, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

const DEFAULT_LINK_PREFIX: &str = "https://page-edge.kakao.com/sdownload/resource*";

#[derive(Debug)]
struct PendingAdvancedDownload {
    blocks_ui: bool,
    rx: Receiver<AdvancedDownloadWorkerEvent>,
}

pub struct AdvancedDownloadController {
    daemon: Arc<Mutex<Option<PythonDaemon>>>,
    pending: Option<PendingAdvancedDownload>,
    available_browsers: Vec<String>,
}

pub struct AdvancedDownloadSuccess {
    pub source_url: String,
    pub pages: Vec<RibbonPage>,
    pub downloaded_images: usize,
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
        found_pages: usize,
    },
    Loaded(AdvancedDownloadSuccess),
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
        found_pages: usize,
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
    InterceptCountUpdated { found_pages: usize },
    Loaded(LoadedAdvancedDownload),
}

struct LoadedAdvancedDownload {
    source_url: String,
    pages: Vec<RibbonPage>,
    downloaded_images: usize,
}

#[derive(Debug)]
struct AdvancedDownloadError {
    user_message: String,
    log_message: String,
}

struct PythonDaemon {
    child: ManagedPythonChild,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    version_warning_sent: bool,
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
    StartLinkCollect {
        browser: String,
        pattern: String,
        max_parallel: usize,
    },
    QueryLinkCollectCount {
        browser: String,
    },
    StopLinkCollect {
        browser: String,
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
}

impl AdvancedDownloadController {
    pub fn new() -> Self {
        Self {
            daemon: Arc::new(Mutex::new(None)),
            pending: None,
            available_browsers: detect_available_browsers(),
        }
    }

    pub fn default_link_prefix() -> &'static str {
        DEFAULT_LINK_PREFIX
    }

    pub fn available_browsers(&self) -> &[String] {
        &self.available_browsers
    }

    pub fn is_loading(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.blocks_ui)
    }

    pub fn has_pending_command(&self) -> bool {
        self.pending.is_some()
    }

    pub fn begin_open(&mut self, browser: String, url: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                AdvancedCommand::OpenUrl { browser, url },
            ),
        });
    }

    pub fn begin_fetch(&mut self, browser: String, pattern: String, max_parallel: usize) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                AdvancedCommand::Fetch {
                    browser,
                    pattern,
                    max_parallel,
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
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                AdvancedCommand::StartLinkCollect {
                    browser,
                    pattern,
                    max_parallel,
                },
            ),
        });
    }

    pub fn begin_query_link_collect_count(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: false,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                AdvancedCommand::QueryLinkCollectCount { browser },
            ),
        });
    }

    pub fn begin_stop_link_collect(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                AdvancedCommand::StopLinkCollect { browser },
            ),
        });
    }

    pub fn begin_fetch_canvas(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                AdvancedCommand::FetchCanvas { browser },
            ),
        });
    }

    pub fn begin_start_canvas_intercept(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                AdvancedCommand::StartCanvasIntercept { browser },
            ),
        });
    }

    pub fn begin_stop_canvas_intercept(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: true,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                AdvancedCommand::StopCanvasIntercept { browser },
            ),
        });
    }

    pub fn begin_query_canvas_intercept_count(&mut self, browser: String) {
        self.pending = Some(PendingAdvancedDownload {
            blocks_ui: false,
            rx: spawn_advanced_command(
                Arc::clone(&self.daemon),
                AdvancedCommand::QueryCanvasInterceptCount { browser },
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
                Ok(AdvancedDownloadWorkerEvent::InterceptCountUpdated { found_pages }) => {
                    ctx.request_repaint();
                    last_progress =
                        Some(AdvancedDownloadEvent::InterceptCountUpdated { found_pages });
                }
                Ok(AdvancedDownloadWorkerEvent::LinkCollectCountUpdated { found_links }) => {
                    ctx.request_repaint();
                    last_progress =
                        Some(AdvancedDownloadEvent::LinkCollectCountUpdated { found_links });
                }
                Ok(AdvancedDownloadWorkerEvent::Finished(result)) => match result {
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
                    Ok(AdvancedWorkerOutcome::InterceptCountUpdated { found_pages }) => {
                        ctx.request_repaint();
                        return Some(AdvancedDownloadEvent::InterceptCountUpdated { found_pages });
                    }
                    Ok(AdvancedWorkerOutcome::Loaded(success)) => {
                        ctx.request_repaint();
                        return Some(AdvancedDownloadEvent::Loaded(AdvancedDownloadSuccess {
                            source_url: success.source_url,
                            pages: success.pages,
                            downloaded_images: success.downloaded_images,
                        }));
                    }
                    Err(err) => {
                        return Some(AdvancedDownloadEvent::Failed {
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
                    return Some(AdvancedDownloadEvent::WorkerDisconnected);
                }
            }
        }
    }
}

impl Drop for AdvancedDownloadController {
    fn drop(&mut self) {
        let lock = self.daemon.lock();
        let mut guard = match lock {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(mut daemon) = guard.take() {
            let shutdown_result = daemon.write_command(&json!({ "command": "shutdown" }));
            if let Err(err) = shutdown_result {
                crate::runtime_log::log_warn(format!(
                    "[new-project] failed to send advanced downloader shutdown command: {err}"
                ));
            }
            let kill_result = daemon.child.kill();
            if let Err(err) = kill_result {
                crate::runtime_log::log_warn(format!(
                    "[new-project] failed to kill advanced downloader daemon: {err}"
                ));
            }
            let wait_result = daemon.child.wait();
            if let Err(err) = wait_result {
                crate::runtime_log::log_warn(format!(
                    "[new-project] failed to wait advanced downloader daemon: {err}"
                ));
            }
        }
    }
}

fn spawn_advanced_command(
    daemon: Arc<Mutex<Option<PythonDaemon>>>,
    command: AdvancedCommand,
) -> Receiver<AdvancedDownloadWorkerEvent> {
    let (tx, rx) = mpsc::channel();
    let tx_worker = tx.clone();
    let spawn_result = thread::Builder::new()
        .name("new-project-advanced-download".to_string())
        .spawn(move || {
            let result = run_advanced_command(&daemon, &command, &tx_worker);
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
    command: &AdvancedCommand,
    progress_tx: &Sender<AdvancedDownloadWorkerEvent>,
) -> Result<AdvancedWorkerOutcome, AdvancedDownloadError> {
    let lock = daemon.lock();
    let mut guard = match lock {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let daemon = ensure_python_daemon(&mut guard)?;

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
                match read_daemon_event(daemon, progress_tx)? {
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
                    DaemonEvent::InterceptCountUpdated { found_pages } => {
                        send_intercept_count(progress_tx, found_pages);
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
                match read_daemon_event(daemon, progress_tx)? {
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
                    DaemonEvent::InterceptCountUpdated { found_pages } => {
                        send_intercept_count(progress_tx, found_pages);
                    }
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
                match read_daemon_event(daemon, progress_tx)? {
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
                    DaemonEvent::InterceptCountUpdated { found_pages } => {
                        send_intercept_count(progress_tx, found_pages);
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
                match read_daemon_event(daemon, progress_tx)? {
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
                    DaemonEvent::InterceptCountUpdated { found_pages } => {
                        send_intercept_count(progress_tx, found_pages);
                    }
                    DaemonEvent::Result {
                        page_url: _,
                        output_dir: _,
                        downloaded_images: _,
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
                match read_daemon_event(daemon, progress_tx)? {
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
                    DaemonEvent::InterceptCountUpdated { found_pages } => {
                        send_intercept_count(progress_tx, found_pages);
                    }
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
                match read_daemon_event(daemon, progress_tx)? {
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
                    DaemonEvent::InterceptCountUpdated { found_pages } => {
                        send_intercept_count(progress_tx, found_pages);
                    }
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
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
                match read_daemon_event(daemon, progress_tx)? {
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
                    DaemonEvent::InterceptCountUpdated { found_pages } => {
                        send_intercept_count(progress_tx, found_pages);
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
                match read_daemon_event(daemon, progress_tx)? {
                    DaemonEvent::InterceptCountUpdated { found_pages } => {
                        return Ok(AdvancedWorkerOutcome::InterceptCountUpdated { found_pages });
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
                match read_daemon_event(daemon, progress_tx)? {
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
                    DaemonEvent::InterceptCountUpdated { found_pages } => {
                        send_intercept_count(progress_tx, found_pages);
                    }
                    DaemonEvent::Log { level, message } => {
                        log_daemon_line(&level, &message);
                    }
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

fn send_intercept_count(tx: &Sender<AdvancedDownloadWorkerEvent>, found_pages: usize) {
    let send_result = tx.send(AdvancedDownloadWorkerEvent::InterceptCountUpdated { found_pages });
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

fn ensure_python_daemon(
    slot: &mut Option<PythonDaemon>,
) -> Result<&mut PythonDaemon, AdvancedDownloadError> {
    let needs_restart = match slot.as_mut() {
        Some(daemon) => match daemon.child.try_wait() {
            Ok(Some(status)) => {
                crate::runtime_log::log_warn(format!(
                    "[new-project] advanced downloader daemon exited with status {status}"
                ));
                true
            }
            Ok(None) => false,
            Err(err) => {
                crate::runtime_log::log_warn(format!(
                    "[new-project] failed to poll advanced downloader daemon: {err}"
                ));
                true
            }
        },
        None => true,
    };

    if needs_restart {
        *slot = Some(start_python_daemon()?);
    }

    slot.as_mut().ok_or_else(|| AdvancedDownloadError {
        user_message: "Не удалось запустить Selenium helper.".to_string(),
        log_message: "python daemon slot remained empty after start".to_string(),
    })
}

fn start_python_daemon() -> Result<PythonDaemon, AdvancedDownloadError> {
    let app_root = resolve_app_root();
    let python = python_manager::resolve_python_executable(&app_root).map_err(|err| {
        AdvancedDownloadError {
            user_message: "Не найдено Python-окружение для Selenium helper.".to_string(),
            log_message: err,
        }
    })?;
    let script_path = app_root
        .join("modules")
        .join("new_project")
        .join("adv_fetch_cli.py");
    if !script_path.is_file() {
        return Err(AdvancedDownloadError {
            user_message: "Не найден Selenium helper для продвинутого выкачивания.".to_string(),
            log_message: format!("missing helper script '{}'", script_path.display()),
        });
    }

    crate::runtime_log::log_info(format!(
        "[new-project] starting advanced downloader daemon via '{}' '{}'",
        python.display(),
        script_path.display(),
    ));

    let mut command = python_manager::build_python_script_path_command(&app_root, &script_path)
        .map_err(|err| AdvancedDownloadError {
            user_message: "Не найдено Python-окружение для Selenium helper.".to_string(),
            log_message: err,
        })?;
    command
        .current_dir(&app_root)
        .arg("--daemon")
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child =
        python_manager::spawn_kill_with_parent(command).map_err(|err| AdvancedDownloadError {
            user_message: "Не удалось запустить Python helper для Selenium.".to_string(),
            log_message: format!(
                "failed to spawn advanced downloader daemon '{}': {err}",
                python.display()
            ),
        })?;

    let stdin = child.stdin.take().ok_or_else(|| AdvancedDownloadError {
        user_message: "Python helper запустился без канала stdin.".to_string(),
        log_message: "advanced downloader daemon missing stdin".to_string(),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| AdvancedDownloadError {
        user_message: "Python helper запустился без канала stdout.".to_string(),
        log_message: "advanced downloader daemon missing stdout".to_string(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| AdvancedDownloadError {
        user_message: "Python helper запустился без канала stderr.".to_string(),
        log_message: "advanced downloader daemon missing stderr".to_string(),
    })?;

    spawn_stderr_logger(stderr);

    Ok(PythonDaemon {
        child,
        stdin: BufWriter::new(stdin),
        stdout: BufReader::new(stdout),
        version_warning_sent: false,
    })
}

fn spawn_stderr_logger(stderr: ChildStderr) {
    let spawn_result = thread::Builder::new()
        .name("advanced-download-stderr".to_string())
        .spawn(move || {
            let reader = BufReader::new(stderr);
            for line_result in reader.lines() {
                match line_result {
                    Ok(line) if !line.trim().is_empty() => {
                        crate::runtime_log::log_warn(format!(
                            "[new-project][advanced-python][stderr] {line}"
                        ));
                    }
                    Ok(_) => {}
                    Err(err) => {
                        crate::runtime_log::log_warn(format!(
                            "[new-project] failed to read advanced downloader stderr: {err}"
                        ));
                        break;
                    }
                }
            }
        });
    if let Err(err) = spawn_result {
        crate::runtime_log::log_warn(format!(
            "[new-project] failed to spawn advanced downloader stderr reader: {err}"
        ));
    }
}

impl PythonDaemon {
    fn write_command(&mut self, command: &Value) -> Result<(), String> {
        serde_json::to_writer(&mut self.stdin, command)
            .map_err(|err| format!("serialize command failed: {err}"))?;
        self.stdin
            .write_all(b"\n")
            .map_err(|err| format!("write newline failed: {err}"))?;
        self.stdin
            .flush()
            .map_err(|err| format!("flush stdin failed: {err}"))?;
        Ok(())
    }

    fn read_payload(&mut self) -> Result<Value, AdvancedDownloadError> {
        let mut line = String::new();
        let bytes = self
            .stdout
            .read_line(&mut line)
            .map_err(|err| AdvancedDownloadError {
                user_message: "Python helper перестал отвечать.".to_string(),
                log_message: format!("failed to read daemon stdout: {err}"),
            })?;
        if bytes == 0 {
            return Err(AdvancedDownloadError {
                user_message: "Selenium helper неожиданно завершился.".to_string(),
                log_message: "advanced downloader daemon closed stdout".to_string(),
            });
        }
        let payload: Value =
            serde_json::from_str(line.trim()).map_err(|err| AdvancedDownloadError {
                user_message: "Python helper вернул некорректный ответ.".to_string(),
                log_message: format!("invalid daemon json '{}': {err}", line.trim()),
            })?;
        Ok(payload)
    }
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
        found_pages: usize,
    },
    Result {
        page_url: String,
        output_dir: PathBuf,
        downloaded_images: usize,
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
        "intercept_count" => Ok(DaemonEvent::InterceptCountUpdated {
            found_pages: payload
                .get("found_pages")
                .and_then(Value::as_u64)
                .map_or(0, u64_to_usize),
        }),
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

fn read_daemon_event(
    daemon: &mut PythonDaemon,
    progress_tx: &Sender<AdvancedDownloadWorkerEvent>,
) -> Result<DaemonEvent, AdvancedDownloadError> {
    let payload = daemon.read_payload()?;
    emit_downloader_version_warning_if_needed(daemon, progress_tx, &payload);
    parse_daemon_event(payload)
}

fn emit_downloader_version_warning_if_needed(
    daemon: &mut PythonDaemon,
    progress_tx: &Sender<AdvancedDownloadWorkerEvent>,
    payload: &Value,
) {
    if daemon.version_warning_sent {
        return;
    }
    let studio_version = env!("CARGO_PKG_VERSION").trim().to_string();
    let downloader_version = payload
        .get("downloader_version")
        .or_else(|| payload.get("version"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("неизвестна")
        .to_string();
    if downloader_version == studio_version {
        return;
    }

    daemon.version_warning_sent = true;
    crate::runtime_log::log_warn(format!(
        "[new-project] Python downloader version mismatch: studio={studio_version} downloader={downloader_version}"
    ));
    if progress_tx
        .send(AdvancedDownloadWorkerEvent::VersionMismatch {
            studio_version,
            downloader_version,
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

fn cleanup_temp_dir(dir: &Path) {
    let remove_result = fs::remove_dir_all(dir);
    if let Err(err) = remove_result {
        crate::runtime_log::log_warn(format!(
            "[new-project] failed to remove advanced downloader temp dir '{}': {err}",
            dir.display()
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

fn resolve_app_root() -> PathBuf {
    let start = config::program_dir();

    for ancestor in start.ancestors() {
        if ancestor.join("modules").join("browser_f.py").is_file() {
            return ancestor.to_path_buf();
        }
        if ancestor.join("launcher.py").is_file() {
            return ancestor.to_path_buf();
        }
    }

    start
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
    use super::advanced_downloader_version_warning_message;

    #[test]
    fn formats_advanced_downloader_version_warning() {
        assert_eq!(
            advanced_downloader_version_warning_message("3.4.0", "3.3.0"),
            "Версии студии и Python-выкачивателя не соответствуют: 3.4.0/3.3.0. Возможна некорректная работа."
        );
    }
}
