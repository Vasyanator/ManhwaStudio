/*
File: crates/ms-browser-bridge/src/app.rs

Purpose:
The app's wiring of the bridge: the ONE place that builds the app's `BridgeConfig` from the
`ms-browser-ext` facts (unpacked folder, Chromium origin, embedded bundle `build_id`) and starts
the process-wide `BridgeService`. Every consumer (machine translation now, a download module
later) calls `ensure_started_for_app`; nobody else spells the config.

Key items:
- `ensure_started_for_app(data_dir, app_version)` — keep the unpacked folder current, start the
  server, start the stale-peer reload watcher. Idempotent; cheap after the first call.
- `install_guarded(bundle, dir)` — the one serialized unpack of the process (auto re-unpack
  here, the explicit install job of the UI): a second concurrent call in this process answers
  "busy" instead of waiting. Across processes, `ms_browser_ext::install`'s folder lock serializes
  the file work itself (another app process's install is waited for).
- `register_app_version` / `registered_app_version` — the executable's version, registered once
  at startup (library crates cannot read `MS_APP_VERSION`); sent in `welcome` (diagnostic only).
- `app_bridge_config`, `unpack_plan`, `ReloadLedger` — the pure decisions, unit-tested.

Contracts (decision D1: `build_id` keeps the unpacked folder current, it never gates):
- Auto re-unpack: when the build embeds a bundle and the folder's stamp names another
  `build_id` (or the stamp is corrupt), the folder is re-unpacked ON THE CALLING THREAD before
  the server starts. Nothing is installed when the folder holds no stamp: installing is an
  explicit user action. Blocking file I/O: worker threads only, never the GUI thread.
- Starting the server writes `bridge.json` into the folder (creating it if absent); that alone
  is not an install (no stamp), so the UI still reads "not installed".
- Reload: a watcher thread (started once per process, only when a bundle is embedded) polls the
  status every `RELOAD_POLL` and, while the folder holds the embedded bundle, sends `reload` to
  EXACTLY the peers (by `PeerInfo::id`) whose `build_id` differs (or whose protocol is another),
  following `ReloadLedger`: a peer connection is reloaded once; it is retried only when the same
  connection is still there `RELOAD_RETRY_AFTER` later (the reload was lost) or when the reload
  could not be queued (it left, or its queue was full: retried on the next poll); and one
  installation (`ReloadIdentity`: browser, extension version, build, protocol) is reloaded at most
  `MAX_RELOADS_PER_IDENTITY` times per process, so a stale peer loaded from some other folder
  never loops. A reload drops that peer's in-flight request (`Disconnected`); routing
  (`api::select_peer`) already prefers a current peer, so this only hits a request when the stale
  peer is the only one.
- Process-wide statics here (`APP_VERSION`, `INSTALLING`, `FOLDER_SETTLED`, `FOLDER_CURRENT`,
  `WATCHER_STARTED`) share the documented global §5 exception of `service.rs`: one extension
  folder and one server per app process.
- Platform gating (`PlatformFeature::BrowserExtension`) is the CALLER's: this crate does not
  depend on `ms-platform`.
*/

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ms_browser_ext::{Bundle, InstallError, InstallReport, chromium_origin, install, install_dir, installed_build_id};
use ms_log::runtime_log;

use crate::api::{BridgeConfig, BridgeTimings, PeerInfo, ReloadTarget};
use crate::errors::BridgeError;
use crate::service::BridgeService;

/// How often the reload watcher looks at the connected peers.
const RELOAD_POLL: Duration = Duration::from_secs(2);

/// A peer connection still present this long after its reload was queued did not act on it: the
/// reload is sent again (bounded by `MAX_RELOADS_PER_IDENTITY`).
pub const RELOAD_RETRY_AFTER: Duration = Duration::from_mins(1);

/// Most reloads queued for one installation (`ReloadIdentity`) per process: the first one plus
/// one retry (a lost reload, or a reload that brought the same stale build back).
pub const MAX_RELOADS_PER_IDENTITY: u32 = 2;

