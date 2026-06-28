/*
File: backend_ipc/client.rs

Purpose:
The framed, multiplexed v2 client for the Rust <-> Python AI-backend IPC. Owns one
connected `BackendStream` plus a background reader thread that decodes frames
(`frame::read_frame`) and demultiplexes them by correlation `id`:

- `response`/`error`/`progress` frames are routed to the caller registered for
  that `id` (a `Mutex<HashMap<u64, Sender<RouterMsg>>>`);
- `event{id:0}` frames are fanned out to per-topic subscribers.

The handshake (`connect`) sends `hello{v}` and awaits the server `hello`, failing
fast on a protocol-version mismatch. Requests are issued via `call` (blocking
request/response) or `call_streaming` (same, with a per-`progress` callback), and
abandoned via `cancel{id}`.

Reconnect: if the reader thread sees EOF or a transport error it marks the
connection dead and fails every pending caller with a transport error; the next
`call` transparently reconnects (re-running the hello handshake), with fail-fast
"backend not running" semantics.

A process-wide lazy singleton (`shared_client`) hands out a cloneable
`BackendClient` so every subsystem shares one connection instead of a
connect-per-call path.
*/

// Not every public entry point is exercised by every call site, so some are
// intentionally unused.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use super::frame::{Frame, read_frame, write_frame};
use super::protocol;
use super::transport::{BackendStream, backend_socket_path, connect_path};

/// Default connect timeout for the v2 socket, matching the fail-fast intent of
/// the legacy backend connect paths.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Outcome of a single `call` / `call_streaming` request.
///
/// `Ok`/`Error` map a terminal `response` (`status` `ok`/`error`); `Interrupted`
/// maps `status:"interrupted"` (a cancel outcome) so callers can distinguish a
/// user-cancelled request from a real failure; `Transport` is a framing/EOF/
/// connection failure that is not tied to the method result.
#[derive(Debug)]
pub enum CallError {
    /// The method ran and reported `status:"error"` with this message.
    Error(String),
    /// The request was cancelled and terminated with `status:"interrupted"`.
    Interrupted(String),
    /// A transport-level failure (EOF, framing error, connection dead, timeout).
    Transport(String),
}

impl CallError {
    /// Returns `true` when this is a cancel (`interrupted`) outcome.
    #[must_use]
    pub fn is_interrupted(&self) -> bool {
        matches!(self, CallError::Interrupted(_))
    }

