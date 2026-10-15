/*
File: crates/ms-tab-translation/src/ocr_model_download.rs

Purpose:
Download / install-state controller for the app-managed EXTERNAL models (pinned specs of
`ms_sysprobe::ai_models::{external, external_catalog, easyocr}`) that a panel offers to
download explicitly: Baberu OCR, the PaddleOCR-VL variants, the EasyOCR script models and the
Surya checkpoints here; the SDXL configs in the Cleaning tab, which reuses this controller and
the panel block through the cleaning -> translation crate edge. The blocking downloader lives
in `ms_sysprobe`; this file only runs it off the GUI thread and keeps the state for the panel.

Key items:
- Bundles: the unit a panel shows and downloads is a list of specs (`&[&'static
  ExternalModelSpec]`, e.g. Surya OCR = detection + recognition); `bundle_key` names it (the
  spec ids joined with `+`, so a one-spec bundle's key is the spec id).
- `OcrModelDownloadState`: the install state of a spec or (aggregated) a bundle (unknown,
  checking, missing, partial, installed, downloading with byte progress, status probe
  unavailable).
- `DownloadNotice`: the outcome of the last finished download of a bundle (cancelled /
  failed), shown NEXT TO the state, never instead of it.
- `OcrModelPanelSnapshot`: what a panel's download block needs for one bundle in one frame
  (aggregated state, bytes still to download, notice, and the OTHER bundle's download that
  blocks this one, if any).
- `OcrModelDownloadController`: one status worker (stat-only, per-spec probes) plus at most one
  job thread at a time — a DOWNLOAD (the bundle's not-installed specs, sequentially) or a
  VERIFY (`start_verify`: hashes every file of every spec against its pin, deleting damaged
  files and the markers of every spec sharing them, the repair path for a same-size
  corruption the stat-only probe cannot see) — reporting one progress over the whole bundle;
  progress is coalesced in a mutex slot that `poll` reads. A finished verify re-probes the
  bundle and every spec it invalidated and leaves a neutral notice (intact / N files removed).
- `offered_download_label`: the one owner of "which download button caption is visible for a
  snapshot" — used by the panel block AND (through `download_label_for_statuses`, the same
  aggregation over probe answers) by the "model is not downloaded" errors, so an error names
  exactly the button the user sees.
- `external_error_message`: the typed `ExternalModelError` -> localized text mapping
  (`ExternalModelError::Display` is English diagnostic text: it goes to the log only, and
  no English detail — OS error text, HTTP detail — is interpolated into the message).
- `format_bytes`: integer-only, localized byte-size formatter.

Notes:
- The GUI thread never stats files or touches the network here: status probes and the
  download run on worker threads; `poll` only drains a channel and reads a snapshot.
- Bundle aggregation (`aggregate_bundle`): a status worker failure wins, then "not probed yet"
  (unknown / checking), then "all installed", then `Partial` (the bytes of every partial spec)
  over `Missing`; the bytes to download are the totals of the specs not installed. A spec that
  two bundles share (Surya detection: Surya OCR and the Surya text detector) has ONE probed
  state, so both bundles agree.
- Every finished download that did not install the bundle (cancel, failure) re-probes its
  specs, so the panel shows the real disk state (`Partial` / `Missing`) and its button; the
  outcome text is kept as a `DownloadNotice`. A spec finished before the failure is reported
  `Installed` right away.
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
    ExternalDownloadPhase, ExternalDownloadProgress, ExternalModelError, ExternalModelSpec, ExternalModelStatus, VerifyReport,
    download_external_model, external_model_status, verify_external_model,
};
use ms_sysprobe::ai_models::external_catalog::all_specs;
use ms_thread::{self as thread, JoinHandle};

/// What a panel knows about one external model spec, or — aggregated — one bundle.
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
    /// A download of this spec (bundle) is running; the last progress report, over the whole
    /// bundle being downloaded.
    Downloading(ExternalDownloadProgress),
    /// A verify of this spec (bundle) is running; the last progress report, over the whole
    /// bundle being verified.
    Verifying(ExternalDownloadProgress),
    /// The status worker is gone, so the state cannot be probed; the localized reason.
    Unavailable(String),
}

/// Outcome of the last finished job of one bundle, shown next to the (re-probed) state: a
/// download that did not install it (until the next job of that bundle starts or the bundle
/// is found installed), or any verify (until the next job of that bundle starts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadNotice {
    /// The user cancelled the download; the staged bytes are kept for a resume.
    Cancelled,
    /// The download or the verify failed; the localized reason.
    Failed(String),
    /// A verify found every file present and matching its pin.
    VerifyIntact,
    /// A verify found `files` files damaged (deleted) or missing; a download repairs them.
    VerifyRemoved { files: usize },
    /// The user cancelled the verify; damaged files found before that are already deleted.
    VerifyCancelled,
}

impl DownloadNotice {
    /// Whether this notice reports a verify (kept even while the bundle reads installed).
    #[must_use]
    pub fn is_verify(&self) -> bool {
        match self {
            Self::VerifyIntact | Self::VerifyRemoved { .. } | Self::VerifyCancelled => true,
            Self::Cancelled | Self::Failed(_) => false,
        }
    }
}

/// What the one running job of the controller does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    /// Downloading (or resuming) the bundle.
    Download,
    /// Hashing the bundle's files against their pins.
    Verify,
}

/// A download of ANOTHER bundle that currently blocks downloading the shown one (at most one
/// download runs at a time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherDownload {
    /// The specs of the bundle being downloaded or verified.
    pub specs: Vec<&'static ExternalModelSpec>,
    /// Whether that job downloads or verifies.
    pub kind: JobKind,
    /// Its last progress report.
    pub progress: ExternalDownloadProgress,
}

/// What a panel's download block shows for one bundle in one frame; built by
/// [`OcrModelDownloadController::panel_snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrModelPanelSnapshot {
    /// [`bundle_key`] of the bundle this snapshot describes; the panel draws it only for that
    /// bundle, so a snapshot taken before an engine / variant / language switch in the same
    /// frame is not shown for the newly selected models.
    pub bundle_key: String,
    /// The aggregated install / download state of the bundle.
    pub state: OcrModelDownloadState,
    /// Bytes a download would transfer: the totals of the bundle's specs not installed.
    pub missing_bytes: u64,
    /// The outcome of its last unsuccessful download, if any (never while installed).
    pub notice: Option<DownloadNotice>,
    /// Another bundle's running download, which disables this bundle's download button.
    pub other_download: Option<OtherDownload>,
}

/// Stable key of a download bundle: its spec ids in order, joined with `+`. A one-spec
/// bundle's key is the spec id itself.
#[must_use]
pub fn bundle_key(specs: &[&'static ExternalModelSpec]) -> String {
    specs.iter().map(|spec| spec.id).collect::<Vec<_>>().join("+")
}

/// Blocking stat-only status probe of one spec (worker thread only).
pub(crate) type StatusProbe = fn(&'static ExternalModelSpec) -> ExternalModelStatus;
/// Blocking download of one spec with a cancel flag and a per-chunk progress callback
/// (worker thread only).
pub(crate) type DownloadJob =
    fn(&'static ExternalModelSpec, &AtomicBool, &mut dyn FnMut(&ExternalDownloadProgress)) -> Result<PathBuf, ExternalModelError>;

/// Blocking verify of one spec with a cancel flag and a per-read progress callback (worker
/// thread only).
pub(crate) type VerifyJob =
    fn(&'static ExternalModelSpec, &AtomicBool, &mut dyn FnMut(&ExternalDownloadProgress)) -> Result<VerifyReport, ExternalModelError>;

/// The worker bodies. Production uses the real `ms_sysprobe` functions under
/// `ms_config::side_models_dir()`; tests (also the tab's) inject fakes.
#[derive(Clone, Copy)]
pub(crate) struct Jobs {
    pub(crate) status: StatusProbe,
    pub(crate) download: DownloadJob,
    pub(crate) verify: VerifyJob,
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

/// Production verify job under `ms_config::side_models_dir()`.
fn real_verify_job(
    spec: &'static ExternalModelSpec,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&ExternalDownloadProgress),
) -> Result<VerifyReport, ExternalModelError> {
    verify_external_model(&config::side_models_dir(), spec, cancel, progress)
}

/// Commands of the status worker.
enum StatusCommand {
    Probe(&'static ExternalModelSpec),
    Stop,
}

/// Events the status worker and the download thread send back to the controller.
enum WorkerEvent {
    Status { spec_id: &'static str, status: ExternalModelStatus },
    /// One spec of the running bundle finished and is installed.
    SpecInstalled { spec_id: &'static str },
    DownloadFinished { key: String, specs: Vec<&'static ExternalModelSpec>, result: Result<(), ExternalModelError> },
    /// A verify ended: the number of damaged / missing files, or why it stopped; `invalidated`
    /// names every spec whose marker it deleted (also when it stopped early).
    VerifyFinished {
        key: String,
        specs: Vec<&'static ExternalModelSpec>,
        invalidated: Vec<&'static str>,
        result: Result<usize, ExternalModelError>,
    },
}

/// The one running job: its kind, bundle, cancel switch and coalesced progress slot.
struct ActiveDownload {
    kind: JobKind,
    specs: Vec<&'static ExternalModelSpec>,
    key: String,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<Option<ExternalDownloadProgress>>>,
    /// The last progress applied by `poll` (the initial estimate until the first report).
    latest: ExternalDownloadProgress,
    /// Kept only to detach explicitly on drop (see the file header).
    thread: Option<JoinHandle<()>>,
}

impl ActiveDownload {
    fn contains(&self, spec_id: &str) -> bool {
        self.specs.iter().any(|spec| spec.id == spec_id)
    }

    /// The state the running job gives every spec of its bundle.
    fn state(&self) -> OcrModelDownloadState {
        match self.kind {
            JobKind::Download => OcrModelDownloadState::Downloading(self.latest),
            JobKind::Verify => OcrModelDownloadState::Verifying(self.latest),
        }
    }
}

/// Status + download controller for the external models (GUI-side handle).
///
/// Every method is cheap and non-blocking; the work runs on a status worker thread and a
/// per-download thread. Call [`Self::poll`] once per frame and repaint while
/// [`Self::is_active`].
pub struct OcrModelDownloadController {
    jobs: Jobs,
    /// Probed state per spec id (never `Downloading`: the active download owns that).
    states: HashMap<&'static str, OcrModelDownloadState>,
    /// Outcome of the last unsuccessful download per bundle key (see [`DownloadNotice`]).
    notices: HashMap<String, DownloadNotice>,
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
            .field("downloading", &self.download.as_ref().map(|active| active.key.as_str()))
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
        Self::with_jobs(Jobs { status: real_status_probe, download: real_download_job, verify: real_verify_job })
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
        if self.download.as_ref().is_some_and(|active| active.contains(spec.id)) {
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

    /// [`Self::request_status`] for every spec of a bundle.
    pub fn request_bundle_status(&mut self, specs: &[&'static ExternalModelSpec]) {
        for spec in specs {
            self.request_status(spec);
        }
    }

    /// Probes the specs of a bundle that were never probed in this session (opening a panel
    /// on, or switching to, a model).
    pub fn probe_unknown(&mut self, specs: &[&'static ExternalModelSpec]) {
        for spec in specs {
            if self.state_for(spec.id) == OcrModelDownloadState::Unknown {
                self.request_status(spec);
            }
        }
    }

    /// Starts downloading (or resuming) the bundle `specs` on a new thread and clears its
    /// notice. The thread probes each spec first and downloads the ones not installed, in
    /// order, reporting one progress over all of them; it stops at the first failure. At most
    /// one download runs at a time: while one runs this is refused with a log line (the panel
    /// disables the button and names the running download, see
    /// [`OcrModelPanelSnapshot::other_download`], so a refused click is not silent). An empty
    /// bundle is refused the same way.
    pub fn start_download(&mut self, specs: &[&'static ExternalModelSpec]) {
        let key = bundle_key(specs);
        if let Some(running) = &self.download {
            ms_log::runtime_log::log_info(format!(
                "[ocr-model] download of '{key}' refused: '{}' is still downloading",
                running.key
            ));
            return;
        }
        if specs.is_empty() {
            ms_log::runtime_log::log_error("[ocr-model] download refused: empty model bundle");
            return;
        }
        self.notices.remove(&key);
        let specs = specs.to_vec();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(None::<ExternalDownloadProgress>));
        let jobs = self.jobs;
        let evt_tx = self.evt_tx.clone();
        let thread_cancel = Arc::clone(&cancel);
        let thread_progress = Arc::clone(&progress);
        let thread_specs = specs.clone();
        let thread_key = key.clone();
        ms_log::runtime_log::log_info(format!(
            "[ocr-model] download started: '{key}' ({} specs, at most {} bytes)",
            specs.len(),
            specs.iter().fold(0_u64, |total, spec| total.saturating_add(spec.total_bytes()))
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
            let mut installed = |spec: &'static ExternalModelSpec| {
                // Best effort: a dropped controller re-probes from disk next time.
                if evt_tx.send(WorkerEvent::SpecInstalled { spec_id: spec.id }).is_err() {
                    ms_log::runtime_log::log_info(format!("[ocr-model] '{}' installed after its controller was dropped", spec.id));
                }
            };
            let result = run_bundle_download(jobs, &thread_specs, &thread_cancel, &mut report, &mut installed);
            if let Err(err) = &result {
                ms_log::runtime_log::log_error(format!("[ocr-model] download of '{thread_key}' ended: {err}"));
            } else {
                ms_log::runtime_log::log_info(format!("[ocr-model] download of '{thread_key}' finished"));
            }
            // The controller may already be dropped (detached thread): nobody to tell.
            if evt_tx.send(WorkerEvent::DownloadFinished { key: thread_key.clone(), specs: thread_specs, result }).is_err() {
                ms_log::runtime_log::log_info(format!("[ocr-model] download of '{thread_key}' ended after its controller was dropped"));
            }
        });
        let latest = initial_progress(&specs, &self.states);
        self.download = Some(ActiveDownload { kind: JobKind::Download, specs, key, cancel, progress, latest, thread: Some(handle) });
    }

    /// Starts verifying the bundle `specs` on a new thread (hashing every file of every spec
    /// against its pin; damaged files and the markers of every spec sharing them are
    /// deleted) and clears its notice. Mutually exclusive with a download: refused with a log
    /// line while any job runs, like [`Self::start_download`]; an empty bundle is refused too.
    /// When it ends, the bundle and every spec it invalidated are re-probed and a verify
    /// notice is left.
    pub fn start_verify(&mut self, specs: &[&'static ExternalModelSpec]) {
        let key = bundle_key(specs);
        if let Some(running) = &self.download {
            ms_log::runtime_log::log_info(format!(
                "[ocr-model] verify of '{key}' refused: '{}' is still running ({:?})",
                running.key, running.kind
            ));
            return;
        }
        if specs.is_empty() {
            ms_log::runtime_log::log_error("[ocr-model] verify refused: empty model bundle");
            return;
        }
        self.notices.remove(&key);
        let specs = specs.to_vec();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(None::<ExternalDownloadProgress>));
        let jobs = self.jobs;
        let evt_tx = self.evt_tx.clone();
        let thread_cancel = Arc::clone(&cancel);
        let thread_progress = Arc::clone(&progress);
        let thread_specs = specs.clone();
        let thread_key = key.clone();
        ms_log::runtime_log::log_info(format!("[ocr-model] verify started: '{key}' ({} specs)", specs.len()));
        let handle = thread::spawn(move || {
            let mut report = |snapshot: &ExternalDownloadProgress| {
                // Latest wins, as for a download (see `start_download`).
                let mut slot = match thread_progress.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *slot = Some(*snapshot);
            };
            let mut invalidated = Vec::new();
            let result = run_bundle_verify(jobs, &thread_specs, &thread_cancel, &mut report, &mut invalidated);
            match &result {
                Ok(0) => ms_log::runtime_log::log_info(format!("[ocr-model] verify of '{thread_key}': every file is intact")),
                Ok(damaged) => ms_log::runtime_log::log_warn(format!(
                    "[ocr-model] verify of '{thread_key}': {damaged} files damaged or missing; invalidated specs: {invalidated:?}"
                )),
                Err(err) => ms_log::runtime_log::log_error(format!("[ocr-model] verify of '{thread_key}' ended: {err}")),
            }
            let finished = WorkerEvent::VerifyFinished { key: thread_key.clone(), specs: thread_specs, invalidated, result };
            if evt_tx.send(finished).is_err() {
                ms_log::runtime_log::log_info(format!("[ocr-model] verify of '{thread_key}' ended after its controller was dropped"));
            }
        });
        let latest = verify_initial_progress(&specs);
        self.download = Some(ActiveDownload { kind: JobKind::Verify, specs, key, cancel, progress, latest, thread: Some(handle) });
    }

    /// Asks the running job (download or verify, if any) to stop at its next chunk; once the
    /// thread reports back the bundle gets a cancelled notice and is re-probed.
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
                    if !self.download.as_ref().is_some_and(|active| active.contains(spec_id)) {
                        changed |= self.set_state(spec_id, state_from_status(status));
                    }
                }
                Ok(WorkerEvent::SpecInstalled { spec_id }) => {
                    changed |= self.set_state(spec_id, OcrModelDownloadState::Installed);
                }
                Ok(WorkerEvent::DownloadFinished { key, specs, result }) => {
                    // The thread has sent its last message and is returning: dropping the
                    // handle detaches a thread that is already done.
                    if self.download.as_ref().is_some_and(|active| active.key == key) {
                        self.download = None;
                    }
                    changed = true;
                    match notice_from_download_result(result) {
                        None => {
                            self.notices.remove(&key);
                            for spec in &specs {
                                self.states.insert(spec.id, OcrModelDownloadState::Installed);
                            }
                        }
                        Some(notice) => {
                            // Re-probe: the panel must show the real disk state (`Partial`
                            // after a cancel, usually) and the button of THAT state — the
                            // same caption the "not downloaded" errors name.
                            self.notices.insert(key, notice);
                            self.request_bundle_status(&specs);
                        }
                    }
                }
                Ok(WorkerEvent::VerifyFinished { key, specs, invalidated, result }) => {
                    if self.download.as_ref().is_some_and(|active| active.key == key) {
                        self.download = None;
                    }
                    changed = true;
                    let notice = match result {
                        Ok(0) => DownloadNotice::VerifyIntact,
                        Ok(files) => DownloadNotice::VerifyRemoved { files },
                        Err(ExternalModelError::Cancelled) => DownloadNotice::VerifyCancelled,
                        Err(err) => DownloadNotice::Failed(external_error_message(&err)),
                    };
                    self.notices.insert(key, notice);
                    // The verify may have deleted files and markers: the bundle and every
                    // spec sharing a deleted file (e.g. other EasyOCR scripts) read their real
                    // state again.
                    self.request_bundle_status(&specs);
                    let others: Vec<&'static ExternalModelSpec> = all_specs()
                        .into_iter()
                        .filter(|spec| invalidated.contains(&spec.id) && !specs.iter().any(|own| own.id == spec.id))
                        .collect();
                    self.request_bundle_status(&others);
                }
                Err(TryRecvError::Empty) => break,
                // Impossible while `self.evt_tx` is alive; kept exhaustive.
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if let Some(active) = &mut self.download {
            let latest = match active.progress.lock() {
                Ok(guard) => *guard,
                Err(poisoned) => *poisoned.into_inner(),
            };
            if let Some(progress) = latest
                && progress != active.latest
            {
                active.latest = progress;
                changed = true;
            }
        }
        changed
    }

    /// The state of the spec with id `spec_id`: `Downloading` / `Verifying` while a running
    /// job's bundle contains it, else the probed state (`Unknown` when never probed).
    #[must_use]
    pub fn state_for(&self, spec_id: &str) -> OcrModelDownloadState {
        if let Some(active) = self.download.as_ref().filter(|active| active.contains(spec_id)) {
            return active.state();
        }
        self.states.get(spec_id).cloned().unwrap_or(OcrModelDownloadState::Unknown)
    }

    /// Everything a panel's download block shows for the bundle `specs` this frame: its
    /// aggregated state and missing bytes, its last unsuccessful download outcome and the
    /// other bundle's download that blocks it.
    #[must_use]
    pub fn panel_snapshot(&self, specs: &[&'static ExternalModelSpec]) -> OcrModelPanelSnapshot {
        let key = bundle_key(specs);
        let probed: Vec<(&ExternalModelSpec, OcrModelDownloadState)> = specs
            .iter()
            .map(|spec| (*spec, self.states.get(spec.id).cloned().unwrap_or(OcrModelDownloadState::Unknown)))
            .collect();
        let (aggregated, missing_bytes) = aggregate_bundle(&probed);
        let (state, other_download) = match &self.download {
            Some(active) if active.key == key => (active.state(), None),
            Some(active) => {
                (aggregated, Some(OtherDownload { specs: active.specs.clone(), kind: active.kind, progress: active.latest }))
            }
            None => (aggregated, None),
        };
        // A download notice goes stale once the bundle reads installed; a verify notice is
        // the answer to "are the installed files intact" and stays.
        let notice =
            self.notices.get(&key).filter(|notice| notice.is_verify() || state != OcrModelDownloadState::Installed).cloned();
        OcrModelPanelSnapshot { bundle_key: key, state, missing_bytes, notice, other_download }
    }

    /// `true` while a job runs or a status probe is unanswered: the GUI keeps
    /// repainting so `poll` sees the result.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.download.is_some() || self.pending_probes > 0
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
                active.key
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

/// The body of the download thread: probes every spec of the bundle, downloads the ones not
/// installed in order and maps each spec's progress onto the bundle (file index and byte
/// counters continue across specs; the totals are those of the specs downloaded). `installed`
/// is told about every spec that finished. Stops at the first error, and before the next spec
/// once `cancel` is set. Blocking: the download thread only.
///
/// # Errors
/// The first spec's `ExternalModelError`; `Cancelled` when `cancel` was set between specs.
fn run_bundle_download(
    jobs: Jobs,
    specs: &[&'static ExternalModelSpec],
    cancel: &AtomicBool,
    report: &mut dyn FnMut(&ExternalDownloadProgress),
    installed: &mut dyn FnMut(&'static ExternalModelSpec),
) -> Result<(), ExternalModelError> {
    let pending: Vec<&'static ExternalModelSpec> =
        specs.iter().copied().filter(|spec| (jobs.status)(spec) != ExternalModelStatus::Installed).collect();
    let file_count = pending.iter().map(|spec| spec.files.len()).sum::<usize>();
    let total_bytes = pending.iter().fold(0_u64, |total, spec| total.saturating_add(spec.total_bytes()));
    let mut files_before = 0_usize;
    let mut bytes_before = 0_u64;
    for spec in pending {
        if cancel.load(Ordering::Relaxed) {
            return Err(ExternalModelError::Cancelled);
        }
        let mut forward =
            |progress: &ExternalDownloadProgress| report(&bundle_progress(progress, files_before, file_count, bytes_before, total_bytes));
        (jobs.download)(spec, cancel, &mut forward)?;
        installed(spec);
        files_before = files_before.saturating_add(spec.files.len());
        bytes_before = bytes_before.saturating_add(spec.total_bytes());
    }
    Ok(())
}

/// The body of the verify thread: verifies EVERY spec of the bundle in order (installed or
/// not), mapping each spec's progress onto the bundle like a download; `invalidated`
/// collects the ids of every spec whose marker a verify deleted. Stops at the first error,
/// and before the next spec once `cancel` is set. Blocking: the verify thread only.
///
/// # Errors
/// The first spec's `ExternalModelError`; `Cancelled` when `cancel` was set between specs.
fn run_bundle_verify(
    jobs: Jobs,
    specs: &[&'static ExternalModelSpec],
    cancel: &AtomicBool,
    report: &mut dyn FnMut(&ExternalDownloadProgress),
    invalidated: &mut Vec<&'static str>,
) -> Result<usize, ExternalModelError> {
    let file_count = specs.iter().map(|spec| spec.files.len()).sum::<usize>();
    let total_bytes = specs.iter().fold(0_u64, |total, spec| total.saturating_add(spec.total_bytes()));
    let mut files_before = 0_usize;
    let mut bytes_before = 0_u64;
    let mut damaged = 0_usize;
    for spec in specs {
        if cancel.load(Ordering::Relaxed) {
            return Err(ExternalModelError::Cancelled);
        }
        let mut forward =
            |progress: &ExternalDownloadProgress| report(&bundle_progress(progress, files_before, file_count, bytes_before, total_bytes));
        let verified = (jobs.verify)(spec, cancel, &mut forward)?;
        damaged = damaged.saturating_add(verified.removed.len()).saturating_add(verified.missing.len());
        for id in verified.invalidated {
            if !invalidated.contains(&id) {
                invalidated.push(id);
            }
        }
        files_before = files_before.saturating_add(spec.files.len());
        bytes_before = bytes_before.saturating_add(spec.total_bytes());
    }
    Ok(damaged)
}

/// One spec's progress report mapped onto its bundle: the file index and byte counters
/// continue after the `files_before` / `bytes_before` of the earlier specs, the totals are the
/// bundle's.
fn bundle_progress(
    progress: &ExternalDownloadProgress,
    files_before: usize,
    file_count: usize,
    bytes_before: u64,
    total_bytes: u64,
) -> ExternalDownloadProgress {
    ExternalDownloadProgress {
        file_index: files_before.saturating_add(progress.file_index),
        file_count,
        total_done: bytes_before.saturating_add(progress.total_done),
        total_bytes,
        ..*progress
    }
}

/// The progress shown between the click and the first report of the verify thread: every
/// spec of the bundle, from the first file of the first one.
fn verify_initial_progress(specs: &[&'static ExternalModelSpec]) -> ExternalDownloadProgress {
    let first = specs.first().and_then(|spec| spec.files.first());
    ExternalDownloadProgress {
        file_index: 0,
        file_count: specs.iter().map(|spec| spec.files.len()).sum(),
        file_path: first.map_or("", |file| file.path),
        phase: ExternalDownloadPhase::Verifying,
        file_done: 0,
        file_total: first.map_or(0, |file| file.download_size()),
        total_done: 0,
        total_bytes: specs.iter().fold(0_u64, |total, spec| total.saturating_add(spec.total_bytes())),
    }
}

/// The progress shown between the click and the first report of the download thread: the
/// bundle's specs not known to be installed, from the first file of the first of them.
fn initial_progress(
    specs: &[&'static ExternalModelSpec],
    states: &HashMap<&'static str, OcrModelDownloadState>,
) -> ExternalDownloadProgress {
    let pending: Vec<&'static ExternalModelSpec> =
        specs.iter().copied().filter(|spec| states.get(spec.id) != Some(&OcrModelDownloadState::Installed)).collect();
    let first = pending.first().and_then(|spec| spec.files.first());
    ExternalDownloadProgress {
        file_index: 0,
        file_count: pending.iter().map(|spec| spec.files.len()).sum(),
        file_path: first.map_or("", |file| file.path),
        phase: ExternalDownloadPhase::Verifying,
        file_done: 0,
        file_total: first.map_or(0, |file| file.download_size()),
        total_done: 0,
        total_bytes: pending.iter().fold(0_u64, |total, spec| total.saturating_add(spec.total_bytes())),
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

/// Aggregates the per-spec states of a bundle into the bundle's state and the bytes a
/// download would transfer (the totals of the specs not installed). Priority: the first
/// `Unavailable` reason, a `Downloading` / `Verifying` report, `Unknown`, `Checking`; then `Installed`
/// when every spec is; then `Partial` with the bytes of every partial spec; else `Missing`.
/// An empty bundle is `Installed` with nothing to download.
fn aggregate_bundle(entries: &[(&ExternalModelSpec, OcrModelDownloadState)]) -> (OcrModelDownloadState, u64) {
    let missing_bytes = entries
        .iter()
        .filter(|(_, state)| *state != OcrModelDownloadState::Installed)
        .fold(0_u64, |total, (spec, _)| total.saturating_add(spec.total_bytes()));
    let states = || entries.iter().map(|(_, state)| state);
    if let Some(reason) =
        states().find_map(|state| if let OcrModelDownloadState::Unavailable(reason) = state { Some(reason.clone()) } else { None })
    {
        return (OcrModelDownloadState::Unavailable(reason), missing_bytes);
    }
    // Stored probe states are never a running job's; kept for completeness of the rule.
    if let Some(running) = states().find(|state| matches!(state, OcrModelDownloadState::Downloading(_) | OcrModelDownloadState::Verifying(_))) {
        return (running.clone(), missing_bytes);
    }
    let state = if states().any(|state| *state == OcrModelDownloadState::Unknown) {
        OcrModelDownloadState::Unknown
    } else if states().any(|state| *state == OcrModelDownloadState::Checking) {
        OcrModelDownloadState::Checking
    } else if states().all(|state| *state == OcrModelDownloadState::Installed) {
        OcrModelDownloadState::Installed
    } else {
        let partial_bytes = states().fold(None::<u64>, |sum, state| {
            if let OcrModelDownloadState::Partial { bytes_present } = state {
                Some(sum.unwrap_or(0).saturating_add(*bytes_present))
            } else {
                sum
            }
        });
        partial_bytes.map_or(OcrModelDownloadState::Missing, |bytes_present| OcrModelDownloadState::Partial { bytes_present })
    };
    (state, missing_bytes)
}

/// The notice a finished download leaves: `None` on success (the bundle is installed), a
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
/// download thread logs the full `Display` with the bundle key when the download ends.
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

/// Caption of the "Download (size)" button for `missing_bytes` to transfer.
pub(crate) fn download_button_label(missing_bytes: u64) -> String {
    tf!("translation.ocr_model.download_button", size = format_bytes(missing_bytes))
}

/// Caption of the "Resume download (done of total)" button with `bytes_present` bytes of
/// `missing_bytes` already on disk.
fn resume_button_label(bytes_present: u64, missing_bytes: u64) -> String {
    tf!(
        "translation.ocr_model.resume_button",
        done = format_bytes(bytes_present),
        total = format_bytes(missing_bytes)
    )
}

/// The caption of the download button a panel shows for `snapshot`, or `None` when its state
/// offers no download button. The ONE owner of this decision: the download block draws
/// exactly this caption, and the "model is not downloaded" errors name it (through
/// [`download_label_for_statuses`], the same aggregation over the probe answers).
#[must_use]
pub fn offered_download_label(snapshot: &OcrModelPanelSnapshot) -> Option<String> {
    offered_label(&snapshot.state, snapshot.missing_bytes)
}

/// [`offered_download_label`] over an aggregated state and its missing bytes.
fn offered_label(state: &OcrModelDownloadState, missing_bytes: u64) -> Option<String> {
    match state {
        OcrModelDownloadState::Missing => Some(download_button_label(missing_bytes)),
        OcrModelDownloadState::Partial { bytes_present } => Some(resume_button_label(*bytes_present, missing_bytes)),
        OcrModelDownloadState::Unknown
        | OcrModelDownloadState::Checking
        | OcrModelDownloadState::Installed
        | OcrModelDownloadState::Downloading(_)
        | OcrModelDownloadState::Verifying(_)
        | OcrModelDownloadState::Unavailable(_) => None,
    }
}

/// The download caption a panel shows for a bundle whose specs got these stat-only probe
/// answers (see [`offered_download_label`]); an all-`Installed` answer, which offers no
/// button, falls back to the plain "Download (size of the whole bundle)" caption so a caller
/// always has a button to name. Callers name it in their "not downloaded" errors.
#[must_use]
pub fn download_label_for_statuses(statuses: &[(&ExternalModelSpec, ExternalModelStatus)]) -> String {
    let entries: Vec<(&ExternalModelSpec, OcrModelDownloadState)> =
        statuses.iter().map(|(spec, status)| (*spec, state_from_status(*status))).collect();
    let (state, missing_bytes) = aggregate_bundle(&entries);
    offered_label(&state, missing_bytes).unwrap_or_else(|| {
        download_button_label(statuses.iter().fold(0_u64, |total, (spec, _)| total.saturating_add(spec.total_bytes())))
    })
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
    use super::{
        ExternalDownloadProgress, ExternalModelError, ExternalModelSpec, ExternalModelStatus, HashMap, VerifyReport, initial_progress,
        verify_initial_progress,
    };
    use ms_sysprobe::ai_models::external::{DirLayout, ExternalFile, ExternalSource};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    static FAKE_FILES: [ExternalFile; 2] = [ExternalFile::direct("a.bin", 100, "00"), ExternalFile::direct("b.bin", 50, "00")];
    static THIRD_FILES: [ExternalFile; 1] = [ExternalFile::direct("c.bin", 30, "00")];
    /// A two-file spec that exists only in tests.
    pub(crate) static FAKE_SPEC: ExternalModelSpec = ExternalModelSpec {
        id: "fake_model",
        source: ExternalSource::HuggingFace { repo_id: "owner/name", revision: "0" },
        dir: "Fake",
        layout: DirLayout::Exclusive,
        files: &FAKE_FILES,
    };
    /// A second spec, to exercise "another model is downloading" and bundles.
    pub(crate) static OTHER_SPEC: ExternalModelSpec = ExternalModelSpec {
        id: "other_model",
        source: ExternalSource::HuggingFace { repo_id: "owner/other", revision: "0" },
        dir: "Other",
        layout: DirLayout::Exclusive,
        files: &FAKE_FILES,
    };
    /// A one-file spec with its own size, so bundle sums are distinguishable.
    pub(crate) static THIRD_SPEC: ExternalModelSpec = ExternalModelSpec {
        id: "third_model",
        source: ExternalSource::Url { base_url: "https://example.invalid/third" },
        dir: "Third",
        layout: DirLayout::Exclusive,
        files: &THIRD_FILES,
    };

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

    /// `OTHER_SPEC` is installed, everything else is missing.
    pub(crate) fn other_installed_probe(spec: &'static ExternalModelSpec) -> ExternalModelStatus {
        if spec.id == OTHER_SPEC.id { ExternalModelStatus::Installed } else { ExternalModelStatus::Missing }
    }

    pub(crate) fn ok_download(
        spec: &'static ExternalModelSpec,
        _cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<PathBuf, ExternalModelError> {
        let mut report = initial_progress(&[spec], &HashMap::new());
        report.total_done = 120;
        progress(&report);
        Ok(PathBuf::from("unused"))
    }

    /// Reports the spec's last file as complete (spec-local counters), then succeeds.
    pub(crate) fn complete_download(
        spec: &'static ExternalModelSpec,
        _cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<PathBuf, ExternalModelError> {
        let mut report = initial_progress(&[spec], &HashMap::new());
        report.file_index = spec.files.len().saturating_sub(1);
        report.total_done = spec.total_bytes();
        progress(&report);
        Ok(PathBuf::from("unused"))
    }

    /// Reports 7 bytes, then runs until cancelled (10 s safety deadline).
    pub(crate) fn waits_for_cancel(
        spec: &'static ExternalModelSpec,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<PathBuf, ExternalModelError> {
        let mut report = initial_progress(&[spec], &HashMap::new());
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

    /// Every file of the spec verifies.
    pub(crate) fn intact_verify(
        spec: &'static ExternalModelSpec,
        _cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<VerifyReport, ExternalModelError> {
        progress(&verify_initial_progress(&[spec]));
        Ok(VerifyReport { verified: spec.files.iter().map(|file| file.path).collect(), ..VerifyReport::default() })
    }

    /// The spec's first file was damaged (deleted); its marker and `OTHER_SPEC`'s (sharing the
    /// file) were invalidated.
    pub(crate) fn damaged_verify(
        spec: &'static ExternalModelSpec,
        _cancel: &AtomicBool,
        _progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<VerifyReport, ExternalModelError> {
        Ok(VerifyReport {
            verified: Vec::new(),
            missing: Vec::new(),
            removed: spec.files.iter().take(1).map(|file| file.path).collect(),
            invalidated: vec![spec.id, OTHER_SPEC.id],
        })
    }

    /// Reports 7 bytes, then runs until cancelled (10 s safety deadline).
    pub(crate) fn verify_waits_for_cancel(
        spec: &'static ExternalModelSpec,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<VerifyReport, ExternalModelError> {
        let mut report = verify_initial_progress(&[spec]);
        report.total_done = 7;
        progress(&report);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cancel.load(Ordering::Relaxed) {
            if Instant::now() > deadline {
                return Err(ExternalModelError::InvalidSpec("test timeout".to_owned()));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Err(ExternalModelError::Cancelled)
    }

    /// Fails `THIRD_SPEC`, succeeds for every other spec.
    pub(crate) fn third_fails(
        spec: &'static ExternalModelSpec,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<PathBuf, ExternalModelError> {
        if spec.id == THIRD_SPEC.id { hash_failure(spec, cancel, progress) } else { complete_download(spec, cancel, progress) }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::{
        FAKE_SPEC, OTHER_SPEC, PARTIAL_BYTES, THIRD_SPEC, busy_failure, complete_download, damaged_verify, hash_failure,
        installed_probe, intact_verify, missing_probe, ok_download, other_installed_probe, partial_probe, third_fails,
        verify_waits_for_cancel, waits_for_cancel,
    };
    use super::*;
    use std::time::{Duration, Instant};

    const FAKE: &[&ExternalModelSpec] = &[&FAKE_SPEC];
    const OTHER: &[&ExternalModelSpec] = &[&OTHER_SPEC];
    const BUNDLE: &[&ExternalModelSpec] = &[&FAKE_SPEC, &THIRD_SPEC];

    /// Polls until `done` holds or a generous deadline passes (worker threads are real).
    fn poll_until(controller: &mut OcrModelDownloadController, done: impl Fn(&OcrModelDownloadController) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(controller) {
            assert!(Instant::now() < deadline, "controller did not settle: {controller:?}");
            controller.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn missing(spec: &'static ExternalModelSpec) -> (&'static ExternalModelSpec, OcrModelDownloadState) {
        (spec, OcrModelDownloadState::Missing)
    }

    #[test]
    fn bundle_key_joins_the_spec_ids_and_a_single_spec_keeps_its_id() {
        assert_eq!(bundle_key(FAKE), FAKE_SPEC.id);
        assert_eq!(bundle_key(BUNDLE), "fake_model+third_model");
        assert_eq!(bundle_key(&[]), "");
    }

    #[test]
    fn status_probe_goes_through_checking_to_the_probed_state() {
        let mut controller = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: ok_download, verify: intact_verify });
        assert_eq!(controller.state_for(FAKE_SPEC.id), OcrModelDownloadState::Unknown);
        controller.request_status(&FAKE_SPEC);
        assert_eq!(controller.state_for(FAKE_SPEC.id), OcrModelDownloadState::Checking);
        assert!(controller.is_active());
        poll_until(&mut controller, |c| !c.is_active());
        assert_eq!(controller.state_for(FAKE_SPEC.id), OcrModelDownloadState::Missing);

        let mut partial = OcrModelDownloadController::with_jobs(Jobs { status: partial_probe, download: ok_download, verify: intact_verify });
        partial.probe_unknown(BUNDLE);
        assert_eq!(partial.state_for(THIRD_SPEC.id), OcrModelDownloadState::Checking);
        poll_until(&mut partial, |c| !c.is_active());
        assert_eq!(partial.state_for(FAKE_SPEC.id), OcrModelDownloadState::Partial { bytes_present: PARTIAL_BYTES });
        // Already probed: `probe_unknown` sends nothing more.
        partial.probe_unknown(BUNDLE);
        assert!(!partial.is_active());
    }

    #[test]
    fn successful_download_ends_installed() {
        let mut controller = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: ok_download, verify: intact_verify });
        controller.start_download(FAKE);
        assert!(matches!(controller.state_for(FAKE_SPEC.id), OcrModelDownloadState::Downloading(_)));
        assert!(controller.is_active());
        poll_until(&mut controller, |c| !c.is_active());
        assert_eq!(controller.state_for(FAKE_SPEC.id), OcrModelDownloadState::Installed);
        assert_eq!(controller.panel_snapshot(FAKE).notice, None);
    }

    /// M1: a cancel re-probes, so the panel shows the real `Partial` state and its Resume
    /// button, with the cancel kept as a notice; the "not downloaded" label built from the
    /// same probe answers names that very button.
    #[test]
    fn cancel_reprobes_and_the_offered_button_matches_the_gate_error_label() {
        let mut controller = OcrModelDownloadController::with_jobs(Jobs { status: partial_probe, download: waits_for_cancel, verify: intact_verify });
        controller.start_download(FAKE);
        // Wait for the first progress report so the snapshot path is exercised.
        poll_until(&mut controller, |c| {
            matches!(c.state_for(FAKE_SPEC.id), OcrModelDownloadState::Downloading(p) if p.total_done == 7)
        });
        // At most one download: a second start (and a status probe of the same spec)
        // must not disturb the running one.
        controller.start_download(FAKE);
        controller.request_status(&FAKE_SPEC);
        assert!(matches!(controller.panel_snapshot(FAKE).state, OcrModelDownloadState::Downloading(_)));
        controller.cancel();
        poll_until(&mut controller, |c| !c.is_active());
        let snapshot = controller.panel_snapshot(FAKE);
        assert_eq!(snapshot.state, OcrModelDownloadState::Partial { bytes_present: PARTIAL_BYTES });
        assert_eq!(snapshot.notice, Some(DownloadNotice::Cancelled));
        let shown = offered_download_label(&snapshot).expect("a Partial state offers a button");
        assert_eq!(shown, resume_button_label(PARTIAL_BYTES, FAKE_SPEC.total_bytes()));
        assert_eq!(download_label_for_statuses(&[(&FAKE_SPEC, partial_probe(&FAKE_SPEC))]), shown);
        // A later download clears the notice.
        controller.start_download(FAKE);
        assert_eq!(controller.panel_snapshot(FAKE).notice, None);
        controller.cancel();
        poll_until(&mut controller, |c| !c.is_active());
    }

    #[test]
    fn failures_become_localized_notices_over_the_reprobed_state() {
        let mut controller = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: hash_failure, verify: intact_verify });
        controller.start_download(FAKE);
        poll_until(&mut controller, |c| !c.is_active());
        let snapshot = controller.panel_snapshot(FAKE);
        assert_eq!(snapshot.state, OcrModelDownloadState::Missing);
        // Only the notice kind is asserted: the text comes from the process-global i18n
        // catalog, which another test of this binary may install between the worker
        // building the message and this comparison (the key mapping itself is covered by
        // `every_error_variant_maps_to_its_own_key_without_english_detail`).
        assert!(matches!(snapshot.notice, Some(DownloadNotice::Failed(_))), "{snapshot:?}");

        let mut busy = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: busy_failure, verify: intact_verify });
        busy.start_download(FAKE);
        poll_until(&mut busy, |c| !c.is_active());
        let snapshot = busy.panel_snapshot(FAKE);
        assert!(matches!(snapshot.notice, Some(DownloadNotice::Failed(_))), "{snapshot:?}");
    }

    /// A notice goes stale once a probe finds the model installed.
    #[test]
    fn an_installed_probe_clears_the_notice() {
        let mut controller = OcrModelDownloadController::with_jobs(Jobs { status: installed_probe, download: hash_failure, verify: intact_verify });
        controller.start_download(FAKE);
        poll_until(&mut controller, |c| !c.is_active());
        let snapshot = controller.panel_snapshot(FAKE);
        assert_eq!(snapshot.state, OcrModelDownloadState::Installed);
        assert_eq!(snapshot.notice, None);
    }

    /// M2: while one bundle downloads, another bundle's snapshot names the running download
    /// (with its progress), and a start of the other bundle is refused without touching it.
    #[test]
    fn a_second_spec_sees_the_running_download_and_cannot_start() {
        let mut controller = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: waits_for_cancel, verify: intact_verify });
        controller.request_status(&OTHER_SPEC);
        poll_until(&mut controller, |c| !c.is_active());
        controller.start_download(FAKE);
        poll_until(&mut controller, |c| {
            matches!(c.state_for(FAKE_SPEC.id), OcrModelDownloadState::Downloading(p) if p.total_done == 7)
        });
        let other = controller.panel_snapshot(OTHER);
        let running = other.other_download.expect("the running download is reported to the other spec");
        assert_eq!(bundle_key(&running.specs), FAKE_SPEC.id);
        assert_eq!(running.progress.total_done, 7);
        assert_eq!(controller.panel_snapshot(FAKE).other_download, None);
        controller.start_download(OTHER);
        assert_eq!(controller.state_for(OTHER_SPEC.id), OcrModelDownloadState::Missing);
        controller.cancel();
        poll_until(&mut controller, |c| !c.is_active());
        assert_eq!(controller.panel_snapshot(OTHER).other_download, None);
    }

    /// The download thread skips installed specs, continues the file index and byte counters
    /// across specs and reports the totals of the specs it downloads.
    #[test]
    fn bundle_download_runs_pending_specs_in_order_with_aggregated_progress() {
        let jobs = Jobs { status: other_installed_probe, download: complete_download, verify: intact_verify };
        let mut reports = Vec::new();
        let mut installed = Vec::new();
        let cancel = AtomicBool::new(false);
        let specs: &[&'static ExternalModelSpec] = &[&FAKE_SPEC, &OTHER_SPEC, &THIRD_SPEC];
        let result = run_bundle_download(jobs, specs, &cancel, &mut |p| reports.push(*p), &mut |spec| installed.push(spec.id));
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(installed, [FAKE_SPEC.id, THIRD_SPEC.id]);
        let total = FAKE_SPEC.total_bytes() + THIRD_SPEC.total_bytes();
        let last = reports.last().copied().expect("progress was reported");
        assert_eq!((last.file_index, last.file_count), (2, 3));
        assert_eq!((last.total_done, last.total_bytes), (total, total));
        assert_eq!(last.file_path, "c.bin");
        assert_eq!(reports.first().map(|p| (p.file_index, p.total_done)), Some((1, FAKE_SPEC.total_bytes())));
    }

    /// A failing spec stops the bundle; the earlier spec is still reported installed. A set
    /// cancel flag stops before the next spec.
    #[test]
    fn bundle_download_stops_at_the_first_failure_and_on_cancel() {
        let cancel = AtomicBool::new(false);
        let mut installed = Vec::new();
        let jobs = Jobs { status: missing_probe, download: third_fails, verify: intact_verify };
        let specs: &[&'static ExternalModelSpec] = &[&FAKE_SPEC, &THIRD_SPEC, &OTHER_SPEC];
        let result = run_bundle_download(jobs, specs, &cancel, &mut |_| {}, &mut |spec| installed.push(spec.id));
        assert!(matches!(result, Err(ExternalModelError::HashMismatch { .. })), "{result:?}");
        assert_eq!(installed, [FAKE_SPEC.id]);

        cancel.store(true, Ordering::Relaxed);
        installed.clear();
        let jobs = Jobs { status: missing_probe, download: complete_download, verify: intact_verify };
        let result = run_bundle_download(jobs, BUNDLE, &cancel, &mut |_| {}, &mut |spec| installed.push(spec.id));
        assert!(matches!(result, Err(ExternalModelError::Cancelled)), "{result:?}");
        assert!(installed.is_empty());
    }

    /// Bundle state machine: a bundle download installs every spec, and a bundle sharing a
    /// spec with it sees the shared spec's state; a partly failed bundle keeps the finished
    /// spec installed and re-probes.
    #[test]
    fn a_bundle_download_installs_every_spec_and_shared_specs_agree() {
        let mut controller = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: complete_download, verify: intact_verify });
        controller.request_bundle_status(BUNDLE);
        poll_until(&mut controller, |c| !c.is_active());
        let before = controller.panel_snapshot(BUNDLE);
        assert_eq!(before.state, OcrModelDownloadState::Missing);
        assert_eq!(before.missing_bytes, FAKE_SPEC.total_bytes() + THIRD_SPEC.total_bytes());
        controller.start_download(BUNDLE);
        let started = controller.panel_snapshot(BUNDLE);
        assert!(
            matches!(started.state, OcrModelDownloadState::Downloading(p) if p.total_bytes == before.missing_bytes && p.file_count == 3),
            "{started:?}"
        );
        poll_until(&mut controller, |c| !c.is_active());
        assert_eq!(controller.panel_snapshot(BUNDLE).state, OcrModelDownloadState::Installed);
        assert_eq!(controller.panel_snapshot(&[&THIRD_SPEC]).state, OcrModelDownloadState::Installed);
        assert_eq!(controller.panel_snapshot(BUNDLE).missing_bytes, 0);

        let mut failing = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: third_fails, verify: intact_verify });
        failing.start_download(BUNDLE);
        poll_until(&mut failing, |c| !c.is_active());
        // The re-probe after the failure reads `Missing` for both (the fake probe), but the
        // finished spec was reported installed before it: the probe answer wins, as on disk.
        let snapshot = failing.panel_snapshot(BUNDLE);
        assert!(matches!(snapshot.notice, Some(DownloadNotice::Failed(_))), "{snapshot:?}");
        assert_eq!(snapshot.state, OcrModelDownloadState::Missing);
    }

    #[test]
    fn bundle_aggregation_follows_the_priority_and_sums_partial_and_missing_bytes() {
        let partial = |bytes_present| OcrModelDownloadState::Partial { bytes_present };
        let both_missing = [missing(&FAKE_SPEC), missing(&THIRD_SPEC)];
        assert_eq!(aggregate_bundle(&both_missing), (OcrModelDownloadState::Missing, 180));
        let one_installed = [(&FAKE_SPEC, OcrModelDownloadState::Installed), missing(&THIRD_SPEC)];
        assert_eq!(aggregate_bundle(&one_installed), (OcrModelDownloadState::Missing, 30));
        let partials = [(&FAKE_SPEC, partial(10)), (&THIRD_SPEC, partial(5))];
        assert_eq!(aggregate_bundle(&partials), (partial(15), 180));
        let mixed = [(&FAKE_SPEC, partial(10)), missing(&THIRD_SPEC)];
        assert_eq!(aggregate_bundle(&mixed), (partial(10), 180));
        let checking = [(&FAKE_SPEC, OcrModelDownloadState::Checking), (&THIRD_SPEC, OcrModelDownloadState::Installed)];
        assert_eq!(aggregate_bundle(&checking).0, OcrModelDownloadState::Checking);
        let unknown = [(&FAKE_SPEC, OcrModelDownloadState::Checking), (&THIRD_SPEC, OcrModelDownloadState::Unknown)];
        assert_eq!(aggregate_bundle(&unknown).0, OcrModelDownloadState::Unknown);
        let gone = [(&FAKE_SPEC, OcrModelDownloadState::Unknown), (&THIRD_SPEC, OcrModelDownloadState::Unavailable("x".to_owned()))];
        assert_eq!(aggregate_bundle(&gone).0, OcrModelDownloadState::Unavailable("x".to_owned()));
        let installed = [(&FAKE_SPEC, OcrModelDownloadState::Installed), (&THIRD_SPEC, OcrModelDownloadState::Installed)];
        assert_eq!(aggregate_bundle(&installed), (OcrModelDownloadState::Installed, 0));
        // The status-based label uses the same rule: one spec installed -> only the other's size.
        assert_eq!(
            download_label_for_statuses(&[(&FAKE_SPEC, ExternalModelStatus::Installed), (&THIRD_SPEC, ExternalModelStatus::Missing)]),
            download_button_label(THIRD_SPEC.total_bytes())
        );
        assert_eq!(
            download_label_for_statuses(&[(&FAKE_SPEC, ExternalModelStatus::Partial { bytes_present: 9 }), (&THIRD_SPEC, ExternalModelStatus::Missing)]),
            resume_button_label(9, 180)
        );
    }

    /// A verify and a download exclude each other in both directions; the verify shows
    /// `Verifying` progress, a cancel leaves a neutral notice and re-probes the bundle.
    #[test]
    fn verify_and_download_exclude_each_other_and_a_cancelled_verify_reprobes() {
        let jobs = Jobs { status: installed_probe, download: waits_for_cancel, verify: verify_waits_for_cancel };
        let mut controller = OcrModelDownloadController::with_jobs(jobs);
        controller.start_verify(BUNDLE);
        poll_until(&mut controller, |c| matches!(c.panel_snapshot(BUNDLE).state, OcrModelDownloadState::Verifying(p) if p.total_done == 7));
        assert!(matches!(controller.state_for(THIRD_SPEC.id), OcrModelDownloadState::Verifying(_)));
        // No download (of this or another bundle) and no second verify while it runs.
        controller.start_download(BUNDLE);
        controller.start_download(OTHER);
        controller.start_verify(OTHER);
        assert!(matches!(controller.panel_snapshot(BUNDLE).state, OcrModelDownloadState::Verifying(_)));
        let other = controller.panel_snapshot(OTHER).other_download.expect("the verify blocks other bundles");
        assert_eq!(other.kind, JobKind::Verify);
        assert_eq!(controller.state_for(OTHER_SPEC.id), OcrModelDownloadState::Unknown);
        controller.cancel();
        poll_until(&mut controller, |c| !c.is_active());
        let snapshot = controller.panel_snapshot(BUNDLE);
        assert_eq!(snapshot.state, OcrModelDownloadState::Installed, "re-probed after the verify");
        assert_eq!(snapshot.notice, Some(DownloadNotice::VerifyCancelled));

        // And the other way round: a running download refuses a verify.
        controller.start_download(OTHER);
        controller.start_verify(BUNDLE);
        assert!(matches!(controller.state_for(OTHER_SPEC.id), OcrModelDownloadState::Downloading(_)));
        assert!(!matches!(controller.state_for(FAKE_SPEC.id), OcrModelDownloadState::Verifying(_)));
        controller.cancel();
        poll_until(&mut controller, |c| !c.is_active());
        // A new job of the bundle clears its old notice.
        controller.start_verify(BUNDLE);
        assert_eq!(controller.panel_snapshot(BUNDLE).notice, None);
        controller.cancel();
        poll_until(&mut controller, |c| !c.is_active());
    }

    /// An intact verify keeps its notice while the bundle reads installed; a damaged one
    /// re-probes the bundle AND every spec it invalidated, and counts the removed files.
    #[test]
    fn a_finished_verify_reprobes_the_bundle_and_the_invalidated_specs() {
        let mut intact = OcrModelDownloadController::with_jobs(Jobs { status: installed_probe, download: ok_download, verify: intact_verify });
        intact.start_verify(BUNDLE);
        poll_until(&mut intact, |c| !c.is_active());
        let snapshot = intact.panel_snapshot(BUNDLE);
        assert_eq!(snapshot.state, OcrModelDownloadState::Installed);
        assert_eq!(snapshot.notice, Some(DownloadNotice::VerifyIntact));

        let mut damaged = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: ok_download, verify: damaged_verify });
        damaged.start_verify(FAKE);
        // `OTHER_SPEC` lies outside the verified bundle; its marker was invalidated too, but
        // only catalog specs can be re-probed (the fakes are not in the catalog), so the
        // bundle's own re-probe is what is observable here.
        poll_until(&mut damaged, |c| !c.is_active());
        let snapshot = damaged.panel_snapshot(FAKE);
        assert_eq!(snapshot.state, OcrModelDownloadState::Missing);
        assert_eq!(snapshot.notice, Some(DownloadNotice::VerifyRemoved { files: 1 }));
        // The repair download clears the verify notice.
        damaged.start_download(FAKE);
        assert_eq!(damaged.panel_snapshot(FAKE).notice, None);
        poll_until(&mut damaged, |c| !c.is_active());
    }

    /// The verify thread verifies every spec (installed or not), maps progress onto the
    /// bundle, sums damaged files and collects invalidated ids without duplicates.
    #[test]
    fn bundle_verify_covers_every_spec_and_collects_invalidated_ids() {
        let cancel = AtomicBool::new(false);
        let mut invalidated = Vec::new();
        let jobs = Jobs { status: installed_probe, download: ok_download, verify: damaged_verify };
        let result = run_bundle_verify(jobs, BUNDLE, &cancel, &mut |_| {}, &mut invalidated);
        assert_eq!(result.ok(), Some(2));
        assert_eq!(invalidated, [FAKE_SPEC.id, OTHER_SPEC.id, THIRD_SPEC.id]);

        let mut reports = Vec::new();
        let jobs = Jobs { status: missing_probe, download: ok_download, verify: intact_verify };
        let result = run_bundle_verify(jobs, BUNDLE, &cancel, &mut |p| reports.push(*p), &mut Vec::new());
        assert_eq!(result.ok(), Some(0));
        let last = reports.last().copied().expect("progress was reported");
        assert_eq!((last.file_index, last.file_count, last.total_bytes), (2, 3, 180));

        cancel.store(true, Ordering::Relaxed);
        let result = run_bundle_verify(jobs, BUNDLE, &cancel, &mut |_| {}, &mut Vec::new());
        assert!(matches!(result, Err(ExternalModelError::Cancelled)), "{result:?}");
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
        assert_eq!(offered_label(&OcrModelDownloadState::Missing, 150), Some(download_button_label(150)));
        for state in [
            OcrModelDownloadState::Unknown,
            OcrModelDownloadState::Checking,
            OcrModelDownloadState::Installed,
            OcrModelDownloadState::Downloading(initial_progress(FAKE, &HashMap::new())),
            OcrModelDownloadState::Unavailable("x".to_owned()),
        ] {
            assert_eq!(offered_label(&state, 150), None, "{state:?}");
        }
        assert_eq!(download_label_for_statuses(&[(&FAKE_SPEC, ExternalModelStatus::Installed)]), download_button_label(FAKE_SPEC.total_bytes()));
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