/// The executable's version (`MS_APP_VERSION`), registered once at startup.
static APP_VERSION: OnceLock<&'static str> = OnceLock::new();
/// An unpack (auto re-unpack or the explicit install) is running.
static INSTALLING: AtomicBool = AtomicBool::new(false);
/// The folder was found current (or re-unpacked) once; later calls skip the stamp read.
static FOLDER_SETTLED: AtomicBool = AtomicBool::new(false);
/// The folder holds the embedded bundle, so reloading a stale peer loads the right code.
static FOLDER_CURRENT: AtomicBool = AtomicBool::new(false);
/// The reload watcher thread was spawned.
static WATCHER_STARTED: AtomicBool = AtomicBool::new(false);

/// Registers the executable's version (`env!("MS_APP_VERSION")` of the binary) for
/// [`registered_app_version`]. The first registration wins; later ones are ignored.
pub fn register_app_version(version: &'static str) {
    if APP_VERSION.set(version).is_err() {
        runtime_log::log_info("[browser_bridge] app version already registered; keeping the first one");
    }
}

/// The version registered at startup, `None` before [`register_app_version`] ran (unit tests,
/// tools). Consumers pass it to [`ensure_started_for_app`].
#[must_use]
pub fn registered_app_version() -> Option<&'static str> {
    APP_VERSION.get().copied()
}

/// What the folder needs before the server starts (pure decision of [`unpack_plan`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnpackPlan {
    /// This build embeds no bundle: nothing to compare, nothing to unpack.
    NoBundle,
    /// No stamp: the user never installed the extension; installing stays explicit.
    NotInstalled,
    /// The stamp names the embedded bundle.
    Current,
    /// The user installed another build (or the stamp is corrupt): re-unpack now.
    Reunpack,
    /// The stamp could not be read (I/O error): leave the folder alone, retry next time.
    Unreadable,
}

/// Decides what the folder needs. `installed` is `installed_build_id(install_dir)`.
#[must_use]
pub fn unpack_plan(bundle: Option<&Bundle>, installed: &Result<Option<String>, InstallError>) -> UnpackPlan {
    let Some(bundle) = bundle else {
        return UnpackPlan::NoBundle;
    };
    match installed {
        Ok(None) => UnpackPlan::NotInstalled,
        Ok(Some(build_id)) if build_id == bundle.build_id => UnpackPlan::Current,
        Ok(Some(_)) | Err(InstallError::StampCorrupt { .. }) => UnpackPlan::Reunpack,
        Err(_) => UnpackPlan::Unreadable,
    }
}

/// The app's server config: the folder `install_dir`, the Chromium origin, the embedded
/// `build_id` (`None` without a bundle) and the app version.
#[must_use]
pub fn app_bridge_config(install_dir: &Path, app_version: &str, bundle: Option<&Bundle>) -> BridgeConfig {
    BridgeConfig {
        install_dir: install_dir.to_path_buf(),
        allowed_origin: chromium_origin(),
        expected_build_id: bundle.map(|bundle| bundle.build_id.to_owned()),
        app_version: app_version.to_owned(),
        timings: BridgeTimings::default(),
    }
}

/// What identifies one extension installation across reconnects (`PeerInfo::id` is per
/// connection; a reloaded extension comes back under a new id). Two profiles running the same
/// build in the same browser share it, which only makes the reload budget shared.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReloadIdentity {
    /// `PeerInfo::browser.name`.
    pub browser: String,
    /// `PeerInfo::ext_version`.
    pub ext_version: String,
    /// `PeerInfo::build_id`.
    pub build_id: Option<String>,
    /// `PeerInfo::protocol`.
    pub protocol: u32,
}

impl ReloadIdentity {
    /// The identity of `peer`.
    #[must_use]
    pub fn of(peer: &PeerInfo) -> Self {
        Self { browser: peer.browser.name.clone(), ext_version: peer.ext_version.clone(), build_id: peer.build_id.clone(), protocol: peer.protocol }
    }
}

/// The reload watcher's memory: which peer connections got a reload and when, and how many
/// reloads each installation got. Pure (time is passed in), so the policy is unit-tested; the
/// rule is in the file header ("Reload").
#[derive(Debug, Default)]
pub struct ReloadLedger {
    /// Peer connection id -> when its reload was queued.
    sent: HashMap<u64, Instant>,
    /// Reloads queued per installation.
    per_identity: HashMap<ReloadIdentity, u32>,
}

