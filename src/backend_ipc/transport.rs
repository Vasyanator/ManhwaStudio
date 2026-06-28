/*
File: backend_ipc/transport.rs

Purpose:
AF_UNIX socket primitives shared by the framed IPC client. Computes the backend
socket path (the single source of truth, identical to the Python side), connects
with a fail-fast timeout, and exposes a platform-agnostic `BackendStream`
(Read + Write) over the OS Unix stream.

Key structures:
- BackendStream: wraps the platform Unix stream, delegates Read/Write, and can
  clone its halves / shut the socket down (used by the v2 reader thread).

Key functions:
- backend_socket_path(): standard AF_UNIX path (the single, sole IPC socket).
- connect_path(): connect-with-timeout + timeout setup against an explicit path
  (the framed client passes `backend_socket_path()`; tests pass a throwaway path).

Notes:
Logging goes through `crate::runtime_log` (the project-wide structured log),
consistent with the rest of the backend IPC code.
*/

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Tracks whether the current backend outage has already been reported with a
/// `warn`. The connection probe retries every couple of seconds, so without this
/// the log would fill with identical "backend unreachable" warnings while the
/// backend is simply not running. We warn once on the first failure, then stay
/// quiet (info level) on every subsequent failed attempt; a successful connect
/// clears the flag so the next outage is reported again.
static CONNECT_FAILURE_WARNED: AtomicBool = AtomicBool::new(false);

/// Reports a failed connect attempt: a `warn` the first time the backend becomes
/// unreachable, then a quiet `info` on each retry until the backend comes back.
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
    #[cfg(unix)]
    {
        PathBuf::from("/tmp/manhwastudio_backend_socket")
    }
    #[cfg(windows)]
    {
        std::env::temp_dir().join("manhwastudio_backend_socket")
    }
}

/// Platform-agnostic wrapper around the OS Unix-domain stream used to talk to
/// the Python backend.
///
/// Delegates `std::io::Read` and `std::io::Write` to the inner stream. On unix it
/// wraps `std::os::unix::net::UnixStream`; on windows it wraps
/// `uds_windows::UnixStream`.
#[derive(Debug)]
pub struct BackendStream {
    inner: UnixStream,
}

impl BackendStream {
    /// Sets the read timeout on the underlying stream. `None` clears the timeout
    /// (blocking reads).
    ///
    /// # Errors
    /// Returns a human-readable error string with diagnostic context if the OS
    /// rejects the timeout.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), String> {
        self.inner
            .set_read_timeout(timeout)
            .map_err(|err| format!("Не удалось выставить read timeout backend-сокета: {err}"))
    }

    /// Sets the write timeout on the underlying stream. `None` clears the timeout
    /// (blocking writes).
    ///
    /// # Errors
    /// Returns a human-readable error string with diagnostic context if the OS
    /// rejects the timeout.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> Result<(), String> {
        self.inner
            .set_write_timeout(timeout)
            .map_err(|err| format!("Не удалось выставить write timeout backend-сокета: {err}"))
    }

    /// Clones the underlying OS stream into a second `BackendStream`.
    ///
    /// Both handles share the same kernel socket, so a reader thread can own one
    /// clone while the caller writes through another. Used by the framed client to
    /// split read/write halves.
    ///
    /// # Errors
    /// Returns a human-readable error string if the OS refuses to duplicate the
    /// descriptor.
    pub fn try_clone(&self) -> Result<BackendStream, String> {
        self.inner
            .try_clone()
            .map(|inner| BackendStream { inner })
            .map_err(|err| format!("Не удалось клонировать backend-сокет: {err}"))
    }

    /// Shuts the underlying socket down in both directions.
    ///
    /// Used by the framed client on teardown so a reader thread blocked in
    /// `read()` on a cloned handle of the same socket unblocks (it sees EOF) and
    /// can exit. Errors (e.g. the socket is already closed) are intentionally
    /// swallowed by callers that shut down best-effort.
    ///
    /// # Errors
    /// Returns the OS error string if the shutdown syscall fails.
    pub fn shutdown(&self) -> Result<(), String> {
        self.inner
            .shutdown(std::net::Shutdown::Both)
            .map_err(|err| format!("Не удалось закрыть backend-сокет: {err}"))
    }
}

impl Read for BackendStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for BackendStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
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

    let stream = BackendStream { inner };
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
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
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
}
