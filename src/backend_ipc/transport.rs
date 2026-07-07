/*
File: backend_ipc/transport.rs

Purpose:
Connection primitives for the framed IPC client. The frame codec (see `frame.rs`)
travels over a pluggable transport:
- AF_UNIX (default on Linux and Windows): `UnixStream`, the primary path.
- loopback WebSocket (fallback, e.g. when AF_UNIX is unavailable): a `tungstenite`
  WS client over `std::net::TcpStream` bound to 127.0.0.1.
Both are exposed uniformly through `BackendStream` (Read + Write + clone +
shutdown), so `client.rs` stays transport-agnostic.

Key structures:
- BackendEndpoint: which transport to dial (Unix path vs Ws { port, token }).
- BackendStream: enum wrapper delegating Read/Write/timeouts/clone/shutdown to the
  Unix stream or the WS adapter. Clones share one underlying connection.
- WsShared/WsHandle: the WS adapter. A dedicated I/O thread OWNS the
  `WebSocket<TcpStream>`; app code exchanges bytes through an inbound byte queue
  (Condvar-signalled) and an outbound message channel — it never touches the socket.

Key functions:
- backend_socket_path(): standard AF_UNIX path (matches the Python side).
- connect_path(): connect-with-timeout against an explicit Unix path.
- connect_ws(): TCP connect + WS handshake (`ws://127.0.0.1:<port>/?token=<token>`)
  + I/O thread spawn.
- connect_endpoint(): dispatches to the Unix or WS connector by `BackendEndpoint`.
- set_ws_endpoint()/current_backend_endpoint(): process-global WS endpoint holder and
  platform-aware endpoint selection (Unix path on unix, published Ws on windows).

Shutdown contract:
`BackendStream::shutdown()` on any clone makes a blocked `read()` on a SIBLING clone
return `Ok(0)` (EOF); the framed client's reader thread relies on this to exit. For
the WS variant, shutdown sets the closed flag WHILE HOLDING the inbound mutex, then
wakes the inbound Condvar (lost-wakeup-free: a reader between its closed-check and its
`wait` cannot miss the notify), drops the outbound channel, and shuts the retained
TcpStream clone down so the I/O thread's blocking WS read returns and the thread exits.

WS lifetime: the I/O thread holds only a `Weak<WsShared>`, never a strong `Arc`. When
every `WsHandle` is dropped WITHOUT an explicit `shutdown` (e.g. a caller's rollback
path), `WsShared::drop` sets closed and shuts the retained TcpStream down, so the I/O
thread unblocks, its `Weak::upgrade` fails, and it exits — no leaked thread or state.

Notes:
Logging goes through `crate::runtime_log`. The WS handshake never enables TLS
(loopback only), keeping the x86_64-pc-windows-gnu target buildable. The auth token
is never logged (only the port is).
*/

// AF_UNIX / TCP sockets and the connect workers are native-only; only the
// socket-path helper stays target-neutral so shared call sites keep compiling on
// wasm.
#[cfg(not(target_arch = "wasm32"))]
use std::collections::VecDeque;
#[cfg(not(target_arch = "wasm32"))]
use std::io::{ErrorKind, Read, Write};
#[cfg(not(target_arch = "wasm32"))]
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock, Weak};
#[cfg(not(target_arch = "wasm32"))]
use ms_thread as thread;
#[cfg(not(target_arch = "wasm32"))]
use tungstenite::{Message, WebSocket};
#[cfg(not(target_arch = "wasm32"))]
use web_time::Duration;

/// Tracks whether the current backend outage has already been reported with a
/// `warn`. The connection probe retries every couple of seconds, so without this
/// the log would fill with identical "backend unreachable" warnings while the
/// backend is simply not running. We warn once on the first failure, then stay
/// quiet (info level) on every subsequent failed attempt; a successful connect
/// clears the flag so the next outage is reported again.
#[cfg(not(target_arch = "wasm32"))]
static CONNECT_FAILURE_WARNED: AtomicBool = AtomicBool::new(false);

/// Poll interval the WS I/O thread uses for its blocking `read`, so it can service
/// the outbound queue and re-check the closed flag between reads. This is an
/// internal detail of the single-thread read+write multiplexing; the caller's
/// app-level read timeout is enforced separately via the inbound Condvar.
#[cfg(not(target_arch = "wasm32"))]
const WS_IO_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Reports a failed connect attempt: a `warn` the first time the backend becomes
/// unreachable, then a quiet `info` on each retry until the backend comes back.
#[cfg(not(target_arch = "wasm32"))]
fn report_connect_failure(msg: &str) {
    if CONNECT_FAILURE_WARNED.swap(true, Ordering::SeqCst) {
        // Already warned about this outage: stay quiet so the ~2s retry loop does
        // not spam the log. Keep an info-level breadcrumb for diagnostics.
        crate::runtime_log::log_info(format!("[backend_ipc] {msg} (повтор, подавлено)"));
    } else {
        crate::runtime_log::log_warn(format!("[backend_ipc] {msg}"));
    }
}

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use uds_windows::UnixStream;

