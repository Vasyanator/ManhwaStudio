/*
File: crates/ms-ai-api/src/neurolate/account.rs

Purpose:
GUI-free state of the NeuroLate key panel: the API key block (reusing the image-edit key
machinery `ImageEditKeyState` / `ImageEditKeyRunner` over the `"neurolate:api"` slot) plus the
key's spending limit (`KeyInfo`), fetched on a single-flight background worker. A consumer owns
one `NeurolateKeyPanel` per screen, draws it with `view::draw_neurolate_key_panel` and calls
`pump` once per frame with the drawn actions.

Key structures:
- NeurolateKeyPanel  : key state + runner + limit state; generic over the key store and the
                       limit fetch so tests run without the credential store or the network.
- NeurolateKeyActions: one frame's user requests (key block buttons, "Check limit").

Notes:
Never blocks: key-store I/O runs on the key runner's worker, the limit fetch (key read +
`GET /ext/v1/key`) on a detached `ms_thread` worker whose answer is polled. The limit is fetched
when a key check or save answers "stored" and on "Check limit"; a delete clears it. Every key
change bumps an epoch, so an answer started for an older key is dropped and, when one is owed,
a fresh fetch follows. The key itself never appears here outside the key state's redacted
buffer.
*/

use std::fmt;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use ms_log::runtime_log;

use super::error::NeurolateError;
use super::protocol::KeyInfo;
use crate::connection::AiApiNotice;
use crate::connection_view::KeyBlockActions;
use crate::image_edit::{ImageEditKeyRunner, ImageEditKeySlot, ImageEditKeyState, ImageEditKeyStore};

/// Name of the limit fetch thread (diagnostics only).
const LIMIT_THREAD_NAME: &str = "neurolate-key-limit";

/// The whole limit fetch: read the key, ask `NeuroLate`. Runs on a worker thread.
pub(crate) type LimitFetch = Arc<dyn Fn() -> Result<KeyInfo, NeurolateError> + Send + Sync>;

/// User requests collected from one `draw_neurolate_key_panel` call.
#[derive(Debug, Clone, Copy, Default)]
pub struct NeurolateKeyActions {
    /// The key block's buttons (refresh / save / delete).
    pub key: KeyBlockActions,
    /// "Check limit" was clicked.
    pub check_limit: bool,
}

/// The limit fetch in flight: its answer channel and the key epoch it was started for.
struct LimitFlight {
    answer: Receiver<Result<KeyInfo, NeurolateError>>,
    epoch: u64,
}

/// The `NeuroLate` key panel's state: the key block over slot `S` and the key's limit. Owned and
/// pumped by the GUI thread.
pub struct NeurolateKeyPanel<S: ImageEditKeyStore = ImageEditKeySlot> {
    key: ImageEditKeyState<S>,
    runner: ImageEditKeyRunner<S>,
    fetch: LimitFetch,
    /// The last limit answer for the current key; `None` before the first one or after a
    /// delete.
    info: Option<Result<KeyInfo, NeurolateError>>,
    in_flight: Option<LimitFlight>,
    /// A fetch is owed; started as soon as none is in flight.
    fetch_wanted: bool,
    /// Bumped on every key change; an answer for an older epoch is dropped.
    epoch: u64,
}

impl NeurolateKeyPanel<ImageEditKeySlot> {
    /// The production panel: the `"neurolate:api"` slot and the real limit fetch. A key check
    /// is owed at once (started by the first `pump`).
    #[must_use]
    pub fn new() -> Self {
        Self::with_parts(super::key_slot(), Arc::new(|| super::read_key().and_then(|key| super::fetch_key_info(&key))))
    }
}

impl Default for NeurolateKeyPanel<ImageEditKeySlot> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: ImageEditKeyStore> NeurolateKeyPanel<S> {
    /// A panel over `slot` whose limit is fetched by `fetch` (the seam tests fake).
    #[must_use]
    pub(crate) fn with_parts(slot: S, fetch: LimitFetch) -> Self {
        let mut key = ImageEditKeyState::default();
        key.select_slot(Ok(slot));
        Self { key, runner: ImageEditKeyRunner::default(), fetch, info: None, in_flight: None, fetch_wanted: false, epoch: 0 }
    }

    /// One frame of background work: runs the key block's operation (`actions.key`), applies a
    /// finished limit fetch and starts the next one when owed. Returns the toasts to show, in
    /// order. Never blocks.
    pub fn pump(&mut self, actions: NeurolateKeyActions) -> Vec<AiApiNotice> {
        let was_busy = self.key.is_busy();
        let notices = self.runner.pump(&mut self.key, actions.key);
        // An operation finished this frame. No other one can have started in the same call:
        // the view disables the key block while busy (so `actions.key` is empty), and the only
        // owed check is the one `with_parts` queues before anything runs.
        if was_busy && !self.key.is_busy() {
            match self.key.key_configured() {
                Some(true) => {
                    self.epoch = self.epoch.wrapping_add(1);
                    self.fetch_wanted = true;
                }
                Some(false) => {
                    self.epoch = self.epoch.wrapping_add(1);
                    self.fetch_wanted = false;
                    self.info = None;
                }
                None => {}
            }
        }
        if actions.check_limit {
            self.fetch_wanted = true;
        }
        self.poll_limit();
        if self.fetch_wanted && self.in_flight.is_none() {
            self.start_limit_fetch();
        }
        notices
    }

