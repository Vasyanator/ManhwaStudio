/*
FILE OVERVIEW: crates/ms-tab-translation/src/tab/browser_extension.rs
The Translation tab's side of the ManhwaStudio browser extension (the "through my browser"
machine-translation transport): what the MT panel shows about the extension and the background
jobs behind its buttons.

Main items:
- `ExtensionUiState` (+ `from_status`): the ONE state the panel's extension block shows, decided
  GUI-free from the bridge status, the installed folder's stamp and the embedded bundle.
- `InstalledProbe`: the installed folder's stamp, as the probe worker read it.
- `ExtensionOps` / `AppExtensionOps`: the seam to the bridge, the extension folder, the OS shell
  and `user_config` (production: `ms_browser_bridge::app`, `ms_browser_ext`, `ms_platform::shell_open`,
  `ms_config::mt_transport`); tests inject a fake.
- `BrowserExtensionController`: owned by `TranslationTabState`. `view` (per frame while the panel
  shows the browser transport) starts the bridge on a worker on first use and builds
  `ExtensionPanelView`; `start_install` runs the install job on a worker; `reveal_folder` is a
  short worker; `save_transport` queues the value for the controller's ONE transport saver;
  `poll` (every frame) collects finished jobs and returns the failures the tab shows as toasts.
- `TransportSaves`: the transport saver's queue. Saves are serialized (one worker at a time) and
  latest-wins per service, so the persisted value is always the last toggle.
- `run_install_job`: the install job body (install -> start -> reveal folder -> reload outdated
  peers -> re-probe), a plain function so its sequence is unit-tested.

Notes:
GUI thread only for the controller; every blocking call (file I/O, the bridge start, spawning
the file manager, the config write) runs on a detached worker started through `ms_thread`. The
bridge status read in `view` is a brief lock + clone (`BridgeService::status`, GUI-safe).
Native-only in practice, like `machine_translators::browser` and the bridge server: the inactive
web build (`PROJECT_RULES.md`) is not kept compiling here.
*/

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};

use eframe::egui;
use ms_browser_bridge::{BridgeError, BridgeStatus, ListenState};
use ms_browser_ext::{Bundle, InstallError, InstallReport, NotIncluded};
use ms_config::mt_transport::MtTransport;
use ms_log::runtime_log;
use ms_thread as thread;
use web_time::Duration;

use crate::machine_translation::MtService;
use crate::machine_translators::browser::{TRANSLATE_MODULE, TRANSLATE_MODULE_VERSION};

/// How often the panel re-reads the bridge status while the extension is not connected (the
/// extension connects on its own, up to a minute after the browser starts).
const NOT_CONNECTED_REPAINT: Duration = Duration::from_secs(1);
/// How often a frame is requested while a job of this controller runs, so its result is
/// collected promptly.
const JOB_REPAINT: Duration = Duration::from_millis(200);

/// The installed folder's stamp as the probe read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstalledProbe {
    /// No stamp: nothing was installed there (or no folder at all).
    Missing,
    /// A complete install of this bundle `build_id`.
    Build(String),
    /// A stamp exists but is not valid: the next install replaces it.
    Corrupt,
    /// The stamp could not be read (I/O error, logged by the probe).
    Unreadable,
}

impl InstalledProbe {
    /// Maps `ms_browser_ext::installed_build_id`'s answer; an unreadable stamp is logged here.
    fn from_result(dir: &Path, result: Result<Option<String>, InstallError>) -> Self {
        match result {
            Ok(None) => Self::Missing,
            Ok(Some(build_id)) => Self::Build(build_id),
            Err(InstallError::StampCorrupt { .. }) => Self::Corrupt,
            Err(err) => {
                runtime_log::log_warn(format!("[MT] browser extension: the install stamp in {} could not be read: {err}", dir.display()));
                Self::Unreadable
            }
        }
    }
}

/// What the MT panel's extension block shows: exactly one of these per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExtensionUiState {
    /// The bridge has not answered yet (start or folder probe still running).
    Checking,
    /// This build carries no extension bundle; `reason` is the build's short English reason
    /// (technical detail, shown under the localized sentence).
    NotInBuild { reason: &'static str },
    /// The loopback server could not start; `error` is the localized `BridgeError` text.
    ServerFailed { error: String },
    /// The extension folder holds no install yet.
    NotInstalled,
    /// The folder holds this build's extension, but no browser is connected (connecting may
    /// take up to a minute after the browser starts).
    InstalledNotConnected,
    /// The folder or the connected extension is another version: install / update and reload.
    Outdated,
    /// An extension that serves machine translation is connected; `browser` as it reports
    /// itself (`name version`, may be empty).
    Connected { browser: String },
}

