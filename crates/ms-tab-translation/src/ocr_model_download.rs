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
  failed / refused by a gated repository), shown NEXT TO the state, never instead of it.
- `OcrModelPanelSnapshot`: what a panel's download block needs for one bundle in one frame
  (aggregated state, bytes still to download — a `Partial` spec counts only what it lacks —,
  notice, the OTHER bundle's download that blocks this one, if any, and the `RepoAccessLine`s
  of its gated repositories).
- Gated Hugging Face repositories (`ExternalSource::HuggingFace { gated: true }`): the
  controller checks access (`ms_sysprobe::ai_models::hf_access::hf_repo_access`, injected as
  `AccessCheck`) on a worker for every gated repository a shown bundle still needs (a spec
  probed `Missing` / `Partial`; an installed spec never), once per repository and token value
  (the repository is `ExternalModelSpec::gated_repo`, the catalog's one owner of it);
  `poll` compares the cached token (a fingerprint, never a copy) and a change drops every
  answer AND every `DownloadNotice::Access`, so the shown repositories are checked again and no
  refusal of the old token stays on screen. An `Unreachable` answer is not kept: it is asked
  again `UNREACHABLE_RECHECK` after it arrived (the per-frame `probe_unknown` compares one
  instant) and as soon as any job ends. A download re-checks the repositories of its pending
  specs before ANY transfer, with the job's cancel flag, and ends with `DownloadNotice::Access`
  on a definite refusal (`HfAccess::refuses_download`); an `Unreachable` answer does not stop
  it (the transfer decides). The Download button is never disabled by an access answer.
- `OcrModelDownloadController`: one status worker (stat-only, per-spec probes) plus at most one
  job thread at a time — a DOWNLOAD (the bundle's not-installed specs, sequentially) or a
  VERIFY (`start_verify`: hashes every file of every spec against its pin, deleting damaged
  files and the markers of every spec sharing them, the repair path for a same-size
  corruption the stat-only probe cannot see) — reporting one progress over the whole bundle;
  progress is coalesced in a mutex slot that `poll` reads. A finished verify re-probes the
  bundle and every spec it invalidated and leaves a neutral notice (intact / N files removed).
- Automatic ADOPTION (`Adoption`, the third job kind): a spec probed `Missing` whose files are
  all on disk at their pinned sizes without its marker (`unmarked_files_present`, e.g. left by
  an older downloader) is adopted once per process — hashed against its pins and given its
  marker by `adopt_external_model`, with no network and nothing deleted. A bundle registered
  through `probe_unknown` / `request_bundle_status` is checked once all its specs are probed,
  then kept as decided (so the per-frame `probe_unknown` costs no allocation) until a status
  answer for one of its specs or an explicit re-ask; the job shows as `Verifying` with phase
  `Adopting`; a mismatch leaves a notice and the regular Download then fetches only the
  differing files. Only files with NO marker are adoptable (`unmarked_files_present`).
- `running_job`: the kind of the one running job, for owners that gate other work on it.
- `offered_download_label`: the one owner of "which download button caption is visible for a
  snapshot" — used by the panel block AND (through `download_label_for_statuses`, the same
  aggregation over probe answers) by the "model is not downloaded" errors, so an error names
  exactly the button the user sees.
- `access_status_key`: the one `HfAccess` -> i18n key mapping (panel lines and errors).
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
  says part of the model is on disk, next to the "download the rest" button. No process-global state is
  kept for it: the on-disk staging is the durable record, and it also covers a crash.
- The worker bodies are injected as plain function pointers (`Jobs`, `Adoption`,
  `AccessCheck`) so the state machine is unit-tested with fakes and no filesystem or network.
- An access check thread is detached like a download thread; its answer carries the token
  fingerprint it ran under and is dropped when the token changed meanwhile.
- The once-per-process adoption record (`AUTO_ADOPT_ATTEMPTED`, spec ids) is the one piece of
  process-global state here: a tab rebuild creates a new controller, and re-hashing ~16 GB on
  every rebuild after a refused adoption is exactly what it prevents. Tests inject their own.
*/

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use ms_config as config;
use ms_sysprobe::ai_models::external::{
    ExternalDownloadPhase, ExternalDownloadProgress, ExternalModelError, ExternalModelSpec, ExternalModelStatus, VerifyReport,
    adopt_external_model, download_external_model, external_model_status, unmarked_files_present, verify_external_model,
};
use ms_sysprobe::ai_models::external_catalog::all_specs;
use ms_sysprobe::ai_models::hf_access::{HfAccess, hf_repo_access};
use ms_thread::{self as thread, JoinHandle};
use web_time::{Duration, Instant};

/// How long an `Unreachable` access answer is shown before the shown repository is asked
/// again (a job's end asks at once). Long enough that a panel left open on a dead network
/// sends one small request a minute, never one per frame.
const UNREACHABLE_RECHECK: Duration = Duration::from_secs(60);

/// What a panel knows about one external model spec, or — aggregated — one bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcrModelDownloadState {
    /// Never probed in this session.
    Unknown,
    /// A status probe is queued or running on the worker.
    Checking,
    /// Nothing usable is installed for this spec.
    Missing,
    /// Part of this spec is on disk (`bytes_present` bytes): an unfinished (cancelled, failed
    /// or interrupted) download, or some files found without any completion marker; a
    /// download keeps what hashes to its pin and fetches only the rest.
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
    /// An automatic adoption found the catalog file `file` on disk with another size or
    /// content; nothing was deleted, and a download fetches only the differing files.
    AdoptMismatch { file: &'static str },
    /// An automatic adoption failed for another reason; the localized reason.
    AdoptFailed(String),
    /// The user cancelled an automatic adoption (it is not retried automatically in this
    /// process; a download keeps the matching files).
    AdoptCancelled,
    /// The download stopped because the gated Hugging Face repository `repo_id` refused access
    /// (`access` is never `Granted`); nothing of that repository was transferred.
    Access {
        /// The refusing repository, `owner/name`.
        repo_id: &'static str,
        /// Why it refused.
        access: HfAccess,
    },
}

impl DownloadNotice {
    /// Whether this notice reports a verify (kept even while the bundle reads installed).
    #[must_use]
    pub fn is_verify(&self) -> bool {
        match self {
            Self::VerifyIntact | Self::VerifyRemoved { .. } | Self::VerifyCancelled => true,
            Self::Cancelled
            | Self::Failed(_)
            | Self::AdoptMismatch { .. }
            | Self::AdoptFailed(_)
            | Self::AdoptCancelled
            | Self::Access { .. } => false,
        }
    }
}

/// The access state of one gated Hugging Face repository that a bundle still needs (one of its
/// specs is probed and not installed); see [`OcrModelPanelSnapshot::access`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoAccessLine {
    /// The repository, `owner/name`.
    pub repo_id: &'static str,
    /// The answer of the last access check made with the current token, or `None` while that
    /// check is queued or running.
    pub access: Option<HfAccess>,
}

/// What the one running job of the controller does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    /// Downloading (or resuming) the bundle.
    Download,
    /// Hashing the bundle's files against their pins.
    Verify,
    /// Adopting files found on disk without their markers (automatic, see [`Adoption`]).
    Adopt,
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
    /// Bytes a download would transfer: the totals of the bundle's specs not installed, minus
    /// what a `Partial` probe found already on disk (see `aggregate_bundle`).
    pub missing_bytes: u64,
    /// The outcome of its last unsuccessful download, if any (never while installed).
    pub notice: Option<DownloadNotice>,
    /// Another bundle's running download, which disables this bundle's download button.
    pub other_download: Option<OtherDownload>,
    /// The gated Hugging Face repositories of the bundle's specs that are probed and not
    /// installed, once each in bundle order, with their access state (granted ones included;
    /// the panel draws only the others). Empty while this bundle's own job runs.
    pub access: Vec<RepoAccessLine>,
}