    /// Applies a finished limit fetch of the current epoch; an older one is dropped (and a
    /// fresh fetch is owed by then).
    fn poll_limit(&mut self) {
        let Some(flight) = &self.in_flight else { return };
        let answer = match flight.answer.try_recv() {
            Ok(answer) => answer,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                runtime_log::log_error(format!("[NeuroLate] the {LIMIT_THREAD_NAME} thread ended without answering"));
                Err(NeurolateError::WorkerLost)
            }
        };
        if flight.epoch == self.epoch {
            self.info = Some(answer);
        }
        self.in_flight = None;
    }

    /// Starts the limit fetch on a detached worker. A failed spawn is logged and shown as the
    /// limit answer.
    fn start_limit_fetch(&mut self) {
        self.fetch_wanted = false;
        let fetch = Arc::clone(&self.fetch);
        let (answer_tx, answer_rx) = channel();
        // Detached worker: the GUI thread never joins it; an answer that arrives after the
        // panel was dropped is discarded.
        let spawned = ms_thread::Builder::new().name(LIMIT_THREAD_NAME.to_string()).spawn(move || {
            if answer_tx.send(fetch()).is_err() {
                runtime_log::log_info("[NeuroLate] key limit answer arrived after its panel was dropped; result discarded");
            }
        });
        match spawned {
            Ok(_detached) => self.in_flight = Some(LimitFlight { answer: answer_rx, epoch: self.epoch }),
            Err(error) => {
                runtime_log::log_error(format!("[NeuroLate] failed to spawn the {LIMIT_THREAD_NAME} thread: {error}"));
                self.info = Some(Err(NeurolateError::TaskSpawn { detail: error.to_string() }));
            }
        }
    }

    /// Whether a key is stored; `None` until the first check answered.
    #[must_use]
    pub fn key_configured(&self) -> Option<bool> {
        self.key.key_configured()
    }

    /// The key block's state (status line, buffer, busy flag).
    #[must_use]
    pub fn key_state(&self) -> &ImageEditKeyState<S> {
        &self.key
    }

    /// The key block's state, mutable for the view's password buffer.
    pub fn key_state_mut(&mut self) -> &mut ImageEditKeyState<S> {
        &mut self.key
    }

    /// The last limit answer for the current key, if any.
    #[must_use]
    pub fn limit(&self) -> Option<&Result<KeyInfo, NeurolateError>> {
        self.info.as_ref()
    }

    /// Whether a limit fetch is running or owed.
    #[must_use]
    pub fn is_checking_limit(&self) -> bool {
        self.in_flight.is_some() || self.fetch_wanted
    }
}