impl ExtensionUiState {
    /// Decides the state. `status` is the bridge snapshot (the controller turns its own start
    /// failure into `ListenState::Failed`), `installed` the folder probe (`None` = not read
    /// yet), `bundle` the embedded bundle (`ms_browser_ext::bundle()`).
    ///
    /// Precedence: a server failure, then a connected extension (a working extension is shown
    /// as connected even when the folder is stale: the reload watcher updates it), then the
    /// build's bundle, then the folder.
    pub(crate) fn from_status(status: &BridgeStatus, installed: Option<&InstalledProbe>, bundle: Result<&Bundle, NotIncluded>) -> Self {
        if let ListenState::Failed { error } = &status.listen {
            return Self::ServerFailed { error: error.to_string() };
        }
        if let Some(peer) = status.serving_peer(TRANSLATE_MODULE, TRANSLATE_MODULE_VERSION) {
            let browser = format!("{} {}", peer.browser.name, peer.browser.version).trim().to_owned();
            return Self::Connected { browser };
        }
        // Connected, but none serves translation: another protocol, or a build without the
        // module version this app speaks.
        if !status.peers.is_empty() {
            return Self::Outdated;
        }
        let bundle = match bundle {
            Ok(bundle) => bundle,
            Err(not_included) => return Self::NotInBuild { reason: not_included.reason },
        };
        match installed {
            None => Self::Checking,
            // An unreadable stamp was logged by the probe; installing again is the remedy the
            // user can take, so it reads like a missing install.
            Some(InstalledProbe::Missing | InstalledProbe::Unreadable) => Self::NotInstalled,
            Some(InstalledProbe::Corrupt) => Self::Outdated,
            Some(InstalledProbe::Build(build_id)) if *build_id == bundle.build_id => Self::InstalledNotConnected,
            Some(InstalledProbe::Build(_)) => Self::Outdated,
        }
    }
}

/// How the last install job ended, shown under the install button until the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstallNotice {
    /// The folder now holds this build's extension; `reloaded` connected extensions were asked
    /// to reload themselves.
    Installed { reloaded: usize },
    /// Another install (the bridge's automatic re-unpack) was running; nothing was done.
    Busy,
    /// The install failed; `error` is the technical reason (the previous install stays usable).
    Failed { error: String },
}

/// Everything the MT panel's extension block draws, built by
/// [`BrowserExtensionController::view`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtensionPanelView {
    /// The state line.
    pub(crate) state: ExtensionUiState,
    /// The build embeds a bundle, so the install button can work.
    pub(crate) can_install: bool,
    /// An install job runs (button disabled, spinner shown).
    pub(crate) install_running: bool,
    /// The last install job's outcome.
    pub(crate) last_install: Option<InstallNotice>,
    /// The extension folder the user loads in the browser; `None` until the first worker
    /// resolved it.
    pub(crate) folder: Option<PathBuf>,
}

/// The seam between the controller and the outside world. Production: [`AppExtensionOps`].
/// Every method except `bundle` and `status` blocks or does I/O: workers only.
pub(crate) trait ExtensionOps: Send + Sync + 'static {
    /// The embedded bundle.
    fn bundle(&self) -> Result<&'static Bundle, NotIncluded>;
    /// The bridge snapshot (GUI-safe: brief lock + clone).
    fn status(&self) -> BridgeStatus;
    /// The extension folder (`ms_browser_ext::install_dir` under the app's data dir).
    fn install_dir(&self) -> PathBuf;
    /// Starts the bridge (`ms_browser_bridge::app::ensure_started_for_app`).
    fn ensure_started(&self) -> Result<(), BridgeError>;
    /// Reads the folder's stamp.
    fn probe_installed(&self, dir: &Path) -> InstalledProbe;
    /// Unpacks `bundle` into `dir`; `Ok(None)` when another install runs.
    fn install(&self, bundle: &'static Bundle, dir: &Path) -> Result<Option<InstallReport>, InstallError>;
    /// Opens `dir` in the file manager; the error is the localized message.
    fn reveal_dir(&self, dir: &Path) -> Result<(), String>;
    /// Sends `reload` to the peers that need it; returns how many were addressed.
    fn reload_outdated(&self) -> usize;
    /// Persists `service`'s transport in `user_config`.
    fn save_transport(&self, service: MtService, transport: MtTransport) -> Result<(), String>;
}