/// Stable key of a download bundle: its spec ids in order, joined with `+`. A one-spec
/// bundle's key is the spec id itself.
#[must_use]
pub fn bundle_key(specs: &[&'static ExternalModelSpec]) -> String {
    specs.iter().map(|spec| spec.id).collect::<Vec<_>>().join("+")
}

/// Whether two bundles hold the same specs in the same order (what [`bundle_key`] would say,
/// without allocating the keys).
fn same_bundle(left: &[&'static ExternalModelSpec], right: &[&'static ExternalModelSpec]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(a, b)| a.id == b.id)
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

/// Blocking stat-only "present but unmarked" probe of one spec (worker thread only).
pub(crate) type UnmarkedProbe = fn(&'static ExternalModelSpec) -> bool;
/// Blocking adoption of one spec with a cancel flag and a per-read progress callback (worker
/// thread only); never downloads, never deletes.
pub(crate) type AdoptJob = DownloadJob;

/// The worker bodies. Production uses the real `ms_sysprobe` functions under
/// `ms_config::side_models_dir()`; tests (also the tab's) inject fakes.
#[derive(Clone, Copy)]
pub(crate) struct Jobs {
    pub(crate) status: StatusProbe,
    pub(crate) download: DownloadJob,
    pub(crate) verify: VerifyJob,
}

/// The worker bodies of automatic adoption: the probe the status worker runs for every spec
/// that reads `Missing`, and the job that adopts one spec.
#[derive(Clone, Copy)]
pub(crate) struct Adoption {
    pub(crate) unmarked: UnmarkedProbe,
    pub(crate) adopt: AdoptJob,
}

/// Blocking access check of one gated Hugging Face repository (`owner/name`) with the current
/// token and a cancel flag (a download's own flag; a never-set one for the controller's own
/// checks) (worker thread only).
pub(crate) type AccessProbe = fn(&str, &AtomicBool) -> HfAccess;
/// Non-blocking read of the current Hugging Face token (empty = none); called on the GUI
/// thread once per `poll`, so it must be a cache read.
pub(crate) type TokenReader = fn() -> String;

/// The access-check seam: the probe the controller runs on a worker for a gated repository a
/// shown bundle still needs (and the download thread runs before any transfer), and the token
/// reader whose change invalidates every answer.
#[derive(Clone, Copy)]
pub(crate) struct AccessCheck {
    pub(crate) probe: AccessProbe,
    pub(crate) token: TokenReader,
}

/// A fingerprint of a token value, so the controller can tell a token change without keeping
/// a second copy of the secret (the value only ever lives in `ms_sysprobe::hf_token`'s cache).
fn token_fingerprint(token: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    // `DefaultHasher::new()` uses fixed keys: equal tokens always give equal fingerprints
    // within the process, which is all the comparison needs.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut hasher);
    hasher.finish()
}

/// What the controller knows about one gated repository under the current token.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RepoAccessState {
    /// A check is running on a worker.
    Checking,
    /// The answer of the finished check, and when it arrived (an `Unreachable` one expires,
    /// see [`UNREACHABLE_RECHECK`]).
    Known { access: HfAccess, at: Instant },
}

impl RepoAccessState {
    /// Whether this is an `Unreachable` answer (never kept for good).
    fn is_unreachable(&self) -> bool {
        matches!(self, Self::Known { access: HfAccess::Unreachable { .. }, .. })
    }
}

/// Spec ids an automatic adoption was started for in this process (D9: at most once each,
/// whatever its outcome); see the file header for why this is process-global.
static AUTO_ADOPT_ATTEMPTED: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());

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

/// Production "present but unmarked" probe under `ms_config::side_models_dir()`.
fn real_unmarked_probe(spec: &'static ExternalModelSpec) -> bool {
    unmarked_files_present(&config::side_models_dir(), spec)
}

/// Production adoption job under `ms_config::side_models_dir()`.
fn real_adopt_job(
    spec: &'static ExternalModelSpec,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&ExternalDownloadProgress),
) -> Result<PathBuf, ExternalModelError> {
    adopt_external_model(&config::side_models_dir(), spec, cancel, progress)
}

/// Production access check: `ms_sysprobe`'s Hub probe with the cached token.
fn real_access_probe(repo_id: &str, cancel: &AtomicBool) -> HfAccess {
    hf_repo_access(repo_id, cancel)
}

