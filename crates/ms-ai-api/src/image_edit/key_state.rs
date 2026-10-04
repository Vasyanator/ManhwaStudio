/*
File: crates/ms-ai-api/src/image_edit/key_state.rs

Purpose:
GUI-free state of the image-edit key block (the provider's API key: verified presence, the
password buffer, the status line) and its single-flight background execution: at most ONE
credential-store operation (check, store, delete) runs at a time, so a "save" followed by a
"delete" can never reach the store in the other order, and the key block is disabled while one
is in flight.

Key structures:
- ImageEditKeyStore   : the seam to the credential store, implemented here for
                        `keys::ImageEditKeySlot` (the production slot, resolved by
                        `keys::key_slot`) and by test fakes. This file never decides where a
                        provider's key lives; it only drives one slot's operations.
- ImageEditKeyOp / ImageEditKeyRequest / ImageEditKeyEvent: one operation, its blocking
                        execution, its result (the key's presence after the operation).
- ImageEditKeyError   : why an operation failed (the store's error, or a lost worker).
- ImageEditKeyState   : the key block's state and the pure single-flight transitions
                        (`select_slot`, `begin`, `apply`).
- ImageEditKeyRunner  : runs the one in-flight request on a detached `ms_thread` worker and
                        hands its event back (`pump` = poll + apply + begin + start).

Notes:
Pure except `ImageEditKeyRequest::execute` (blocking credential-store I/O, worker threads only)
and `ImageEditKeyRunner` (spawns that worker). The plaintext key exists only in
`ImageEditKeyState::key_edit` and in a `Store` request; both redact it from `Debug`, and it is
never logged. A result that arrives for a slot that is no longer selected changes no state
(its notice is still produced: the user started that operation). Target-neutral: on wasm the
store implementations answer with their web errors.
*/

use std::fmt;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use ms_log::runtime_log;
use ms_theme::Severity;

use super::error::ImageEditError;
use super::keys::ImageEditKeySlot;
use crate::connection::AiApiNotice;
use crate::connection_view::{KeyBlockActions, KeyBlockView};
use crate::error::AiApiError;

/// Toast duration of the "key saved" notice, seconds (the chat connection's value).
const KEY_STORED_NOTICE_S: f64 = 2.2;
/// Toast duration of a failed operation notice, seconds (the chat connection's value).
const KEY_FAILED_NOTICE_S: f64 = 3.0;
/// Name of the key worker thread (diagnostics only).
const WORKER_THREAD_NAME: &str = "ai-api-image-edit-key";

/// One credential-store slot of an image-edit provider: the identity of the entry and the three
/// blocking operations on it. Implemented by the slot type that resolves a provider (and its
/// region or base URL) to a store entry; equality must mean "the same store entry".
pub trait ImageEditKeyStore: Clone + PartialEq + fmt::Debug + Send + 'static {
    /// Display name of the key's owner (provider or service brand) for notices.
    fn label(&self) -> &'static str;
    /// Reads the stored key, untrimmed; `Ok("")` when none is stored. Blocking.
    ///
    /// # Errors
    /// The store's read error.
    fn read_key(&self) -> Result<String, ImageEditError>;
    /// Stores the trimmed `key`, replacing any previous one. Blocking.
    ///
    /// # Errors
    /// `Key(AiApiError::EmptyKey)` for a blank key, otherwise the store's write error.
    fn store_key(&self, key: &str) -> Result<(), ImageEditError>;
    /// Deletes the stored key; a missing key is success. Blocking.
    ///
    /// # Errors
    /// The store's delete error.
    fn clear_key(&self) -> Result<(), ImageEditError>;
}

/// One key operation.
pub enum ImageEditKeyOp {
    /// Check whether a key is stored.
    Refresh,
    /// Store `key` (trimmed by the store).
    Store { key: String },
    /// Delete the stored key.
    Clear,
}