/// The production [`ExtensionOps`].
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct AppExtensionOps;

impl ExtensionOps for AppExtensionOps {
    fn bundle(&self) -> Result<&'static Bundle, NotIncluded> {
        ms_browser_ext::bundle()
    }

    fn status(&self) -> BridgeStatus {
        let status = ms_browser_bridge::BridgeService::shared().status();
        // The panel's per-frame read also dates the listener for the MT readiness wait.
        crate::machine_translators::browser::note_listen_state(&status);
        status
    }

    fn install_dir(&self) -> PathBuf {
        ms_browser_ext::install_dir(&ms_config::data_dir())
    }

    fn ensure_started(&self) -> Result<(), BridgeError> {
        crate::machine_translators::browser::start_app_bridge()
    }

    fn probe_installed(&self, dir: &Path) -> InstalledProbe {
        InstalledProbe::from_result(dir, ms_browser_ext::installed_build_id(dir))
    }

    fn install(&self, bundle: &'static Bundle, dir: &Path) -> Result<Option<InstallReport>, InstallError> {
        ms_browser_bridge::app::install_guarded(bundle, dir)
    }

    fn reveal_dir(&self, dir: &Path) -> Result<(), String> {
        ms_platform::shell_open::reveal_dir(dir).map_err(|err| {
            runtime_log::log_warn(format!("[MT] browser extension: opening the folder {} failed: {err:?}", dir.display()));
            err.user_message()
        })
    }

    fn reload_outdated(&self) -> usize {
        ms_browser_bridge::BridgeService::shared().request_reload(ms_browser_bridge::ReloadTarget::NeedsUpdate)
    }

    fn save_transport(&self, service: MtService, transport: MtTransport) -> Result<(), String> {
        ms_config::mt_transport::save_mt_transport(&ms_config::user_config_path(), service.key(), transport)
    }
}

/// What the start worker found.
#[derive(Debug)]
struct StartOutcome {
    started: Result<(), BridgeError>,
    dir: PathBuf,
    probe: InstalledProbe,
}

/// What the install job did (also carries the start result and the fresh probe it took).
#[derive(Debug)]
pub(crate) struct InstallJobOutcome {
    notice: InstallNotice,
    /// `None` when the job stopped before starting the bridge.
    started: Option<Result<(), BridgeError>>,
    dir: PathBuf,
    probe: InstalledProbe,
    /// The localized "could not open the folder" message, when that step failed.
    reveal_error: Option<String>,
}

/// The start worker body: start the bridge (this may re-unpack a stale folder), then read the
/// folder's stamp.
fn run_start_job(ops: &dyn ExtensionOps) -> StartOutcome {
    let started = ops.ensure_started();
    if let Err(err) = &started {
        runtime_log::log_error(format!("[MT] browser extension: the bridge did not start: {err:?}"));
    }
    let dir = ops.install_dir();
    let probe = ops.probe_installed(&dir);
    StartOutcome { started, dir, probe }
}

