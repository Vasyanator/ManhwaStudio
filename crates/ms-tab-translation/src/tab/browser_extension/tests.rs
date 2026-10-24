//! Tests of `tab/browser_extension.rs`: the `ExtensionUiState` mapping table and the
//! controller's job state machine over an injected `ExtensionOps`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use ms_browser_bridge::protocol::{BrowserInfo, ModuleInfo};
use ms_browser_bridge::{PeerInfo, PeerState};

use super::*;

static BUNDLE: Bundle = Bundle { build_id: "build-new", files: &[] };

fn status(listen: ListenState, peers: Vec<PeerInfo>) -> BridgeStatus {
    BridgeStatus { listen, peers }
}

fn listening() -> ListenState {
    ListenState::Listening { port: 4242 }
}

fn peer(state: PeerState, module_version: Option<u32>, build_current: bool) -> PeerInfo {
    PeerInfo {
        id: 1,
        browser: BrowserInfo { name: "Chrome".into(), version: "140".into() },
        ext_version: "1.0.0".into(),
        build_id: Some("build-new".into()),
        protocol: 1,
        modules: module_version.map(|version| ModuleInfo { name: TRANSLATE_MODULE.into(), version, sites: Vec::new() }).into_iter().collect(),
        state,
        build_current,
    }
}

fn not_included() -> Result<&'static Bundle, NotIncluded> {
    Err(NotIncluded { reason: "node was not found" })
}

/// One row of the mapping table: name, bridge status, folder probe, bundle, expected state.
type MappingCase = (&'static str, BridgeStatus, Option<InstalledProbe>, Result<&'static Bundle, NotIncluded>, ExtensionUiState);

#[test]
fn ui_state_mapping_table() {
    let built = Some(InstalledProbe::Build("build-new".into()));
    let stale = Some(InstalledProbe::Build("build-old".into()));
    let ready = || peer(PeerState::Ready, Some(TRANSLATE_MODULE_VERSION), true);
    let cases: Vec<MappingCase> = vec![
        (
            "server failure wins over everything",
            status(ListenState::Failed { error: BridgeError::NotStarted }, vec![ready()]),
            built.clone(),
            Ok(&BUNDLE),
            ExtensionUiState::ServerFailed { error: BridgeError::NotStarted.to_string() },
        ),
        ("serving peer", status(listening(), vec![ready()]), built.clone(), Ok(&BUNDLE), ExtensionUiState::Connected { browser: "Chrome 140".into() }),
        (
            "a working foreign build still counts as connected",
            status(listening(), vec![peer(PeerState::Ready, Some(TRANSLATE_MODULE_VERSION), false)]),
            stale.clone(),
            Ok(&BUNDLE),
            ExtensionUiState::Connected { browser: "Chrome 140".into() },
        ),
        ("connected without a bundle in this build", status(listening(), vec![ready()]), None, not_included(), ExtensionUiState::Connected { browser: "Chrome 140".into() }),
        ("protocol-outdated peer", status(listening(), vec![peer(PeerState::Outdated, None, false)]), built.clone(), Ok(&BUNDLE), ExtensionUiState::Outdated),
        (
            "ready peer without the module version",
            status(listening(), vec![peer(PeerState::Ready, Some(TRANSLATE_MODULE_VERSION + 1), true)]),
            built.clone(),
            Ok(&BUNDLE),
            ExtensionUiState::Outdated,
        ),
        ("no bundle, no peer", status(listening(), Vec::new()), None, not_included(), ExtensionUiState::NotInBuild { reason: "node was not found" }),
        ("probe pending", status(ListenState::Starting, Vec::new()), None, Ok(&BUNDLE), ExtensionUiState::Checking),
        ("nothing installed", status(listening(), Vec::new()), Some(InstalledProbe::Missing), Ok(&BUNDLE), ExtensionUiState::NotInstalled),
        ("unreadable stamp", status(listening(), Vec::new()), Some(InstalledProbe::Unreadable), Ok(&BUNDLE), ExtensionUiState::NotInstalled),
        ("corrupt stamp", status(listening(), Vec::new()), Some(InstalledProbe::Corrupt), Ok(&BUNDLE), ExtensionUiState::Outdated),
        ("this build installed, browser not connected", status(listening(), Vec::new()), built.clone(), Ok(&BUNDLE), ExtensionUiState::InstalledNotConnected),
        ("not started yet but installed", status(ListenState::NotStarted, Vec::new()), built, Ok(&BUNDLE), ExtensionUiState::InstalledNotConnected),
        ("another build installed", status(listening(), Vec::new()), stale, Ok(&BUNDLE), ExtensionUiState::Outdated),
    ];
    for (name, status, probe, bundle, expected) in cases {
        assert_eq!(ExtensionUiState::from_status(&status, probe.as_ref(), bundle), expected, "{name}");
    }
}