impl ImageEditKeyOp {
    /// The operation's kind (without the key).
    #[must_use]
    pub fn kind(&self) -> ImageEditKeyOpKind {
        match self {
            Self::Refresh => ImageEditKeyOpKind::Refresh,
            Self::Store { .. } => ImageEditKeyOpKind::Store,
            Self::Clear => ImageEditKeyOpKind::Clear,
        }
    }
}

// Manual `Debug`: the plaintext key must never reach a log or a panic message.
impl fmt::Debug for ImageEditKeyOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refresh => f.write_str("Refresh"),
            Self::Store { .. } => f.debug_struct("Store").field("key", &"<redacted>").finish(),
            Self::Clear => f.write_str("Clear"),
        }
    }
}

/// The kind of a key operation, carried by its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageEditKeyOpKind {
    Refresh,
    Store,
    Clear,
}

impl ImageEditKeyOpKind {
    /// Short kind name for log lines.
    fn log_name(self) -> &'static str {
        match self {
            Self::Refresh => "check",
            Self::Store => "store",
            Self::Clear => "delete",
        }
    }
}

/// Why a key operation failed. `Display` is the localized user message.
#[derive(Debug, Clone)]
pub enum ImageEditKeyError {
    /// The credential store (or the worker spawn) reported an error.
    Store(ImageEditError),
    /// The worker ended without answering (it panicked); the operation's outcome is unknown.
    WorkerLost,
}

impl fmt::Display for ImageEditKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => fmt::Display::fmt(error, f),
            Self::WorkerLost => f.write_str(&tf!("ai_api.image_edit.key.worker_lost_error", button = t!("ai_api.connection.refresh_button"))),
        }
    }
}

/// One operation on one slot, ready for a worker thread.
#[derive(Debug)]
pub struct ImageEditKeyRequest<S> {
    pub slot: S,
    pub op: ImageEditKeyOp,
}

impl<S: ImageEditKeyStore> ImageEditKeyRequest<S> {
    /// Runs the operation on the calling thread (blocking credential-store I/O; worker threads
    /// only) and returns its event: on success, whether a key is stored afterwards.
    #[must_use]
    pub fn execute(self) -> ImageEditKeyEvent<S> {
        let kind = self.op.kind();
        let result = match &self.op {
            ImageEditKeyOp::Refresh => self.slot.read_key().map(|key| !key.trim().is_empty()),
            ImageEditKeyOp::Store { key } => self.slot.store_key(key).map(|()| true),
            ImageEditKeyOp::Clear => self.slot.clear_key().map(|()| false),
        };
        ImageEditKeyEvent { slot: self.slot, kind, result: result.map_err(ImageEditKeyError::Store) }
    }
}

/// The result of one `ImageEditKeyRequest`: `Ok(stored)` is whether a key is stored after the
/// operation.
#[derive(Debug)]
pub struct ImageEditKeyEvent<S> {
    pub slot: S,
    pub kind: ImageEditKeyOpKind,
    pub result: Result<bool, ImageEditKeyError>,
}

/// State of one image-edit key block. `key_edit` is the password buffer the view edits; the
/// rest changes only through `select_slot`, `begin` and `apply`. `Debug` redacts `key_edit`.
pub struct ImageEditKeyState<S> {
    /// Password-field buffer; cleared when the key is confirmed stored or deleted, or the slot
    /// changes.
    pub key_edit: String,
    /// The selected slot; `None` while the selection has no addressable slot.
    slot: Option<S>,
    /// Whether a key is stored in `slot`; `None` until a check for that slot answered.
    key_configured: Option<bool>,
    /// Localized status line; empty hides it.
    status: String,
    /// The one operation in flight (whatever slot it was started for).
    in_flight: Option<ImageEditKeyOpKind>,
    /// A key check is owed for `slot` (the slot changed); started as soon as nothing is in
    /// flight.
    refresh_wanted: bool,
}

impl<S> Default for ImageEditKeyState<S> {
    fn default() -> Self {
        Self { key_edit: String::new(), slot: None, key_configured: None, status: String::new(), in_flight: None, refresh_wanted: false }
    }
}