    /// Returns `true` when this is a transport-level failure.
    #[must_use]
    pub fn is_transport(&self) -> bool {
        matches!(self, CallError::Transport(_))
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Error(msg) => write!(f, "{msg}"),
            CallError::Interrupted(msg) => write!(f, "Запрос прерван: {msg}"),
            CallError::Transport(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for CallError {}

/// A message the reader thread routes to a waiting caller, keyed by request `id`.
enum RouterMsg {
    /// An interim `progress` frame: `(header, optional preview blob)`.
    Progress(Value, Vec<u8>),
    /// The terminal `response`/`error` frame: `(header, response blob)`.
    Terminal(Frame),
    /// The connection died before a terminal frame arrived.
    Transport(String),
}

/// Shared, reference-counted client state. Cloning a `BackendClient` clones this
/// `Arc`, so the reader thread and every caller share one connection.
struct Shared {
    /// AF_UNIX path of the frame socket (the base backend socket path).
    socket_path: std::path::PathBuf,
    /// Write half of the connection. `None` until first connect / after death.
    write_half: Mutex<Option<BackendStream>>,
    /// Monotonic correlation-id source. Starts at 1 (0 is reserved for events).
    next_id: AtomicU64,
    /// Pending callers, keyed by request id.
    pending: Mutex<HashMap<u64, Sender<RouterMsg>>>,
    /// Event subscribers, keyed by topic.
    subscribers: Mutex<HashMap<String, Vec<Sender<Value>>>>,
    /// `false` once the reader thread observes EOF/error; cleared on reconnect.
    alive: AtomicBool,
    /// Serializes the reconnect path in `ensure_connected()` so N caller threads
    /// that simultaneously observe a dead connection do not each call `establish()`
    /// (which would open N sockets and spawn N reader threads). The first thread to
    /// take this lock reconnects; the rest double-check `alive` inside the lock and
    /// return early.
    reconnect_lock: Mutex<()>,
    /// Backend version reported by the most recent `hello` reply.
    backend_version: Mutex<Option<String>>,
    /// Generation counter bumped on each successful connect, so a stale reader
    /// thread from a previous connection knows to exit without clobbering state.
    generation: AtomicU64,
    /// A clone of the read-side socket kept solely so the connection can be shut
    /// down on drop. When the last `BackendClient` is dropped this `Shared` drops,
    /// `shutdown()` is called here, and the reader thread (blocked in `read_frame`
    /// on its own clone of the same socket) unblocks with EOF and exits.
    shutdown_handle: Mutex<Option<BackendStream>>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Bump the generation so any live reader treats itself as stale, then shut
        // the socket down to unblock its pending `read_frame`. Best-effort.
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(handle) = self.shutdown_handle.lock().unwrap().take() {
            let _ = handle.shutdown();
        }
    }
}

/// The framed v2 IPC client. Cheap to clone (shares one connection + reader
/// thread). See the module docs for the routing/reconnect model.
#[derive(Clone)]
pub struct BackendClient {
    shared: Arc<Shared>,
}

impl BackendClient {
    /// Connects to the frame socket ([`backend_socket_path`]) and performs the
    /// `hello` handshake.
    ///
    /// Sends `hello{v}`, awaits the server `hello{v, backend_version}`, and errors
    /// on a protocol-version mismatch (a clean, fatal handshake failure — no
    /// requests are sent). On success a background reader thread is spawned.
    ///
    /// # Errors
    /// Returns a human-readable error string on connect failure (backend not
    /// running), handshake I/O failure, or version mismatch.
    pub fn connect() -> Result<Self, String> {
        let shared = Arc::new(Shared {
            socket_path: backend_socket_path(),
            write_half: Mutex::new(None),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            subscribers: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(false),
            reconnect_lock: Mutex::new(()),
            backend_version: Mutex::new(None),
            generation: AtomicU64::new(0),
            shutdown_handle: Mutex::new(None),
        });
        let client = BackendClient { shared };
        client.establish()?;
        Ok(client)
    }

    /// (Re)establishes the connection: connect + hello handshake + reader thread.
    ///
    /// Used by `connect()` and by `ensure_connected()` on the reconnect path.
    fn establish(&self) -> Result<(), String> {
        // Connect with a fail-fast timeout. No read timeout on the connection: the
        // reader thread blocks on frames; writes get a modest timeout.
        let stream = connect_path(
            self.shared.socket_path.clone(),
            DEFAULT_CONNECT_TIMEOUT,
            None,
            Some(Duration::from_secs(30)),
        )?;

        // The reader owns its own clone of the OS socket; the write half stays on
        // the client behind a mutex. A third clone is parked as the shutdown handle
        // so dropping the client can unblock the reader's `read_frame`.
        let mut read_half = stream.try_clone()?;
        let shutdown_handle = stream.try_clone()?;
        let write_half = stream;

        // Handshake on the write half, read the reply on the read half. The reader
        // thread is not running yet, so we read the hello reply inline here.
        {
            let mut wh = write_half;
            write_frame(&mut wh, &protocol::hello_header(), &[])
                .map_err(|err| format!("Не удалось отправить hello backend: {err}"))?;

            let hello = read_frame(&mut read_half)
                .map_err(|err| format!("Не удалось прочитать hello backend: {err}"))?;
            verify_hello(&hello.header)?;
            if let Some(ver) = hello
                .header
                .get(protocol::HEADER_BACKEND_VERSION)
                .and_then(Value::as_str)
            {
                *self.shared.backend_version.lock().unwrap() = Some(ver.to_string());
            }

            *self.shared.write_half.lock().unwrap() = Some(wh);
            *self.shared.shutdown_handle.lock().unwrap() = Some(shutdown_handle);
        }

        self.shared.alive.store(true, Ordering::SeqCst);
        let generation = self.shared.generation.fetch_add(1, Ordering::SeqCst) + 1;

        // The reader holds only a Weak so it never keeps the connection alive past
        // the last `BackendClient`; when all clients drop, the upgrade fails and the
        // reader exits (after `Shared::drop` shuts the socket down to unblock it).
        let weak = Arc::downgrade(&self.shared);
        if let Err(err) = thread::Builder::new()
            .name("backend-ipc-reader".to_string())
            .spawn(move || reader_loop(weak, read_half, generation))
        {
            // The reader thread never started, so the connection has no router. Roll
            // back the state we stored above (write half, shutdown handle, alive flag)
            // so `ensure_connected()` does not short-circuit on a half-open connection
            // and a subsequent `call` cleanly reconnects instead of hanging forever.
            self.shared.alive.store(false, Ordering::SeqCst);
            *self.shared.write_half.lock().unwrap() = None;
            *self.shared.shutdown_handle.lock().unwrap() = None;
            return Err(format!("Не удалось запустить reader-поток backend: {err}"));
        }

        crate::runtime_log::log_info(format!(
            "[backend_ipc] v2 client connected to {} (gen {generation})",
            self.shared.socket_path.display()
        ));
        Ok(())
    }

    /// Ensures the connection is live, reconnecting if the reader thread marked it
    /// dead. Called at the start of every `call`/`cancel`.
    fn ensure_connected(&self) -> Result<(), String> {
        if self.shared.alive.load(Ordering::SeqCst)
            && self.shared.write_half.lock().unwrap().is_some()
        {
            return Ok(());
        }
        // Serialize reconnects: only one thread may run `establish()` for a given
        // dead connection. Other threads that raced into here block on this lock and,
        // once they acquire it, find `alive` already restored and return early
        // instead of opening a second socket / reader thread.
        let _reconnect = self.shared.reconnect_lock.lock().unwrap();
        // DOUBLE-CHECK inside the lock: another thread may have reconnected while we
        // waited for the lock.
        if self.shared.alive.load(Ordering::SeqCst)
            && self.shared.write_half.lock().unwrap().is_some()
        {
            return Ok(());
        }
        crate::runtime_log::log_info(
            "[backend_ipc] v2 client reconnecting (previous connection dead)",
        );
        self.establish()
    }

    /// The backend version reported by the last successful `hello`, if any.
    #[must_use]
    pub fn backend_version(&self) -> Option<String> {
        self.shared.backend_version.lock().unwrap().clone()
    }

    /// Returns `true` while the connection is believed live.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }

    /// Allocates the next monotonic request id (>= 1).
    fn next_id(&self) -> u64 {
        self.shared.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Writes a frame on the shared write half, mapping a write failure to a
    /// transport error and marking the connection dead.
    fn write_locked(&self, header: &Value, blob: &[u8]) -> Result<(), String> {
        write_locked_shared(&self.shared, header, blob)
    }

    /// Registers a oneshot router channel for `id` and returns its receiver.
    fn register(&self, id: u64) -> Receiver<RouterMsg> {
        let (tx, rx) = mpsc::channel();
        self.shared.pending.lock().unwrap().insert(id, tx);
        rx
    }

    /// Removes any pending router registration for `id` (terminal cleanup).
    fn unregister(&self, id: u64) {
        unregister_shared(&self.shared, id);
    }

    /// Issues a request and blocks until the terminal `response`, ignoring any
    /// interim `progress` frames.
    ///
    /// Thin wrapper over [`begin_call`](Self::begin_call) + [`CallHandle::wait`], so
    /// the request lifecycle has a single code path. Returns the response header
    /// `Value` and its blob on `status:"ok"`.
    ///
    /// # Errors
    /// [`CallError::Error`] on `status:"error"`, [`CallError::Interrupted`] on
    /// `status:"interrupted"`, [`CallError::Transport`] on timeout / EOF / framing
    /// failure / connect failure.
    pub fn call(
        &self,
        method: &str,
        header_fields: Value,
        blob: &[u8],
        timeout: Duration,
    ) -> Result<(Value, Vec<u8>), CallError> {
        self.begin_call(method, header_fields, blob)
            .map_err(CallError::Transport)?
            .wait(timeout)
    }

    /// Issues a request and blocks until the terminal `response`, invoking
    /// `on_progress(header, progress_blob)` for each interim `progress` frame.
    ///
    /// Thin wrapper over [`begin_call`](Self::begin_call) +
    /// [`CallHandle::wait_streaming`]. The progress header includes the protocol
    /// fields (`step`, `total`, ...); the progress blob carries any per-step preview
    /// PNG (e.g. SDXL latent previews), empty when none.
    ///
    /// # Errors
    /// Same as [`call`](Self::call).
    pub fn call_streaming(
        &self,
        method: &str,
        header_fields: Value,
        blob: &[u8],
        on_progress: impl FnMut(&Value, &[u8]),
        timeout: Duration,
    ) -> Result<(Value, Vec<u8>), CallError> {
        self.begin_call(method, header_fields, blob)
            .map_err(CallError::Transport)?
            .wait_streaming(on_progress, timeout)
    }

    /// Registers a request id, writes the request frame, and returns a non-blocking
    /// [`CallHandle`] immediately (the request is in flight). The caller then drives
    /// it with [`CallHandle::wait`] / [`CallHandle::wait_streaming`], and any other
    /// thread holding the same handle can [`CallHandle::cancel`] it by id.
    ///
    /// This is the single underlying primitive behind [`call`](Self::call) and
    /// [`call_streaming`](Self::call_streaming).
    ///
    /// # Errors
    /// Returns a transport error string on connect / write failure (in which case
    /// the pending id is cleaned up and no handle is returned).
    pub fn begin_call(
        &self,
        method: &str,
        header_fields: Value,
        blob: &[u8],
    ) -> Result<CallHandle, String> {
        self.ensure_connected()?;

        let id = self.next_id();
        let rx = self.register(id);
        let header = protocol::request_header(id, method, &header_fields);

        if let Err(err) = self.write_locked(&header, blob) {
            self.unregister(id);
            return Err(err);
        }

        Ok(CallHandle {
            id,
            rx,
            shared: Arc::clone(&self.shared),
        })
    }

    /// Writes a `cancel{id}` frame, requesting cancellation of an in-flight
    /// request. A cancel for an unknown/finished id is a server-side no-op.
    ///
    /// # Errors
    /// Returns a transport error string on write failure.
    pub fn cancel(&self, id: u64) -> Result<(), String> {
        self.write_locked(&protocol::cancel_header(id), &[])
    }

    /// Subscribes to an event `topic`, returning a receiver that yields each
    /// matching `event` frame's header `Value`. Multiple subscribers per topic are
    /// supported; a subscriber whose receiver is dropped is pruned on the next push.
    #[must_use]
    pub fn subscribe(&self, topic: &str) -> Receiver<Value> {
        let (tx, rx) = mpsc::channel();
        self.shared
            .subscribers
            .lock()
            .unwrap()
            .entry(topic.to_string())
            .or_default()
            .push(tx);
        rx
    }
}

/// A non-blocking handle to a single in-flight request, returned by
/// [`BackendClient::begin_call`].
///
/// The handle owns the request `id` and its router receiver. Any thread holding the
/// handle (or just its [`id`](CallHandle::id)) can [`cancel`](CallHandle::cancel) the
/// request, while another thread blocks in [`wait`](CallHandle::wait) /
/// [`wait_streaming`](CallHandle::wait_streaming) for the terminal frame. This
/// supports a future "Stop" button: the UI begins a call, hands the id to a worker
/// that waits, and cancels by id from the UI thread.
///
/// The pending-id registration is cleaned up on every exit of `wait`/`wait_streaming`
/// (success, error, timeout, transport). Dropping the handle without waiting also
/// cleans the registration up so a leaked id cannot accumulate.
pub struct CallHandle {
    id: u64,
    rx: Receiver<RouterMsg>,
    shared: Arc<Shared>,
}

impl CallHandle {
    /// The correlation id assigned to this in-flight request.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Requests cancellation of this in-flight request by writing a `cancel{id}`
    /// frame. Safe to call from another thread while one thread is in `wait*`. A
    /// cancel for an already-finished id is a server-side no-op.
    ///
    /// # Errors
    /// Returns a transport error string on write failure.
    pub fn cancel(&self) -> Result<(), String> {
        write_locked_shared(&self.shared, &protocol::cancel_header(self.id), &[])
    }

    /// Blocks until the terminal `response`, ignoring interim `progress` frames.
    ///
    /// Consumes the handle. Cleans up the pending-id registration on every exit.
    ///
    /// # Errors
    /// [`CallError::Error`] on `status:"error"`, [`CallError::Interrupted`] on
    /// `status:"interrupted"` (incl. a cancel), [`CallError::Transport`] on timeout /
    /// EOF / framing failure.
    pub fn wait(self, timeout: Duration) -> Result<(Value, Vec<u8>), CallError> {
        self.wait_streaming(|_, _| {}, timeout)
    }

    /// Blocks until the terminal `response`, invoking `on_progress(header, blob)` for
    /// each interim `progress` frame (the header plus any per-step preview PNG blob).
    ///
    /// Consumes the handle. Cleans up the pending-id registration on every exit.
    ///
    /// # Errors
    /// Same as [`wait`](Self::wait).
    pub fn wait_streaming(
        self,
        mut on_progress: impl FnMut(&Value, &[u8]),
        timeout: Duration,
    ) -> Result<(Value, Vec<u8>), CallError> {
        let id = self.id;
        let result = loop {
            match self.rx.recv_timeout(timeout) {
                Ok(RouterMsg::Progress(header, preview)) => {
                    on_progress(&header, &preview);
                }
                Ok(RouterMsg::Terminal(frame)) => break interpret_terminal(frame),
                Ok(RouterMsg::Transport(msg)) => break Err(CallError::Transport(msg)),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Abandon the request server-side; the id is retired locally.
                    let _ = write_locked_shared(&self.shared, &protocol::cancel_header(id), &[]);
                    // FIX-3: the server may deliver an `interrupted` terminal in
                    // response to our cancel (or just after the timeout). Give it a
                    // brief grace window and prefer that terminal over the timeout
                    // error, so a cancel/interrupt is not misreported as Transport.
                    break drain_terminal_after_timeout(&self.rx, id);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(CallError::Transport(
                        "Reader-поток backend завершился до ответа.".to_string(),
                    ));
                }
            }
        };

        unregister_shared(&self.shared, id);
        result
    }
}

impl Drop for CallHandle {
    fn drop(&mut self) {
        // If the handle is dropped without being waited on (the `wait*` methods
        // consume `self`, so this only fires on an un-awaited handle), clean its
        // pending-id registration so leaked ids cannot accumulate.
        unregister_shared(&self.shared, self.id);
    }
}

/// Grace window for FIX-3: after a `wait` timeout fires and we send `cancel(id)`,
/// briefly poll for a terminal frame so a quickly-delivered `interrupted` (or even a
/// late `ok`) is preferred over the timeout transport error.
const TIMEOUT_TERMINAL_GRACE: Duration = Duration::from_millis(250);

/// After a `recv_timeout` timeout, drains the router channel for up to
/// [`TIMEOUT_TERMINAL_GRACE`] looking for a terminal frame. Returns the interpreted
/// terminal if one arrives; otherwise returns the timeout transport error. Interim
/// `progress` frames are skipped.
fn drain_terminal_after_timeout(
    rx: &Receiver<RouterMsg>,
    id: u64,
) -> Result<(Value, Vec<u8>), CallError> {
    let deadline = std::time::Instant::now() + TIMEOUT_TERMINAL_GRACE;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(RouterMsg::Terminal(frame)) => return interpret_terminal(frame),
            Ok(RouterMsg::Transport(msg)) => return Err(CallError::Transport(msg)),
            // A progress frame in the grace window: keep waiting for the terminal.
            Ok(RouterMsg::Progress(_, _)) => {}
            Err(_) => break,
        }
    }
    Err(CallError::Transport(format!(
        "Тайм-аут ожидания ответа backend (id {id})."
    )))
}