#[test]
fn unnamed_browser_is_an_empty_string() {
    let mut anonymous = peer(PeerState::Ready, Some(TRANSLATE_MODULE_VERSION), true);
    anonymous.browser = BrowserInfo::default();
    let state = ExtensionUiState::from_status(&status(listening(), vec![anonymous]), None, Ok(&BUNDLE));
    assert_eq!(state, ExtensionUiState::Connected { browser: String::new() });
}

/// A scripted `ExtensionOps` that records every call.
struct FakeOps {
    calls: Mutex<Vec<&'static str>>,
    bundle_present: bool,
    install_result: fn() -> Result<Option<InstallReport>, InstallError>,
    start_result: Result<(), BridgeError>,
    reveal_result: Result<(), String>,
    save_result: Result<(), String>,
    peers: Vec<PeerInfo>,
    /// Held by the test to keep `install` blocked; `install` waits for it to be released.
    install_gate: Mutex<()>,
    /// Held by the test to keep `save_transport` blocked, like `install_gate`.
    save_gate: Mutex<()>,
    /// Every persisted `(service, transport)`, in write order.
    saved: Mutex<Vec<(MtService, MtTransport)>>,
    /// Saves running right now, and the most that ever ran at once.
    saving: AtomicUsize,
    max_saving: AtomicUsize,
}