/// Returns the standard AF_UNIX socket path used to reach the Python AI backend.
///
/// This is the single source of truth for the path: the framed IPC client
/// connects here and the Python backend binds the same path, so a manually
/// launched backend needs no `--socket` argument.
///
/// Path by platform:
/// - unix: `/tmp/manhwastudio_backend_socket`
/// - windows: `std::env::temp_dir().join("manhwastudio_backend_socket")`
#[must_use]
pub fn backend_socket_path() -> PathBuf {
    // `not(windows)` covers unix (identical to the previous `cfg(unix)` path) and
    // the wasm target, where the returned path is only ever used for display/logging
    // since the socket transport itself is compiled out.
    #[cfg(not(windows))]
    {
        PathBuf::from("/tmp/manhwastudio_backend_socket")
    }
    #[cfg(windows)]
    {
        std::env::temp_dir().join("manhwastudio_backend_socket")
    }
}

/// Which transport to dial for the framed AI-backend IPC.
///
/// `Unix` carries the AF_UNIX socket path (the default on Linux/Windows). `Ws`
/// carries the loopback WebSocket `port` and the auth `token` echoed in the
/// handshake URL query string; the backend validates the token constant-time and
/// rejects the upgrade on mismatch.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub enum BackendEndpoint {
    /// AF_UNIX transport at the given socket path.
    ///
    /// `current_backend_endpoint` constructs this variant only on unix; on windows
    /// it builds `Ws` exclusively, so there the variant is still *read* (by
    /// `connect_endpoint`'s match and `Display`) but never *constructed*. Allow that
    /// one target-specific dead construction; both variants are required so the
    /// cross-platform `connect_endpoint` compiles on either target.
    #[cfg_attr(windows, allow(dead_code))]
    Unix(PathBuf),
    /// Loopback WebSocket transport on `127.0.0.1:port`, authenticated by `token`.
    ///
    /// Mirror of `Unix`: constructed only on windows, so on a unix build (including
    /// `--all-targets`, where no test builds a `Ws` endpoint) it is read but never
    /// constructed. Allow that one target-specific dead construction on unix.
    #[cfg_attr(unix, allow(dead_code))]
    Ws {
        /// TCP port the backend WS server is bound to on 127.0.0.1.
        port: u16,
        /// Shared secret echoed as the `token` query param during the handshake.
        token: String,
    },
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Display for BackendEndpoint {
    /// Human/log-friendly rendering. The WS variant intentionally omits the token
    /// so it never leaks into logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendEndpoint::Unix(path) => write!(f, "unix:{}", path.display()),
            BackendEndpoint::Ws { port, token: _ } => write!(f, "ws://127.0.0.1:{port}"),
        }
    }
}

/// Locks a WS-shared mutex, recovering the guard if the lock is poisoned.
///
/// Each `WsShared` mutex guards a single self-contained value (the inbound byte
/// queue, the `Option<Sender>`, or a timeout) with no cross-field invariant, so a
/// panic in a previous holder cannot leave a half-updated state. Recovering and
/// continuing is therefore correct and avoids cascading one thread's panic into
/// every reader/writer via `.unwrap()`. Mirrors the poison handling in
/// [`set_ws_endpoint`].
#[cfg(not(target_arch = "wasm32"))]
fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Shared state of a WebSocket [`BackendStream`], reference-counted so every clone
/// (`WsHandle`) talks to the same connection.
///
/// A dedicated I/O thread owns the `tungstenite::WebSocket<TcpStream>` and is the
/// only code that touches the socket. App reads pull from `inbound` (a byte queue,
/// so the framed reader can span WS message boundaries via the length prefixes);
/// app writes push whole buffers to `outbound` for the I/O thread to send as WS
/// BINARY messages. `closed` + the inbound Condvar implement the EOF-on-shutdown
/// contract the framed reader thread relies on.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
struct WsShared {
    /// Bytes received from the peer, not yet consumed by an app `read`.
    inbound: Mutex<VecDeque<u8>>,
    /// Signalled whenever `inbound` gains bytes or `closed` flips, to wake readers.
    inbound_cv: Condvar,
    /// Outbound message sink drained by the I/O thread. `None` after `shutdown`.
    outbound: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    /// Set once the connection ends (EOF, error, or `shutdown`). Readers then see EOF.
    closed: AtomicBool,
    /// A second handle to the same TCP socket, kept solely so `shutdown` can force
    /// the I/O thread's blocking WS read to return (`shutdown(Both)`).
    tcp_shutdown: TcpStream,
    /// App-level read timeout (enforced via `inbound_cv`, not on the TCP socket).
    read_timeout: Mutex<Option<Duration>>,
    /// App-level write timeout (best-effort: also pushed to the TCP write side).
    write_timeout: Mutex<Option<Duration>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for WsShared {
    /// Backstop teardown for when every `WsHandle` is dropped WITHOUT an explicit
    /// [`WsHandle::shutdown`] (e.g. the client's reader-spawn rollback path). The I/O
    /// thread holds only a `Weak<WsShared>`, so reaching this drop means the strong
    /// count hit zero and no handles remain: set `closed` and shut the retained TCP
    /// clone down so the I/O thread's blocking read returns, its `Weak::upgrade`
    /// fails, and it exits instead of polling the closed flag forever.
    ///
    /// No reader can be blocked on the inbound Condvar here — a blocked `read` borrows
    /// a `WsHandle`, which would keep the strong count above zero — so no notify is
    /// needed. Idempotent with a prior `shutdown`: a second `shutdown(Both)` on an
    /// already-closed socket returns an error that is intentionally ignored.
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.tcp_shutdown.shutdown(std::net::Shutdown::Both);
    }
}

/// Cheap cloneable handle to a WebSocket connection; all clones share one
/// [`WsShared`] (hence one socket + one I/O thread).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
struct WsHandle {
    shared: Arc<WsShared>,
}