impl ReloadLedger {
    /// The ids of the peers to reload now, in connection order. Forgets peers that left.
    #[must_use]
    pub fn plan(&mut self, peers: &[PeerInfo], now: Instant) -> Vec<u64> {
        self.sent.retain(|id, _| peers.iter().any(|peer| peer.id == *id));
        let mut planned: HashMap<ReloadIdentity, u32> = HashMap::new();
        let mut targets = Vec::new();
        for peer in peers.iter().filter(|peer| ReloadTarget::NeedsUpdate.includes(peer)) {
            if self.sent.get(&peer.id).is_some_and(|queued_at| now.saturating_duration_since(*queued_at) < RELOAD_RETRY_AFTER) {
                continue;
            }
            let identity = ReloadIdentity::of(peer);
            let used = self.per_identity.get(&identity).copied().unwrap_or(0) + planned.get(&identity).copied().unwrap_or(0);
            if used >= MAX_RELOADS_PER_IDENTITY {
                continue;
            }
            *planned.entry(identity).or_insert(0) += 1;
            targets.push(peer.id);
        }
        targets
    }

    /// Records that a reload was queued for `queued` (ids from `plan`, as confirmed by
    /// `BridgeService::request_reload_peers`). Ids not in `queued` are left as they were, so they
    /// are planned again on the next poll.
    pub fn record(&mut self, peers: &[PeerInfo], queued: &[u64], now: Instant) {
        for peer in peers.iter().filter(|peer| queued.contains(&peer.id)) {
            self.sent.insert(peer.id, now);
            *self.per_identity.entry(ReloadIdentity::of(peer)).or_insert(0) += 1;
        }
    }
}

/// Clears `INSTALLING` when the unpack ends (also on a panic inside `install`).
struct InstallSlot;

impl Drop for InstallSlot {
    fn drop(&mut self) {
        INSTALLING.store(false, Ordering::Release);
    }
}

/// Unpacks `bundle` into `dir` unless another unpack runs right now (`Ok(None)`: busy, nothing
/// done). The one serialized install of the process: the auto re-unpack and the UI's explicit
/// install both go through it. On success the folder counts as current for the reload watcher.
/// Blocking file I/O, and it waits while ANOTHER process installs into `dir` (the folder lock of
/// `ms_browser_ext::install`): worker threads only.
///
/// # Errors
/// The `ms_browser_ext::install` failure (the previous install, if any, stays usable: the stamp
/// is written last).
pub fn install_guarded(bundle: &Bundle, dir: &Path) -> Result<Option<InstallReport>, InstallError> {
    if INSTALLING.swap(true, Ordering::AcqRel) {
        return Ok(None);
    }
    let _slot = InstallSlot;
    let report = install(bundle, dir)?;
    FOLDER_CURRENT.store(true, Ordering::Release);
    FOLDER_SETTLED.store(true, Ordering::Release);
    Ok(Some(report))
}

/// Makes the app's bridge run: re-unpacks a stale installed extension (decision D1; blocking,
/// on this thread), starts the process-wide server with the app config (non-blocking,
/// idempotent) and, when a bundle is embedded, the stale-peer reload watcher. Cheap after the
/// first successful call. Worker threads only (the first call may unpack files).
///
/// An unpack failure is logged and does not stop the start: the previous install keeps
/// working and the UI shows the folder as outdated.
///
/// # Errors
/// `BridgeError::System` when the start thread cannot be spawned (`BridgeService::ensure_started`).
pub fn ensure_started_for_app(data_dir: &Path, app_version: &str) -> Result<(), BridgeError> {
    let dir = install_dir(data_dir);
    let bundle = ms_browser_ext::bundle().ok();
    if !FOLDER_SETTLED.load(Ordering::Acquire) {
        refresh_folder(bundle, &dir);
    }
    BridgeService::shared().ensure_started(app_bridge_config(&dir, app_version, bundle))?;
    if bundle.is_some() {
        start_reload_watcher();
    }
    Ok(())
}