impl FakeOps {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            bundle_present: true,
            install_result: installed_report,
            start_result: Ok(()),
            reveal_result: Ok(()),
            save_result: Ok(()),
            peers: Vec::new(),
            install_gate: Mutex::new(()),
            save_gate: Mutex::new(()),
            saved: Mutex::new(Vec::new()),
            saving: AtomicUsize::new(0),
            max_saving: AtomicUsize::new(0),
        }
    }

    fn record(&self, call: &'static str) {
        self.calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(call);
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

fn installed_report() -> Result<Option<InstallReport>, InstallError> {
    Ok(Some(InstallReport { build_id: "build-new".into(), previous_build_id: None, written: 3, unchanged: 0, removed: 0, previous_stamp_corrupt: false }))
}

impl ExtensionOps for FakeOps {
    fn bundle(&self) -> Result<&'static Bundle, NotIncluded> {
        if self.bundle_present { Ok(&BUNDLE) } else { not_included() }
    }

    fn status(&self) -> BridgeStatus {
        status(listening(), self.peers.clone())
    }

    fn install_dir(&self) -> PathBuf {
        PathBuf::from("/home/u/ms/browser_extension/chromium")
    }

    fn ensure_started(&self) -> Result<(), BridgeError> {
        self.record("ensure_started");
        self.start_result.clone()
    }

    fn probe_installed(&self, _dir: &Path) -> InstalledProbe {
        self.record("probe");
        InstalledProbe::Build("build-new".into())
    }

    fn install(&self, _bundle: &'static Bundle, _dir: &Path) -> Result<Option<InstallReport>, InstallError> {
        self.record("install");
        drop(self.install_gate.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        (self.install_result)()
    }

    fn reveal_dir(&self, _dir: &Path) -> Result<(), String> {
        self.record("reveal");
        self.reveal_result.clone()
    }

    fn reload_outdated(&self) -> usize {
        self.record("reload");
        self.peers.iter().filter(|peer| peer.needs_reload()).count()
    }

    fn save_transport(&self, service: MtService, transport: MtTransport) -> Result<(), String> {
        self.record("save");
        let running = self.saving.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_saving.fetch_max(running, Ordering::SeqCst);
        drop(self.save_gate.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        self.saved.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((service, transport));
        self.saving.fetch_sub(1, Ordering::SeqCst);
        self.save_result.clone()
    }
}

/// Polls until no job runs (5 s deadline) and returns every notice seen.
fn settle(controller: &mut BrowserExtensionController) -> Vec<String> {
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    let mut notices = Vec::new();
    loop {
        notices.extend(controller.poll());
        if !controller.busy() {
            return notices;
        }
        assert!(Instant::now() < deadline, "the controller's jobs did not finish: {controller:?}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn install_job_runs_the_full_sequence_and_reloads_stale_peers() {
    let mut ops = FakeOps::new();
    ops.peers = vec![peer(PeerState::Ready, Some(TRANSLATE_MODULE_VERSION), false)];
    let outcome = run_install_job(&ops);
    assert_eq!(ops.calls(), ["install", "ensure_started", "reveal", "reload", "probe"]);
    assert_eq!(outcome.notice, InstallNotice::Installed { reloaded: 1 });
    assert_eq!(outcome.started, Some(Ok(())));
    assert_eq!(outcome.reveal_error, None);
    assert_eq!(outcome.probe, InstalledProbe::Build("build-new".into()));
}

#[test]
fn install_job_skips_the_reload_when_every_peer_is_current() {
    let mut ops = FakeOps::new();
    ops.peers = vec![peer(PeerState::Ready, Some(TRANSLATE_MODULE_VERSION), true)];
    ops.reveal_result = Err("no file manager".into());
    let outcome = run_install_job(&ops);
    assert_eq!(ops.calls(), ["install", "ensure_started", "reveal", "probe"]);
    assert_eq!(outcome.notice, InstallNotice::Installed { reloaded: 0 });
    assert_eq!(outcome.reveal_error.as_deref(), Some("no file manager"));
}

#[test]
fn install_job_stops_after_a_busy_or_failed_unpack() {
    let mut busy = FakeOps::new();
    busy.install_result = || Ok(None);
    let outcome = run_install_job(&busy);
    assert_eq!(busy.calls(), ["install", "probe"]);
    assert_eq!((outcome.notice, outcome.started), (InstallNotice::Busy, None));

    let mut failing = FakeOps::new();
    failing.install_result = || Err(InstallError::Io { op: "write", path: "x".into(), source: std::io::Error::other("disk full") });
    let outcome = run_install_job(&failing);
    assert_eq!(failing.calls(), ["install", "probe"]);
    assert!(matches!(&outcome.notice, InstallNotice::Failed { error } if error.contains("disk full")), "{:?}", outcome.notice);

    let mut no_bundle = FakeOps::new();
    no_bundle.bundle_present = false;
    let outcome = run_install_job(&no_bundle);
    assert_eq!(no_bundle.calls(), ["probe"]);
    assert!(matches!(outcome.notice, InstallNotice::Failed { .. }));
}

#[test]
fn controller_starts_once_and_reports_the_probe() {
    let ops = Arc::new(FakeOps::new());
    let mut controller = BrowserExtensionController::with_ops(ops.clone());
    controller.ensure_started();
    controller.ensure_started();
    assert!(settle(&mut controller).is_empty());
    assert_eq!(ops.calls(), ["ensure_started", "probe"]);
    assert_eq!(controller.probe, Some(InstalledProbe::Build("build-new".into())));
    assert_eq!(controller.folder.as_deref(), Some(Path::new("/home/u/ms/browser_extension/chromium")));
    assert_eq!(controller.start_error, None);
}

#[test]
fn controller_start_failure_reads_as_server_failure() {
    let mut fake = FakeOps::new();
    fake.start_result = Err(BridgeError::System { detail: "no thread".into() });
    let ops = Arc::new(fake);
    let mut controller = BrowserExtensionController::with_ops(ops);
    controller.ensure_started();
    settle(&mut controller);
    let error = BridgeError::System { detail: "no thread".into() };
    assert_eq!(controller.start_error, Some(error.clone()));
    let mut shown = controller.ops.status();
    shown.listen = ListenState::Failed { error: error.clone() };
    assert_eq!(ExtensionUiState::from_status(&shown, controller.probe.as_ref(), controller.ops.bundle()), ExtensionUiState::ServerFailed { error: error.to_string() });
}

#[test]
fn controller_install_job_ignores_a_second_click_and_clears_a_start_error() {
    let mut fake = FakeOps::new();
    fake.start_result = Ok(());
    let ops = Arc::new(fake);
    let mut controller = BrowserExtensionController::with_ops(ops.clone());
    controller.start_error = Some(BridgeError::NotStarted);
    controller.last_install = Some(InstallNotice::Busy);
    let gate = ops.install_gate.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    controller.start_install();
    assert!(controller.install.is_running());
    assert_eq!(controller.last_install, None, "a new job clears the previous outcome");
    controller.start_install();
    drop(gate);
    let notices = settle(&mut controller);
    assert!(notices.is_empty(), "{notices:?}");
    assert_eq!(ops.calls().iter().filter(|call| **call == "install").count(), 1);
    assert_eq!(controller.last_install, Some(InstallNotice::Installed { reloaded: 0 }));
    assert_eq!(controller.start_error, None);
    // The install job started the bridge: no separate start worker afterwards.
    controller.ensure_started();
    assert!(!controller.start.is_running());
}

#[test]
fn controller_short_worker_failures_come_back_as_notices() {
    let mut fake = FakeOps::new();
    fake.save_result = Err("read-only".into());
    fake.reveal_result = Err("no file manager".into());
    let ops = Arc::new(fake);
    let mut controller = BrowserExtensionController::with_ops(ops.clone());
    // The folder is unknown until a worker resolved it: no reveal yet.
    controller.reveal_folder();
    controller.save_transport(MtService::Deepl, MtTransport::Browser);
    let notices = settle(&mut controller);
    // The text is the localized `transport_save_error` (catalog-dependent), so only its count
    // is pinned; the error itself is in the log line.
    assert_eq!(notices.len(), 1, "{notices:?}");
    controller.ensure_started();
    settle(&mut controller);
    controller.reveal_folder();
    assert_eq!(settle(&mut controller), ["no file manager"]);
    assert_eq!(ops.calls(), ["save", "ensure_started", "probe", "reveal"]);
}

#[test]
fn transport_saves_are_serialized_and_the_last_toggle_is_persisted() {
    let ops = Arc::new(FakeOps::new());
    let mut controller = BrowserExtensionController::with_ops(ops.clone());
    let gate = ops.save_gate.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    controller.save_transport(MtService::Deepl, MtTransport::Browser);
    // Let the saver enter the first write, so the next toggles queue behind it.
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while ops.saving.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the saver never started");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    controller.save_transport(MtService::Deepl, MtTransport::Builtin);
    controller.save_transport(MtService::Google, MtTransport::Browser);
    controller.save_transport(MtService::Deepl, MtTransport::Browser);
    controller.save_transport(MtService::Deepl, MtTransport::Builtin);
    drop(gate);
    assert!(settle(&mut controller).is_empty());
    let saved = ops.saved.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    // The first write, then ONE write per service with its latest value (the toggles in
    // between were coalesced), never two at once.
    assert_eq!(saved.first(), Some(&(MtService::Deepl, MtTransport::Browser)));
    assert_eq!(saved.len(), 3, "{saved:?}");
    assert!(saved[1..].contains(&(MtService::Google, MtTransport::Browser)), "{saved:?}");
    assert_eq!(saved.iter().rev().find(|(service, _)| *service == MtService::Deepl), Some(&(MtService::Deepl, MtTransport::Builtin)), "the last toggle is persisted last");
    assert_eq!(ops.max_saving.load(Ordering::SeqCst), 1, "saves never overlap");
    // The queue is idle again: the next toggle starts a new saver.
    controller.save_transport(MtService::Yandex, MtTransport::Browser);
    assert!(settle(&mut controller).is_empty());
    assert_eq!(ops.saved.lock().unwrap_or_else(std::sync::PoisonError::into_inner).last(), Some(&(MtService::Yandex, MtTransport::Browser)));
}