/// Production token reader: the cached Hugging Face token (never the OS secret store).
fn real_token_reader() -> String {
    ms_sysprobe::hf_token::hf_token()
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
    /// A probe answer; `adoptable` is the "present but unmarked" probe, run only for `Missing`.
    Status { spec_id: &'static str, status: ExternalModelStatus, adoptable: bool },
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
    /// An automatic adoption of `specs` (the adoptable specs of bundle `key`) ended: the first
    /// failure, or `Ok` when every spec was adopted.
    AdoptFinished { key: String, specs: Vec<&'static ExternalModelSpec>, result: Result<(), ExternalModelError> },
    /// An access check of `repo_id`, started under the token with fingerprint `token`,
    /// answered `access`; `proactive` for the controller's own check (counted in
    /// `pending_access`), `false` for the download thread's re-check.
    Access { repo_id: &'static str, token: u64, access: HfAccess, proactive: bool },
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
    /// Fingerprint of the token the job started under: an access refusal it reports is kept
    /// only while that token is still the current one.
    token: u64,
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
            // An adoption hashes files like a verify; its progress phase says `Adopting`.
            JobKind::Verify | JobKind::Adopt => OcrModelDownloadState::Verifying(self.latest),
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
    adoption: Adoption,
    /// Spec ids whose last probe read `Missing` with every file present but unmarked.
    adoptable: HashSet<&'static str>,
    /// Bundles registered for the automatic-adoption check, decided once all their specs
    /// are probed (see `start_auto_adoption`).
    adopt_watch: Vec<Vec<&'static ExternalModelSpec>>,
    /// Bundles already decided, so the per-frame [`Self::probe_unknown`] does not register them
    /// again. A bundle leaves this list when a status answer arrives for one of its specs or
    /// [`Self::request_bundle_status`] re-asks it (its disk state may have changed).
    adopt_decided: Vec<Vec<&'static ExternalModelSpec>>,
    /// The once-per-process adoption record ([`AUTO_ADOPT_ATTEMPTED`] in production).
    auto_adopt_attempted: &'static Mutex<BTreeSet<&'static str>>,
    /// The access-check seam (production: the Hub probe and the cached token).
    access_check: AccessCheck,
    /// Access per gated repository id under the token with fingerprint `access_token`;
    /// cleared when the token changes, so every shown repository is checked again.
    access: HashMap<&'static str, RepoAccessState>,
    /// Fingerprint of the token the `access` answers belong to.
    access_token: u64,
    /// Access checks started and not answered yet (drives `is_active`).
    pending_access: usize,
    /// Age after which an `Unreachable` answer is asked again ([`UNREACHABLE_RECHECK`];
    /// tests shorten it).
    unreachable_recheck: Duration,
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
            .field("access", &self.access)
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
    /// Creates the controller and its status worker over the real `ms_sysprobe` probes, with
    /// automatic adoption recorded process-wide.
    #[must_use]
    pub fn new() -> Self {
        Self::with_parts(
            Jobs { status: real_status_probe, download: real_download_job, verify: real_verify_job },
            Adoption { unmarked: real_unmarked_probe, adopt: real_adopt_job },
            AccessCheck { probe: real_access_probe, token: real_token_reader },
            &AUTO_ADOPT_ATTEMPTED,
        )
    }

    /// Creates the controller over the given worker bodies, with automatic adoption disabled
    /// (nothing is ever adoptable) and every repository granted without a token; for the tests
    /// of the download / verify state machine.
    #[cfg(test)]
    pub(crate) fn with_jobs(jobs: Jobs) -> Self {
        Self::with_parts(jobs, fakes::NO_ADOPTION, fakes::GRANTED_ACCESS, Box::leak(Box::new(Mutex::new(BTreeSet::new()))))
    }

    /// Creates the controller over the given worker bodies, adoption bodies, access-check seam
    /// and adoption record.
    pub(crate) fn with_parts(
        jobs: Jobs,
        adoption: Adoption,
        access_check: AccessCheck,
        auto_adopt_attempted: &'static Mutex<BTreeSet<&'static str>>,
    ) -> Self {
        let (status_tx, status_rx) = mpsc::channel::<StatusCommand>();
        let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();
        let worker_tx = evt_tx.clone();
        let (probe, unmarked) = (jobs.status, adoption.unmarked);
        let status_worker = thread::spawn(move || status_worker_loop(&status_rx, &worker_tx, probe, unmarked));
        Self {
            jobs,
            adoption,
            adoptable: HashSet::new(),
            adopt_watch: Vec::new(),
            adopt_decided: Vec::new(),
            auto_adopt_attempted,
            access_check,
            access: HashMap::new(),
            access_token: token_fingerprint(&(access_check.token)()),
            pending_access: 0,
            unreachable_recheck: UNREACHABLE_RECHECK,
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

    /// [`Self::request_status`] for every spec of a bundle; also registers the bundle for
    /// the automatic-adoption check again, even when it was decided before (see
    /// [`Self::probe_unknown`]).
    pub fn request_bundle_status(&mut self, specs: &[&'static ExternalModelSpec]) {
        for spec in specs {
            self.request_status(spec);
        }
        self.adopt_decided.retain(|decided| !same_bundle(decided, specs));
        self.watch_for_adoption(specs);
    }

    /// Starts an access check on a worker for every gated Hugging Face repository that a spec
    /// of `specs` downloads from while it is probed `Missing` / `Partial`, unless that
    /// repository already has an answer (or a running check) under the current token; an
    /// `Unreachable` answer counts only while younger than `unreachable_recheck`. A spec of the
    /// running job is skipped (the job checks its repositories itself), an installed spec is
    /// never checked. Allocates nothing when there is nothing to start, so the per-frame
    /// [`Self::probe_unknown`] calls it.
    fn check_access(&mut self, specs: &[&'static ExternalModelSpec]) {
        for spec in specs {
            let Some(repo_id) = spec.gated_repo() else { continue };
            let needs_download = matches!(self.states.get(spec.id), Some(OcrModelDownloadState::Missing | OcrModelDownloadState::Partial { .. }));
            let answered = match self.access.get(repo_id) {
                None => false,
                Some(RepoAccessState::Checking) => true,
                Some(RepoAccessState::Known { access: HfAccess::Unreachable { .. }, at }) => at.elapsed() < self.unreachable_recheck,
                Some(RepoAccessState::Known { .. }) => true,
            };
            if !needs_download || answered || self.download.as_ref().is_some_and(|active| active.contains(spec.id)) {
                continue;
            }
            self.start_access_check(repo_id);
        }
    }

    /// Drops every `Unreachable` answer, so the shown repositories are asked again at once
    /// (called when a job ends: the network may be back, and the job's own answer is no
    /// fresher than a new check).
    fn forget_unreachable_answers(&mut self) {
        self.access.retain(|_repo_id, state| !state.is_unreachable());
    }

    /// Runs the access check of `repo_id` on a new, detached thread (a short request: at most
    /// the probe's own small retry budget) and marks the repository `Checking`.
    fn start_access_check(&mut self, repo_id: &'static str) {
        let probe = self.access_check.probe;
        let token = self.access_token;
        let evt_tx = self.evt_tx.clone();
        // The handle is dropped on purpose: like a download thread, the check is detached and
        // a controller dropped meanwhile simply never reads its answer.
        drop(thread::spawn(move || {
            // The controller's own check is short and has no Cancel button of its own.
            let access = probe(repo_id, &AtomicBool::new(false));
            if evt_tx.send(WorkerEvent::Access { repo_id, token, access, proactive: true }).is_err() {
                ms_log::runtime_log::log_info(format!("[ocr-model] access check of '{repo_id}' answered after its controller was dropped"));
            }
        }));
        self.access.insert(repo_id, RepoAccessState::Checking);
        self.pending_access = self.pending_access.saturating_add(1);
    }

    /// Re-reads the token; when it changed, drops every access answer so the shown
    /// repositories are checked again with the new one, and every `DownloadNotice::Access`
    /// (a refusal of the old token must not hide the new answer). Returns `true` when it
    /// changed.
    fn sync_access_token(&mut self) -> bool {
        let token = token_fingerprint(&(self.access_check.token)());
        if token == self.access_token {
            return false;
        }
        ms_log::runtime_log::log_info("[ocr-model] Hugging Face token changed: repository access is checked again");
        self.access_token = token;
        self.access.clear();
        self.notices.retain(|_key, notice| !matches!(notice, DownloadNotice::Access { .. }));
        true
    }

    /// Probes the specs of a bundle that were never probed in this session (opening a panel
    /// on, or switching to, a model), and registers the bundle for the automatic-adoption
    /// check: once every spec of it is probed, a [`Self::poll`] adopts the specs that read
    /// `Missing` with all files present but unmarked, unless this process already tried. Also
    /// starts the access check of every gated repository the bundle still needs (once per
    /// repository and token, see `check_access`).
    ///
    /// Meant to be called every frame: when every spec is probed and the bundle is watched or
    /// already decided, it allocates nothing and sends nothing.
    pub fn probe_unknown(&mut self, specs: &[&'static ExternalModelSpec]) {
        for spec in specs {
            // `request_status` itself skips a spec the running job owns.
            if matches!(self.states.get(spec.id), None | Some(OcrModelDownloadState::Unknown)) {
                self.request_status(spec);
            }
        }
        self.watch_for_adoption(specs);
        self.check_access(specs);
    }

    /// Registers a non-empty bundle for the automatic-adoption check unless it is watched or
    /// already decided (callers may call this every frame; the comparison allocates nothing).
    fn watch_for_adoption(&mut self, specs: &[&'static ExternalModelSpec]) {
        if specs.is_empty()
            || self.adopt_watch.iter().any(|watched| same_bundle(watched, specs))
            || self.adopt_decided.iter().any(|decided| same_bundle(decided, specs))
        {
            return;
        }
        self.adopt_watch.push(specs.to_vec());
    }

    /// The kind of the one running job (download, verify or automatic adoption), or `None`
    /// when no job runs. Owners that must not start a competing transfer elsewhere (the FLUX.2
    /// klein engine's switch gate) read it.
    #[must_use]
    pub fn running_job(&self) -> Option<JobKind> {
        self.download.as_ref().map(|active| active.kind)
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
        // The job re-checks access under the token of this moment; answers of an older token
        // are dropped first so the job's answers are the ones kept.
        self.sync_access_token();
        let token = self.access_token;
        let access_probe = self.access_check.probe;
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
            let access_tx = evt_tx.clone();
            let mut access_seen = |repo_id: &'static str, access: &HfAccess| {
                let answer = WorkerEvent::Access { repo_id, token, access: access.clone(), proactive: false };
                // Best effort, like `installed`: a dropped controller has no access lines.
                if access_tx.send(answer).is_err() {
                    ms_log::runtime_log::log_info(format!("[ocr-model] access of '{repo_id}' checked after its controller was dropped"));
                }
            };
            let result =
                run_bundle_download(jobs, access_probe, &thread_specs, &thread_cancel, &mut report, &mut installed, &mut access_seen);
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
        self.download = Some(ActiveDownload { kind: JobKind::Download, specs, key, cancel, progress, latest, token, thread: Some(handle) });
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
        let token = self.access_token;
        self.download = Some(ActiveDownload { kind: JobKind::Verify, specs, key, cancel, progress, latest, token, thread: Some(handle) });
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
        let mut changed = self.sync_access_token();
        loop {
            match self.evt_rx.try_recv() {
                Ok(WorkerEvent::Status { spec_id, status, adoptable }) => {
                    self.pending_probes = self.pending_probes.saturating_sub(1);
                    // A probe answered after a download started describes the old disk
                    // state; the download owns the state until it finishes.
                    // A fresh answer may change what is adoptable: the bundles holding this spec
                    // are decided again the next time they are watched.
                    self.adopt_decided.retain(|decided| !decided.iter().any(|spec| spec.id == spec_id));
                    if !self.download.as_ref().is_some_and(|active| active.contains(spec_id)) {
                        changed |= self.set_state(spec_id, state_from_status(status));
                        if adoptable {
                            self.adoptable.insert(spec_id);
                        } else {
                            self.adoptable.remove(spec_id);
                        }
                    }
                }
                Ok(WorkerEvent::SpecInstalled { spec_id }) => {
                    changed |= self.set_state(spec_id, OcrModelDownloadState::Installed);
                }
                Ok(WorkerEvent::Access { repo_id, token, access, proactive }) => {
                    if proactive {
                        self.pending_access = self.pending_access.saturating_sub(1);
                    }
                    // An answer made with an older token describes that token: dropped (the
                    // repository is checked again with the current one).
                    if token == self.access_token {
                        self.access.insert(repo_id, RepoAccessState::Known { access, at: Instant::now() });
                        changed = true;
                    }
                }
                Ok(WorkerEvent::DownloadFinished { key, specs, result }) => {
                    // The thread has sent its last message and is returning: dropping the
                    // handle detaches a thread that is already done.
                    let job_token = self.download.as_ref().filter(|active| active.key == key).map(|active| active.token);
                    if job_token.is_some() {
                        self.download = None;
                    }
                    changed = true;
                    // A 401 / 403 met mid-transfer is reported only through the result: it is
                    // the repository's current answer too (while the job's token is current).
                    self.forget_unreachable_answers();
                    if let Err(ExternalModelError::AccessDenied { repo_id, access }) = &result
                        && job_token == Some(self.access_token)
                    {
                        self.access.insert(repo_id, RepoAccessState::Known { access: access.clone(), at: Instant::now() });
                    }
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
                    self.forget_unreachable_answers();
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
                Ok(WorkerEvent::AdoptFinished { key, specs, result }) => {
                    if self.download.as_ref().is_some_and(|active| active.key == key) {
                        self.download = None;
                    }
                    changed = true;
                    self.forget_unreachable_answers();
                    let notice = match result {
                        Ok(()) => None,
                        Err(ExternalModelError::Cancelled) => Some(DownloadNotice::AdoptCancelled),
                        Err(ExternalModelError::HashMismatch { file, .. } | ExternalModelError::SizeMismatch { file, .. }) => {
                            Some(DownloadNotice::AdoptMismatch { file })
                        }
                        Err(err) => Some(DownloadNotice::AdoptFailed(external_error_message(&err))),
                    };
                    match notice {
                        None => {
                            self.notices.remove(&key);
                            for spec in &specs {
                                self.adoptable.remove(spec.id);
                                self.states.insert(spec.id, OcrModelDownloadState::Installed);
                            }
                        }
                        Some(notice) => {
                            // Adopted specs were reported installed on the way; the re-probe
                            // shows the real state of the rest (`Missing`, with the Download
                            // button that repairs only the differing files).
                            self.notices.insert(key, notice);
                            self.request_bundle_status(&specs);
                        }
                    }
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
        changed |= self.start_auto_adoption();
        changed
    }

    /// Decides the watched bundles whose specs are all probed: starts ONE automatic adoption
    /// of the first bundle with adoptable specs (probed `Missing`, files present but unmarked,
    /// never tried in this process) and moves every decided bundle to `adopt_decided`. Waits
    /// while any job runs. Returns `true` when an adoption started.
    fn start_auto_adoption(&mut self) -> bool {
        if self.download.is_some() || self.adopt_watch.is_empty() {
            return false;
        }
        // Copied out of `self` so the guard does not borrow it; held only for this in-memory
        // pass (no I/O, no thread start under it).
        let record = self.auto_adopt_attempted;
        // A poisoned record only means a panic while inserting ids; its set is intact.
        let mut attempted = match record.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut chosen: Option<(String, Vec<&'static ExternalModelSpec>)> = None;
        let mut undecided = Vec::new();
        for bundle in std::mem::take(&mut self.adopt_watch) {
            let probed = bundle.iter().all(|spec| {
                !matches!(self.states.get(spec.id), None | Some(OcrModelDownloadState::Unknown | OcrModelDownloadState::Checking))
            });
            if !probed || chosen.is_some() {
                undecided.push(bundle);
                continue;
            }
            let candidates: Vec<&'static ExternalModelSpec> = bundle
                .iter()
                .copied()
                .filter(|spec| {
                    self.adoptable.contains(spec.id)
                        && self.states.get(spec.id) == Some(&OcrModelDownloadState::Missing)
                        && !attempted.contains(spec.id)
                })
                .collect();
            if !candidates.is_empty() {
                chosen = Some((bundle_key(&bundle), candidates));
            }
            self.adopt_decided.push(bundle);
        }
        self.adopt_watch = undecided;
        let Some((key, specs)) = chosen else { return false };
        attempted.extend(specs.iter().map(|spec| spec.id));
        drop(attempted);
        self.start_adoption(key, specs);
        true
    }

    /// Starts adopting `specs` (the adoptable specs of the bundle with key `key`, so that
    /// bundle's snapshot shows the job as its own) on a new thread and clears the bundle's
    /// notice. Callers ensure no job runs.
    fn start_adoption(&mut self, key: String, specs: Vec<&'static ExternalModelSpec>) {
        self.notices.remove(&key);
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(None::<ExternalDownloadProgress>));
        let adopt = self.adoption.adopt;
        let evt_tx = self.evt_tx.clone();
        let thread_cancel = Arc::clone(&cancel);
        let thread_progress = Arc::clone(&progress);
        let thread_specs = specs.clone();
        let thread_key = key.clone();
        ms_log::runtime_log::log_info(format!(
            "[ocr-model] automatic adoption started: '{key}' adopts {:?} (files present without markers; hashing, no download)",
            specs.iter().map(|spec| spec.id).collect::<Vec<_>>()
        ));
        let handle = thread::spawn(move || {
            let mut report = |snapshot: &ExternalDownloadProgress| {
                // Latest wins, as for a download (see `start_download`).
                let mut slot = match thread_progress.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *slot = Some(*snapshot);
            };
            let mut installed = |spec: &'static ExternalModelSpec| {
                // Best effort: a dropped controller re-probes from disk next time.
                if evt_tx.send(WorkerEvent::SpecInstalled { spec_id: spec.id }).is_err() {
                    ms_log::runtime_log::log_info(format!("[ocr-model] '{}' adopted after its controller was dropped", spec.id));
                }
            };
            let result = run_bundle_adopt(adopt, &thread_specs, &thread_cancel, &mut report, &mut installed);
            match &result {
                Ok(()) => ms_log::runtime_log::log_info(format!("[ocr-model] automatic adoption of '{thread_key}' finished")),
                Err(err) => ms_log::runtime_log::log_warn(format!("[ocr-model] automatic adoption of '{thread_key}' ended: {err}")),
            }
            if evt_tx.send(WorkerEvent::AdoptFinished { key: thread_key.clone(), specs: thread_specs, result }).is_err() {
                ms_log::runtime_log::log_info(format!("[ocr-model] adoption of '{thread_key}' ended after its controller was dropped"));
            }
        });
        let latest = adopt_initial_progress(&specs);
        let token = self.access_token;
        self.download = Some(ActiveDownload { kind: JobKind::Adopt, specs, key, cancel, progress, latest, token, thread: Some(handle) });
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
        let own_job_runs = self.download.as_ref().is_some_and(|active| active.key == key);
        let access = if own_job_runs { Vec::new() } else { self.access_lines(specs) };
        OcrModelPanelSnapshot { bundle_key: key, state, missing_bytes, notice, other_download, access }
    }

    /// The access lines of `specs`: each gated repository of a spec probed `Missing` /
    /// `Partial`, once, in bundle order, with its answer under the current token (`None`
    /// while unchecked or checking).
    fn access_lines(&self, specs: &[&'static ExternalModelSpec]) -> Vec<RepoAccessLine> {
        let mut lines: Vec<RepoAccessLine> = Vec::new();
        for spec in specs {
            let Some(repo_id) = spec.gated_repo() else { continue };
            let needs_download = matches!(self.states.get(spec.id), Some(OcrModelDownloadState::Missing | OcrModelDownloadState::Partial { .. }));
            if !needs_download || lines.iter().any(|line| line.repo_id == repo_id) {
                continue;
            }
            let access = match self.access.get(repo_id) {
                Some(RepoAccessState::Known { access, .. }) => Some(access.clone()),
                Some(RepoAccessState::Checking) | None => None,
            };
            lines.push(RepoAccessLine { repo_id, access });
        }
        lines
    }

    /// `true` while a job runs, a status probe or an access check is unanswered: the GUI
    /// keeps repainting so `poll` sees the result.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.download.is_some() || self.pending_probes > 0 || self.pending_access > 0
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
            let key = &active.key;
            let message = match active.kind {
                JobKind::Download => format!(
                    "[ocr-model] download of '{key}' interrupted: its controller was dropped (tab rebuild or exit); \
                     the staged bytes stay on disk and the next probe offers a resume"
                ),
                JobKind::Verify => format!(
                    "[ocr-model] verify of '{key}' interrupted: its controller was dropped (tab rebuild or exit); \
                     files and markers it already removed stay removed, the rest was not checked; the next probe shows the real state"
                ),
                JobKind::Adopt => format!(
                    "[ocr-model] automatic adoption of '{key}' interrupted: its controller was dropped (tab rebuild or exit); \
                     specs adopted before stay marked, the others stay unmarked and are not re-adopted in this process \
                     (a Download keeps their matching files)"
                ),
            };
            ms_log::runtime_log::log_info(message);
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

/// Status worker: answers probes in order until `Stop` or the controller is gone; a `Missing`
/// answer also runs the stat-only `unmarked` probe (adoption candidate).
fn status_worker_loop(commands: &Receiver<StatusCommand>, events: &Sender<WorkerEvent>, probe: StatusProbe, unmarked: UnmarkedProbe) {
    while let Ok(command) = commands.recv() {
        match command {
            StatusCommand::Probe(spec) => {
                let status = probe(spec);
                let adoptable = status == ExternalModelStatus::Missing && unmarked(spec);
                if events.send(WorkerEvent::Status { spec_id: spec.id, status, adoptable }).is_err() {
                    break;
                }
            }
            StatusCommand::Stop => break,
        }
    }
}

/// The body of the download thread: probes every spec of the bundle, checks access to every
/// gated repository the specs not installed download from (each once, through `access` with
/// `cancel`; `access_seen` is told every answer; only a definite refusal stops the job, an
/// `Unreachable` answer lets the transfer decide), then downloads those specs in order and maps each
/// spec's progress onto the bundle (file index and byte counters continue across specs; the
/// totals are those of the specs downloaded). `installed` is told about every spec that
/// finished. Stops at the first error, and before the next spec once `cancel` is set.
/// Blocking: the download thread only.
///
/// # Errors
/// `AccessDenied` for the first definitely refusing repository, before any download call; the
/// first spec's `ExternalModelError`; `Cancelled` when `cancel` was set between steps (an
/// access check included).
fn run_bundle_download(
    jobs: Jobs,
    access: AccessProbe,
    specs: &[&'static ExternalModelSpec],
    cancel: &AtomicBool,
    report: &mut dyn FnMut(&ExternalDownloadProgress),
    installed: &mut dyn FnMut(&'static ExternalModelSpec),
    access_seen: &mut dyn FnMut(&'static str, &HfAccess),
) -> Result<(), ExternalModelError> {
    let pending: Vec<&'static ExternalModelSpec> =
        specs.iter().copied().filter(|spec| (jobs.status)(spec) != ExternalModelStatus::Installed).collect();
    // Access first, for the whole bundle: a refusal must end the job before ANY transfer, not
    // after an open repository's spec already downloaded gigabytes.
    let mut checked: Vec<&'static str> = Vec::new();
    for repo_id in pending.iter().filter_map(|spec| spec.gated_repo()) {
        if checked.contains(&repo_id) {
            continue;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(ExternalModelError::Cancelled);
        }
        checked.push(repo_id);
        let answer = access(repo_id, cancel);
        if cancel.load(Ordering::Relaxed) {
            return Err(ExternalModelError::Cancelled);
        }
        access_seen(repo_id, &answer);
        if answer.refuses_download() {
            return Err(ExternalModelError::AccessDenied { repo_id, access: answer });
        }
        if let HfAccess::Unreachable { detail } = &answer {
            ms_log::runtime_log::log_warn(format!(
                "[ocr-model] access check of '{repo_id}' failed, downloading anyway (the transfer reports a real refusal): {detail}"
            ));
        }
    }
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

/// The body of the adoption thread: adopts every spec of `specs` in order, mapping each
/// spec's progress onto the whole list like a download; `installed` is told about every spec
/// adopted. A refused spec does not stop the others (each is independent and nothing is
/// deleted); a cancel stops at once. Blocking: the adoption thread only.
///
/// # Errors
/// `Cancelled` when cancelled; otherwise the first spec's `ExternalModelError`.
fn run_bundle_adopt(
    adopt: AdoptJob,
    specs: &[&'static ExternalModelSpec],
    cancel: &AtomicBool,
    report: &mut dyn FnMut(&ExternalDownloadProgress),
    installed: &mut dyn FnMut(&'static ExternalModelSpec),
) -> Result<(), ExternalModelError> {
    let file_count = specs.iter().map(|spec| spec.files.len()).sum::<usize>();
    let total_bytes = specs.iter().fold(0_u64, |total, spec| total.saturating_add(spec.total_bytes()));
    let mut files_before = 0_usize;
    let mut bytes_before = 0_u64;
    let mut first_error = None;
    for spec in specs {
        if cancel.load(Ordering::Relaxed) {
            return Err(ExternalModelError::Cancelled);
        }
        let mut forward =
            |progress: &ExternalDownloadProgress| report(&bundle_progress(progress, files_before, file_count, bytes_before, total_bytes));
        match adopt(spec, cancel, &mut forward) {
            Ok(_dir) => installed(spec),
            Err(ExternalModelError::Cancelled) => return Err(ExternalModelError::Cancelled),
            Err(err) => {
                ms_log::runtime_log::log_warn(format!("[ocr-model] adoption of '{}' refused: {err}", spec.id));
                first_error.get_or_insert(err);
            }
        }
        files_before = files_before.saturating_add(spec.files.len());
        bytes_before = bytes_before.saturating_add(spec.total_bytes());
    }
    first_error.map_or(Ok(()), Err)
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

/// The progress shown between the start and the first report of the adoption thread: the
/// verify estimate over `specs`, in phase `Adopting`.
fn adopt_initial_progress(specs: &[&'static ExternalModelSpec]) -> ExternalDownloadProgress {
    ExternalDownloadProgress { phase: ExternalDownloadPhase::Adopting, ..verify_initial_progress(specs) }
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
/// download would transfer: per spec not installed, its total minus the `bytes_present` of a
/// `Partial` probe (an estimate — the download hashes what is present and may fetch a file
/// again). Priority: the first
/// `Unavailable` reason, a `Downloading` / `Verifying` report, `Unknown`, `Checking`; then `Installed`
/// when every spec is; then `Partial` with the bytes of every partial spec; else `Missing`.
/// An empty bundle is `Installed` with nothing to download.
fn aggregate_bundle(entries: &[(&ExternalModelSpec, OcrModelDownloadState)]) -> (OcrModelDownloadState, u64) {
    let missing_bytes = entries.iter().fold(0_u64, |total, (spec, state)| {
        let remaining = match state {
            OcrModelDownloadState::Installed => 0,
            OcrModelDownloadState::Partial { bytes_present } => spec.total_bytes().saturating_sub(*bytes_present),
            OcrModelDownloadState::Unknown
            | OcrModelDownloadState::Checking
            | OcrModelDownloadState::Missing
            | OcrModelDownloadState::Downloading(_)
            | OcrModelDownloadState::Verifying(_)
            | OcrModelDownloadState::Unavailable(_) => spec.total_bytes(),
        };
        total.saturating_add(remaining)
    });
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
/// user cancel is `Cancelled` (not a failure), a refused gated repository is `Access`,
/// anything else is `Failed` with the localized reason.
fn notice_from_download_result(result: Result<(), ExternalModelError>) -> Option<DownloadNotice> {
    match result {
        Ok(()) => None,
        Err(ExternalModelError::Cancelled) => Some(DownloadNotice::Cancelled),
        Err(ExternalModelError::AccessDenied { repo_id, access }) => Some(DownloadNotice::Access { repo_id, access }),
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
        // `Granted` never travels in a refusal; should it, the neutral "could not be checked"
        // text is the least wrong one (the next download checks again).
        ExternalModelError::AccessDenied { access, .. } => access_status_key(access).unwrap_or("translation.ocr_model.access_unreachable_status"),
    }
}

/// The i18n key (taking `{repo}`) that explains a refused access state, `None` for `Granted`
/// (nothing to explain). Shared by the panel's access lines and the error mapping.
#[must_use]
pub(crate) fn access_status_key(access: &HfAccess) -> Option<&'static str> {
    match access {
        HfAccess::Granted => None,
        HfAccess::NoToken => Some("translation.ocr_model.access_no_token_status"),
        HfAccess::TokenRejected => Some("translation.ocr_model.access_token_rejected_status"),
        HfAccess::TermsNotAccepted => Some("translation.ocr_model.access_terms_status"),
        HfAccess::RepoNotFound => Some("translation.ocr_model.access_not_found_status"),
        HfAccess::Unreachable { .. } => Some("translation.ocr_model.access_unreachable_status"),
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
        // The repository id is a language-neutral identifier; the `Unreachable` detail is
        // English diagnostic text and stays in the log.
        ExternalModelError::AccessDenied { repo_id, .. } => ms_i18n::interpolate(template, &[("repo", repo_id)]),
    }
}

/// Caption of the "Download (size)" button for `missing_bytes` to transfer.
pub(crate) fn download_button_label(missing_bytes: u64) -> String {
    tf!("translation.ocr_model.download_button", size = format_bytes(missing_bytes))
}

/// Caption of the "Download the rest (size)" button of a partly present bundle, with
/// `missing_bytes` still to transfer.
fn resume_button_label(missing_bytes: u64) -> String {
    tf!("translation.ocr_model.download_rest_button", size = format_bytes(missing_bytes))
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
        OcrModelDownloadState::Partial { .. } => Some(resume_button_label(missing_bytes)),
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
        AccessCheck, Adoption, ExternalDownloadProgress, ExternalModelError, ExternalModelSpec, ExternalModelStatus, HashMap, HfAccess,
        VerifyReport, adopt_initial_progress, initial_progress, verify_initial_progress,
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
        source: ExternalSource::HuggingFace { repo_id: "owner/name", revision: "0", gated: false },
        dir: "Fake",
        layout: DirLayout::Exclusive,
        files: &FAKE_FILES,
    };
    /// A second spec, to exercise "another model is downloading" and bundles.
    pub(crate) static OTHER_SPEC: ExternalModelSpec = ExternalModelSpec {
        id: "other_model",
        source: ExternalSource::HuggingFace { repo_id: "owner/other", revision: "0", gated: false },
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

    /// Every repository granted, no token stored: access checks never refuse.
    pub(crate) const GRANTED_ACCESS: AccessCheck = AccessCheck { probe: always_granted, token: no_token };

    pub(crate) fn always_granted(_repo_id: &str, _cancel: &AtomicBool) -> HfAccess {
        HfAccess::Granted
    }

    pub(crate) fn no_token() -> String {
        String::new()
    }

    /// Automatic adoption off: nothing is ever "present but unmarked".
    pub(crate) const NO_ADOPTION: Adoption = Adoption { unmarked: never_unmarked, adopt: adoption_disabled };

    pub(crate) fn never_unmarked(_spec: &'static ExternalModelSpec) -> bool {
        false
    }

    pub(crate) fn always_unmarked(_spec: &'static ExternalModelSpec) -> bool {
        true
    }

    /// The adoption body of [`NO_ADOPTION`]; unreachable while `never_unmarked` answers.
    fn adoption_disabled(
        _spec: &'static ExternalModelSpec,
        _cancel: &AtomicBool,
        _progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<PathBuf, ExternalModelError> {
        Err(ExternalModelError::InvalidSpec("adoption is disabled in this test".to_owned()))
    }

    /// Reports the spec as fully hashed (phase `Adopting`), then succeeds.
    pub(crate) fn ok_adopt(
        spec: &'static ExternalModelSpec,
        _cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<PathBuf, ExternalModelError> {
        let mut report = adopt_initial_progress(&[spec]);
        report.total_done = spec.total_bytes();
        progress(&report);
        Ok(PathBuf::from("unused"))
    }

    /// `THIRD_SPEC`'s file differs from its pin; every other spec adopts.
    pub(crate) fn third_mismatches_adopt(
        spec: &'static ExternalModelSpec,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<PathBuf, ExternalModelError> {
        if spec.id == THIRD_SPEC.id {
            return Err(ExternalModelError::HashMismatch { file: "c.bin", expected: "00", actual: "11".to_owned() });
        }
        ok_adopt(spec, cancel, progress)
    }

    /// Reports 7 bytes in phase `Adopting`, then runs until cancelled (10 s safety deadline).
    pub(crate) fn adopt_waits_for_cancel(
        spec: &'static ExternalModelSpec,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<PathBuf, ExternalModelError> {
        let mut report = adopt_initial_progress(&[spec]);
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
        FAKE_SPEC, GRANTED_ACCESS, OTHER_SPEC, PARTIAL_BYTES, THIRD_SPEC, adopt_waits_for_cancel, always_granted, always_unmarked,
        busy_failure, complete_download, damaged_verify, hash_failure, installed_probe, intact_verify, missing_probe, ok_adopt,
        ok_download, other_installed_probe, partial_probe, third_fails, third_mismatches_adopt, verify_waits_for_cancel,
        waits_for_cancel,
    };
    use super::*;
    use ms_sysprobe::ai_models::external::ExternalSource;
    use std::sync::atomic::AtomicUsize;
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
        assert_eq!(snapshot.missing_bytes, FAKE_SPEC.total_bytes() - PARTIAL_BYTES, "only what is not on disk");
        let shown = offered_download_label(&snapshot).expect("a Partial state offers a button");
        assert_eq!(shown, resume_button_label(FAKE_SPEC.total_bytes() - PARTIAL_BYTES));
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
        let result = run_bundle_download(jobs, always_granted, specs, &cancel, &mut |p| reports.push(*p), &mut |spec| installed.push(spec.id), &mut |_, _| {});
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
        let result = run_bundle_download(jobs, always_granted, specs, &cancel, &mut |_| {}, &mut |spec| installed.push(spec.id), &mut |_, _| {});
        assert!(matches!(result, Err(ExternalModelError::HashMismatch { .. })), "{result:?}");
        assert_eq!(installed, [FAKE_SPEC.id]);

        cancel.store(true, Ordering::Relaxed);
        installed.clear();
        let jobs = Jobs { status: missing_probe, download: complete_download, verify: intact_verify };
        let result = run_bundle_download(jobs, always_granted, BUNDLE, &cancel, &mut |_| {}, &mut |spec| installed.push(spec.id), &mut |_, _| {});
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
        // A partial spec costs only what it lacks.
        assert_eq!(aggregate_bundle(&partials), (partial(15), 165));
        let mixed = [(&FAKE_SPEC, partial(10)), missing(&THIRD_SPEC)];
        assert_eq!(aggregate_bundle(&mixed), (partial(10), 170));
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
            resume_button_label(171)
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

    /// A fresh once-per-process adoption record, so parallel tests do not share one.
    fn fresh_record() -> &'static Mutex<BTreeSet<&'static str>> {
        Box::leak(Box::new(Mutex::new(BTreeSet::new())))
    }

    fn adopting(adopt: AdoptJob, status: StatusProbe, record: &'static Mutex<BTreeSet<&'static str>>) -> OcrModelDownloadController {
        OcrModelDownloadController::with_parts(
            Jobs { status, download: ok_download, verify: intact_verify },
            Adoption { unmarked: always_unmarked, adopt },
            GRANTED_ACCESS,
            record,
        )
    }

    /// D9: a probed bundle whose specs are present but unmarked is adopted automatically and
    /// reads installed; a later controller of the same process never adopts those specs again.
    #[test]
    fn present_unmarked_specs_are_adopted_automatically_once_per_process() {
        let record = fresh_record();
        let mut controller = adopting(ok_adopt, missing_probe, record);
        controller.probe_unknown(BUNDLE);
        poll_until(&mut controller, |c| !c.is_active());
        let snapshot = controller.panel_snapshot(BUNDLE);
        assert_eq!(snapshot.state, OcrModelDownloadState::Installed, "{snapshot:?}");
        assert_eq!(snapshot.notice, None);

        // A tab rebuild: the probe reads Missing again (fake), but the record forbids a second
        // automatic adoption, so the state stays Missing with its Download button.
        let mut rebuilt = adopting(ok_adopt, missing_probe, record);
        rebuilt.probe_unknown(BUNDLE);
        poll_until(&mut rebuilt, |c| !c.is_active());
        assert_eq!(rebuilt.panel_snapshot(BUNDLE).state, OcrModelDownloadState::Missing);

        // Only a `Missing` answer is a candidate: an installed spec never starts a job.
        let mut installed = adopting(ok_adopt, installed_probe, fresh_record());
        installed.probe_unknown(BUNDLE);
        poll_until(&mut installed, |c| !c.is_active());
        assert_eq!(installed.panel_snapshot(BUNDLE).state, OcrModelDownloadState::Installed);
        // Disabled adoption (`with_jobs`): a missing bundle just reads Missing.
        let mut disabled = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: ok_download, verify: intact_verify });
        disabled.probe_unknown(BUNDLE);
        poll_until(&mut disabled, |c| !c.is_active());
        assert_eq!(disabled.panel_snapshot(BUNDLE).state, OcrModelDownloadState::Missing);
    }

    /// `running_job` names the kind of the one running job (what the FLUX.2 klein switch gate
    /// reads) and returns `None` once it ended.
    #[test]
    fn running_job_names_the_kind_of_the_one_running_job() {
        let jobs = Jobs { status: installed_probe, download: waits_for_cancel, verify: verify_waits_for_cancel };
        let mut controller = OcrModelDownloadController::with_jobs(jobs);
        assert_eq!(controller.running_job(), None);
        controller.start_download(BUNDLE);
        assert_eq!(controller.running_job(), Some(JobKind::Download));
        controller.cancel();
        poll_until(&mut controller, |c| !c.is_active());
        assert_eq!(controller.running_job(), None);
        controller.start_verify(BUNDLE);
        assert_eq!(controller.running_job(), Some(JobKind::Verify));
        controller.cancel();
        poll_until(&mut controller, |c| !c.is_active());
        assert_eq!(controller.running_job(), None);

        let mut adoption = adopting(adopt_waits_for_cancel, missing_probe, fresh_record());
        adoption.probe_unknown(BUNDLE);
        poll_until(&mut adoption, |c| c.running_job().is_some());
        assert_eq!(adoption.running_job(), Some(JobKind::Adopt));
        adoption.cancel();
        poll_until(&mut adoption, |c| !c.is_active());
        assert_eq!(adoption.running_job(), None);
    }

    /// Whether `unmarked_after_reprobe` answers "present but unmarked"; only the test below
    /// flips it.
    static UNMARKED_NOW: AtomicBool = AtomicBool::new(false);

    fn unmarked_after_reprobe(_spec: &'static ExternalModelSpec) -> bool {
        UNMARKED_NOW.load(Ordering::SeqCst)
    }

    /// The per-frame `probe_unknown` re-registers nothing once a bundle is decided (no
    /// allocation, no probe), yet a later status answer that makes a spec adoptable still
    /// starts the automatic adoption the next time the bundle is shown; an explicit
    /// `request_bundle_status` re-watches a decided bundle at once.
    #[test]
    fn a_decided_bundle_is_not_rewatched_per_frame_but_a_reprobe_can_still_adopt() {
        UNMARKED_NOW.store(false, Ordering::SeqCst);
        let mut controller = OcrModelDownloadController::with_parts(
            Jobs { status: missing_probe, download: ok_download, verify: intact_verify },
            Adoption { unmarked: unmarked_after_reprobe, adopt: ok_adopt },
            GRANTED_ACCESS,
            fresh_record(),
        );
        controller.probe_unknown(BUNDLE);
        poll_until(&mut controller, |c| !c.is_active() && c.adopt_watch.is_empty());
        assert_eq!(controller.adopt_decided.len(), 1, "decided: nothing adoptable");
        for _frame in 0..3 {
            controller.probe_unknown(BUNDLE);
            assert!(controller.adopt_watch.is_empty() && !controller.is_active(), "no re-registration, no probe");
            assert!(!controller.poll());
        }
        assert_eq!(controller.adopt_decided.len(), 1);

        // The explicit re-ask watches it again right away and decides it again.
        controller.request_bundle_status(BUNDLE);
        assert!(controller.adopt_decided.is_empty() && controller.adopt_watch.len() == 1);
        poll_until(&mut controller, |c| !c.is_active() && c.adopt_watch.is_empty());
        assert_eq!(controller.running_job(), None, "still nothing adoptable");

        // A re-probe of one spec now finds it adoptable: its answer reopens the bundle, and the
        // next frame's `probe_unknown` adopts it.
        UNMARKED_NOW.store(true, Ordering::SeqCst);
        controller.request_status(&FAKE_SPEC);
        poll_until(&mut controller, |c| !c.is_active());
        assert!(controller.adopt_decided.is_empty(), "a status answer reopens the bundles holding the spec");
        controller.probe_unknown(BUNDLE);
        poll_until(&mut controller, |c| !c.is_active() && c.adopt_watch.is_empty());
        assert_eq!(controller.state_for(FAKE_SPEC.id), OcrModelDownloadState::Installed, "adopted after the re-probe");
        assert_eq!(controller.state_for(THIRD_SPEC.id), OcrModelDownloadState::Missing, "its last answer was not adoptable");
    }

    /// A running adoption is the bundle's own `Verifying` job in phase `Adopting`, blocks other
    /// bundles as an `Adopt` job, and a cancel leaves its notice without a restart.
    #[test]
    fn a_running_adoption_shows_progress_blocks_others_and_cancels_without_restart() {
        let mut controller = adopting(adopt_waits_for_cancel, missing_probe, fresh_record());
        controller.probe_unknown(BUNDLE);
        poll_until(&mut controller, |c| {
            matches!(c.panel_snapshot(BUNDLE).state, OcrModelDownloadState::Verifying(p) if p.total_done == 7 && p.phase == ExternalDownloadPhase::Adopting)
        });
        let other = controller.panel_snapshot(OTHER).other_download.expect("the adoption blocks other bundles");
        assert_eq!(other.kind, JobKind::Adopt);
        controller.start_download(OTHER);
        assert!(!matches!(controller.state_for(OTHER_SPEC.id), OcrModelDownloadState::Downloading(_)));
        controller.cancel();
        poll_until(&mut controller, |c| !c.is_active());
        let snapshot = controller.panel_snapshot(BUNDLE);
        assert_eq!(snapshot.state, OcrModelDownloadState::Missing, "re-probed, not adopted again");
        assert_eq!(snapshot.notice, Some(DownloadNotice::AdoptCancelled));
        // The Download button stays the repair path and clears the notice.
        controller.start_download(BUNDLE);
        assert_eq!(controller.panel_snapshot(BUNDLE).notice, None);
        poll_until(&mut controller, |c| !c.is_active());
        assert_eq!(controller.panel_snapshot(BUNDLE).state, OcrModelDownloadState::Installed);
    }

    /// A mismatch leaves an `AdoptMismatch` notice naming the file over the re-probed state.
    #[test]
    fn an_adoption_mismatch_leaves_a_notice_over_the_reprobed_state() {
        let mut controller = adopting(third_mismatches_adopt, missing_probe, fresh_record());
        controller.probe_unknown(BUNDLE);
        poll_until(&mut controller, |c| !c.is_active());
        let snapshot = controller.panel_snapshot(BUNDLE);
        assert_eq!(snapshot.notice, Some(DownloadNotice::AdoptMismatch { file: "c.bin" }), "{snapshot:?}");
        assert_eq!(snapshot.state, OcrModelDownloadState::Missing);
    }

    /// The adoption thread continues past a refused spec (adopting the rest), reports the first
    /// failure and maps progress onto the whole list; a cancel stops it.
    #[test]
    fn bundle_adoption_continues_past_a_mismatch_and_stops_on_cancel() {
        let cancel = AtomicBool::new(false);
        let mut installed = Vec::new();
        let mut reports = Vec::new();
        let specs: &[&'static ExternalModelSpec] = &[&FAKE_SPEC, &THIRD_SPEC, &OTHER_SPEC];
        let result = run_bundle_adopt(third_mismatches_adopt, specs, &cancel, &mut |p| reports.push(*p), &mut |spec| installed.push(spec.id));
        assert!(matches!(result, Err(ExternalModelError::HashMismatch { file: "c.bin", .. })), "{result:?}");
        assert_eq!(installed, [FAKE_SPEC.id, OTHER_SPEC.id]);
        let last = reports.last().copied().expect("progress was reported");
        assert_eq!((last.file_index, last.file_count, last.total_bytes), (3, 5, 330));
        assert!(reports.iter().all(|p| p.phase == ExternalDownloadPhase::Adopting));

        cancel.store(true, Ordering::Relaxed);
        installed.clear();
        let result = run_bundle_adopt(ok_adopt, BUNDLE, &cancel, &mut |_| {}, &mut |spec| installed.push(spec.id));
        assert!(matches!(result, Err(ExternalModelError::Cancelled)), "{result:?}");
        assert!(installed.is_empty());
    }

    static GATED_FILES: [ms_sysprobe::ai_models::external::ExternalFile; 1] =
        [ms_sysprobe::ai_models::external::ExternalFile::direct("g.bin", 20, "00")];
    /// A spec from a gated repository.
    static GATED_SPEC: ExternalModelSpec = ExternalModelSpec {
        id: "gated_model",
        source: ExternalSource::HuggingFace { repo_id: "owner/gated", revision: "0", gated: true },
        dir: "Gated",
        layout: ms_sysprobe::ai_models::external::DirLayout::Shared,
        files: &GATED_FILES,
    };
    /// A second spec from the same gated repository (one check covers both).
    static GATED_TWIN_SPEC: ExternalModelSpec = ExternalModelSpec {
        id: "gated_twin_model",
        source: ExternalSource::HuggingFace { repo_id: "owner/gated", revision: "0", gated: true },
        dir: "Gated",
        layout: ms_sysprobe::ai_models::external::DirLayout::Shared,
        files: &GATED_FILES,
    };
    /// An open spec, then the two gated ones.
    const GATED_BUNDLE: &[&ExternalModelSpec] = &[&FAKE_SPEC, &GATED_SPEC, &GATED_TWIN_SPEC];

    /// Shows `specs` frame by frame (`probe_unknown` + `poll`, as a panel does) until nothing
    /// is pending even after one more `probe_unknown` (which may start an access check).
    fn show_until_settled(controller: &mut OcrModelDownloadController, specs: &[&'static ExternalModelSpec]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "controller did not settle: {controller:?}");
            controller.probe_unknown(specs);
            controller.poll();
            if !controller.is_active() {
                controller.probe_unknown(specs);
                if !controller.is_active() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn line(access: Option<HfAccess>) -> Vec<RepoAccessLine> {
        vec![RepoAccessLine { repo_id: "owner/gated", access }]
    }

    fn fixed_token() -> String {
        "token-a".to_owned()
    }

    static ONCE_CHECKS: AtomicUsize = AtomicUsize::new(0);

    fn once_probe(_repo_id: &str, _cancel: &AtomicBool) -> HfAccess {
        ONCE_CHECKS.fetch_add(1, Ordering::SeqCst);
        HfAccess::NoToken
    }

    /// A shown bundle's gated repository is checked once per token, however many frames show
    /// it and however many of its specs come from that repository; the open spec has no line.
    #[test]
    fn a_gated_repository_is_checked_once_per_token() {
        let mut controller = OcrModelDownloadController::with_parts(
            Jobs { status: missing_probe, download: ok_download, verify: intact_verify },
            super::fakes::NO_ADOPTION,
            AccessCheck { probe: once_probe, token: fixed_token },
            fresh_record(),
        );
        show_until_settled(&mut controller, GATED_BUNDLE);
        for _frame in 0..5 {
            controller.probe_unknown(GATED_BUNDLE);
            controller.poll();
        }
        assert!(!controller.is_active());
        assert_eq!(ONCE_CHECKS.load(Ordering::SeqCst), 1);
        assert_eq!(controller.panel_snapshot(GATED_BUNDLE).access, line(Some(HfAccess::NoToken)));
        // Another bundle holding the same repository reuses the answer.
        show_until_settled(&mut controller, &[&GATED_TWIN_SPEC]);
        assert_eq!(ONCE_CHECKS.load(Ordering::SeqCst), 1);
        assert_eq!(controller.panel_snapshot(&[&GATED_TWIN_SPEC]).access, line(Some(HfAccess::NoToken)));
    }

    static ROTATING_TOKEN: AtomicUsize = AtomicUsize::new(0);
    static ROTATING_CHECKS: AtomicUsize = AtomicUsize::new(0);

    fn rotating_token() -> String {
        format!("token-{}", ROTATING_TOKEN.load(Ordering::SeqCst))
    }

    /// Answers by token: the first token is rejected, any later one is granted.
    fn rotating_probe(_repo_id: &str, _cancel: &AtomicBool) -> HfAccess {
        ROTATING_CHECKS.fetch_add(1, Ordering::SeqCst);
        if ROTATING_TOKEN.load(Ordering::SeqCst) == 0 { HfAccess::TokenRejected } else { HfAccess::Granted }
    }

    /// A token change drops the answers and checks the shown repository again.
    #[test]
    fn a_token_change_checks_the_repository_again() {
        ROTATING_TOKEN.store(0, Ordering::SeqCst);
        let mut controller = OcrModelDownloadController::with_parts(
            Jobs { status: missing_probe, download: ok_download, verify: intact_verify },
            super::fakes::NO_ADOPTION,
            AccessCheck { probe: rotating_probe, token: rotating_token },
            fresh_record(),
        );
        show_until_settled(&mut controller, &[&GATED_SPEC]);
        assert_eq!(ROTATING_CHECKS.load(Ordering::SeqCst), 1);
        assert_eq!(controller.panel_snapshot(&[&GATED_SPEC]).access, line(Some(HfAccess::TokenRejected)));

        ROTATING_TOKEN.store(1, Ordering::SeqCst);
        assert!(controller.poll(), "a token change is a change");
        assert_eq!(controller.panel_snapshot(&[&GATED_SPEC]).access, line(None), "old answer dropped");
        show_until_settled(&mut controller, &[&GATED_SPEC]);
        assert_eq!(ROTATING_CHECKS.load(Ordering::SeqCst), 2);
        assert_eq!(controller.panel_snapshot(&[&GATED_SPEC]).access, line(Some(HfAccess::Granted)));
    }

    static DENIED_CHECKS: AtomicUsize = AtomicUsize::new(0);
    static DENIED_DOWNLOADS: AtomicUsize = AtomicUsize::new(0);

    fn denied_probe(_repo_id: &str, _cancel: &AtomicBool) -> HfAccess {
        DENIED_CHECKS.fetch_add(1, Ordering::SeqCst);
        HfAccess::TermsNotAccepted
    }

    fn counted_download(
        spec: &'static ExternalModelSpec,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ExternalDownloadProgress),
    ) -> Result<PathBuf, ExternalModelError> {
        DENIED_DOWNLOADS.fetch_add(1, Ordering::SeqCst);
        complete_download(spec, cancel, progress)
    }

    /// `start_download` re-checks access before anything else: a refusal ends the job with an
    /// `Access` notice and no download call, even for the open spec listed first.
    #[test]
    fn a_download_rechecks_access_and_a_refusal_downloads_nothing() {
        let mut controller = OcrModelDownloadController::with_parts(
            Jobs { status: missing_probe, download: counted_download, verify: intact_verify },
            super::fakes::NO_ADOPTION,
            AccessCheck { probe: denied_probe, token: fixed_token },
            fresh_record(),
        );
        controller.start_download(GATED_BUNDLE);
        poll_until(&mut controller, |c| !c.is_active());
        let snapshot = controller.panel_snapshot(GATED_BUNDLE);
        assert_eq!(snapshot.notice, Some(DownloadNotice::Access { repo_id: "owner/gated", access: HfAccess::TermsNotAccepted }));
        assert_eq!(DENIED_DOWNLOADS.load(Ordering::SeqCst), 0, "nothing was downloaded");
        assert_eq!(DENIED_CHECKS.load(Ordering::SeqCst), 1, "one check for the two specs of one repository");
        // The job's answer is the repository's current answer: the shown bundle needs no new check.
        show_until_settled(&mut controller, GATED_BUNDLE);
        assert_eq!(DENIED_CHECKS.load(Ordering::SeqCst), 1);
        assert_eq!(controller.panel_snapshot(GATED_BUNDLE).access, line(Some(HfAccess::TermsNotAccepted)));

        // The thread body directly: the refusal comes before the open spec's download.
        let mut installed = Vec::new();
        let mut seen = Vec::new();
        let jobs = Jobs { status: missing_probe, download: complete_download, verify: intact_verify };
        let result = run_bundle_download(
            jobs,
            |_, _| HfAccess::NoToken,
            GATED_BUNDLE,
            &AtomicBool::new(false),
            &mut |_| {},
            &mut |spec| installed.push(spec.id),
            &mut |repo, access| seen.push((repo, access.clone())),
        );
        assert!(matches!(result, Err(ExternalModelError::AccessDenied { repo_id: "owner/gated", access: HfAccess::NoToken })), "{result:?}");
        assert!(installed.is_empty());
        assert_eq!(seen, [("owner/gated", HfAccess::NoToken)]);
    }

    /// The thread body with an unanswered check: not a refusal, every pending spec downloads.
    #[test]
    fn an_unreachable_check_does_not_stop_a_bundle_download() {
        let mut installed = Vec::new();
        let mut seen = Vec::new();
        let jobs = Jobs { status: missing_probe, download: complete_download, verify: intact_verify };
        let result = run_bundle_download(
            jobs,
            |_, _| HfAccess::Unreachable { detail: "offline".to_owned() },
            GATED_BUNDLE,
            &AtomicBool::new(false),
            &mut |_| {},
            &mut |spec| installed.push(spec.id),
            &mut |repo, access| seen.push((repo, access.clone())),
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(installed, ["fake_model", "gated_model", "gated_twin_model"]);
        assert_eq!(seen.len(), 1, "one check per repository");
    }

    static TOKEN_SWAP: AtomicUsize = AtomicUsize::new(0);

    fn token_swap_token() -> String {
        format!("swap-{}", TOKEN_SWAP.load(Ordering::SeqCst))
    }

    /// No token at first, granted once a token is stored.
    fn token_swap_probe(_repo_id: &str, _cancel: &AtomicBool) -> HfAccess {
        if TOKEN_SWAP.load(Ordering::SeqCst) == 0 { HfAccess::NoToken } else { HfAccess::Granted }
    }

    /// A refused download's notice belongs to the token it ran under: storing a new token
    /// drops it, and the fresh answer (granted) is what the panel sees — no stale refusal.
    #[test]
    fn a_token_change_drops_an_access_refusal_notice() {
        TOKEN_SWAP.store(0, Ordering::SeqCst);
        let mut controller = OcrModelDownloadController::with_parts(
            Jobs { status: missing_probe, download: ok_download, verify: intact_verify },
            super::fakes::NO_ADOPTION,
            AccessCheck { probe: token_swap_probe, token: token_swap_token },
            fresh_record(),
        );
        controller.start_download(&[&GATED_SPEC]);
        poll_until(&mut controller, |c| !c.is_active());
        assert_eq!(controller.panel_snapshot(&[&GATED_SPEC]).notice, Some(DownloadNotice::Access { repo_id: "owner/gated", access: HfAccess::NoToken }));

        TOKEN_SWAP.store(1, Ordering::SeqCst);
        assert!(controller.poll(), "a token change is a change");
        assert_eq!(controller.panel_snapshot(&[&GATED_SPEC]).notice, None, "the old token's refusal is dropped");
        show_until_settled(&mut controller, &[&GATED_SPEC]);
        let snapshot = controller.panel_snapshot(&[&GATED_SPEC]);
        assert_eq!(snapshot.notice, None);
        assert_eq!(snapshot.access, line(Some(HfAccess::Granted)));
    }

    static UNREACHABLE_CHECKS: AtomicUsize = AtomicUsize::new(0);

    fn unreachable_probe(_repo_id: &str, _cancel: &AtomicBool) -> HfAccess {
        UNREACHABLE_CHECKS.fetch_add(1, Ordering::SeqCst);
        HfAccess::Unreachable { detail: "offline".to_owned() }
    }

    /// An `Unreachable` answer is shown, not kept: frames within the re-check delay reuse it,
    /// an older one is asked again, and any job's end asks again at once.
    #[test]
    fn an_unreachable_answer_is_asked_again_after_the_delay_and_after_a_job() {
        let mut controller = OcrModelDownloadController::with_parts(
            Jobs { status: missing_probe, download: ok_download, verify: intact_verify },
            super::fakes::NO_ADOPTION,
            AccessCheck { probe: unreachable_probe, token: fixed_token },
            fresh_record(),
        );
        show_until_settled(&mut controller, &[&GATED_SPEC]);
        for _frame in 0..5 {
            controller.probe_unknown(&[&GATED_SPEC]);
            controller.poll();
        }
        assert_eq!(UNREACHABLE_CHECKS.load(Ordering::SeqCst), 1, "no per-frame storm");
        assert!(matches!(controller.panel_snapshot(&[&GATED_SPEC]).access.as_slice(), [RepoAccessLine { access: Some(HfAccess::Unreachable { .. }), .. }]));

        // Past the delay: one new check.
        controller.unreachable_recheck = Duration::ZERO;
        controller.probe_unknown(&[&GATED_SPEC]);
        controller.unreachable_recheck = UNREACHABLE_RECHECK;
        poll_until(&mut controller, |c| !c.is_active());
        assert_eq!(UNREACHABLE_CHECKS.load(Ordering::SeqCst), 2);

        // A finished job forgets the answer: the shown repository is asked again.
        controller.start_verify(&[&GATED_SPEC]);
        poll_until(&mut controller, |c| !c.is_active());
        show_until_settled(&mut controller, &[&GATED_SPEC]);
        assert_eq!(UNREACHABLE_CHECKS.load(Ordering::SeqCst), 3);
    }

    static INSTALLED_CHECKS: AtomicUsize = AtomicUsize::new(0);

    fn installed_counting_probe(_repo_id: &str, _cancel: &AtomicBool) -> HfAccess {
        INSTALLED_CHECKS.fetch_add(1, Ordering::SeqCst);
        HfAccess::NoToken
    }

    /// An installed gated spec needs nothing from its repository: no check, no line; nor does
    /// a granted download of an installed bundle ask.
    #[test]
    fn an_installed_gated_spec_is_not_checked() {
        let mut controller = OcrModelDownloadController::with_parts(
            Jobs { status: installed_probe, download: ok_download, verify: intact_verify },
            super::fakes::NO_ADOPTION,
            AccessCheck { probe: installed_counting_probe, token: fixed_token },
            fresh_record(),
        );
        show_until_settled(&mut controller, GATED_BUNDLE);
        assert_eq!(INSTALLED_CHECKS.load(Ordering::SeqCst), 0);
        assert!(controller.panel_snapshot(GATED_BUNDLE).access.is_empty());
        controller.start_download(GATED_BUNDLE);
        poll_until(&mut controller, |c| !c.is_active());
        assert_eq!(INSTALLED_CHECKS.load(Ordering::SeqCst), 0, "nothing pending, nothing checked");
        assert_eq!(controller.panel_snapshot(GATED_BUNDLE).notice, None);
    }

    /// L1: no English diagnostic detail reaches the user-facing text.
    #[test]
    fn every_error_variant_maps_to_its_own_key_without_english_detail() {
        const DETAIL: &str = "english-diagnostic-detail";
        let cases: [(ExternalModelError, &str); 11] = [
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
            (
                ExternalModelError::AccessDenied { repo_id: "owner/gated", access: HfAccess::TermsNotAccepted },
                "translation.ocr_model.access_terms_status",
            ),
            (
                ExternalModelError::AccessDenied { repo_id: "owner/gated", access: HfAccess::Unreachable { detail: DETAIL.to_owned() } },
                "translation.ocr_model.access_unreachable_status",
            ),
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