/// Writes a frame on a [`Shared`]'s write half, mapping a write failure to a
/// transport error and marking the connection dead. Shared by [`BackendClient`] and
/// [`CallHandle`].
fn write_locked_shared(shared: &Arc<Shared>, header: &Value, blob: &[u8]) -> Result<(), String> {
    let mut guard = shared.write_half.lock().unwrap();
    let Some(stream) = guard.as_mut() else {
        return Err("Соединение с backend закрыто.".to_string());
    };
    match write_frame(stream, header, blob) {
        Ok(()) => Ok(()),
        Err(err) => {
            // A failed write means the socket is unusable; drop it so the next call
            // reconnects, and let the reader thread tear down pending.
            *guard = None;
            shared.alive.store(false, Ordering::SeqCst);
            Err(err)
        }
    }
}

/// Removes any pending router registration for `id` (terminal cleanup).
fn unregister_shared(shared: &Arc<Shared>, id: u64) {
    shared.pending.lock().unwrap().remove(&id);
}

/// Verifies a server `hello` reply: it must be a `hello` kind with a matching
/// protocol version, or a clean `error` frame on mismatch.
fn verify_hello(header: &Value) -> Result<(), String> {
    let kind = header.get(protocol::HEADER_KIND).and_then(Value::as_str);
    if kind == Some(protocol::KIND_ERROR) {
        let msg = header
            .get(protocol::HEADER_ERROR)
            .and_then(Value::as_str)
            .unwrap_or("протокольная ошибка при handshake");
        return Err(format!("Backend отклонил handshake: {msg}"));
    }
    if kind != Some(protocol::KIND_HELLO) {
        return Err(format!(
            "Ожидался hello от backend, получено kind={kind:?}."
        ));
    }
    let server_v = header
        .get(protocol::HEADER_VERSION)
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if server_v != u64::from(protocol::PROTOCOL_VERSION) {
        return Err(format!(
            "Несовместимая версия протокола backend: сервер {server_v}, клиент {}.",
            protocol::PROTOCOL_VERSION
        ));
    }
    Ok(())
}