#[cfg(not(target_arch = "wasm32"))]
impl WsHandle {
    /// Drains up to `buf.len()` bytes from the inbound queue. Blocks (or waits up to
    /// the read timeout) when empty; returns `Ok(0)` once the connection is closed.
    ///
    /// # Errors
    /// Returns `WouldBlock` when a read timeout is set and elapses with no data.
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut inbound = lock_recover(&self.shared.inbound);
        loop {
            if !inbound.is_empty() {
                let n = buf.len().min(inbound.len());
                for (slot, byte) in buf[..n].iter_mut().zip(inbound.drain(..n)) {
                    *slot = byte;
                }
                return Ok(n);
            }
            // EOF: closed with no buffered bytes. This is the signal the framed
            // reader thread waits for on a sibling clone after `shutdown`. The load is
            // performed while holding `inbound`, so a concurrent `shutdown` /
            // `mark_ws_closed` (which set `closed` under the same lock) either happens
            // before this check (we return EOF) or after we are parked in `wait`
            // below (the notify wakes us) — no lost wakeup.
            if self.shared.closed.load(Ordering::SeqCst) {
                return Ok(0);
            }
            let timeout = *lock_recover(&self.shared.read_timeout);
            match timeout {
                Some(t) => {
                    let (guard, res) = self
                        .shared
                        .inbound_cv
                        .wait_timeout(inbound, t)
                        .unwrap_or_else(PoisonError::into_inner);
                    inbound = guard;
                    // Only report a timeout if nothing arrived AND we are still open;
                    // a `shutdown` wake re-loops and returns EOF above instead.
                    if res.timed_out()
                        && inbound.is_empty()
                        && !self.shared.closed.load(Ordering::SeqCst)
                    {
                        return Err(std::io::Error::from(ErrorKind::WouldBlock));
                    }
                }
                None => {
                    inbound = self
                        .shared
                        .inbound_cv
                        .wait(inbound)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }

    /// Queues `buf` as one outbound WS BINARY message. Returns the whole length so a
    /// single `write` never fragments a frame across messages.
    ///
    /// # Errors
    /// Returns `BrokenPipe` if the connection has been shut down / the I/O thread has
    /// exited.
    fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        let guard = lock_recover(&self.shared.outbound);
        match guard.as_ref() {
            Some(tx) => match tx.send(buf.to_vec()) {
                Ok(()) => Ok(buf.len()),
                Err(_) => Err(std::io::Error::new(
                    ErrorKind::BrokenPipe,
                    "WS I/O поток backend завершился, запись невозможна",
                )),
            },
            None => Err(std::io::Error::new(
                ErrorKind::BrokenPipe,
                "WS соединение с backend закрыто",
            )),
        }
    }

    /// Tears the connection down: marks it closed (readers see EOF), drops the
    /// outbound channel, wakes blocked readers, and force-unblocks the I/O thread's
    /// blocking read by shutting the retained TCP clone down.
    fn shutdown(&self) {
        {
            // Set `closed` while HOLDING the inbound mutex, then notify. A reader
            // between its `closed == false` check and its `wait` holds this same lock,
            // so it cannot miss the wake: the store either lands before its check
            // (it returns EOF) or after it is parked (the notify wakes it). This makes
            // shutdown self-sufficient against the lost-wakeup race, independent of the
            // I/O thread's poll backstop (fix #1). Named guard held for its lock scope.
            let _guard = lock_recover(&self.shared.inbound);
            self.shared.closed.store(true, Ordering::SeqCst);
        }
        self.shared.inbound_cv.notify_all();
        // Drop the outbound sender so the I/O thread observes `Disconnected` and can
        // send a WS Close before exiting.
        *lock_recover(&self.shared.outbound) = None;
        // Unblock the I/O thread if it is parked in a blocking WS read.
        let _ = self.shared.tcp_shutdown.shutdown(std::net::Shutdown::Both);
    }

    /// Sets the app-level read timeout (enforced via the inbound Condvar).
    fn set_read_timeout(&self, timeout: Option<Duration>) {
        *lock_recover(&self.shared.read_timeout) = timeout;
    }

    /// Sets the app-level write timeout and best-effort pushes it to the TCP write
    /// side (the retained clone shares the socket with the I/O thread's stream).
    fn set_write_timeout(&self, timeout: Option<Duration>) {
        *lock_recover(&self.shared.write_timeout) = timeout;
        let _ = self.shared.tcp_shutdown.set_write_timeout(timeout);
    }
}

/// Marks a WS connection closed and wakes any blocked app readers so they observe
/// EOF. Called on the I/O thread's exit path.
///
/// Sets `closed` while HOLDING the inbound mutex, then notifies, using the same
/// lost-wakeup-free discipline as [`WsHandle::shutdown`] (fix #1): a reader parking
/// on the inbound Condvar cannot miss this wake.
#[cfg(not(target_arch = "wasm32"))]
fn mark_ws_closed(shared: &WsShared) {
    {
        // Named guard held for its lock scope so the store is ordered against a
        // reader's `closed` check + `wait` transition.
        let _guard = lock_recover(&shared.inbound);
        shared.closed.store(true, Ordering::SeqCst);
    }
    shared.inbound_cv.notify_all();
}

/// The WS I/O thread body: the sole owner of the `WebSocket<TcpStream>`. It
/// multiplexes writes (draining `outbound`) and reads (poll-blocking, feeding
/// `inbound`) on one thread, responds to PING with PONG, and exits on close/error,
/// always marking the connection closed so readers unblock with EOF.
///
/// Holds only a `Weak<WsShared>` and re-upgrades per iteration, releasing the strong
/// ref BEFORE each (up-to-poll-interval) blocking read. This is load-bearing for the
/// no-leak contract (fix #2): if it retained a strong `Arc`, the last handle drop
/// could never drive the strong count to zero, so `WsShared::drop` (which unblocks
/// this read) would never run and the thread would poll forever. With a `Weak`, once
/// the last handle drops the upgrade fails and the thread exits.
#[cfg(not(target_arch = "wasm32"))]
fn ws_io_loop(weak: Weak<WsShared>, mut ws: WebSocket<TcpStream>, outbound: mpsc::Receiver<Vec<u8>>) {
    'io: loop {
        // Upgrade for this iteration's bookkeeping. A failed upgrade means the last
        // handle is gone and `WsShared::drop` is tearing the socket down: exit.
        let Some(shared) = weak.upgrade() else {
            break 'io;
        };

        // 1. Flush every queued outbound buffer as its own WS BINARY message.
        loop {
            match outbound.try_recv() {
                Ok(buf) => {
                    if let Err(err) = ws.send(Message::Binary(buf.into())) {
                        crate::runtime_log::log_info(format!(
                            "[backend_ipc] WS I/O thread stopping (send failed): {err}"
                        ));
                        break 'io;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // `shutdown` dropped the sender: send a Close best-effort and exit.
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    break 'io;
                }
            }
        }
        if shared.closed.load(Ordering::SeqCst) {
            let _ = ws.close(None);
            let _ = ws.flush();
            break 'io;
        }

        // Release the strong ref before the blocking read so a concurrent last-handle
        // drop can run `WsShared::drop` and unblock us (see the fn-level note).
        drop(shared);

        // 2. Poll-read one inbound message.
        match ws.read() {
            Ok(Message::Binary(data)) => {
                let Some(shared) = weak.upgrade() else {
                    break 'io;
                };
                {
                    let mut inbound = lock_recover(&shared.inbound);
                    inbound.extend(data.iter().copied());
                }
                shared.inbound_cv.notify_all();
            }
            Ok(Message::Ping(_)) => {
                // tungstenite auto-queued the matching Pong; flush it out promptly.
                let _ = ws.flush();
            }
            // Pong / Text / Frame are unused by the framed binary protocol.
            Ok(Message::Close(_)) => break 'io,
            Ok(_) => {}
            Err(tungstenite::Error::Io(err))
                if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut =>
            {
                // Poll timeout: no data this tick. Loop to service outbound / closed.
            }
            Err(err) => {
                // Only surface an error if we are still open; a `shutdown` / drop that
                // shut the retained TCP clone down is an expected exit, not a fault.
                if let Some(shared) = weak.upgrade()
                    && !shared.closed.load(Ordering::SeqCst)
                {
                    crate::runtime_log::log_info(format!(
                        "[backend_ipc] WS I/O thread stopping (read ended): {err}"
                    ));
                }
                break 'io;
            }
        }
    }
    // Wake any readers still blocked on the inbound Condvar with EOF, if a handle (and
    // thus a possible reader) still exists. If the upgrade fails the state is already
    // being dropped and no reader can be blocked.
    if let Some(shared) = weak.upgrade() {
        mark_ws_closed(&shared);
    }
}

/// Platform-agnostic wrapper around the transport stream used to talk to the Python
/// backend: either an OS Unix-domain stream or the loopback WebSocket adapter.
///
/// Delegates `std::io::Read` / `std::io::Write` to the active variant. Clones share
/// the same underlying connection (a duplicated Unix fd, or the same `WsShared`).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
pub struct BackendStream {
    inner: Inner,
}

/// The transport variant backing a [`BackendStream`].
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
enum Inner {
    /// AF_UNIX stream (`std::os::unix::net::UnixStream` / `uds_windows::UnixStream`).
    Unix(UnixStream),
    /// Loopback WebSocket adapter.
    Ws(WsHandle),
}

#[cfg(not(target_arch = "wasm32"))]
impl BackendStream {
    /// Sets the read timeout on the underlying stream. `None` clears the timeout
    /// (blocking reads).
    ///
    /// # Errors
    /// Returns a human-readable error string with diagnostic context if the OS
    /// rejects the timeout (Unix variant only; the WS variant cannot fail).
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), String> {
        match &self.inner {
            Inner::Unix(stream) => stream
                .set_read_timeout(timeout)
                .map_err(|err| format!("Не удалось выставить read timeout backend-сокета: {err}")),
            Inner::Ws(handle) => {
                handle.set_read_timeout(timeout);
                Ok(())
            }
        }
    }