/// Applies [`unpack_plan`] to the folder: re-unpacks when needed, records whether the folder
/// holds the embedded bundle, and marks it settled when no later call needs to look again.
fn refresh_folder(bundle: Option<&'static Bundle>, dir: &Path) {
    let installed = installed_build_id(dir);
    match unpack_plan(bundle, &installed) {
        UnpackPlan::NoBundle => FOLDER_SETTLED.store(true, Ordering::Release),
        // Looked at again on the next call: the user may install meanwhile (that install marks
        // the folder current itself).
        UnpackPlan::NotInstalled => {}
        UnpackPlan::Current => {
            FOLDER_CURRENT.store(true, Ordering::Release);
            FOLDER_SETTLED.store(true, Ordering::Release);
        }
        UnpackPlan::Unreadable => {
            if let Err(err) = &installed {
                runtime_log::log_warn(format!("[browser_bridge] the installed extension stamp in {} could not be read: {err}; not re-unpacking now", dir.display()));
            }
        }
        UnpackPlan::Reunpack => {
            // `Reunpack` is only decided with a bundle.
            let Some(bundle) = bundle else { return };
            let previous = installed.as_ref().ok().cloned().flatten();
            runtime_log::log_info(format!(
                "[browser_bridge] the installed extension in {} is build {} (stamp {}); re-unpacking build {}",
                dir.display(),
                previous.as_deref().unwrap_or("?"),
                if installed.is_err() { "corrupt" } else { "readable" },
                bundle.build_id
            ));
            match install_guarded(bundle, dir) {
                Ok(Some(report)) => runtime_log::log_info(format!(
                    "[browser_bridge] extension re-unpacked: build {} ({} written, {} unchanged, {} removed)",
                    report.build_id, report.written, report.unchanged, report.removed
                )),
                Ok(None) => runtime_log::log_info("[browser_bridge] another extension install is running; the re-unpack is left to it"),
                Err(err) => runtime_log::log_error(format!("[browser_bridge] re-unpacking the extension into {} failed: {err}; the previous install stays in place", dir.display())),
            }
        }
    }
}