/// Maps a terminal `response`/`error` frame to the public call result.
fn interpret_terminal(frame: Frame) -> Result<(Value, Vec<u8>), CallError> {
    let header = &frame.header;
    let kind = header.get(protocol::HEADER_KIND).and_then(Value::as_str);

    // A protocol `error` frame attributed to this id is a transport-style failure.
    if kind == Some(protocol::KIND_ERROR) {
        let msg = header
            .get(protocol::HEADER_ERROR)
            .and_then(Value::as_str)
            .unwrap_or("протокольная ошибка backend")
            .to_string();
        return Err(CallError::Transport(msg));
    }

    let status = header
        .get(protocol::HEADER_STATUS)
        .and_then(Value::as_str)
        .unwrap_or(protocol::STATUS_ERROR);
    match status {
        protocol::STATUS_OK => Ok((frame.header, frame.blob)),
        protocol::STATUS_INTERRUPTED => Err(CallError::Interrupted(
            header
                .get(protocol::HEADER_ERROR)
                .and_then(Value::as_str)
                .unwrap_or("прервано")
                .to_string(),
        )),
        _ => Err(CallError::Error(
            header
                .get(protocol::HEADER_ERROR)
                .and_then(Value::as_str)
                .unwrap_or("неизвестная ошибка backend")
                .to_string(),
        )),
    }
}

/// Background reader loop: decodes frames and demultiplexes them by `id` /
/// topic until EOF or a framing error, then tears the connection down.
///
/// Holds only a `Weak<Shared>`: it upgrades per iteration, so once the last
/// `BackendClient` is dropped (and `Shared::drop` shuts the socket down) the
/// upgrade fails and the loop exits without keeping the connection open.
fn reader_loop(weak: Weak<Shared>, mut read_half: BackendStream, generation: u64) {
    loop {
        let Some(shared) = weak.upgrade() else {
            return; // all clients dropped; nothing to route to.
        };
        // If a newer connection has superseded us, exit quietly.
        if shared.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        // Drop the strong ref while blocked in read so we never keep `Shared` (and
        // thus the connection) alive past the last client.
        drop(shared);

        match read_frame(&mut read_half) {
            Ok(frame) => {
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                if shared.generation.load(Ordering::SeqCst) == generation {
                    route_frame(&shared, frame);
                }
            }
            Err(err) => {
                // Only tear down if we are still the active generation.
                if let Some(shared) = weak.upgrade()
                    && shared.generation.load(Ordering::SeqCst) == generation
                {
                    crate::runtime_log::log_warn(format!(
                        "[backend_ipc] v2 reader stopping (gen {generation}): {err}"
                    ));
                    teardown(&shared, &err);
                }
                return;
            }
        }
    }
}

/// Routes one decoded frame to the right destination.
fn route_frame(shared: &Arc<Shared>, frame: Frame) {
    let kind = frame
        .header
        .get(protocol::HEADER_KIND)
        .and_then(Value::as_str)
        .unwrap_or("");

    match kind {
        protocol::KIND_EVENT => {
            let topic = frame
                .header
                .get(protocol::HEADER_TOPIC)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            dispatch_event(shared, &topic, frame.header);
        }
        protocol::KIND_PROGRESS => {
            let id = frame_id(&frame.header);
            send_to_pending(shared, id, RouterMsg::Progress(frame.header, frame.blob));
        }
        protocol::KIND_RESPONSE | protocol::KIND_ERROR => {
            let id = frame_id(&frame.header);
            if id == 0 {
                // A protocol error with id 0 is connection-fatal; tear down.
                let msg = frame
                    .header
                    .get(protocol::HEADER_ERROR)
                    .and_then(Value::as_str)
                    .unwrap_or("протокольная ошибка backend (id 0)")
                    .to_string();
                crate::runtime_log::log_warn(format!("[backend_ipc] {msg}"));
                teardown(shared, &msg);
            } else {
                send_to_pending(shared, id, RouterMsg::Terminal(frame));
            }
        }
        // hello / request / cancel are not expected server->client mid-stream.
        other => {
            crate::runtime_log::log_warn(format!(
                "[backend_ipc] v2 reader ignoring unexpected frame kind {other:?}"
            ));
        }
    }
}