// Manual `Debug`: the fetch closure has none; the key state redacts its buffer itself.
impl<S: ImageEditKeyStore> fmt::Debug for NeurolateKeyPanel<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NeurolateKeyPanel")
            .field("key", &self.key)
            .field("info", &self.info)
            .field("limit_in_flight", &self.in_flight.as_ref().map(|flight| flight.epoch))
            .field("fetch_wanted", &self.fetch_wanted)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::{LimitFetch, NeurolateKeyActions, NeurolateKeyPanel};
    use crate::connection_view::KeyBlockActions;
    use crate::image_edit::{ImageEditError, ImageEditKeyStore};
    use crate::neurolate::error::NeurolateError;
    use crate::neurolate::protocol::{KeyInfo, KeyStatus, LimitPeriod};

    /// A credential-store slot without a store: `stored` is what a read returns; a store sets
    /// it, a clear empties it. No keyring access.
    #[derive(Debug, Clone)]
    struct FakeSlot {
        stored: Arc<Mutex<String>>,
    }

    impl PartialEq for FakeSlot {
        fn eq(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self.stored, &other.stored)
        }
    }

    impl ImageEditKeyStore for FakeSlot {
        fn label(&self) -> &'static str {
            "NeuroLate"
        }
        fn read_key(&self) -> Result<String, ImageEditError> {
            Ok(self.stored.lock().map(|key| key.clone()).unwrap_or_default())
        }
        fn store_key(&self, key: &str) -> Result<(), ImageEditError> {
            if let Ok(mut stored) = self.stored.lock() {
                *stored = key.trim().to_string();
            }
            Ok(())
        }
        fn clear_key(&self) -> Result<(), ImageEditError> {
            if let Ok(mut stored) = self.stored.lock() {
                stored.clear();
            }
            Ok(())
        }
    }

    fn info(remaining: f64) -> KeyInfo {
        KeyInfo { name: "test".to_string(), key_prefix: "nlk_TEST0000".to_string(), limit_value: 10.0, limit_period: LimitPeriod::Week, used_value: 10.0 - remaining, remaining_value: remaining, resets_on: None, expires_at: None, status: KeyStatus::Active }
    }

    /// A panel over a fake slot holding `stored`, whose fetch counts its calls and answers
    /// `info(remaining)` with the call number as the remaining value.
    fn panel(stored: &str) -> (NeurolateKeyPanel<FakeSlot>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let fetch: LimitFetch = Arc::new(move || {
            let call = counter.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(info(f64::from(u32::try_from(call).unwrap_or(u32::MAX))))
        });
        (NeurolateKeyPanel::with_parts(FakeSlot { stored: Arc::new(Mutex::new(stored.to_string())) }, fetch), calls)
    }

    const NONE: NeurolateKeyActions = NeurolateKeyActions { key: KeyBlockActions { refresh: false, save_key: false, clear_key: false }, check_limit: false };

    /// Pumps `panel` until the key and the limit are idle (a 10 s deadline).
    fn pump_until_idle(panel: &mut NeurolateKeyPanel<FakeSlot>, first: NeurolateKeyActions) {
        panel.pump(first);
        let deadline = Instant::now() + Duration::from_secs(10);
        while (panel.key_state().is_busy() || panel.is_checking_limit()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            panel.pump(NONE);
        }
        assert!(!panel.key_state().is_busy() && !panel.is_checking_limit(), "the panel became idle");
    }

    fn remaining(panel: &NeurolateKeyPanel<FakeSlot>) -> Option<f64> {
        match panel.limit() {
            Some(Ok(info)) => Some(info.remaining_value),
            Some(Err(_)) | None => None,
        }
    }

    #[test]
    fn a_stored_key_fetches_the_limit_once_after_the_first_check() {
        let (mut panel, calls) = panel("nlk_stored");
        pump_until_idle(&mut panel, NONE);
        assert_eq!(panel.key_configured(), Some(true));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(remaining(&panel), Some(1.0));
        pump_until_idle(&mut panel, NONE);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "nothing new to fetch");
    }

    #[test]
    fn no_key_means_no_fetch_until_a_key_is_saved() {
        let (mut panel, calls) = panel("");
        pump_until_idle(&mut panel, NONE);
        assert_eq!((panel.key_configured(), calls.load(Ordering::SeqCst)), (Some(false), 0));
        panel.key_state_mut().key_edit = "nlk_new".to_string();
        pump_until_idle(&mut panel, NeurolateKeyActions { key: KeyBlockActions { save_key: true, ..NONE.key }, ..NONE });
        assert_eq!(panel.key_configured(), Some(true));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the save answered stored: one fetch");
    }

    #[test]
    fn check_limit_is_single_flight_and_delete_clears_the_limit() {
        // A fetch that blocks until the test releases it, so "in flight" is deterministic.
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let fetch: LimitFetch = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            gate.lock().map_err(|_| NeurolateError::WorkerLost)?.recv().map_err(|_| NeurolateError::WorkerLost)?;
            Ok(info(1.0))
        });
        let mut panel = NeurolateKeyPanel::with_parts(FakeSlot { stored: Arc::new(Mutex::new("nlk_stored".to_string())) }, fetch);
        let open = |count: usize| (0..count).for_each(|_| release.send(()).unwrap_or_else(|error| panic!("release: {error}")));
        open(1);
        pump_until_idle(&mut panel, NONE);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Three clicks while one fetch is blocked coalesce into exactly one more fetch.
        for _ in 0..3 {
            panel.pump(NeurolateKeyActions { check_limit: true, ..NONE });
        }
        assert!(panel.is_checking_limit());
        open(2);
        pump_until_idle(&mut panel, NONE);
        assert_eq!(calls.load(Ordering::SeqCst), 3, "the blocked fetch plus one owed fetch");
        pump_until_idle(&mut panel, NeurolateKeyActions { key: KeyBlockActions { clear_key: true, ..NONE.key }, ..NONE });
        assert_eq!(panel.key_configured(), Some(false));
        assert!(panel.limit().is_none(), "the deleted key's limit is gone");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_failed_fetch_is_shown_as_the_limit_answer() {
        let fetch: LimitFetch = Arc::new(|| Err(NeurolateError::KeyRejected));
        let mut panel = NeurolateKeyPanel::with_parts(FakeSlot { stored: Arc::new(Mutex::new("nlk_bad".to_string())) }, fetch);
        pump_until_idle(&mut panel, NONE);
        assert!(matches!(panel.limit(), Some(Err(NeurolateError::KeyRejected))));
    }

    #[test]
    fn debug_never_shows_the_typed_key() {
        let (mut panel, _calls) = panel("");
        panel.key_state_mut().key_edit = "nlk_secret_123".to_string();
        assert!(!format!("{panel:?}").contains("nlk_secret_123"));
    }
}