    /// Sets the write timeout on the underlying stream. `None` clears the timeout
    /// (blocking writes).
    ///
    /// # Errors
    /// Returns a human-readable error string with diagnostic context if the OS
    /// rejects the timeout (Unix variant only; the WS variant cannot fail).
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> Result<(), String> {
        match &self.inner {
            Inner::Unix(stream) => stream
                .set_write_timeout(timeout)
                .map_err(|err| format!("Не удалось выставить write timeout backend-сокета: {err}")),
            Inner::Ws(handle) => {
                handle.set_write_timeout(timeout);
                Ok(())
            }
        }
    }

    /// Clones the stream into a second `BackendStream` over the same connection.
    ///
    /// Both handles share the same underlying transport (a duplicated Unix fd, or the
    /// same `WsShared`), so a reader thread can own one clone while the caller writes
    /// through another. Used by the framed client to split read/write halves.
    ///
    /// # Errors
    /// Returns a human-readable error string if the OS refuses to duplicate the Unix
    /// descriptor (the WS variant cannot fail).
    pub fn try_clone(&self) -> Result<BackendStream, String> {
        match &self.inner {
            Inner::Unix(stream) => stream
                .try_clone()
                .map(|inner| BackendStream {
                    inner: Inner::Unix(inner),
                })
                .map_err(|err| format!("Не удалось клонировать backend-сокет: {err}")),
            Inner::Ws(handle) => Ok(BackendStream {
                inner: Inner::Ws(handle.clone()),
            }),
        }
    }