/// Extracts the `id` from a frame header (0 if absent/invalid).
fn frame_id(header: &Value) -> u64 {
    header
        .get(protocol::HEADER_ID)
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Sends a router message to the caller registered for `id`, if any. For a
/// terminal message the registration is removed.
fn send_to_pending(shared: &Arc<Shared>, id: u64, msg: RouterMsg) {
    let is_terminal = matches!(msg, RouterMsg::Terminal(_) | RouterMsg::Transport(_));
    let sender = {
        let mut pending = shared.pending.lock().unwrap();
        if is_terminal {
            pending.remove(&id)
        } else {
            pending.get(&id).cloned()
        }
    };
    if let Some(tx) = sender {
        let _ = tx.send(msg);
    }
}

/// Fans an event header out to every live subscriber of `topic`, pruning closed
/// receivers.
fn dispatch_event(shared: &Arc<Shared>, topic: &str, header: Value) {
    let mut subs = shared.subscribers.lock().unwrap();
    if let Some(list) = subs.get_mut(topic) {
        list.retain(|tx| tx.send(header.clone()).is_ok());
        if list.is_empty() {
            subs.remove(topic);
        }
    }
}

/// Marks the connection dead and fails every pending caller with a transport
/// error so no `call` blocks forever after an EOF.
fn teardown(shared: &Arc<Shared>, reason: &str) {
    shared.alive.store(false, Ordering::SeqCst);
    *shared.write_half.lock().unwrap() = None;
    let drained: Vec<Sender<RouterMsg>> = {
        let mut pending = shared.pending.lock().unwrap();
        pending.drain().map(|(_, tx)| tx).collect()
    };
    for tx in drained {
        let _ = tx.send(RouterMsg::Transport(format!(
            "Соединение с backend разорвано: {reason}"
        )));
    }
}

/// Process-wide lazily-connected client. Phase 3 subsystems call this instead of
/// opening their own connection per request.
static SHARED_CLIENT: OnceLock<Mutex<Option<BackendClient>>> = OnceLock::new();

/// Returns the process-wide shared [`BackendClient`], connecting on first use and
/// reconnecting if the cached client's connection has died.
///
/// Thread-safe: concurrent callers serialize on an internal mutex; the returned
/// client is a cheap clone that shares the underlying connection + reader thread.
///
/// # Errors
/// Returns a human-readable error string if connecting / the hello handshake
/// fails (e.g. the backend is not running).
pub fn shared_client() -> Result<BackendClient, String> {
    let cell = SHARED_CLIENT.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap();

    if let Some(existing) = guard.as_ref()
        && existing.is_alive()
    {
        return Ok(existing.clone());
    }

    let client = BackendClient::connect()?;
    *guard = Some(client.clone());
    Ok(client)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    /// Spins up a throwaway frame-protocol listener on a unique temp path. The
    /// server: completes the hello handshake, then for each request echoes a
    /// known result + blob (emitting a progress frame first for streaming methods),
    /// honors cancel by replying `interrupted`, and supports two concurrent ids.
    struct TestServer {
        path: PathBuf,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn start() -> Self {
            let unique = format!(
                "manhwastudio_v2_test_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let path = std::env::temp_dir().join(unique);
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).expect("bind v2 test listener");

            let handle = thread::spawn(move || {
                let (conn, _addr) = listener.accept().expect("accept v2 connection");
                let mut read = conn.try_clone().expect("clone read half");
                let mut write = conn;

                // Handshake.
                let hello = read_frame(&mut read).expect("read client hello");
                assert_eq!(
                    hello.header.get("kind").and_then(Value::as_str),
                    Some("hello")
                );
                let reply = json!({
                    "v": 1, "id": 0, "kind": "hello", "backend_version": "9.9.9"
                });
                write_frame(&mut write, &reply, &[]).expect("write server hello");

                // Serve frames until the client disconnects.
                while let Ok(frame) = read_frame(&mut read) {
                    let kind = frame
                        .header
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let id = frame.header.get("id").and_then(Value::as_u64).unwrap_or(0);
                    let method = frame
                        .header
                        .get("method")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();

                    match kind {
                        "request" if method == "test.cancel_me" => {
                            // Don't answer immediately; wait for the cancel frame.
                            let next = read_frame(&mut read).expect("await cancel frame");
                            assert_eq!(
                                next.header.get("kind").and_then(Value::as_str),
                                Some("cancel")
                            );
                            let resp = json!({
                                "v": 1, "id": id, "kind": "response",
                                "status": "interrupted", "error": "cancelled by client"
                            });
                            write_frame(&mut write, &resp, &[]).expect("write interrupted");
                        }
                        "request" if method == "test.stream" => {
                            // Emit two progress frames then the terminal response.
                            for step in 1..=2 {
                                let prog = json!({
                                    "v": 1, "id": id, "kind": "progress",
                                    "step": step, "total": 2
                                });
                                write_frame(&mut write, &prog, &[]).expect("write progress");
                            }
                            let resp = json!({
                                "v": 1, "id": id, "kind": "response",
                                "status": "ok", "echo": method
                            });
                            write_frame(&mut write, &resp, b"stream-blob")
                                .expect("write stream response");
                        }
                        "request" if method == "test.stream_preview" => {
                            // FIX-4: emit a progress frame WITH a preview blob, then
                            // the terminal response.
                            let prog = json!({
                                "v": 1, "id": id, "kind": "progress",
                                "step": 1, "total": 1
                            });
                            write_frame(&mut write, &prog, b"preview-png")
                                .expect("write progress with blob");
                            let resp = json!({
                                "v": 1, "id": id, "kind": "response",
                                "status": "ok", "echo": method
                            });
                            write_frame(&mut write, &resp, b"final-blob")
                                .expect("write stream_preview response");
                        }
                        "request" if method == "test.timeout_then_interrupt" => {
                            // FIX-3: never answer until the client's timeout fires and
                            // it sends a cancel; then (after a tiny delay, still inside
                            // the grace window) deliver the `interrupted` terminal. The
                            // caller must prefer Interrupted over the timeout error.
                            let next = read_frame(&mut read).expect("await cancel frame");
                            assert_eq!(
                                next.header.get("kind").and_then(Value::as_str),
                                Some("cancel")
                            );
                            thread::sleep(Duration::from_millis(30));
                            let resp = json!({
                                "v": 1, "id": id, "kind": "response",
                                "status": "interrupted", "error": "interrupted after timeout"
                            });
                            write_frame(&mut write, &resp, &[])
                                .expect("write interrupted after timeout");
                        }
                        "request" => {
                            // Echo: result carries the method + the request blob back.
                            let resp = json!({
                                "v": 1, "id": id, "kind": "response",
                                "status": "ok", "echo": method
                            });
                            write_frame(&mut write, &resp, &frame.blob)
                                .expect("write echo response");
                        }
                        "cancel" => { /* stray cancel: no-op */ }
                        _ => {}
                    }
                }
            });

            TestServer {
                path,
                handle: Some(handle),
            }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Connects a `BackendClient` directly to a test server path (bypassing the
    /// production `backend_socket_path`).
    fn connect_to(path: PathBuf) -> BackendClient {
        let shared = Arc::new(Shared {
            socket_path: path,
            write_half: Mutex::new(None),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            subscribers: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(false),
            reconnect_lock: Mutex::new(()),
            backend_version: Mutex::new(None),
            generation: AtomicU64::new(0),
            shutdown_handle: Mutex::new(None),
        });
        let client = BackendClient { shared };
        client.establish().expect("handshake with test server");
        client
    }

    #[test]
    fn hello_handshake_records_backend_version() {
        let server = TestServer::start();
        let client = connect_to(server.path.clone());
        assert_eq!(client.backend_version().as_deref(), Some("9.9.9"));
        assert!(client.is_alive());
        drop(client);
        drop(server);
    }

    #[test]
    fn call_round_trip_echoes_blob() {
        let server = TestServer::start();
        let client = connect_to(server.path.clone());

        let (header, blob) = client
            .call(
                "test.echo",
                json!({ "foo": 1 }),
                b"hello-blob",
                Duration::from_secs(5),
            )
            .expect("echo call");
        assert_eq!(
            header.get("echo").and_then(Value::as_str),
            Some("test.echo")
        );
        assert_eq!(blob, b"hello-blob");

        drop(client);
        drop(server);
    }

    #[test]
    fn call_streaming_invokes_progress() {
        let server = TestServer::start();
        let client = connect_to(server.path.clone());

        let mut steps = Vec::new();
        let (header, blob) = client
            .call_streaming(
                "test.stream",
                json!({}),
                &[],
                |p, _blob| {
                    if let Some(step) = p.get("step").and_then(Value::as_u64) {
                        steps.push(step);
                    }
                },
                Duration::from_secs(5),
            )
            .expect("streaming call");
        assert_eq!(steps, vec![1, 2]);
        assert_eq!(
            header.get("echo").and_then(Value::as_str),
            Some("test.stream")
        );
        assert_eq!(blob, b"stream-blob");

        drop(client);
        drop(server);
    }

    #[test]
    fn cancel_yields_interrupted() {
        let server = TestServer::start();
        let client = connect_to(server.path.clone());

        // The server waits for a cancel before answering test.cancel_me, so issue
        // the call from a worker thread and cancel it from the test thread.
        let id = client.shared.next_id.load(Ordering::SeqCst);
        let worker = {
            let client = client.clone();
            thread::spawn(move || {
                client.call("test.cancel_me", json!({}), &[], Duration::from_secs(5))
            })
        };
        // Give the worker a moment to register + send the request, then cancel.
        thread::sleep(Duration::from_millis(100));
        client.cancel(id).expect("send cancel");

        let result = worker.join().expect("worker join");
        match result {
            Err(CallError::Interrupted(_)) => {}
            other => panic!("expected Interrupted, got {other:?}"),
        }

        drop(client);
        drop(server);
    }

    /// FIX-2: `begin_call` exposes the in-flight id; another thread cancels via the
    /// handle and the waiting caller observes `Interrupted`.
    #[test]
    fn begin_call_handle_cancel_round_trip() {
        let server = TestServer::start();
        let client = connect_to(server.path.clone());

        // Begin the call (request is now in flight); the handle exposes its id.
        let handle = client
            .begin_call("test.cancel_me", json!({}), &[])
            .expect("begin_call");
        let id = handle.id();
        assert!(id >= 1, "handle exposes a real id");

        // Cancel from a separate thread while the test thread waits on the handle.
        let canceller = {
            let client = client.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                client.cancel(id).expect("cancel by id");
            })
        };

        let result = handle.wait(Duration::from_secs(5));
        canceller.join().expect("canceller join");
        match result {
            Err(CallError::Interrupted(_)) => {}
            other => panic!("expected Interrupted, got {other:?}"),
        }

        drop(client);
        drop(server);
    }

    /// FIX-2 (variant): cancel via the handle itself from another thread.
    #[test]
    fn handle_self_cancel_yields_interrupted() {
        let server = TestServer::start();
        let client = connect_to(server.path.clone());

        let handle = client
            .begin_call("test.cancel_me", json!({}), &[])
            .expect("begin_call");

        // Hand the work of cancelling to the test thread via a shared id; the worker
        // waits. We model the "Stop button" by cancelling through the client by id
        // (the handle's cancel is exercised in the streaming-preview server path).
        let id = handle.id();
        let worker = thread::spawn(move || handle.wait(Duration::from_secs(5)));
        thread::sleep(Duration::from_millis(100));
        client.cancel(id).expect("cancel");

        match worker.join().expect("worker join") {
            Err(CallError::Interrupted(_)) => {}
            other => panic!("expected Interrupted, got {other:?}"),
        }

        drop(client);
        drop(server);
    }

    #[test]
    fn multiplexes_two_concurrent_ids() {
        let server = TestServer::start();
        let client = connect_to(server.path.clone());

        let c1 = client.clone();
        let c2 = client.clone();
        let t1 = thread::spawn(move || {
            c1.call("test.one", json!({}), b"blob-1", Duration::from_secs(5))
        });
        let t2 = thread::spawn(move || {
            c2.call("test.two", json!({}), b"blob-2", Duration::from_secs(5))
        });

        let (h1, b1) = t1.join().unwrap().expect("call one");
        let (h2, b2) = t2.join().unwrap().expect("call two");
        assert_eq!(h1.get("echo").and_then(Value::as_str), Some("test.one"));
        assert_eq!(b1, b"blob-1");
        assert_eq!(h2.get("echo").and_then(Value::as_str), Some("test.two"));
        assert_eq!(b2, b"blob-2");

        drop(client);
        drop(server);
    }

    /// FIX-4: the streaming progress callback now receives both the progress header
    /// and the per-step preview blob (e.g. SDXL latent preview PNG).
    #[test]
    fn streaming_progress_delivers_blob() {
        let server = TestServer::start();
        let client = connect_to(server.path.clone());

        let mut previews: Vec<Vec<u8>> = Vec::new();
        let (header, blob) = client
            .call_streaming(
                "test.stream_preview",
                json!({}),
                &[],
                |h, preview| {
                    assert_eq!(h.get("step").and_then(Value::as_u64), Some(1));
                    previews.push(preview.to_vec());
                },
                Duration::from_secs(5),
            )
            .expect("streaming call with preview");

        assert_eq!(previews, vec![b"preview-png".to_vec()]);
        assert_eq!(
            header.get("echo").and_then(Value::as_str),
            Some("test.stream_preview")
        );
        assert_eq!(blob, b"final-blob");

        drop(client);
        drop(server);
    }

    /// FIX-3: when the wait times out and we cancel, an `interrupted` terminal that
    /// the server delivers within the grace window is preferred over the timeout
    /// transport error.
    #[test]
    fn timeout_then_interrupted_prefers_interrupted() {
        let server = TestServer::start();
        let client = connect_to(server.path.clone());

        // A short timeout fires the cancel path; the server then delivers
        // `interrupted` ~30ms later, inside the 250ms grace window.
        let result = client.call(
            "test.timeout_then_interrupt",
            json!({}),
            &[],
            Duration::from_millis(80),
        );
        match result {
            Err(CallError::Interrupted(msg)) => {
                assert!(
                    msg.contains("interrupted"),
                    "unexpected interrupt msg: {msg}"
                );
            }
            other => panic!("expected Interrupted (not Transport timeout), got {other:?}"),
        }

        drop(client);
        drop(server);
    }

    /// A connection-counting listener for the reconnect-lock test (FIX-1). It counts
    /// every accepted connection and serves a normal echo session on each, so the
    /// test can assert exactly how many sockets the client opened.
    struct CountingServer {
        path: PathBuf,
        accepted: Arc<AtomicU64>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl CountingServer {
        fn start() -> Self {
            let unique = format!(
                "manhwastudio_v2_recon_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let path = std::env::temp_dir().join(unique);
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).expect("bind counting listener");
            let accepted = Arc::new(AtomicU64::new(0));
            let accepted_for_thread = Arc::clone(&accepted);

            let handle = thread::spawn(move || {
                loop {
                    let (conn, _addr) = match listener.accept() {
                        Ok(c) => c,
                        Err(_) => break,
                    };
                    accepted_for_thread.fetch_add(1, Ordering::SeqCst);
                    // Each connection gets its own server thread doing the handshake +
                    // echo loop, so concurrent reconnect attempts would each be counted.
                    thread::spawn(move || {
                        let mut read = conn.try_clone().expect("clone read half");
                        let mut write = conn;
                        let hello = match read_frame(&mut read) {
                            Ok(f) => f,
                            Err(_) => return,
                        };
                        if hello.header.get("kind").and_then(Value::as_str) != Some("hello") {
                            return;
                        }
                        let reply = json!({
                            "v": 1, "id": 0, "kind": "hello", "backend_version": "9.9.9"
                        });
                        if write_frame(&mut write, &reply, &[]).is_err() {
                            return;
                        }
                        while let Ok(frame) = read_frame(&mut read) {
                            if frame.header.get("kind").and_then(Value::as_str) != Some("request") {
                                continue;
                            }
                            let id = frame.header.get("id").and_then(Value::as_u64).unwrap_or(0);
                            let resp = json!({
                                "v": 1, "id": id, "kind": "response", "status": "ok", "echo": "ok"
                            });
                            if write_frame(&mut write, &resp, &frame.blob).is_err() {
                                break;
                            }
                        }
                    });
                }
            });

            CountingServer {
                path,
                accepted,
                handle: Some(handle),
            }
        }
    }

    impl Drop for CountingServer {
        fn drop(&mut self) {
            // The accept loop exits when the listener is dropped (path removed).
            let _ = std::fs::remove_file(&self.path);
            if let Some(h) = self.handle.take() {
                // Detach: the blocking accept may outlive the test, but the process
                // ends and the temp socket is gone. Avoid hanging the test on join.
                drop(h);
            }
        }
    }

    /// FIX-1: N caller threads that simultaneously observe a dead connection must
    /// trigger exactly ONE reconnect (one new accepted socket), not N. The first
    /// connection is the initial `connect_to`; after marking it dead, a burst of
    /// concurrent `call`s must open exactly one more.
    #[test]
    fn reconnect_lock_single_reconnect() {
        let server = CountingServer::start();
        let client = connect_to(server.path.clone());

        // Wait until the initial connection is accounted for.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while server.accepted.load(Ordering::SeqCst) < 1 {
            if std::time::Instant::now() > deadline {
                panic!("initial connection never accepted");
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            server.accepted.load(Ordering::SeqCst),
            1,
            "one initial connection"
        );

        // Mark the connection dead WITHOUT actually closing the socket, simulating
        // the reader thread having flagged EOF: drop the write half + clear alive.
        *client.shared.write_half.lock().unwrap() = None;
        client.shared.alive.store(false, Ordering::SeqCst);

        // Fire a burst of concurrent calls; each sees the dead connection and races
        // into ensure_connected(). The reconnect lock must serialize them so only
        // one establish()/socket happens.
        let mut workers = Vec::new();
        for _ in 0..8 {
            let c = client.clone();
            workers.push(thread::spawn(move || {
                let _ = c.call("test.echo", json!({}), b"x", Duration::from_secs(5));
            }));
        }
        for w in workers {
            let _ = w.join();
        }

        // Exactly one additional connection (total 2): the single reconnect.
        assert_eq!(
            server.accepted.load(Ordering::SeqCst),
            2,
            "expected exactly one reconnect (2 total accepts), got {}",
            server.accepted.load(Ordering::SeqCst)
        );

        drop(client);
        drop(server);
    }

    // ========================================================================
    // LIVE cross-language integration test (Wave 3).
    // ------------------------------------------------------------------------
    // This is the only test in this module that talks to a REAL running Python
    // backend instead of the in-process `TestServer`. It proves the v2 framed
    // protocol works end-to-end across the Rust<->Python boundary:
    //   1. the `hello` handshake completes and `backend_version()` is populated,
    //   2. a `health` `call(...)` returns a snapshot Value with the contracted
    //      keys (`ok`, `backend_version`, `is_torch_available`, ...),
    //   3. `subscribe(TOPIC_HEALTH)` receives at least one server-pushed health
    //      `event` within a few seconds (proves SERVER->CLIENT push, the core
    //      point of the whole rework).
    //
    // It is `#[ignore]`d so normal `cargo test`/CI never spawns Python. Run it
    // on demand (see docs/ipc_rework/LIVE_INTEGRATION.md):
    //
    //   MS_IPC_PYTHON=venv_new/bin/python \
    //     cargo test --lib backend_ipc::client::tests::live_ -- --ignored --nocapture
    //
    // `MS_IPC_PYTHON` selects the interpreter (default `venv/bin/python`). It
    // must be a venv whose torch actually imports, otherwise the backend's
    // health-snapshot worker raises and never publishes a `health` event (see
    // the run note).
    #[test]
    #[ignore = "spawns the real Python backend; run manually, see docs/ipc_rework/LIVE_INTEGRATION.md"]
    fn live_backend_roundtrip_and_health_push() {
        use std::io::Read as _;
        use std::path::Path;
        use std::process::{Command, Stdio};

        let python =
            std::env::var("MS_IPC_PYTHON").unwrap_or_else(|_| "venv/bin/python".to_string());
        // Repo root is two levels up from this file's crate root at runtime: the
        // test process cwd is the crate manifest dir, which is the repo root.
        let repo_root = std::env::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().expect("cwd"));

        // Unique temp socket so we never clobber a real backend's socket. The
        // backend binds this base path directly (the single, sole IPC socket).
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let socket_path =
            std::env::temp_dir().join(format!("ms_ipc_live_{}_{}", std::process::id(), stamp));
        let _ = std::fs::remove_file(&socket_path);

        let log_path =
            std::env::temp_dir().join(format!("ms_ipc_live_{}_{}.log", std::process::id(), stamp));
        let log_file = std::fs::File::create(&log_path).expect("create backend log");
        let log_file_err = log_file.try_clone().expect("clone log handle");

        eprintln!(
            "[live] launching backend: {python} ai_backend.py --socket {} (cwd {})",
            socket_path.display(),
            repo_root.display()
        );
        let mut child = Command::new(&python)
            .arg("ai_backend.py")
            .arg("--socket")
            .arg(&socket_path)
            .current_dir(&repo_root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_file_err))
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn backend ({python}): {e}"));

        // Always tear the child down + clean sockets, even on panic.
        struct Guard {
            child: std::process::Child,
            sockets: Vec<PathBuf>,
            log: PathBuf,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                for s in &self.sockets {
                    let _ = std::fs::remove_file(s);
                }
                let _ = std::fs::remove_file(&self.log);
            }
        }

        // Wait (poll) up to 120s for the socket file to appear; the backend
        // imports the model stack lazily but binds the socket early.
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        loop {
            if Path::new(&socket_path).exists() {
                break;
            }
            if std::time::Instant::now() > deadline {
                let mut log = String::new();
                if let Ok(mut f) = std::fs::File::open(&log_path) {
                    let _ = f.read_to_string(&mut log);
                }
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&socket_path);
                panic!(
                    "socket {} never appeared within 120s. Backend log:\n{log}",
                    socket_path.display()
                );
            }
            thread::sleep(Duration::from_millis(200));
        }
        eprintln!("[live] socket up: {}", socket_path.display());

        // Move the child into the cleanup guard now that the socket is up.
        let _guard = {
            // `child` was used above for the panic path; re-grab it by value.
            // (It has not been moved, so this is fine.)
            Guard {
                child,
                sockets: vec![socket_path.clone()],
                log: log_path.clone(),
            }
        };

        // (1) Real BackendClient hello handshake against the live socket.
        let client = connect_to(socket_path.clone());
        let version = client
            .backend_version()
            .expect("hello must populate backend_version");
        eprintln!("[live] hello ok, backend_version = {version}");
        assert!(!version.is_empty(), "backend_version must be non-empty");
        assert!(client.is_alive());

        // (2) `health` request/response carries the snapshot Value.
        let (health, blob) = client
            .call(
                protocol::METHOD_HEALTH,
                json!({}),
                &[],
                Duration::from_secs(10),
            )
            .expect("health call must succeed");
        assert!(blob.is_empty(), "health response has no blob");
        eprintln!(
            "[live] health response keys: {:?}",
            health
                .as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        );
        assert_eq!(
            health.get("ok").and_then(Value::as_bool),
            Some(true),
            "health.ok must be true"
        );
        assert_eq!(
            health.get("service").and_then(Value::as_str),
            Some("mf_ai_backend")
        );
        assert!(
            health
                .get("backend_version")
                .and_then(Value::as_str)
                .is_some(),
            "health.backend_version present"
        );
        assert!(
            health
                .get("is_torch_available")
                .and_then(Value::as_bool)
                .is_some(),
            "health.is_torch_available present"
        );

        // (2b) `browser.command` version handshake: drives the in-process browser
        // service (`BrowserService.dispatch`) without needing Selenium/Playwright,
        // proving the merged scraping endpoint is wired into the backend.
        let (browser_version, browser_blob) = client
            .call(
                protocol::METHOD_BROWSER_COMMAND,
                json!({ "payload": { "command": "version" } }),
                &[],
                Duration::from_secs(10),
            )
            .expect("browser.command version call must succeed");
        assert!(browser_blob.is_empty(), "browser version has no blob");
        assert_eq!(
            browser_version.get("event").and_then(Value::as_str),
            Some("version"),
            "browser.command version returns a version event"
        );
        assert!(
            browser_version
                .get("downloader_version")
                .and_then(Value::as_str)
                .is_some(),
            "browser.command version carries downloader_version"
        );
        eprintln!(
            "[live] browser.command version ok, downloader_version = {:?}",
            browser_version.get("downloader_version").and_then(Value::as_str)
        );

        // (3) SERVER->CLIENT push: subscribe to the health topic and require at
        // least one pushed event within a few snapshot intervals.
        let rx = client.subscribe(protocol::TOPIC_HEALTH);
        let event = rx.recv_timeout(Duration::from_secs(8)).unwrap_or_else(|_| {
            panic!(
                "no pushed health event within 8s (server->client push). The frame \
                 protocol is fine, but the backend's health worker only publishes a \
                 `health` event after `_build_health_snapshot` succeeds; if torch \
                 cannot import in {python}, `surya.health()` raises and the worker \
                 never publishes. Re-run with MS_IPC_PYTHON pointing at a venv whose \
                 torch imports."
            )
        });
        eprintln!(
            "[live] received pushed health event: topic={:?} ok={:?} backend_version={:?}",
            event.get(protocol::HEADER_TOPIC).and_then(Value::as_str),
            event.get("ok").and_then(Value::as_bool),
            event.get("backend_version").and_then(Value::as_str),
        );
        assert_eq!(
            event.get(protocol::HEADER_KIND).and_then(Value::as_str),
            Some(protocol::KIND_EVENT)
        );
        assert_eq!(
            event.get(protocol::HEADER_TOPIC).and_then(Value::as_str),
            Some(protocol::TOPIC_HEALTH)
        );

        drop(client);
        // `_guard` drops here: kills the backend and removes the socket files.
    }
}
