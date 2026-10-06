/*
File: crates/ms-tab-translation/src/ocr_model_download.rs

Purpose:
Download / install-state controller for the EXTERNAL OCR models (Baberu OCR and the
PaddleOCR-VL variants) that the OCR panel offers to download explicitly. The pinned specs
and the blocking downloader live in `ms_sysprobe::ai_models::{external, external_catalog}`;
this file only runs them off the GUI thread and keeps a per-spec state for the panel.

Key items:
- `OcrModelDownloadState`: the install state of one spec (unknown, checking, missing,
  partial, installed, downloading with byte progress, status probe unavailable).
- `DownloadNotice`: the outcome of the last finished download of a spec (cancelled /
  failed), shown NEXT TO the state, never instead of it.
- `OcrModelPanelSnapshot`: what the panel's download block needs for one spec in one frame
  (state, notice, and the OTHER spec's download that blocks this one, if any).
- `OcrModelDownloadController`: one status worker (stat-only probes) plus at most one
  download thread at a time; progress is coalesced in a mutex snapshot that `poll` reads.
- `offered_download_label`: the one owner of "which download button caption is visible
  for a state" — used by the panel block AND by the OCR "model is not downloaded" error, so
  the error names exactly the button the user sees.
- `external_error_message`: the typed `ExternalModelError` -> localized text mapping
  (`ExternalModelError::Display` is English diagnostic text: it goes to the log only, and
  no English detail — OS error text, HTTP detail — is interpolated into the message).
- `format_bytes`: integer-only, localized byte-size formatter.

Notes:
- The GUI thread never stats files or touches the network here: status probes and the
  download run on worker threads; `poll` only drains a channel and reads a snapshot.
- Every finished download that did not install the model (cancel, failure) re-probes the
  spec, so the panel shows the real disk state (`Partial` / `Missing`) and its button; the
  outcome text is kept as a `DownloadNotice`.
- Dropping the controller sets the cancel flag and DETACHES a running download thread
  instead of joining it: a stalled HTTP read can take up to the downloader's 60 s read
  timeout to notice the flag, and the GUI thread must not wait for that. The downloader's
  staging (`.part` files under `.download/` + the in-progress marker) is crash-safe, so a thread that dies
  with the process loses nothing that a resume cannot recover.
- How the user learns about such an interruption (tab rebuild, crash): the next
  controller's first probe finds the staged bytes and reports `Partial`, whose status line
  says the download was interrupted, next to the Resume button. No process-global state is
  kept for it: the on-disk staging is the durable record, and it also covers a crash.
- The worker bodies are injected as plain function pointers (`Jobs`) so the state machine
  is unit-tested with fakes and no filesystem or network.
*/

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use ms_config as config;
use ms_sysprobe::ai_models::external::{
    ExternalDownloadProgress, ExternalModelError, ExternalModelSpec, ExternalModelStatus, download_external_model,
    external_model_status,
};
use ms_thread::{self as thread, JoinHandle};

/// What the OCR panel knows about one external model spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcrModelDownloadState {
    /// Never probed in this session.
    Unknown,
    /// A status probe is queued or running on the worker.
    Checking,
    /// Nothing usable is installed for this spec.
    Missing,
    /// An unfinished (cancelled, failed or interrupted) download of this spec left
    /// `bytes_present` bytes on disk; a resume continues from them.
    Partial { bytes_present: u64 },
    /// Completely downloaded and verified (completion marker matches).
    Installed,
    /// A download of this spec is running; the last progress report.
    Downloading(ExternalDownloadProgress),
    /// The status worker is gone, so the state cannot be probed; the localized reason.
    Unavailable(String),
}

/// Outcome of the last finished download of one spec that did not install it. Shown next
/// to the (re-probed) state until the next download of that spec starts or the spec is
/// found installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadNotice {
    /// The user cancelled it; the staged bytes are kept for a resume.
    Cancelled,
    /// It failed; the localized reason.
    Failed(String),
}

/// A download of ANOTHER spec that currently blocks downloading the shown one (at most one
/// download runs at a time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtherDownload {
    /// The spec being downloaded.
    pub spec: &'static ExternalModelSpec,
    /// Its last progress report.
    pub progress: ExternalDownloadProgress,
}

/// What the OCR panel's download block shows for one spec in one frame; built by
/// [`OcrModelDownloadController::panel_snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrModelPanelSnapshot {
    /// The spec this snapshot describes; the panel draws it only for this spec, so a
    /// snapshot taken before an engine / variant switch in the same frame is not shown
    /// for the newly selected model.
    pub spec_id: &'static str,
    /// The install / download state of the spec.
    pub state: OcrModelDownloadState,
    /// The outcome of its last unsuccessful download, if any.
    pub notice: Option<DownloadNotice>,
    /// Another spec's running download, which disables this spec's download button.
    pub other_download: Option<OtherDownload>,
}