    /// Shuts the underlying connection down in both directions.
    ///
    /// Used by the framed client on teardown so a reader thread blocked in `read()`
    /// on a cloned handle of the same connection unblocks (it sees EOF) and can exit.
    /// Errors (e.g. already closed) are intentionally swallowed by best-effort
    /// callers.
    ///
    /// # Errors
    /// Returns the OS error string if the Unix shutdown syscall fails (the WS variant
    /// is best-effort and cannot fail).
    pub fn shutdown(&self) -> Result<(), String> {
        match &self.inner {
            Inner::Unix(stream) => stream
                .shutdown(std::net::Shutdown::Both)
                .map_err(|err| format!("Не удалось закрыть backend-сокет: {err}")),
            Inner::Ws(handle) => {
                handle.shutdown();
                Ok(())
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Read for BackendStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.inner {
            Inner::Unix(stream) => stream.read(buf),
            Inner::Ws(handle) => handle.read(buf),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Write for BackendStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match &mut self.inner {
            Inner::Unix(stream) => stream.write(buf),
            Inner::Ws(handle) => handle.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.inner {
            Inner::Unix(stream) => stream.flush(),
            // WS writes are flushed by the I/O thread when it sends the message.
            Inner::Ws(_handle) => Ok(()),
        }
    }
}

/// Connects to a specific AF_UNIX `path` with a fail-fast connect timeout.
///
/// `connect_timeout` bounds the blocking `connect` call so the caller fails fast
/// when the backend is down (neither std nor `uds_windows` Unix streams expose a
/// native `connect_timeout`, so the blocking connect runs on a spawned thread and
/// is awaited via `recv_timeout`). On success, `read_timeout` and `write_timeout`
/// are applied (`None` leaves the stream blocking).
///
/// The framed client passes [`backend_socket_path`]; tests pass a throwaway
/// listener path so they never touch the production socket.
///
/// # Errors
/// Returns a human-readable error string (including the socket path and the OS
/// error) on connect timeout, connect failure, or timeout-setup failure.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn connect_path(
    path: PathBuf,
    connect_timeout: Duration,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
) -> Result<BackendStream, String> {
    let path_display = path.display().to_string();

    // The blocking connect runs on a throwaway thread; the parent bounds the wait
    // with recv_timeout so a hung/missing backend fails fast instead of blocking.
    let (tx, rx) = mpsc::channel::<Result<UnixStream, String>>();
    let connect_path = path.clone();
    thread::spawn(move || {
        let result = UnixStream::connect(&connect_path).map_err(|err| {
            format!(
                "Не удалось подключиться к AI backend по сокету {}: {err}",
                connect_path.display()
            )
        });
        // The receiver may already be gone on timeout; dropping the result is the
        // intended cleanup, so the send error is deliberately ignored.
        if tx.send(result).is_err() {
            crate::runtime_log::log_info(format!(
                "[backend_ipc] connect result dropped after timeout for socket {}",
                connect_path.display()
            ));
        }
    });

    let inner = match rx.recv_timeout(connect_timeout) {
        Ok(Ok(stream)) => stream,
        Ok(Err(err)) => {
            report_connect_failure(&err);
            return Err(err);
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let msg = format!(
                "Тайм-аут подключения к AI backend по сокету {path_display} \
                 (превышено {} мс). Возможная причина: backend не запущен.",
                connect_timeout.as_millis()
            );
            report_connect_failure(&msg);
            return Err(msg);
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let msg = format!(
                "Поток подключения к AI backend ({path_display}) завершился без результата."
            );
            crate::runtime_log::log_error(format!("[backend_ipc] {msg}"));
            return Err(msg);
        }
    };

    let stream = BackendStream {
        inner: Inner::Unix(inner),
    };
    stream.set_read_timeout(read_timeout)?;
    stream.set_write_timeout(write_timeout)?;
    // Connected: clear the outage flag so the next outage is reported again. If we
    // had previously warned, note that the backend is reachable once more.
    if CONNECT_FAILURE_WARNED.swap(false, Ordering::SeqCst) {
        crate::runtime_log::log_info(format!(
            "[backend_ipc] AI backend снова доступен по сокету {path_display}"
        ));
    }
    crate::runtime_log::log_info(format!(
        "[backend_ipc] connected to AI backend socket {path_display}"
    ));
    Ok(stream)
}

/// Connects to the loopback WebSocket backend at `127.0.0.1:port`, authenticating
/// with `token` in the handshake URL, and spawns the dedicated I/O thread.
///
/// The TCP connect is bounded by `connect_timeout`; `read_timeout`/`write_timeout`
/// become the app-level timeouts on the returned stream. The handshake URL is
/// `ws://127.0.0.1:<port>/?token=<token>`; the server validates the token and
/// rejects the upgrade on mismatch.
///
/// # Errors
/// Returns a human-readable error string on TCP connect failure/timeout, socket
/// option failure, WebSocket handshake failure (e.g. bad token), or I/O-thread spawn
/// failure.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn connect_ws(
    port: u16,
    token: &str,
    connect_timeout: Duration,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
) -> Result<BackendStream, String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let tcp = TcpStream::connect_timeout(&addr, connect_timeout).map_err(|err| {
        let msg = format!(
            "Не удалось подключиться к AI backend по WebSocket 127.0.0.1:{port}: {err}. \
             Возможная причина: backend не запущен или порт не совпадает."
        );
        report_connect_failure(&msg);
        msg
    })?;
    tcp.set_nodelay(true).map_err(|err| {
        format!("Не удалось выставить TCP_NODELAY для WS backend (порт {port}): {err}")
    })?;

    // Retained clone of the same socket: the I/O thread owns the WebSocket, and this
    // handle lets `shutdown` force-unblock the thread's blocking read.
    let tcp_shutdown = tcp.try_clone().map_err(|err| {
        format!("Не удалось клонировать WS TCP-сокет backend (порт {port}): {err}")
    })?;

    // Bound handshake reads/writes so a hung server fails fast; the I/O thread
    // switches the read timeout to a short poll interval right after the handshake.
    tcp.set_write_timeout(write_timeout)
        .map_err(|err| format!("Не удалось выставить WS write timeout (порт {port}): {err}"))?;
    tcp.set_read_timeout(Some(connect_timeout)).map_err(|err| {
        format!("Не удалось выставить WS handshake read timeout (порт {port}): {err}")
    })?;

    let url = format!("ws://127.0.0.1:{port}/?token={token}");
    let (ws, _response) = tungstenite::client(url.as_str(), tcp).map_err(|err| {
        let msg = format!(
            "Не удалось выполнить WebSocket-рукопожатие с AI backend (порт {port}): {err}. \
             Возможная причина: неверный токен или backend не готов."
        );
        report_connect_failure(&msg);
        msg
    })?;

    // The I/O thread multiplexes read+write over one WebSocket, so its blocking read
    // must return periodically to service the outbound queue. The caller's
    // `read_timeout` is honoured at the app level via the inbound Condvar, NOT on the
    // TCP socket (a `None` read timeout must not wedge the I/O thread forever).
    ws.get_ref()
        .set_read_timeout(Some(WS_IO_POLL_INTERVAL))
        .map_err(|err| format!("Не удалось выставить WS poll timeout (порт {port}): {err}"))?;

    let (outbound_tx, outbound_rx) = mpsc::channel::<Vec<u8>>();
    let shared = Arc::new(WsShared {
        inbound: Mutex::new(VecDeque::new()),
        inbound_cv: Condvar::new(),
        outbound: Mutex::new(Some(outbound_tx)),
        closed: AtomicBool::new(false),
        tcp_shutdown,
        read_timeout: Mutex::new(read_timeout),
        write_timeout: Mutex::new(write_timeout),
    });

    // The I/O thread holds only a `Weak`, so it never keeps `WsShared` alive past the
    // last `WsHandle`; when every handle drops (even without an explicit `shutdown`),
    // `WsShared::drop` unblocks the read and the thread exits (fix #2).
    let io_weak = Arc::downgrade(&shared);
    thread::Builder::new()
        .name("backend-ipc-ws-io".to_string())
        .spawn(move || ws_io_loop(io_weak, ws, outbound_rx))
        .map_err(|err| format!("Не удалось запустить WS I/O поток backend (порт {port}): {err}"))?;

    // Connected: clear the outage flag so the next outage is reported again.
    if CONNECT_FAILURE_WARNED.swap(false, Ordering::SeqCst) {
        crate::runtime_log::log_info(format!(
            "[backend_ipc] AI backend снова доступен по WebSocket (порт {port})"
        ));
    }
    crate::runtime_log::log_info(format!(
        "[backend_ipc] connected to AI backend over WebSocket (порт {port})"
    ));
    Ok(BackendStream {
        inner: Inner::Ws(WsHandle { shared }),
    })
}

/// Connects to `endpoint`, dispatching to the Unix or WebSocket connector.
///
/// # Errors
/// Propagates the error string from [`connect_path`] or [`connect_ws`].
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn connect_endpoint(
    endpoint: &BackendEndpoint,
    connect_timeout: Duration,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
) -> Result<BackendStream, String> {
    match endpoint {
        BackendEndpoint::Unix(path) => {
            connect_path(path.clone(), connect_timeout, read_timeout, write_timeout)
        }
        BackendEndpoint::Ws { port, token } => {
            connect_ws(*port, token, connect_timeout, read_timeout, write_timeout)
        }
    }
}

/// Process-global WebSocket endpoint published by the backend supervisor once the
/// Python backend prints its `MS_BACKEND_WS_PORT` line. A `RwLock` (not `OnceLock`)
/// because the backend can be restarted with a new port/token during the process
/// lifetime.
#[cfg(not(target_arch = "wasm32"))]
static WS_ENDPOINT: RwLock<Option<(u16, String)>> = RwLock::new(None);

/// Publishes (or overwrites) the loopback WebSocket endpoint the backend is
/// listening on. Called by the backend supervisor after it parses the backend's
/// `MS_BACKEND_WS_PORT` line. The token is never logged.
#[cfg(not(target_arch = "wasm32"))]
pub fn set_ws_endpoint(port: u16, token: String) {
    match WS_ENDPOINT.write() {
        Ok(mut guard) => *guard = Some((port, token)),
        // A poisoned lock only means a previous writer panicked mid-write; the value
        // itself is a plain tuple with no broken invariant, so recover and overwrite.
        Err(poisoned) => *poisoned.into_inner() = Some((port, token)),
    }
    crate::runtime_log::log_info(format!(
        "[backend_ipc] WS endpoint опубликован (порт {port})"
    ));
}

/// Returns the endpoint the framed client should dial on this platform.
///
/// On unix this is always the AF_UNIX [`backend_socket_path`]. On windows it is the
/// WebSocket endpoint published via [`set_ws_endpoint`].
///
/// # Errors
/// On windows, returns an error string if no WS endpoint has been published yet
/// (the backend has not reported its port).
#[cfg(not(target_arch = "wasm32"))]
pub fn current_backend_endpoint() -> Result<BackendEndpoint, String> {
    #[cfg(unix)]
    {
        Ok(BackendEndpoint::Unix(backend_socket_path()))
    }
    #[cfg(windows)]
    {
        let guard = WS_ENDPOINT
            .read()
            .map_err(|_| "Внутренняя ошибка: WS endpoint RwLock отравлён.".to_string())?;
        match guard.as_ref() {
            Some((port, token)) => Ok(BackendEndpoint::Ws {
                port: *port,
                token: token.clone(),
            }),
            None => {
                Err("WS endpoint не опубликован (backend ещё не сообщил порт).".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `backend_socket_path()` must yield a non-empty path that ends with the
    /// shared socket file name on every platform.
    #[test]
    fn socket_path_has_expected_suffix() {
        let path = backend_socket_path();
        assert!(
            !path.as_os_str().is_empty(),
            "socket path must not be empty"
        );
        assert!(
            path.ends_with("manhwastudio_backend_socket"),
            "socket path {} must end with the standard file name",
            path.display()
        );
        #[cfg(unix)]
        assert_eq!(
            path,
            PathBuf::from("/tmp/manhwastudio_backend_socket"),
            "unix socket path must match the documented constant"
        );
    }

    /// Round-trip: a throwaway `UnixListener` accepts one connection and
    /// `connect_path` must connect to it within the timeout.
    #[cfg(unix)]
    #[test]
    fn connect_path_round_trip_against_temp_listener() {
        use std::os::unix::net::UnixListener;

        // Unique temp path (not the production socket) so concurrent test runs and
        // a real backend never collide.
        let unique = format!(
            "manhwastudio_test_socket_{}_{}",
            std::process::id(),
            web_time::SystemTime::now()
                .duration_since(web_time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let socket_path = std::env::temp_dir().join(unique);
        // Stale leftovers would make bind fail with EADDRINUSE.
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path).expect("bind temp test listener");
        let server = thread::spawn(move || {
            let _ = listener.accept().expect("accept test connection");
        });

        let stream = connect_path(
            socket_path.clone(),
            Duration::from_secs(2),
            Some(Duration::from_secs(5)),
            Some(Duration::from_secs(5)),
        )
        .expect("connect to temp listener");
        drop(stream);

        server.join().expect("server thread must not panic");
        let _ = std::fs::remove_file(&socket_path);
    }

    /// End-to-end WS transport round-trip against a `tungstenite::accept_hdr`
    /// listener on 127.0.0.1:0. Asserts:
    /// (a) a frame written via `write_frame` is echoed and read back via `read_frame`;
    /// (b) after `shutdown()` on one clone, a blocked `read()` on a sibling returns
    ///     `Ok(0)` (the EOF contract the framed reader relies on);
    /// (c) the server sees the auth token in the handshake request URL.
    //
    // `result_large_err`: the WS handshake callback's return type
    // (`Result<Response, ErrorResponse>`) is fixed by tungstenite's `Callback` trait,
    // whose large `ErrorResponse` we cannot box. This is a test-only false positive.
    #[allow(clippy::result_large_err)]
    #[test]
    fn connect_ws_round_trip_and_shutdown_eof() {
        use crate::backend_ipc::frame::{read_frame, write_frame};
        use serde_json::json;
        use std::net::TcpListener;
        use std::sync::mpsc as std_mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ws test listener");
        let port = listener
            .local_addr()
            .expect("local addr of ws test listener")
            .port();

        // The server reports back the handshake request URI so the test can assert
        // the token travelled in the URL.
        let (url_tx, url_rx) = std_mpsc::channel::<String>();
        let server = thread::spawn(move || {
            let (tcp, _addr) = listener.accept().expect("accept ws connection");
            let captured = Arc::new(Mutex::new(String::new()));
            let captured_for_cb = Arc::clone(&captured);
            let callback = move |req: &tungstenite::handshake::server::Request,
                                 resp: tungstenite::handshake::server::Response| {
                *captured_for_cb.lock().unwrap() = req.uri().to_string();
                Ok(resp)
            };
            let mut ws = tungstenite::accept_hdr(tcp, callback).expect("ws server handshake");
            url_tx
                .send(captured.lock().unwrap().clone())
                .expect("report handshake url");

            // Echo each binary message back verbatim until the client closes.
            loop {
                match ws.read() {
                    Ok(Message::Binary(data)) => {
                        if ws.send(Message::Binary(data)).is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });

        let token = "secret-token-abc123";
        let stream = connect_ws(
            port,
            token,
            Duration::from_secs(2),
            Some(Duration::from_secs(5)),
            Some(Duration::from_secs(5)),
        )
        .expect("connect ws to test server");

        // (a) Frame round-trip: write on one clone, read the echo on another.
        let mut writer = stream.try_clone().expect("clone ws writer");
        let mut reader = stream.try_clone().expect("clone ws reader");
        let header = json!({ "v": 1, "id": 7, "kind": "request", "method": "test.ws" });
        let blob = b"ws-echo-blob".to_vec();
        write_frame(&mut writer, &header, &blob).expect("write frame over ws");
        let frame = read_frame(&mut reader).expect("read echoed frame over ws");
        assert_eq!(frame.header, header, "echoed header must match");
        assert_eq!(frame.blob, blob, "echoed blob must match");

        // (c) The token must have reached the server in the handshake URL.
        let seen_url = url_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("server must report the handshake url");
        assert!(
            seen_url.contains("token=secret-token-abc123"),
            "handshake url must carry the token, got: {seen_url}"
        );

        // (b) A blocked read on a sibling clone must return Ok(0) after shutdown.
        let mut sibling = stream.try_clone().expect("clone ws sibling");
        let blocked = thread::spawn(move || {
            let mut buf = [0_u8; 8];
            sibling.read(&mut buf)
        });
        // Give the reader time to park on the empty inbound queue, then shut down.
        thread::sleep(Duration::from_millis(150));
        stream.shutdown().expect("shutdown ws stream");
        let n = blocked
            .join()
            .expect("blocked reader thread must not panic")
            .expect("read after shutdown must not error");
        assert_eq!(n, 0, "read after shutdown must observe EOF (Ok(0))");

        // Closing our side ends the server's echo loop; join to keep the test clean.
        drop(writer);
        drop(reader);
        drop(stream);
        server.join().expect("ws server thread must not panic");
    }

    /// Fix #2: dropping EVERY `BackendStream`/`WsHandle` clone WITHOUT calling
    /// `shutdown()` must still terminate the WS I/O thread (it holds only a `Weak`, so
    /// `WsShared::drop` unblocks its read). Observed via the server side: once the I/O
    /// thread exits, its `WebSocket<TcpStream>` drops and the server's `read()` returns
    /// EOF/error. Before the fix the I/O thread held a strong `Arc` and polled forever,
    /// so the socket never closed and this test would hang.
    #[test]
    fn connect_ws_io_thread_exits_when_all_handles_dropped_without_shutdown() {
        use std::net::TcpListener;
        use std::sync::mpsc as std_mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ws test listener");
        let port = listener
            .local_addr()
            .expect("local addr of ws test listener")
            .port();

        // The server signals `closed_tx` once its read loop ends, which only happens
        // when the client's I/O thread exits and drops its socket.
        let (closed_tx, closed_rx) = std_mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (tcp, _addr) = listener.accept().expect("accept ws connection");
            let mut ws = tungstenite::accept(tcp).expect("ws server handshake");
            loop {
                match ws.read() {
                    Ok(Message::Binary(_)) => {}
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    // Client I/O thread exited and closed the socket.
                    Err(_) => break,
                }
            }
            let _ = closed_tx.send(());
        });

        let stream = connect_ws(
            port,
            "tok",
            Duration::from_secs(2),
            Some(Duration::from_secs(5)),
            Some(Duration::from_secs(5)),
        )
        .expect("connect ws to test server");

        // Drop ALL handles (here, the single one) WITHOUT calling shutdown().
        drop(stream);

        // The I/O thread must exit promptly (well within the 50ms poll backstop plus
        // teardown slack), closing the socket so the server observes EOF.
        closed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("WS I/O thread must exit and close the socket after all handles drop");
        server.join().expect("ws server thread must not panic");
    }

    /// Fix #1: `shutdown()` must wake a blocked reader PROMPTLY via its own
    /// notify-under-lock, not only via the up-to-50ms I/O-thread poll backstop. The
    /// sibling read blocks with no app-level read timeout, so the Condvar notify is the
    /// only fast wake source; we assert the wake lands well under the poll interval.
    #[test]
    fn connect_ws_shutdown_wakes_reader_promptly() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ws test listener");
        let port = listener
            .local_addr()
            .expect("local addr of ws test listener")
            .port();

        let server = thread::spawn(move || {
            let (tcp, _addr) = listener.accept().expect("accept ws connection");
            let mut ws = tungstenite::accept(tcp).expect("ws server handshake");
            // Idle: never send data, just drain until the client goes away.
            loop {
                match ws.read() {
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });

        let stream = connect_ws(
            port,
            "tok",
            Duration::from_secs(2),
            // No read timeout: the reader blocks purely on the inbound Condvar, so only
            // a notify (not a poll timeout) can wake it.
            None,
            Some(Duration::from_secs(5)),
        )
        .expect("connect ws to test server");

        let mut sibling = stream.try_clone().expect("clone ws sibling");
        let blocked = thread::spawn(move || {
            let mut buf = [0_u8; 8];
            let n = sibling.read(&mut buf).expect("read after shutdown must not error");
            (n, web_time::Instant::now())
        });

        // Let the reader park on the empty inbound queue.
        thread::sleep(Duration::from_millis(100));
        let before = web_time::Instant::now();
        stream.shutdown().expect("shutdown ws stream");
        let (n, woke_at) = blocked.join().expect("blocked reader thread must not panic");
        let elapsed = woke_at.saturating_duration_since(before);

        assert_eq!(n, 0, "read after shutdown must observe EOF (Ok(0))");
        assert!(
            elapsed < Duration::from_millis(40),
            "shutdown wake must be prompt (<40ms), took {elapsed:?}; the notify-under-lock \
             fix should not rely on the 50ms poll backstop"
        );

        drop(stream);
        server.join().expect("ws server thread must not panic");
    }
}