impl<S: ImageEditKeyStore> ImageEditKeyState<S> {
    /// Selects the slot of the current provider selection; call it whenever that selection may
    /// have changed (provider, region, committed base URL). The same slot again changes
    /// nothing. A different slot drops the old slot's key state and buffer and owes a key
    /// check. `Err` (the selection has no addressable slot, e.g. an invalid base URL) clears
    /// the slot and shows the error as the status line.
    pub fn select_slot(&mut self, slot: Result<S, ImageEditError>) {
        match slot {
            Ok(slot) => {
                if self.slot.as_ref() == Some(&slot) {
                    return;
                }
                self.slot = Some(slot);
                self.status.clear();
                self.refresh_wanted = true;
            }
            Err(error) => {
                self.slot = None;
                self.status = error.to_string();
                self.refresh_wanted = false;
            }
        }
        self.key_edit.clear();
        self.key_configured = None;
    }

    /// Turns this frame's key-block actions (and an owed check) into at most ONE request, in
    /// the priority save, delete, check, and marks it in flight. Returns `None` while an
    /// operation is in flight (the view disables the block meanwhile; an owed check waits),
    /// without a slot, for nothing to do, or for a blank key to save (the status says so).
    #[must_use]
    pub fn begin(&mut self, actions: KeyBlockActions) -> Option<ImageEditKeyRequest<S>> {
        if self.in_flight.is_some() {
            return None;
        }
        let slot = self.slot.clone()?;
        let op = if actions.save_key {
            if self.key_edit.trim().is_empty() {
                self.status = AiApiError::EmptyKey.to_string();
                return None;
            }
            self.status = t!("ai_api.connection.saving_key_status").to_string();
            ImageEditKeyOp::Store { key: self.key_edit.clone() }
        } else if actions.clear_key {
            self.status = t!("ai_api.connection.deleting_key_status").to_string();
            ImageEditKeyOp::Clear
        } else if actions.refresh || self.refresh_wanted {
            self.status = t!("ai_api.image_edit.key.checking_status").to_string();
            ImageEditKeyOp::Refresh
        } else {
            return None;
        };
        // Any answer for this slot settles the owed check.
        self.refresh_wanted = false;
        self.in_flight = Some(op.kind());
        Some(ImageEditKeyRequest { slot, op })
    }

    /// Applies a finished operation: ends the in-flight operation and, when the event's slot is
    /// still the selected one, updates the key state and status. Returns the toast to show
    /// ("key saved", or a failure), produced even for a slot no longer selected.
    pub fn apply(&mut self, event: ImageEditKeyEvent<S>) -> Option<AiApiNotice> {
        self.in_flight = None;
        let current = self.slot.as_ref() == Some(&event.slot);
        let label = event.slot.label();
        match event.result {
            Ok(stored) => {
                let text = match event.kind {
                    ImageEditKeyOpKind::Refresh => String::new(),
                    ImageEditKeyOpKind::Store => tf!("ai_api.connection.api_key_saved_status", service = label),
                    ImageEditKeyOpKind::Clear => tf!("ai_api.connection.api_key_deleted_status", service = label),
                };
                if current {
                    self.key_configured = Some(stored);
                    if event.kind != ImageEditKeyOpKind::Refresh {
                        self.key_edit.clear();
                    }
                    self.status.clone_from(&text);
                }
                (event.kind == ImageEditKeyOpKind::Store).then_some(AiApiNotice { text, severity: Severity::Success, duration_s: KEY_STORED_NOTICE_S })
            }
            Err(error) => {
                if current {
                    self.status = error.to_string();
                }
                Some(AiApiNotice { text: tf!("ai_api.connection.request_failed_error", error = error), severity: Severity::Error, duration_s: KEY_FAILED_NOTICE_S })
            }
        }
    }

    /// The selected slot.
    #[must_use]
    pub fn slot(&self) -> Option<&S> {
        self.slot.as_ref()
    }