/// Blocking stat-only status probe of one spec (worker thread only).
pub(crate) type StatusProbe = fn(&'static ExternalModelSpec) -> ExternalModelStatus;
/// Blocking download of one spec with a cancel flag and a per-chunk progress callback
/// (worker thread only).
pub(crate) type DownloadJob =
    fn(&'static ExternalModelSpec, &AtomicBool, &mut dyn FnMut(&ExternalDownloadProgress)) -> Result<PathBuf, ExternalModelError>;

/// The worker bodies. Production uses the real `ms_sysprobe` functions under
/// `ms_config::side_models_dir()`; tests (also the tab's) inject fakes.
#[derive(Clone, Copy)]
pub(crate) struct Jobs {
    pub(crate) status: StatusProbe,
    pub(crate) download: DownloadJob,
}

/// Production status probe: resolves the `side_models` root on the worker (path
/// resolution may stat program markers) and stats the spec there.
fn real_status_probe(spec: &'static ExternalModelSpec) -> ExternalModelStatus {
    external_model_status(&config::side_models_dir(), spec)
}

/// Production download job under `ms_config::side_models_dir()`.
fn real_download_job(
    spec: &'static ExternalModelSpec,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&ExternalDownloadProgress),
) -> Result<PathBuf, ExternalModelError> {
    download_external_model(&config::side_models_dir(), spec, cancel, progress)
}

/// Commands of the status worker.
enum StatusCommand {
    Probe(&'static ExternalModelSpec),
    Stop,
}

/// Events the status worker and the download thread send back to the controller.
enum WorkerEvent {
    Status { spec_id: &'static str, status: ExternalModelStatus },
    DownloadFinished { spec: &'static ExternalModelSpec, result: Result<(), ExternalModelError> },
}

/// The one running download: its spec, cancel switch and coalesced progress slot.
struct ActiveDownload {
    spec: &'static ExternalModelSpec,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<Option<ExternalDownloadProgress>>>,
    /// Kept only to detach explicitly on drop (see the file header).
    thread: Option<JoinHandle<()>>,
}

/// Status + download controller for the external OCR models (GUI-side handle).
///
/// Every method is cheap and non-blocking; the work runs on a status worker thread and a
/// per-download thread. Call [`Self::poll`] once per frame and repaint while
/// [`Self::is_active`].
pub struct OcrModelDownloadController {
    jobs: Jobs,
    states: HashMap<&'static str, OcrModelDownloadState>,
    /// Outcome of the last unsuccessful download per spec (see [`DownloadNotice`]).
    notices: HashMap<&'static str, DownloadNotice>,
    status_tx: Sender<StatusCommand>,
    evt_tx: Sender<WorkerEvent>,
    evt_rx: Receiver<WorkerEvent>,
    status_worker: Option<JoinHandle<()>>,
    /// Status probes sent and not answered yet (drives `is_active`).
    pending_probes: usize,
    download: Option<ActiveDownload>,
}

impl std::fmt::Debug for OcrModelDownloadController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OcrModelDownloadController")
            .field("states", &self.states)
            .field("pending_probes", &self.pending_probes)
            .field("downloading", &self.download.as_ref().map(|active| active.spec.id))
            .finish_non_exhaustive()
    }
}

impl Default for OcrModelDownloadController {
    fn default() -> Self {
        Self::new()
    }
}

impl OcrModelDownloadController {
    /// Creates the controller and its status worker over the real `ms_sysprobe` probes.
    #[must_use]
    pub fn new() -> Self {
        Self::with_jobs(Jobs { status: real_status_probe, download: real_download_job })
    }

    /// Creates the controller over the given worker bodies.
    pub(crate) fn with_jobs(jobs: Jobs) -> Self {
        let (status_tx, status_rx) = mpsc::channel::<StatusCommand>();
        let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();
        let worker_tx = evt_tx.clone();
        let probe = jobs.status;
        let status_worker = thread::spawn(move || status_worker_loop(&status_rx, &worker_tx, probe));
        Self {
            jobs,
            states: HashMap::new(),
            notices: HashMap::new(),
            status_tx,
            evt_tx,
            evt_rx,
            status_worker: Some(status_worker),
            pending_probes: 0,
            download: None,
        }
    }

    /// Queues a stat-only status probe of `spec` on the worker; the state becomes
    /// `Checking` until [`Self::poll`] receives the answer. A no-op while `spec` is being
    /// downloaded (the download owns its state then).
    pub fn request_status(&mut self, spec: &'static ExternalModelSpec) {
        if self.download_spec_id() == Some(spec.id) {
            return;
        }
        if self.status_tx.send(StatusCommand::Probe(spec)).is_err() {
            ms_log::runtime_log::log_error(format!("[ocr-model] status worker is gone; cannot probe '{}'", spec.id));
            self.states.insert(spec.id, OcrModelDownloadState::Unavailable(t!("translation.ocr.worker_unavailable_error").to_string()));
            return;
        }
        self.pending_probes = self.pending_probes.saturating_add(1);
        self.states.insert(spec.id, OcrModelDownloadState::Checking);
    }

    /// Starts downloading (or resuming) `spec` on a new thread and clears its notice. At
    /// most one download runs at a time: while one runs this is refused with a log line
    /// (the panel disables the button and names the running download, see
    /// [`OcrModelPanelSnapshot::other_download`], so a refused click is not silent).
    pub fn start_download(&mut self, spec: &'static ExternalModelSpec) {
        if let Some(running) = self.download_spec_id() {
            ms_log::runtime_log::log_info(format!(
                "[ocr-model] download of '{}' refused: '{running}' is still downloading",
                spec.id
            ));
            return;
        }
        self.notices.remove(spec.id);
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(None::<ExternalDownloadProgress>));
        let job = self.jobs.download;
        let evt_tx = self.evt_tx.clone();
        let thread_cancel = Arc::clone(&cancel);
        let thread_progress = Arc::clone(&progress);
        ms_log::runtime_log::log_info(format!(
            "[ocr-model] download started: '{}' ({} bytes, {} files)",
            spec.id,
            spec.total_bytes(),
            spec.files.len()
        ));
        let handle = thread::spawn(move || {
            let mut report = |snapshot: &ExternalDownloadProgress| {
                // Latest wins: the GUI reads the slot once per frame. A poisoned slot only
                // means a previous writer panicked mid-store; the value is a plain Copy.
                let mut slot = match thread_progress.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *slot = Some(*snapshot);
            };
            let result = job(spec, &thread_cancel, &mut report).map(|_dir| ());
            if let Err(err) = &result {
                ms_log::runtime_log::log_error(format!("[ocr-model] download of '{}' ended: {err}", spec.id));
            } else {
                ms_log::runtime_log::log_info(format!("[ocr-model] download of '{}' finished", spec.id));
            }
            // The controller may already be dropped (detached thread): nobody to tell.
            if evt_tx.send(WorkerEvent::DownloadFinished { spec, result }).is_err() {
                ms_log::runtime_log::log_info(format!("[ocr-model] download of '{}' ended after its controller was dropped", spec.id));
            }
        });
        self.states.insert(spec.id, OcrModelDownloadState::Downloading(initial_progress(spec)));
        self.download = Some(ActiveDownload { spec, cancel, progress, thread: Some(handle) });
    }

    /// Asks the running download (if any) to stop at its next chunk; once the thread
    /// reports back the spec gets a `DownloadNotice::Cancelled` and is re-probed.
    pub fn cancel(&self) {
        if let Some(active) = &self.download {
            active.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Applies worker events and the latest download progress. Returns `true` when any
    /// state changed (the caller may repaint).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.evt_rx.try_recv() {
                Ok(WorkerEvent::Status { spec_id, status }) => {
                    self.pending_probes = self.pending_probes.saturating_sub(1);
                    // A probe answered after a download started describes the old disk
                    // state; the download owns the state until it finishes.
                    if self.download_spec_id() != Some(spec_id) {
                        let state = state_from_status(status);
                        // A model found installed (e.g. by a resume that succeeded later)
                        // makes an older failure / cancel notice stale.
                        if state == OcrModelDownloadState::Installed {
                            changed |= self.notices.remove(spec_id).is_some();
                        }
                        changed |= self.set_state(spec_id, state);
                    }
                }
                Ok(WorkerEvent::DownloadFinished { spec, result }) => {
                    // The thread has sent its last message and is returning: dropping the
                    // handle detaches a thread that is already done.
                    if self.download_spec_id() == Some(spec.id) {
                        self.download = None;
                    }
                    changed = true;
                    match notice_from_download_result(result) {
                        None => {
                            self.notices.remove(spec.id);
                            self.states.insert(spec.id, OcrModelDownloadState::Installed);
                        }
                        Some(notice) => {
                            // Re-probe: the panel must show the real disk state (`Partial`
                            // after a cancel, usually) and the button of THAT state — the
                            // same caption the OCR "not downloaded" error names.
                            self.notices.insert(spec.id, notice);
                            self.request_status(spec);
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                // Impossible while `self.evt_tx` is alive; kept exhaustive.
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if let Some(active) = &self.download {
            let latest = match active.progress.lock() {
                Ok(guard) => *guard,
                Err(poisoned) => *poisoned.into_inner(),
            };
            if let Some(progress) = latest {
                let spec_id = active.spec.id;
                changed |= self.set_state(spec_id, OcrModelDownloadState::Downloading(progress));
            }
        }
        changed
    }

    /// The state of the spec with id `spec_id` (`Unknown` when never probed).
    #[must_use]
    pub fn state_for(&self, spec_id: &str) -> OcrModelDownloadState {
        self.states.get(spec_id).cloned().unwrap_or(OcrModelDownloadState::Unknown)
    }

    /// Everything the panel's download block shows for `spec` this frame: its state, its
    /// last unsuccessful download outcome and the other spec's download that blocks it.
    #[must_use]
    pub fn panel_snapshot(&self, spec: &'static ExternalModelSpec) -> OcrModelPanelSnapshot {
        let other_download = self.download.as_ref().filter(|active| active.spec.id != spec.id).map(|active| OtherDownload {
            spec: active.spec,
            progress: if let Some(OcrModelDownloadState::Downloading(progress)) = self.states.get(active.spec.id) {
                *progress
            } else {
                initial_progress(active.spec)
            },
        });
        OcrModelPanelSnapshot {
            spec_id: spec.id,
            state: self.state_for(spec.id),
            notice: self.notices.get(spec.id).cloned(),
            other_download,
        }
    }

    /// `true` while a download runs or a status probe is unanswered: the GUI keeps
    /// repainting so `poll` sees the result.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.download.is_some() || self.pending_probes > 0
    }

    fn download_spec_id(&self) -> Option<&'static str> {
        self.download.as_ref().map(|active| active.spec.id)
    }

    fn set_state(&mut self, spec_id: &'static str, state: OcrModelDownloadState) -> bool {
        if self.states.get(spec_id) == Some(&state) {
            return false;
        }
        self.states.insert(spec_id, state);
        true
    }
}

impl Drop for OcrModelDownloadController {
    fn drop(&mut self) {
        if let Some(mut active) = self.download.take() {
            active.cancel.store(true, Ordering::Relaxed);
            ms_log::runtime_log::log_info(format!(
                "[ocr-model] download of '{}' interrupted: its controller was dropped (tab rebuild or exit); \
                 the staged bytes stay on disk and the next probe offers a resume",
                active.spec.id
            ));
            // Detached on purpose, see the file header: never wait for a network read here.
            drop(active.thread.take());
        }
        if self.status_tx.send(StatusCommand::Stop).is_err() {
            ms_log::runtime_log::log_info("[ocr-model] status worker already gone at drop");
        }
        if let Some(handle) = self.status_worker.take()
            && handle.join().is_err()
        {
            ms_log::runtime_log::log_error("[ocr-model] status worker panicked");
        }
    }
}

/// Status worker: answers probes in order until `Stop` or the controller is gone.
fn status_worker_loop(commands: &Receiver<StatusCommand>, events: &Sender<WorkerEvent>, probe: StatusProbe) {
    while let Ok(command) = commands.recv() {
        match command {
            StatusCommand::Probe(spec) => {
                let status = probe(spec);
                if events.send(WorkerEvent::Status { spec_id: spec.id, status }).is_err() {
                    break;
                }
            }
            StatusCommand::Stop => break,
        }
    }
}

/// The progress shown between the click and the first report of the download thread.
fn initial_progress(spec: &'static ExternalModelSpec) -> ExternalDownloadProgress {
    ExternalDownloadProgress {
        file_index: 0,
        file_count: spec.files.len(),
        file_path: spec.files.first().map_or("", |file| file.path),
        phase: ms_sysprobe::ai_models::external::ExternalDownloadPhase::Verifying,
        file_done: 0,
        file_total: spec.files.first().map_or(0, |file| file.size),
        total_done: 0,
        total_bytes: spec.total_bytes(),
    }
}

/// Maps a stat-only probe answer to the panel state.
fn state_from_status(status: ExternalModelStatus) -> OcrModelDownloadState {
    match status {
        ExternalModelStatus::Installed => OcrModelDownloadState::Installed,
        ExternalModelStatus::Partial { bytes_present } => OcrModelDownloadState::Partial { bytes_present },
        ExternalModelStatus::Missing => OcrModelDownloadState::Missing,
    }
}

/// The notice a finished download leaves: `None` on success (the model is installed), a
/// user cancel is `Cancelled` (not a failure), anything else is `Failed` with the
/// localized reason.
fn notice_from_download_result(result: Result<(), ExternalModelError>) -> Option<DownloadNotice> {
    match result {
        Ok(()) => None,
        Err(ExternalModelError::Cancelled) => Some(DownloadNotice::Cancelled),
        Err(err) => Some(DownloadNotice::Failed(external_error_message(&err))),
    }
}

/// The i18n key that explains `err` to the user. Pure, so the mapping is testable.
fn external_error_key(err: &ExternalModelError) -> &'static str {
    match err {
        ExternalModelError::NotDownloaded { .. } => "translation.ocr_model.not_downloaded_status",
        ExternalModelError::Busy { .. } => "translation.ocr_model.busy_error",
        ExternalModelError::Cancelled => "translation.ocr_model.cancelled_status",
        ExternalModelError::Io { .. } => "translation.ocr_model.io_error",
        ExternalModelError::Http { .. } => "translation.ocr_model.http_error",
        ExternalModelError::SizeMismatch { .. } => "translation.ocr_model.size_mismatch_error",
        ExternalModelError::HashMismatch { .. } => "translation.ocr_model.hash_mismatch_error",
        ExternalModelError::InvalidSpec(_) => "translation.ocr_model.invalid_spec_error",
        ExternalModelError::Unsupported => "translation.ocr_model.unsupported_error",
    }
}

/// Localized user-facing text for an external-model error. Only language-neutral facts
/// (a local path, a pinned file name) are substituted; the English diagnostic parts (the
/// `Display`, the OS error text, the HTTP detail, the spec detail) are never shown — the
/// download thread logs the full `Display` with the spec id when the download ends.
pub(crate) fn external_error_message(err: &ExternalModelError) -> String {
    // The key is chosen at runtime by `external_error_key` (one owner of the mapping), so
    // the runtime-key pair `resolve_key` + `interpolate` replaces the literal-only `tf!`.
    let template = ms_i18n::resolve_key(external_error_key(err));
    match err {
        ExternalModelError::NotDownloaded { .. }
        | ExternalModelError::Busy { .. }
        | ExternalModelError::Cancelled
        | ExternalModelError::InvalidSpec(_)
        | ExternalModelError::Unsupported => template.to_string(),
        ExternalModelError::Io { path, .. } => ms_i18n::interpolate(template, &[("path", &path.display())]),
        ExternalModelError::Http { file, .. }
        | ExternalModelError::SizeMismatch { file, .. }
        | ExternalModelError::HashMismatch { file, .. } => ms_i18n::interpolate(template, &[("file", file)]),
    }
}

/// Caption of the "Download (size)" button for `spec`.
fn download_button_label(spec: &ExternalModelSpec) -> String {
    tf!("translation.ocr_model.download_button", size = format_bytes(spec.total_bytes()))
}

/// Caption of the "Resume download (done of total)" button for `spec` with
/// `bytes_present` bytes already on disk.
fn resume_button_label(spec: &ExternalModelSpec, bytes_present: u64) -> String {
    tf!(
        "translation.ocr_model.resume_button",
        done = format_bytes(bytes_present),
        total = format_bytes(spec.total_bytes())
    )
}

/// The caption of the download button the panel shows for `spec` in `state`, or `None`
/// when that state offers no download button. The ONE owner of this decision: the panel's
/// download block draws exactly this caption, and the OCR "model is not downloaded" error
/// names it (from the same `ExternalModelStatus` -> state mapping the panel's probe uses).
pub(crate) fn offered_download_label(spec: &ExternalModelSpec, state: &OcrModelDownloadState) -> Option<String> {
    match state {
        OcrModelDownloadState::Missing => Some(download_button_label(spec)),
        OcrModelDownloadState::Partial { bytes_present } => Some(resume_button_label(spec, *bytes_present)),
        OcrModelDownloadState::Unknown
        | OcrModelDownloadState::Checking
        | OcrModelDownloadState::Installed
        | OcrModelDownloadState::Downloading(_)
        | OcrModelDownloadState::Unavailable(_) => None,
    }
}

/// The download caption for a stat-only probe answer (see [`offered_download_label`]);
/// an `Installed` answer, which offers no button, falls back to the plain "Download (size)"
/// caption so a caller always has a button to name.
pub(crate) fn download_label_for_status(spec: &ExternalModelSpec, status: ExternalModelStatus) -> String {
    offered_download_label(spec, &state_from_status(status)).unwrap_or_else(|| download_button_label(spec))
}

/// Binary byte-size unit chosen by [`split_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteUnit {
    Bytes,
    Kib,
    Mib,
    Gib,
}

/// Splits `bytes` into a decimal number string and a 1024-based unit: GiB with 2
/// decimals, MiB with 1, KiB and bytes as integers, rounded half up. The unit is re-picked
/// after rounding, so a value never prints as "1024 KB" or "1024.0 MB" (it becomes
/// "1.0 MB" / "1.00 GB"). Integer math only, so no lossy float cast is involved.
fn split_bytes(bytes: u64) -> (String, ByteUnit) {
    const KIB: u128 = 1024;
    // (unit, divisor, decimals), smallest first.
    const UNITS: [(ByteUnit, u128, u32); 4] =
        [(ByteUnit::Bytes, 1, 0), (ByteUnit::Kib, KIB, 0), (ByteUnit::Mib, KIB * KIB, 1), (ByteUnit::Gib, KIB * KIB * KIB, 2)];
    let value = u128::from(bytes);
    // Largest unit whose divisor fits the raw value (`Bytes` always does).
    let mut index = UNITS.iter().rposition(|&(_, divisor, _)| value >= divisor).unwrap_or(0);
    loop {
        let (unit, divisor, decimals) = UNITS[index];
        let scale = 10_u128.pow(decimals);
        let scaled = (value * scale + divisor / 2) / divisor;
        let whole = scaled / scale;
        // Rounding reached the next unit (e.g. 1 048 575 B -> "1024 KB"): print it there.
        if whole >= KIB && index + 1 < UNITS.len() {
            index += 1;
            continue;
        }
        let number = if decimals == 0 {
            whole.to_string()
        } else {
            // `decimals` is 1 or 2, so the width conversion cannot fail.
            let width = usize::try_from(decimals).unwrap_or(2);
            format!("{whole}.{:0width$}", scaled % scale)
        };
        return (number, unit);
    }
}

/// Localized human-readable byte size. The units are 1024-based but carry the common UI
/// labels KB / MB / GB (КБ / МБ / ГБ), so a size can read lower than a decimal
/// (Hugging Face) figure for the same file.
pub(crate) fn format_bytes(bytes: u64) -> String {
    let (value, unit) = split_bytes(bytes);
    match unit {
        ByteUnit::Bytes => tf!("translation.ocr_model.size_bytes", value = value),
        ByteUnit::Kib => tf!("translation.ocr_model.size_kb", value = value),
        ByteUnit::Mib => tf!("translation.ocr_model.size_mb", value = value),
        ByteUnit::Gib => tf!("translation.ocr_model.size_gb", value = value),
    }
}

/// Fake worker bodies for the controller's state machine, shared with the tab's tests (no
/// filesystem, no network).
#[cfg(test)]
pub(crate) mod fakes {
    use super::{ExternalDownloadProgress, ExternalModelError, ExternalModelSpec, ExternalModelStatus, initial_progress};
    use ms_sysprobe::ai_models::external::ExternalFile;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    static FAKE_FILES: [ExternalFile; 2] = [
        ExternalFile { path: "a.bin", size: 100, sha256: "00" },
        ExternalFile { path: "b.bin", size: 50, sha256: "00" },
    ];
    /// A two-file spec that exists only in tests.
    pub(crate) static FAKE_SPEC: ExternalModelSpec =
        ExternalModelSpec { id: "fake_model", repo_id: "owner/name", revision: "0", dir: "Fake", files: &FAKE_FILES };
    /// A second spec, to exercise "another model is downloading".
    pub(crate) static OTHER_SPEC: ExternalModelSpec =
        ExternalModelSpec { id: "other_model", repo_id: "owner/other", revision: "0", dir: "Other", files: &FAKE_FILES };

    /// Bytes the `partial_probe` reports.
    pub(crate) const PARTIAL_BYTES: u64 = 42;

    pub(crate) fn missing_probe(_spec: &'static ExternalModelSpec) -> ExternalModelStatus {
        ExternalModelStatus::Missing
    }

    pub(crate) fn partial_probe(_spec: &'static ExternalModelSpec) -> ExternalModelStatus {
        ExternalModelStatus::Partial { bytes_present: PARTIAL_BYTES }
    }

    pub(crate) fn installed_probe(_spec: &'static ExternalModelSpec) -> ExternalModelStatus {
        ExternalModelStatus::Installed
    }

    pub(crate) fn ok_download(
        spec: &'static ExternalModelSpec,
        _cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<PathBuf, ExternalModelError> {
        let mut report = initial_progress(spec);
        report.total_done = 120;
        progress(&report);
        Ok(PathBuf::from("unused"))
    }

    /// Reports 7 bytes, then runs until cancelled (10 s safety deadline).
    pub(crate) fn waits_for_cancel(
        spec: &'static ExternalModelSpec,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<PathBuf, ExternalModelError> {
        let mut report = initial_progress(spec);
        report.total_done = 7;
        progress(&report);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cancel.load(Ordering::Relaxed) {
            if Instant::now() > deadline {
                return Err(ExternalModelError::Http { file: "a.bin", detail: "test timeout".to_owned() });
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Err(ExternalModelError::Cancelled)
    }

    pub(crate) fn hash_failure(
        _spec: &'static ExternalModelSpec,
        _cancel: &AtomicBool,
        _progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<PathBuf, ExternalModelError> {
        Err(ExternalModelError::HashMismatch { file: "a.bin", expected: "00", actual: "11".to_owned() })
    }

    pub(crate) fn busy_failure(
        spec: &'static ExternalModelSpec,
        _cancel: &AtomicBool,
        _progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<PathBuf, ExternalModelError> {
        Err(ExternalModelError::Busy { id: spec.id })
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::{
        FAKE_SPEC, OTHER_SPEC, PARTIAL_BYTES, busy_failure, hash_failure, installed_probe, missing_probe, ok_download,
        partial_probe, waits_for_cancel,
    };
    use super::*;
    use std::time::{Duration, Instant};

    /// Polls until `done` holds or a generous deadline passes (worker threads are real).
    fn poll_until(controller: &mut OcrModelDownloadController, done: impl Fn(&OcrModelDownloadController) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(controller) {
            assert!(Instant::now() < deadline, "controller did not settle: {controller:?}");
            controller.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn status_probe_goes_through_checking_to_the_probed_state() {
        let mut controller = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: ok_download });
        assert_eq!(controller.state_for(FAKE_SPEC.id), OcrModelDownloadState::Unknown);
        controller.request_status(&FAKE_SPEC);
        assert_eq!(controller.state_for(FAKE_SPEC.id), OcrModelDownloadState::Checking);
        assert!(controller.is_active());
        poll_until(&mut controller, |c| !c.is_active());
        assert_eq!(controller.state_for(FAKE_SPEC.id), OcrModelDownloadState::Missing);

        let mut partial = OcrModelDownloadController::with_jobs(Jobs { status: partial_probe, download: ok_download });
        partial.request_status(&FAKE_SPEC);
        poll_until(&mut partial, |c| !c.is_active());
        assert_eq!(partial.state_for(FAKE_SPEC.id), OcrModelDownloadState::Partial { bytes_present: PARTIAL_BYTES });
    }

    #[test]
    fn successful_download_ends_installed() {
        let mut controller = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: ok_download });
        controller.start_download(&FAKE_SPEC);
        assert!(matches!(controller.state_for(FAKE_SPEC.id), OcrModelDownloadState::Downloading(_)));
        assert!(controller.is_active());
        poll_until(&mut controller, |c| !c.is_active());
        assert_eq!(controller.state_for(FAKE_SPEC.id), OcrModelDownloadState::Installed);
        assert_eq!(controller.panel_snapshot(&FAKE_SPEC).notice, None);
    }

    /// M1: a cancel re-probes, so the panel shows the real `Partial` state and its Resume
    /// button, with the cancel kept as a notice; the "not downloaded" error built from the
    /// same probe answer names that very button.
    #[test]
    fn cancel_reprobes_and_the_offered_button_matches_the_gate_error_label() {
        let mut controller = OcrModelDownloadController::with_jobs(Jobs { status: partial_probe, download: waits_for_cancel });
        controller.start_download(&FAKE_SPEC);
        // Wait for the first progress report so the snapshot path is exercised.
        poll_until(&mut controller, |c| {
            matches!(c.state_for(FAKE_SPEC.id), OcrModelDownloadState::Downloading(p) if p.total_done == 7)
        });
        // At most one download: a second start (and a status probe of the same spec)
        // must not disturb the running one.
        controller.start_download(&FAKE_SPEC);
        controller.request_status(&FAKE_SPEC);
        assert!(matches!(controller.state_for(FAKE_SPEC.id), OcrModelDownloadState::Downloading(_)));
        controller.cancel();
        poll_until(&mut controller, |c| !c.is_active());
        let snapshot = controller.panel_snapshot(&FAKE_SPEC);
        assert_eq!(snapshot.state, OcrModelDownloadState::Partial { bytes_present: PARTIAL_BYTES });
        assert_eq!(snapshot.notice, Some(DownloadNotice::Cancelled));
        let shown = offered_download_label(&FAKE_SPEC, &snapshot.state).expect("a Partial state offers a button");
        assert_eq!(shown, resume_button_label(&FAKE_SPEC, PARTIAL_BYTES));
        assert_eq!(download_label_for_status(&FAKE_SPEC, partial_probe(&FAKE_SPEC)), shown);
        // A later download clears the notice.
        controller.start_download(&FAKE_SPEC);
        assert_eq!(controller.panel_snapshot(&FAKE_SPEC).notice, None);
        controller.cancel();
        poll_until(&mut controller, |c| !c.is_active());
    }

    #[test]
    fn failures_become_localized_notices_over_the_reprobed_state() {
        let mut controller = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: hash_failure });
        controller.start_download(&FAKE_SPEC);
        poll_until(&mut controller, |c| !c.is_active());
        let snapshot = controller.panel_snapshot(&FAKE_SPEC);
        assert_eq!(snapshot.state, OcrModelDownloadState::Missing);
        // Only the notice kind is asserted: the text comes from the process-global i18n
        // catalog, which another test of this binary may install between the worker
        // building the message and this comparison (the key mapping itself is covered by
        // `every_error_variant_maps_to_its_own_key_without_english_detail`).
        assert!(matches!(snapshot.notice, Some(DownloadNotice::Failed(_))), "{snapshot:?}");

        let mut busy = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: busy_failure });
        busy.start_download(&FAKE_SPEC);
        poll_until(&mut busy, |c| !c.is_active());
        let snapshot = busy.panel_snapshot(&FAKE_SPEC);
        assert!(matches!(snapshot.notice, Some(DownloadNotice::Failed(_))), "{snapshot:?}");
    }

    /// A notice goes stale once a probe finds the model installed.
    #[test]
    fn an_installed_probe_clears_the_notice() {
        let mut controller = OcrModelDownloadController::with_jobs(Jobs { status: installed_probe, download: hash_failure });
        controller.start_download(&FAKE_SPEC);
        poll_until(&mut controller, |c| !c.is_active());
        let snapshot = controller.panel_snapshot(&FAKE_SPEC);
        assert_eq!(snapshot.state, OcrModelDownloadState::Installed);
        assert_eq!(snapshot.notice, None);
    }

    /// M2: while one spec downloads, a second spec's snapshot names the running download
    /// (with its progress), and a start of the second spec is refused without touching it.
    #[test]
    fn a_second_spec_sees_the_running_download_and_cannot_start() {
        let mut controller = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: waits_for_cancel });
        controller.request_status(&OTHER_SPEC);
        poll_until(&mut controller, |c| !c.is_active());
        controller.start_download(&FAKE_SPEC);
        poll_until(&mut controller, |c| {
            matches!(c.state_for(FAKE_SPEC.id), OcrModelDownloadState::Downloading(p) if p.total_done == 7)
        });
        let other = controller.panel_snapshot(&OTHER_SPEC);
        let running = other.other_download.expect("the running download is reported to the other spec");
        assert_eq!(running.spec.id, FAKE_SPEC.id);
        assert_eq!(running.progress.total_done, 7);
        assert_eq!(controller.panel_snapshot(&FAKE_SPEC).other_download, None);
        controller.start_download(&OTHER_SPEC);
        assert_eq!(controller.state_for(OTHER_SPEC.id), OcrModelDownloadState::Missing);
        controller.cancel();
        poll_until(&mut controller, |c| !c.is_active());
        assert_eq!(controller.panel_snapshot(&OTHER_SPEC).other_download, None);
    }

    /// L1: no English diagnostic detail reaches the user-facing text.
    #[test]
    fn every_error_variant_maps_to_its_own_key_without_english_detail() {
        const DETAIL: &str = "english-diagnostic-detail";
        let cases: [(ExternalModelError, &str); 9] = [
            (ExternalModelError::NotDownloaded { dir: PathBuf::from("d") }, "translation.ocr_model.not_downloaded_status"),
            (ExternalModelError::Busy { id: "x" }, "translation.ocr_model.busy_error"),
            (ExternalModelError::Cancelled, "translation.ocr_model.cancelled_status"),
            (
                ExternalModelError::Io { path: PathBuf::from("p"), source: std::io::Error::other(DETAIL) },
                "translation.ocr_model.io_error",
            ),
            (ExternalModelError::Http { file: "f", detail: DETAIL.to_owned() }, "translation.ocr_model.http_error"),
            (ExternalModelError::SizeMismatch { file: "f", expected: 1, actual: 2 }, "translation.ocr_model.size_mismatch_error"),
            (
                ExternalModelError::HashMismatch { file: "f", expected: "00", actual: DETAIL.to_owned() },
                "translation.ocr_model.hash_mismatch_error",
            ),
            (ExternalModelError::InvalidSpec(DETAIL.to_owned()), "translation.ocr_model.invalid_spec_error"),
            (ExternalModelError::Unsupported, "translation.ocr_model.unsupported_error"),
        ];
        for (err, key) in &cases {
            assert_eq!(external_error_key(err), *key, "{err:?}");
            assert!(!external_error_message(err).contains(DETAIL), "{err:?}");
        }
    }

    #[test]
    fn offered_label_exists_only_for_missing_and_partial() {
        assert_eq!(offered_download_label(&FAKE_SPEC, &OcrModelDownloadState::Missing), Some(download_button_label(&FAKE_SPEC)));
        for state in [
            OcrModelDownloadState::Unknown,
            OcrModelDownloadState::Checking,
            OcrModelDownloadState::Installed,
            OcrModelDownloadState::Downloading(initial_progress(&FAKE_SPEC)),
            OcrModelDownloadState::Unavailable("x".to_owned()),
        ] {
            assert_eq!(offered_download_label(&FAKE_SPEC, &state), None, "{state:?}");
        }
        assert_eq!(download_label_for_status(&FAKE_SPEC, ExternalModelStatus::Installed), download_button_label(&FAKE_SPEC));
    }

    #[test]
    fn byte_sizes_split_into_number_and_unit() {
        assert_eq!(split_bytes(0), ("0".to_owned(), ByteUnit::Bytes));
        assert_eq!(split_bytes(1023), ("1023".to_owned(), ByteUnit::Bytes));
        assert_eq!(split_bytes(1024), ("1".to_owned(), ByteUnit::Kib));
        assert_eq!(split_bytes(1536), ("2".to_owned(), ByteUnit::Kib));
        assert_eq!(split_bytes(1024 * 1024), ("1.0".to_owned(), ByteUnit::Mib));
        assert_eq!(split_bytes(242_908_475), ("231.7".to_owned(), ByteUnit::Mib));
        assert_eq!(split_bytes(1024 * 1024 * 1024), ("1.00".to_owned(), ByteUnit::Gib));
        assert_eq!(split_bytes(1_986_000_000), ("1.85".to_owned(), ByteUnit::Gib));
        assert_eq!(split_bytes(u64::MAX), ("17179869184.00".to_owned(), ByteUnit::Gib));
    }

    /// Rounding up to 1024 of a unit moves the value to the next unit.
    #[test]
    fn byte_sizes_never_print_1024_of_a_unit() {
        assert_eq!(split_bytes(1_048_575), ("1.0".to_owned(), ByteUnit::Mib));
        assert_eq!(split_bytes(1_048_063), ("1023".to_owned(), ByteUnit::Kib));
        assert_eq!(split_bytes(1_073_741_823), ("1.00".to_owned(), ByteUnit::Gib));
        assert_eq!(split_bytes(1_073_689_395), ("1023.9".to_owned(), ByteUnit::Mib));
    }
}