/// The install job body: unpack the bundle (serialized with the bridge's own re-unpack), start
/// the bridge, open the folder for the "Load unpacked" step, ask connected stale extensions to
/// reload, then re-read the stamp. A failed or skipped unpack stops the sequence.
pub(crate) fn run_install_job(ops: &dyn ExtensionOps) -> InstallJobOutcome {
    let dir = ops.install_dir();
    let bundle = match ops.bundle() {
        Ok(bundle) => bundle,
        Err(not_included) => {
            // The button is disabled without a bundle; reaching here means a stale click.
            let probe = ops.probe_installed(&dir);
            return InstallJobOutcome { notice: InstallNotice::Failed { error: not_included.to_string() }, started: None, dir, probe, reveal_error: None };
        }
    };
    let notice = match ops.install(bundle, &dir) {
        Ok(Some(report)) => {
            runtime_log::log_info(format!(
                "[MT] browser extension installed into {}: build {} (previous {:?}; {} written, {} unchanged, {} removed)",
                dir.display(),
                report.build_id,
                report.previous_build_id,
                report.written,
                report.unchanged,
                report.removed
            ));
            None
        }
        Ok(None) => {
            runtime_log::log_info("[MT] browser extension install skipped: another install is running");
            Some(InstallNotice::Busy)
        }
        Err(err) => {
            runtime_log::log_error(format!("[MT] browser extension install into {} failed: {err:?}", dir.display()));
            Some(InstallNotice::Failed { error: err.to_string() })
        }
    };
    if let Some(notice) = notice {
        let probe = ops.probe_installed(&dir);
        return InstallJobOutcome { notice, started: None, dir, probe, reveal_error: None };
    }
    let started = ops.ensure_started();
    if let Err(err) = &started {
        runtime_log::log_error(format!("[MT] browser extension: the bridge did not start after the install: {err:?}"));
    }
    let reveal_error = ops.reveal_dir(&dir).err();
    // The bridge's watcher would reload them too; asking right away makes the update visible
    // without waiting for its poll.
    let reloaded = if ops.status().peers.iter().any(ms_browser_bridge::PeerInfo::needs_reload) { ops.reload_outdated() } else { 0 };
    let probe = ops.probe_installed(&dir);
    InstallJobOutcome { notice: InstallNotice::Installed { reloaded }, started: Some(started), dir, probe, reveal_error }
}

/// One background job's receiver, or nothing running.
#[derive(Debug)]
enum Job<T> {
    Idle,
    Running(Receiver<T>),
}

impl<T> Job<T> {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running(_))
    }

    /// The job's result once it is done. A worker that ended without sending (it panicked) is
    /// logged and the slot freed, so the action can be retried.
    fn take_finished(&mut self, what: &str) -> Option<T> {
        let Self::Running(rx) = self else { return None };
        match rx.try_recv() {
            Ok(value) => {
                *self = Self::Idle;
                Some(value)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                runtime_log::log_error(format!("[MT] browser extension: the {what} worker ended without a result"));
                *self = Self::Idle;
                None
            }
        }
    }
}

/// The transport saver's queue: the value still to persist per service (`MtService` order of
/// [`TransportSaves::slot`]) and whether a saver worker is draining it.
#[derive(Debug, Default)]
pub(crate) struct TransportSaves {
    pending: [Option<MtTransport>; 3],
    running: bool,
}

impl TransportSaves {
    /// Index of `service` in `pending`.
    const fn slot(service: MtService) -> usize {
        match service {
            MtService::Google => 0,
            MtService::Yandex => 1,
            MtService::Deepl => 2,
        }
    }

    /// Records `transport` as `service`'s value to persist (replacing an unsaved older one).
    /// Returns true when the caller must start a saver worker (none is running).
    fn queue(&mut self, service: MtService, transport: MtTransport) -> bool {
        self.pending[Self::slot(service)] = Some(transport);
        !std::mem::replace(&mut self.running, true)
    }

    /// The next value to persist, taken out of the queue; `None` ends the worker (and marks it
    /// stopped in the same lock, so a later `queue` starts a new one).
    fn next(&mut self) -> Option<(MtService, MtTransport)> {
        let next = MtService::all().iter().find_map(|&service| self.pending[Self::slot(service)].take().map(|transport| (service, transport)));
        if next.is_none() {
            self.running = false;
        }
        next
    }
}

/// The transport saver worker: persists queued values one at a time until the queue is empty.
/// A value toggled while one is written is written after it, so the last toggle always lands
/// last.
fn drain_transport_saves(ops: &dyn ExtensionOps, saves: &Mutex<TransportSaves>, notices: &Sender<String>) {
    loop {
        // The lock is held only to take the next value, never across the write.
        let next = saves.lock().unwrap_or_else(PoisonError::into_inner).next();
        let Some((service, transport)) = next else { return };
        if let Err(err) = ops.save_transport(service, transport) {
            runtime_log::log_error(format!("[MT] saving the {} transport ({transport:?}) to user_config failed: {err}", service.key()));
            send_notice(notices, tf!("translation.mt_panel.transport_save_error", service = service.title(), error = err));
        }
    }
}