/// Spawns the reload watcher once per process.
fn start_reload_watcher() {
    if WATCHER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("ms-bridge-reload-watch".into()).spawn(|| {
        // Lives for the process, like the server's listener; owns its own ledger.
        let mut ledger = ReloadLedger::default();
        loop {
            std::thread::sleep(RELOAD_POLL);
            if !FOLDER_CURRENT.load(Ordering::Acquire) {
                continue;
            }
            let status = BridgeService::shared().status();
            let now = Instant::now();
            let targets = ledger.plan(&status.peers, now);
            if targets.is_empty() {
                continue;
            }
            let queued = BridgeService::shared().request_reload_peers(&targets);
            runtime_log::log_info(format!("[browser_bridge] reload queued for peer(s) {queued:?} of {targets:?} running another extension build"));
            ledger.record(&status.peers, &queued, now);
        }
    });
    if let Err(err) = spawned {
        WATCHER_STARTED.store(false, Ordering::Release);
        runtime_log::log_error(format!("[browser_bridge] could not spawn the reload watcher: {err}; stale extensions are not reloaded automatically"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{BrowserInfo, CONFIG_FILE_NAME, PeerState};

    static BUNDLE: Bundle = Bundle { build_id: "build-new", files: &[] };

    fn peer(id: u64, build_id: Option<&str>, state: PeerState, build_current: bool) -> PeerInfo {
        PeerInfo { id, browser: BrowserInfo::default(), ext_version: "1.0.0".into(), build_id: build_id.map(str::to_owned), protocol: 1, modules: Vec::new(), state, build_current }
    }

    #[test]
    fn unpack_plan_reunpacks_only_an_existing_foreign_install() {
        let corrupt = serde_json::from_str::<serde_json::Value>("{").map_err(|source| InstallError::StampCorrupt { path: "stamp".into(), source });
        let io = Err(InstallError::Io { op: "read", path: "stamp".into(), source: std::io::Error::other("denied") });
        assert_eq!(unpack_plan(None, &Ok(Some("x".into()))), UnpackPlan::NoBundle);
        assert_eq!(unpack_plan(Some(&BUNDLE), &Ok(None)), UnpackPlan::NotInstalled);
        assert_eq!(unpack_plan(Some(&BUNDLE), &Ok(Some("build-new".into()))), UnpackPlan::Current);
        assert_eq!(unpack_plan(Some(&BUNDLE), &Ok(Some("build-old".into()))), UnpackPlan::Reunpack);
        assert_eq!(unpack_plan(Some(&BUNDLE), &corrupt.map(|_value| None::<String>)), UnpackPlan::Reunpack);
        assert_eq!(unpack_plan(Some(&BUNDLE), &io), UnpackPlan::Unreadable);
    }

    #[test]
    fn app_config_names_the_extension_facts() {
        let dir = Path::new("/home/u/data/browser_extension/chromium");
        let cfg = app_bridge_config(dir, "412", Some(&BUNDLE));
        assert_eq!(cfg.install_dir, dir);
        assert_eq!(cfg.allowed_origin, chromium_origin());
        assert_eq!(cfg.expected_build_id.as_deref(), Some("build-new"));
        assert_eq!(cfg.app_version, "412");
        assert_eq!(cfg.timings, BridgeTimings::default());
        assert_eq!(app_bridge_config(dir, "412", None).expected_build_id, None);
    }

    #[test]
    fn each_stale_peer_is_reloaded_once_and_current_ones_never() {
        let peers = [
            peer(1, Some("build-new"), PeerState::Ready, true),
            peer(2, Some("build-old"), PeerState::Ready, false),
            peer(3, None, PeerState::Outdated, false),
        ];
        let now = Instant::now();
        let mut ledger = ReloadLedger::default();
        let first = ledger.plan(&peers, now);
        assert_eq!(first, [2, 3], "only the stale peers, each by id");
        ledger.record(&peers, &first, now);
        assert!(ledger.plan(&peers, now + RELOAD_POLL).is_empty(), "a queued reload is not repeated at once");
    }

    #[test]
    fn a_reload_that_could_not_be_queued_is_retried_on_the_next_poll() {
        let peers = [peer(2, Some("build-old"), PeerState::Ready, false)];
        let now = Instant::now();
        let mut ledger = ReloadLedger::default();
        assert_eq!(ledger.plan(&peers, now), [2]);
        ledger.record(&peers, &[], now);
        assert_eq!(ledger.plan(&peers, now + RELOAD_POLL), [2]);
    }

    #[test]
    fn a_lost_reload_is_retried_later_and_the_retries_are_bounded() {
        let peers = [peer(2, Some("build-old"), PeerState::Ready, false)];
        let start = Instant::now();
        let mut ledger = ReloadLedger::default();
        let first = ledger.plan(&peers, start);
        ledger.record(&peers, &first, start);
        // The same connection is still there after the retry interval: the reload was lost.
        let later = start + RELOAD_RETRY_AFTER;
        assert_eq!(ledger.plan(&peers, later), [2]);
        ledger.record(&peers, &[2], later);
        // The identity's budget is spent: never again in this process.
        assert!(ledger.plan(&peers, later + RELOAD_RETRY_AFTER * 10).is_empty());
    }

    #[test]
    fn an_installation_that_comes_back_stale_does_not_loop() {
        let start = Instant::now();
        let mut ledger = ReloadLedger::default();
        let mut now = start;
        let mut reloads = 0;
        // Loaded from another folder: every reload reconnects with the same build under a new id.
        for id in 2..10 {
            let peers = [peer(1, Some("build-new"), PeerState::Ready, true), peer(id, Some("build-old"), PeerState::Ready, false)];
            let targets = ledger.plan(&peers, now);
            reloads += targets.len();
            ledger.record(&peers, &targets, now);
            now += RELOAD_POLL;
        }
        assert_eq!(reloads, usize::try_from(MAX_RELOADS_PER_IDENTITY).unwrap());
    }

    #[test]
    fn different_installations_of_one_foreign_build_are_each_reloaded() {
        let mut edge = peer(3, Some("build-old"), PeerState::Ready, false);
        edge.browser.name = "Edge".into();
        let peers = [peer(2, Some("build-old"), PeerState::Ready, false), edge];
        let mut ledger = ReloadLedger::default();
        assert_eq!(ledger.plan(&peers, Instant::now()), [2, 3]);
    }

    #[test]
    fn the_embedded_bundle_never_carries_the_pairing_file() {
        // `install` keeps files the stamp does not list, so `bridge.json` must never be a bundle
        // file: a re-unpack would overwrite the pairing and later delete it as stale.
        match ms_browser_ext::bundle() {
            Ok(bundle) => assert!(bundle.files.iter().all(|(path, _)| *path != CONFIG_FILE_NAME), "the bundle contains {CONFIG_FILE_NAME}"),
            Err(not_included) => eprintln!("no bundle in this build ({}); nothing to check", not_included.reason),
        }
    }
}