    /// Whether a key is stored in the selected slot; `None` until a check answered.
    #[must_use]
    pub fn key_configured(&self) -> Option<bool> {
        self.key_configured
    }

    /// Whether an operation is in flight (the key block is then disabled).
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.in_flight.is_some()
    }

    /// The localized status line; empty when there is nothing to say.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// What `draw_key_block` shows for this state; `key_required` is the provider's.
    #[must_use]
    pub fn block_view(&self, key_required: bool) -> KeyBlockView {
        KeyBlockView { key_configured: self.key_configured, key_required }
    }
}

// Manual `Debug`: `key_edit` holds a plaintext API key while the user types it.
impl<S: fmt::Debug> fmt::Debug for ImageEditKeyState<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key_edit = if self.key_edit.is_empty() { "" } else { "<redacted>" };
        f.debug_struct("ImageEditKeyState")
            .field("key_edit", &key_edit)
            .field("slot", &self.slot)
            .field("key_configured", &self.key_configured)
            .field("status", &self.status)
            .field("in_flight", &self.in_flight)
            .field("refresh_wanted", &self.refresh_wanted)
            .finish()
    }
}

// The production slot: a provider's key resolved by `keys::key_slot`. The operations delegate
// to `ImageEditKeySlot::{read, store, clear}`, the one dispatch onto `crate::keys`.
impl ImageEditKeyStore for ImageEditKeySlot {
    fn label(&self) -> &'static str {
        match self {
            Self::Chat(target) => target.service().label(),
            Self::Named { label, .. } => label,
        }
    }

    fn read_key(&self) -> Result<String, ImageEditError> {
        self.read()
    }

    fn store_key(&self, key: &str) -> Result<(), ImageEditError> {
        self.store(key)
    }

    fn clear_key(&self) -> Result<(), ImageEditError> {
        self.clear()
    }
}

/// The request a runner's worker is executing: its event channel plus what is needed to report
/// a lost worker.
#[derive(Debug)]
struct Pending<S> {
    events: Receiver<ImageEditKeyEvent<S>>,
    slot: S,
    kind: ImageEditKeyOpKind,
}

/// Runs an `ImageEditKeyState`'s requests, one at a time, each on its own detached `ms_thread`
/// worker, and hands the results back. Owned and polled by the GUI thread; the state's
/// single-flight rule guarantees at most one worker per runner.
#[derive(Debug)]
pub struct ImageEditKeyRunner<S> {
    pending: Option<Pending<S>>,
}

impl<S> Default for ImageEditKeyRunner<S> {
    fn default() -> Self {
        Self { pending: None }
    }
}

impl<S: ImageEditKeyStore> ImageEditKeyRunner<S> {
    /// One frame of the key block's background work: applies a finished operation, then starts
    /// the next request `state.begin(actions)` yields. Returns the toasts to show, in order.
    /// Never blocks.
    pub fn pump(&mut self, state: &mut ImageEditKeyState<S>, actions: KeyBlockActions) -> Vec<AiApiNotice> {
        let mut notices = Vec::new();
        if let Some(event) = self.poll() {
            notices.extend(state.apply(event));
        }
        // A worker still running means its answer is not applied yet: nothing new may start,
        // even if the state was made idle behind the runner's back (`apply` called directly).
        if self.pending.is_some() {
            return notices;
        }
        if let Some(request) = state.begin(actions)
            && let Err(event) = self.start(request)
        {
            notices.extend(state.apply(event));
        }
        notices
    }