/// The Translation tab's browser-extension state: bridge start, folder probe, install job and
/// the short workers of the MT panel's extension block. See the file header.
pub(crate) struct BrowserExtensionController {
    ops: Arc<dyn ExtensionOps>,
    /// The bridge start was requested once (retries come from the install job).
    start_requested: bool,
    start: Job<StartOutcome>,
    install: Job<InstallJobOutcome>,
    /// The last start failure seen (shown as a server failure until a start succeeds).
    start_error: Option<BridgeError>,
    probe: Option<InstalledProbe>,
    folder: Option<PathBuf>,
    last_install: Option<InstallNotice>,
    /// Failures of the short workers (config save, open folder), drained by `poll`.
    notices_tx: Sender<String>,
    notices_rx: Receiver<String>,
    /// Short workers still running (for the repaint request only).
    short_jobs: Vec<Receiver<()>>,
    /// The transport saver's queue, shared with its worker.
    transport_saves: Arc<Mutex<TransportSaves>>,
}

impl std::fmt::Debug for BrowserExtensionController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserExtensionController")
            .field("start_requested", &self.start_requested)
            .field("start_running", &self.start.is_running())
            .field("install_running", &self.install.is_running())
            .field("start_error", &self.start_error)
            .field("probe", &self.probe)
            .field("folder", &self.folder)
            .field("last_install", &self.last_install)
            .finish_non_exhaustive()
    }
}

impl Default for BrowserExtensionController {
    fn default() -> Self {
        Self::with_ops(Arc::new(AppExtensionOps))
    }
}

impl BrowserExtensionController {
    /// A controller over `ops` (tests inject a fake). Nothing starts until the first `view`.
    pub(crate) fn with_ops(ops: Arc<dyn ExtensionOps>) -> Self {
        let (notices_tx, notices_rx) = mpsc::channel();
        Self {
            ops,
            start_requested: false,
            start: Job::Idle,
            install: Job::Idle,
            start_error: None,
            probe: None,
            folder: None,
            last_install: None,
            notices_tx,
            notices_rx,
            short_jobs: Vec::new(),
            transport_saves: Arc::new(Mutex::new(TransportSaves::default())),
        }
    }

    /// True while any job of this controller runs.
    fn busy(&self) -> bool {
        self.start.is_running() || self.install.is_running() || !self.short_jobs.is_empty()
    }

