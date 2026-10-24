/*
File: crates/ms-browser-bridge/src/service.rs

Purpose:
`BridgeService`, the process-wide owner of the one bridge server: non-blocking idempotent
start, GUI-safe status, and the blocking request / reload entry points every consumer
(machine translation now, the download module later) goes through.

Key items:
- `BridgeService::shared()` — the process-wide instance.
- `ensure_started`, `status`, `request`, `request_reload`, `request_reload_peers`.

Notes:
- `SHARED` is a documented exception to global §5 ("no global mutable state"): the extension
  pairs with ONE port and ONE secret per app process (`bridge.json` in one folder), so a
  per-owner server would make owners fight over that file. Precedents: the MT limiter registry
  (`ms_tab_translation::mt_limits::registry`), `ms_download::busy::ACTIVE`.
- The slot lock is held only to read or swap the slot (and clone the server `Arc`); the start
  (file I/O + bind) runs on its own thread, requests run on the caller's worker thread.
- The first `BridgeConfig` wins while the server runs; a `Failed` start is retried by the next
  `ensure_started` with the config passed then. `SHARED` is per process only: across processes,
  the folder's owner lock (`pairing.rs`) makes a second app process read
  `Failed { OwnedByOtherProcess }` (and fail requests with it) until the owner exits.
- Tests never use `shared()`: they create their own `BridgeService` or `BridgeServer`.
*/

use std::sync::{Arc, Mutex, OnceLock};

use ms_log::runtime_log;
use serde_json::Value;

use crate::api::{BridgeConfig, BridgeRequest, BridgeStatus, ListenState, ReloadTarget, RequestOptions};
use crate::errors::BridgeError;
use crate::server::{BridgeServer, lock};

/// Where the service is in its lifecycle.
#[derive(Debug)]
enum Slot {
    Idle,
    Starting,
    Running(Arc<BridgeServer>),
    Failed(BridgeError),
}

/// The bridge of one app process. Obtain it with [`BridgeService::shared`].
#[derive(Debug)]
pub struct BridgeService {
    slot: Arc<Mutex<Slot>>,
}

static SHARED: OnceLock<BridgeService> = OnceLock::new();

impl BridgeService {
    /// A service in the `Idle` state (tests; the app uses `shared()`).
    fn new() -> Self {
        Self { slot: Arc::new(Mutex::new(Slot::Idle)) }
    }