    /// Whether a worker is executing a request.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.pending.is_some()
    }

    /// The finished request's event, if any. A worker that ended without answering (it
    /// panicked) is logged and reported as `ImageEditKeyError::WorkerLost`.
    fn poll(&mut self) -> Option<ImageEditKeyEvent<S>> {
        let pending = self.pending.as_ref()?;
        let event = match pending.events.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                runtime_log::log_error(format!("[AI API] the image-edit key worker for {} ({}) stopped without answering", pending.slot.label(), pending.kind.log_name()));
                ImageEditKeyEvent { slot: pending.slot.clone(), kind: pending.kind, result: Err(ImageEditKeyError::WorkerLost) }
            }
        };
        self.pending = None;
        Some(event)
    }

    /// Starts `request` on a new worker; only called by `pump` while no worker runs. A failed
    /// spawn is logged and returned as the request's failure event.
    fn start(&mut self, request: ImageEditKeyRequest<S>) -> Result<(), ImageEditKeyEvent<S>> {
        let slot = request.slot.clone();
        let kind = request.op.kind();
        let label = slot.label();
        let (events_tx, events_rx) = channel();
        // Detached worker: the GUI thread never joins it; a result that arrives after the
        // runner was dropped has nobody to show it to and is discarded with a warning.
        let spawned = ms_thread::Builder::new().name(WORKER_THREAD_NAME.to_string()).spawn(move || {
            if events_tx.send(request.execute()).is_err() {
                runtime_log::log_warn(format!("[AI API] image-edit key {} for {label} finished after its runner was dropped; result discarded", kind.log_name()));
            }
        });
        match spawned {
            Ok(_detached) => {
                self.pending = Some(Pending { events: events_rx, slot, kind });
                Ok(())
            }
            Err(err) => {
                runtime_log::log_error(format!("[AI API] failed to spawn the {WORKER_THREAD_NAME} thread for the {} of {label}: {err}", kind.log_name()));
                Err(ImageEditKeyEvent { slot, kind, result: Err(ImageEditKeyError::Store(ImageEditError::Key(AiApiError::TaskSpawn { detail: err.to_string() }))) })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use ms_theme::Severity;

    use super::{ImageEditKeyError, ImageEditKeyOp, ImageEditKeyOpKind, ImageEditKeyRunner, ImageEditKeyState, ImageEditKeyStore};
    use crate::connection::AiApiNotice;
    use crate::connection_view::KeyBlockActions;
    use crate::error::AiApiError;
    use crate::image_edit::error::ImageEditError;

    /// A credential-store slot without a store: `stored` is what a read returns, `fail` makes
    /// every operation fail, `panic_on_read` unwinds the worker. No keyring access.
    #[derive(Debug, Clone, PartialEq)]
    struct FakeSlot {
        id: u8,
        stored: &'static str,
        fail: bool,
        panic_on_read: bool,
    }

    impl ImageEditKeyStore for FakeSlot {
        fn label(&self) -> &'static str {
            "Fake"
        }
        fn read_key(&self) -> Result<String, ImageEditError> {
            assert!(!self.panic_on_read, "test slot: simulated worker crash");
            if self.fail { Err(failure()) } else { Ok(self.stored.to_string()) }
        }
        fn store_key(&self, key: &str) -> Result<(), ImageEditError> {
            if self.fail {
                Err(failure())
            } else if key.trim().is_empty() {
                Err(ImageEditError::Key(AiApiError::EmptyKey))
            } else {
                Ok(())
            }
        }
        fn clear_key(&self) -> Result<(), ImageEditError> {
            if self.fail { Err(failure()) } else { Ok(()) }
        }
    }

    fn failure() -> ImageEditError {
        ImageEditError::Key(AiApiError::KeyringUnavailable { detail: "test store".to_string() })
    }

    fn slot(id: u8) -> FakeSlot {
        FakeSlot { id, stored: "sk-stored", fail: false, panic_on_read: false }
    }

    fn pressed(save_key: bool, clear_key: bool, refresh: bool) -> KeyBlockActions {
        KeyBlockActions { refresh, save_key, clear_key }
    }

    const NONE: KeyBlockActions = KeyBlockActions { refresh: false, save_key: false, clear_key: false };

    fn kind_of(request: Option<&super::ImageEditKeyRequest<FakeSlot>>) -> Option<ImageEditKeyOpKind> {
        request.map(|request| request.op.kind())
    }

    #[test]
    fn a_new_slot_owes_one_check_and_single_flight_blocks_everything_else() {
        let mut state = ImageEditKeyState::default();
        assert!(state.begin(pressed(true, true, true)).is_none(), "no slot, nothing to address");
        state.select_slot(Ok(slot(1)));
        let check = state.begin(NONE);
        assert_eq!(kind_of(check.as_ref()), Some(ImageEditKeyOpKind::Refresh));
        assert!(state.is_busy());
        state.key_edit = "sk-new".to_string();
        assert!(state.begin(pressed(true, true, true)).is_none(), "one operation at a time");
        let check = check.unwrap_or_else(|| panic!("check request"));
        assert!(state.apply(check.execute()).is_none(), "a check produces no toast");
        assert!(!state.is_busy());
        assert_eq!(state.key_configured(), Some(true));
        assert_eq!(state.key_edit, "sk-new", "a check keeps the typed buffer");
        assert!(state.begin(NONE).is_none(), "the owed check was settled");
    }

    #[test]
    fn save_then_delete_runs_in_that_order_one_at_a_time() {
        let mut state = ImageEditKeyState::default();
        state.select_slot(Ok(slot(1)));
        state.key_edit = " sk-new ".to_string();
        // Save outranks delete and check in one frame.
        let save = state.begin(pressed(true, true, true)).unwrap_or_else(|| panic!("save request"));
        assert!(matches!(&save.op, ImageEditKeyOp::Store { key } if key == " sk-new "));
        assert!(state.begin(pressed(false, true, false)).is_none(), "the delete waits for the save");
        let notice = state.apply(save.execute()).unwrap_or_else(|| panic!("saved toast"));
        assert_eq!(notice.severity, Severity::Success);
        assert_eq!(state.key_configured(), Some(true));
        assert!(state.key_edit.is_empty(), "a stored key leaves the buffer");
        let delete = state.begin(pressed(false, true, false)).unwrap_or_else(|| panic!("delete request"));
        assert_eq!(delete.op.kind(), ImageEditKeyOpKind::Clear);
        assert!(state.apply(delete.execute()).is_none());
        assert_eq!(state.key_configured(), Some(false));
    }

    #[test]
    fn a_blank_key_is_refused_before_any_request() {
        let mut state = ImageEditKeyState::default();
        state.select_slot(Ok(slot(1)));
        state.key_edit = "   ".to_string();
        assert!(state.begin(pressed(true, false, false)).is_none());
        assert!(!state.is_busy());
        assert_eq!(state.status(), AiApiError::EmptyKey.to_string());
    }

    #[test]
    fn a_result_for_an_old_slot_frees_the_flight_but_changes_no_state() {
        let mut state = ImageEditKeyState::default();
        state.select_slot(Ok(slot(1)));
        state.key_edit = "sk-1".to_string();
        let save = state.begin(pressed(true, false, false)).unwrap_or_else(|| panic!("save request"));
        state.select_slot(Ok(slot(2)));
        assert!(state.key_edit.is_empty() && state.key_configured().is_none());
        assert!(state.begin(NONE).is_none(), "the new slot's check waits for the old operation");
        let notice = state.apply(save.execute());
        assert_eq!(notice.map(|notice| notice.severity), Some(Severity::Success), "the user still learns the old save landed");
        assert_eq!(state.key_configured(), None, "the old slot's answer is not the new slot's");
        let check = state.begin(NONE).unwrap_or_else(|| panic!("owed check"));
        assert_eq!(check.slot.id, 2);
        assert_eq!(check.op.kind(), ImageEditKeyOpKind::Refresh);
    }

    #[test]
    fn reselecting_the_same_slot_keeps_everything() {
        let mut state = ImageEditKeyState::default();
        state.select_slot(Ok(slot(1)));
        let check = state.begin(NONE).unwrap_or_else(|| panic!("check request"));
        assert!(state.apply(check.execute()).is_none());
        state.key_edit = "sk-typing".to_string();
        state.select_slot(Ok(slot(1)));
        assert_eq!(state.key_edit, "sk-typing");
        assert_eq!(state.key_configured(), Some(true));
        assert!(state.begin(NONE).is_none(), "no new check owed");
    }

    #[test]
    fn an_unaddressable_selection_shows_its_error_and_starts_nothing() {
        let mut state: ImageEditKeyState<FakeSlot> = ImageEditKeyState::default();
        let error = ImageEditError::InvalidEndpoint { detail: "nope".to_string() };
        state.select_slot(Err(error.clone()));
        assert_eq!(state.status(), error.to_string());
        assert!(state.slot().is_none());
        assert!(state.begin(pressed(true, true, true)).is_none());
    }

    #[test]
    fn a_failure_reports_the_store_error_and_keeps_the_known_state() {
        let mut state = ImageEditKeyState::default();
        state.select_slot(Ok(FakeSlot { fail: true, ..slot(1) }));
        let check = state.begin(NONE).unwrap_or_else(|| panic!("check request"));
        let notice = state.apply(check.execute()).unwrap_or_else(|| panic!("failure toast"));
        assert_eq!(notice.severity, Severity::Error);
        assert_eq!(state.key_configured(), None);
        assert_eq!(state.status(), failure().to_string());
        assert!(!state.is_busy());
    }

    #[test]
    fn a_check_reads_a_blank_stored_key_as_missing() {
        let event = super::ImageEditKeyRequest { slot: FakeSlot { stored: "  ", ..slot(1) }, op: ImageEditKeyOp::Refresh }.execute();
        assert!(matches!(event.result, Ok(false)));
    }

    #[test]
    fn debug_never_shows_the_key() {
        let mut state = ImageEditKeyState::default();
        state.select_slot(Ok(slot(1)));
        state.key_edit = "sk-secret-123".to_string();
        let request = super::ImageEditKeyRequest { slot: slot(1), op: ImageEditKeyOp::Store { key: "sk-secret-123".to_string() } };
        for debug in [format!("{state:?}"), format!("{request:?}")] {
            assert!(!debug.contains("sk-secret-123"), "{debug}");
        }
    }

    /// Pumps `runner` until it is idle (a 10 s deadline), collecting the toasts.
    fn pump_until_idle(runner: &mut ImageEditKeyRunner<FakeSlot>, state: &mut ImageEditKeyState<FakeSlot>, first: KeyBlockActions) -> Vec<AiApiNotice> {
        let mut notices = runner.pump(state, first);
        let deadline = Instant::now() + Duration::from_secs(10);
        while (runner.is_running() || state.is_busy()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            notices.extend(runner.pump(state, NONE));
        }
        notices
    }

    #[test]
    fn the_runner_executes_on_a_worker_and_applies_the_answer() {
        let mut runner = ImageEditKeyRunner::default();
        let mut state = ImageEditKeyState::default();
        state.select_slot(Ok(slot(1)));
        let toasts = pump_until_idle(&mut runner, &mut state, NONE);
        assert!(toasts.is_empty());
        assert_eq!(state.key_configured(), Some(true));
        state.key_edit = "sk-new".to_string();
        let toasts = pump_until_idle(&mut runner, &mut state, pressed(true, false, false));
        assert_eq!(toasts.iter().map(|toast| toast.severity).collect::<Vec<_>>(), [Severity::Success], "the saved toast");
        assert!(state.key_edit.is_empty());
    }

    #[test]
    fn a_crashed_worker_is_reported_as_lost_and_frees_the_flight() {
        let mut runner = ImageEditKeyRunner::default();
        let mut state = ImageEditKeyState::default();
        state.select_slot(Ok(FakeSlot { panic_on_read: true, ..slot(1) }));
        let toasts = pump_until_idle(&mut runner, &mut state, NONE);
        assert_eq!(toasts.iter().map(|toast| toast.severity).collect::<Vec<_>>(), [Severity::Error], "the failure toast");
        assert!(!state.is_busy() && !runner.is_running());
        assert_eq!(state.status(), ImageEditKeyError::WorkerLost.to_string());
    }
}