    /// Collects finished jobs. Returns the localized failures of the short workers (config
    /// save, open folder) for the tab's toasts; install outcomes stay in the panel view. Cheap;
    /// call every frame.
    pub(crate) fn poll(&mut self) -> Vec<String> {
        let mut notices = Vec::new();
        if let Some(outcome) = self.start.take_finished("bridge start") {
            self.apply_start(outcome.started);
            self.folder = Some(outcome.dir);
            self.probe = Some(outcome.probe);
        }
        if let Some(outcome) = self.install.take_finished("extension install") {
            if let Some(started) = outcome.started {
                self.apply_start(started);
            }
            self.folder = Some(outcome.dir);
            self.probe = Some(outcome.probe);
            // Reported as a toast, like a failure of the panel's own "open folder" button.
            notices.extend(outcome.reveal_error);
            self.last_install = Some(outcome.notice);
        }
        self.short_jobs.retain(|rx| matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        notices.extend(self.notices_rx.try_iter());
        notices
    }

    fn apply_start(&mut self, started: Result<(), BridgeError>) {
        self.start_error = started.err();
    }

    /// Starts the bridge and the folder probe on a worker the first time it is called.
    pub(crate) fn ensure_started(&mut self) {
        if self.start_requested {
            return;
        }
        self.start_requested = true;
        let ops = Arc::clone(&self.ops);
        match spawn_job("ms-bridge-start", move || run_start_job(ops.as_ref())) {
            Ok(rx) => self.start = Job::Running(rx),
            Err(err) => {
                runtime_log::log_error(format!("[MT] browser extension: could not spawn the bridge start worker: {err}"));
                self.start_error = Some(BridgeError::System { detail: err.to_string() });
            }
        }
    }

    /// The panel view for this frame: starts the bridge on first use and requests a repaint
    /// every second while not connected (the extension connects on its own) and while a job
    /// runs. GUI-safe.
    pub(crate) fn view(&mut self, ctx: &egui::Context) -> ExtensionPanelView {
        self.ensure_started();
        let mut status = self.ops.status();
        if let Some(error) = &self.start_error {
            status.listen = ListenState::Failed { error: error.clone() };
        }
        let bundle = self.ops.bundle();
        let state = ExtensionUiState::from_status(&status, self.probe.as_ref(), bundle);
        if self.busy() {
            ctx.request_repaint_after(JOB_REPAINT);
        } else if !matches!(state, ExtensionUiState::Connected { .. }) {
            ctx.request_repaint_after(NOT_CONNECTED_REPAINT);
        }
        ExtensionPanelView { state, can_install: bundle.is_ok(), install_running: self.install.is_running(), last_install: self.last_install.clone(), folder: self.folder.clone() }
    }

    /// Runs the install job on a worker (ignored while one runs).
    pub(crate) fn start_install(&mut self) {
        if self.install.is_running() {
            return;
        }
        // The install job starts the bridge itself; a pending first start is harmless.
        self.start_requested = true;
        let ops = Arc::clone(&self.ops);
        match spawn_job("ms-ext-install", move || run_install_job(ops.as_ref())) {
            Ok(rx) => {
                self.install = Job::Running(rx);
                self.last_install = None;
            }
            Err(err) => {
                runtime_log::log_error(format!("[MT] browser extension: could not spawn the install worker: {err}"));
                self.last_install = Some(InstallNotice::Failed { error: err.to_string() });
            }
        }
    }

    /// Opens the extension folder in the file manager on a worker (no-op until the folder is
    /// known). A failure comes back from `poll`.
    pub(crate) fn reveal_folder(&mut self) {
        let Some(dir) = self.folder.clone() else { return };
        let ops = Arc::clone(&self.ops);
        let tx = self.notices_tx.clone();
        // A spawn failure is already reported as a notice; nothing else to undo.
        self.spawn_short("ms-ext-reveal", move || {
            if let Err(message) = ops.reveal_dir(&dir) {
                send_notice(&tx, message);
            }
        });
    }

    /// Persists `service`'s transport: queues it for the controller's one saver worker
    /// (started when none runs), so saves never race and the persisted value is the last one
    /// queued. The live value is the caller's (`mt_transport::set`, on the GUI thread). A
    /// failure is logged and comes back from `poll`. GUI-safe (brief lock).
    pub(crate) fn save_transport(&mut self, service: MtService, transport: MtTransport) {
        let start_worker = self.transport_saves.lock().unwrap_or_else(PoisonError::into_inner).queue(service, transport);
        if !start_worker {
            return;
        }
        let ops = Arc::clone(&self.ops);
        let tx = self.notices_tx.clone();
        let saves = Arc::clone(&self.transport_saves);
        if !self.spawn_short("ms-mt-transport-save", move || drain_transport_saves(ops.as_ref(), &saves, &tx)) {
            // No worker took the queue: forget it, so the next toggle tries a worker again.
            *self.transport_saves.lock().unwrap_or_else(PoisonError::into_inner) = TransportSaves::default();
        }
    }

    /// Spawns a short detached worker; a spawn failure is reported like a job failure. Returns
    /// whether the worker runs.
    fn spawn_short(&mut self, name: &str, body: impl FnOnce() + Send + 'static) -> bool {
        match spawn_job(name, body) {
            Ok(rx) => {
                self.short_jobs.push(rx);
                true
            }
            Err(err) => {
                runtime_log::log_error(format!("[MT] browser extension: could not spawn the {name} worker: {err}"));
                send_notice(&self.notices_tx, tf!("translation.mt_panel.extension_worker_error", error = err));
                false
            }
        }
    }
}

/// Queues a notice for `poll`. The receiver lives in the same controller as every sender clone
/// handed to its workers, so a closed channel only means the tab was dropped meanwhile.
fn send_notice(tx: &Sender<String>, message: String) {
    if tx.send(message).is_err() {
        runtime_log::log_info("[MT] browser extension: a worker finished after the Translation tab closed; its notice was dropped");
    }
}

/// Spawns a detached worker running `body` and returns the receiver of its result.
///
/// # Errors
/// The OS error when the thread could not be created.
fn spawn_job<T: Send + 'static>(name: &str, body: impl FnOnce() -> T + Send + 'static) -> std::io::Result<Receiver<T>> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            // A closed receiver means the tab was dropped while the job ran; nothing waits for
            // the result any more.
            if tx.send(body()).is_err() {
                runtime_log::log_info("[MT] browser extension: a job finished after the Translation tab closed; result dropped");
            }
        })
        // Detached on purpose: the result comes back through the channel.
        .map(|_detached| rx)
}

#[cfg(test)]
mod tests;