    /// The process-wide service (see the file header for why it is global).
    #[must_use]
    pub fn shared() -> &'static Self {
        SHARED.get_or_init(Self::new)
    }

    /// Starts the server on a background thread unless it is already starting or running.
    /// Never blocks; idempotent. Watch `status()` for the outcome.
    ///
    /// # Errors
    /// `System` when the start thread cannot be spawned (the state then reads `Failed`).
    pub fn ensure_started(&self, cfg: BridgeConfig) -> Result<(), BridgeError> {
        {
            let mut slot = lock(&self.slot);
            match &*slot {
                Slot::Starting | Slot::Running(_) => return Ok(()),
                Slot::Idle | Slot::Failed(_) => *slot = Slot::Starting,
            }
        }
        let slot = Arc::clone(&self.slot);
        let spawned = std::thread::Builder::new().name("ms-bridge-start".into()).spawn(move || {
            let next = match BridgeServer::start(cfg) {
                Ok(server) => Slot::Running(Arc::new(server)),
                Err(err) => {
                    match &err {
                        // An expected state (another app window runs), not a fault.
                        BridgeError::OwnedByOtherProcess => runtime_log::log_warn(format!("[browser_bridge] server not started: {err}")),
                        _ => runtime_log::log_error(format!("[browser_bridge] server start failed: {err}")),
                    }
                    Slot::Failed(err)
                }
            };
            *lock(&slot) = next;
        });
        spawned.map(|_handle| ()).map_err(|err| {
            let error = BridgeError::System { detail: err.to_string() };
            runtime_log::log_error(format!("[browser_bridge] could not spawn the start thread: {err}"));
            *lock(&self.slot) = Slot::Failed(error.clone());
            error
        })
    }

    /// Listener state and connected peers. Brief locks only; safe on the GUI thread.
    #[must_use]
    pub fn status(&self) -> BridgeStatus {
        let listen = match &*lock(&self.slot) {
            Slot::Idle => ListenState::NotStarted,
            Slot::Starting => ListenState::Starting,
            Slot::Failed(error) => ListenState::Failed { error: error.clone() },
            Slot::Running(server) => return server.status(),
        };
        BridgeStatus { listen, peers: Vec::new() }
    }

    /// Runs `req` on the oldest ready peer announcing the module; BLOCKS until the response,
    /// the deadline, a cancel or a disconnect (cancel noticed within 250 ms). Worker threads
    /// only, never the GUI thread. Events reach `opts.on_event` on this thread, no lock held.
    ///
    /// # Errors
    /// `NotStarted` (never started), `NotConnected` (starting, or no peer), the start failure
    /// itself (e.g. `OwnedByOtherProcess`: another app process serves the extension), and every error of the request (`Outdated`, `CapabilityMissing`, `Cancelled`,
    /// `Timeout`, `AttentionTimeout`, `Disconnected`, `Protocol`, `Remote`).
    pub fn request(&self, req: &BridgeRequest<'_>, opts: &RequestOptions<'_>) -> Result<Value, BridgeError> {
        self.running()?.request(req, opts)
    }

    /// Sends `reload` to the peers `target` selects (e.g. after the app re-unpacked the
    /// extension); returns how many were addressed (0 when not running).
    #[must_use]
    pub fn request_reload(&self, target: ReloadTarget) -> usize {
        self.running().map_or(0, |server| server.request_reload(target))
    }

    /// Sends `reload` to exactly the connected peers with these `PeerInfo::id`s; returns the ids it
    /// was queued for (empty when not running; a peer that left or does not take messages is
    /// missing, so the caller can retry it later).
    #[must_use]
    pub fn request_reload_peers(&self, ids: &[u64]) -> Vec<u64> {
        self.running().map_or_else(|_| Vec::new(), |server| server.request_reload_peers(ids))
    }

    fn running(&self) -> Result<Arc<BridgeServer>, BridgeError> {
        match &*lock(&self.slot) {
            Slot::Running(server) => Ok(Arc::clone(server)),
            Slot::Idle => Err(BridgeError::NotStarted),
            Slot::Starting => Err(BridgeError::NotConnected),
            Slot::Failed(error) => Err(error.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::pairing::{bridge_file_path, read_bridge_file};
    use crate::test_support::{TempDir, test_config};

    fn wait_listening(service: &BridgeService) -> u16 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let ListenState::Listening { port } = service.status().listen {
                return port;
            }
            assert!(Instant::now() < deadline, "service did not start: {:?}", service.status());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn idle_service_reports_not_started() {
        let service = BridgeService::new();
        assert_eq!(service.status().listen, ListenState::NotStarted);
        let params = Value::Null;
        let req = BridgeRequest { module: "translate", module_version: 1, method: "translate", params: &params };
        let opts = RequestOptions { timeout: Duration::from_secs(1), attention_timeout: Duration::from_secs(1), cancel: None, on_event: None };
        assert_eq!(service.request(&req, &opts), Err(BridgeError::NotStarted));
        assert_eq!(service.request_reload(ReloadTarget::All), 0);
        assert_eq!(service.request_reload_peers(&[1]), [] as [u64; 0]);
    }

    #[test]
    fn ensure_started_is_idempotent_and_publishes_the_port() {
        let dir = TempDir::new("service-start");
        let service = BridgeService::new();
        service.ensure_started(test_config(dir.path())).unwrap();
        service.ensure_started(test_config(dir.path())).unwrap();
        let port = wait_listening(&service);
        service.ensure_started(test_config(dir.path())).unwrap();
        assert_eq!(wait_listening(&service), port);
        let file = read_bridge_file(&bridge_file_path(dir.path())).unwrap().unwrap();
        assert_eq!(file.port, port);
        let params = Value::Null;
        let req = BridgeRequest { module: "translate", module_version: 1, method: "translate", params: &params };
        let opts = RequestOptions { timeout: Duration::from_secs(1), attention_timeout: Duration::from_secs(1), cancel: None, on_event: None };
        let started = Instant::now();
        assert_eq!(service.request(&req, &opts), Err(BridgeError::NotConnected));
        assert!(started.elapsed() < Duration::from_millis(500), "a request without peers fails at once");
        if let Slot::Running(server) = &*lock(&service.slot) {
            server.shutdown();
        }
    }

    fn wait_failed(service: &BridgeService) -> BridgeError {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let ListenState::Failed { error } = service.status().listen {
                return error;
            }
            assert!(Instant::now() < deadline, "start did not fail: {:?}", service.status());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_second_service_on_the_folder_reports_the_other_owner_and_takes_over_later() {
        let dir = TempDir::new("service-owner");
        let first = BridgeService::new();
        first.ensure_started(test_config(dir.path())).unwrap();
        let port = wait_listening(&first);
        // An independent owner (what a second app process is) on the same folder.
        let second = BridgeService::new();
        second.ensure_started(test_config(dir.path())).unwrap();
        assert_eq!(wait_failed(&second), BridgeError::OwnedByOtherProcess);
        let params = Value::Null;
        let req = BridgeRequest { module: "translate", module_version: 1, method: "translate", params: &params };
        let opts = RequestOptions { timeout: Duration::from_secs(1), attention_timeout: Duration::from_secs(1), cancel: None, on_event: None };
        assert_eq!(second.request(&req, &opts), Err(BridgeError::OwnedByOtherProcess));
        assert_eq!(read_bridge_file(&bridge_file_path(dir.path())).unwrap().unwrap().port, port, "the owner's pairing stays published");

        // Once the owner is gone, the next `ensure_started` of the other one takes over.
        if let Slot::Running(server) = &*lock(&first.slot) {
            server.shutdown();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            second.ensure_started(test_config(dir.path())).unwrap();
            match second.status().listen {
                ListenState::Listening { .. } => break,
                ListenState::Failed { error } => assert_eq!(error, BridgeError::OwnedByOtherProcess),
                ListenState::Starting | ListenState::NotStarted => {}
            }
            assert!(Instant::now() < deadline, "the second service never took over");
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Slot::Running(server) = &*lock(&second.slot) {
            server.shutdown();
        }
    }

    #[test]
    fn failed_start_is_reported_and_retried() {
        let dir = TempDir::new("service-fail");
        // A regular FILE where the install folder should be: the pairing write must fail.
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let service = BridgeService::new();
        service.ensure_started(test_config(&blocker)).unwrap();
        let error = wait_failed(&service);
        assert!(matches!(error, BridgeError::PairingIo { .. }), "{error:?}");
        // The next call retries with the new config and succeeds.
        service.ensure_started(test_config(&dir.path().join("ok"))).unwrap();
        wait_listening(&service);
        if let Slot::Running(server) = &*lock(&service.slot) {
            server.shutdown();
        }
    }
}
