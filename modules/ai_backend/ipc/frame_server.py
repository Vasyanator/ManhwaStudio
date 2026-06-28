"""
File: modules/ai_backend/ipc/frame_server.py

Purpose:
Raw (NOT HTTP) AF_UNIX stream server for the IPC protocol. Binds the base
backend socket path and serves each connection with the frame `Dispatcher`.
This is the single, sole IPC transport.

Main responsibilities:
- `FrameUnixServer`: `ThreadingMixIn + UnixStreamServer` with single-instance /
  stale-socket detection, `chmod 0o600`, and unlink-on-close safety;
- a per-connection handler that wraps the accepted socket in buffered
  read/write file objects and runs `serve_connection`;
- `run_frame_server(state, socket_path, stop_event, ...)`: build the shared
  `EventBus`, `HandlerContext`, and worker pool, then serve until `stop_event`.

Notes:
This module is intentionally free of any heavy/torch imports so the framing,
dispatcher, and smoke tests can import it without the AI model stack. The
`AppState` and the health-snapshot getter are passed in by `server.run_server`.
"""

from __future__ import annotations

import errno
import os
import socket
import socketserver
import sys
import threading
import traceback
from concurrent.futures import ThreadPoolExecutor
from typing import Any, Callable

from .dispatcher import serve_connection
from .events import EventBus
from .registry import HandlerContext

# Worker pool size for concurrent request handlers across all connections.
# Generous enough to run OCR/inpaint in parallel without starving progress/event
# writes, which happen on the connection's own thread, not the pool.
_DEFAULT_MAX_WORKERS = 16


class FrameBackendInstanceError(RuntimeError):
    """Raised when the v2 AF_UNIX socket is already owned by a live instance."""


def _socket_has_live_backend(path: str) -> bool:
    """Probe whether a live peer is already listening on AF_UNIX `path`.

    Mirrors `server._socket_has_live_backend`: a successful connect means another
    instance owns the socket; a refused connect means the file is stale.
    """
    probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    probe.settimeout(0.5)
    try:
        probe.connect(path)
    except OSError:
        return False
    finally:
        probe.close()
    return True


class FrameUnixServer(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    """Threaded raw-frame server bound to an AF_UNIX socket.

    Single-instance safety: a pre-existing socket file is probed; a live peer
    raises `FrameBackendInstanceError`, a stale file is unlinked before bind. The
    socket is `chmod 0o600` on posix and unlinked on `server_close`.
    """

    address_family = socket.AF_UNIX
    daemon_threads = True
    # Block on accept indefinitely; shutdown comes via `shutdown()` from the
    # owning thread (the stop-watcher thread in `run_frame_server`).
    allow_reuse_address = False

    def __init__(self, socket_path: str, handler_cls):  # noqa: ANN001 - handler factory
        self._socket_path = socket_path
        super().__init__(socket_path, handler_cls)

    def server_bind(self) -> None:
        path = self._socket_path
        if os.path.exists(path):
            if _socket_has_live_backend(path):
                raise FrameBackendInstanceError(
                    f"Another ManhwaStudio AI backend (v2) is already listening on {path}."
                )
            os.unlink(path)
        super().server_bind()
        if os.name == "posix":
            os.chmod(path, 0o600)

    def server_close(self) -> None:
        super().server_close()
        try:
            os.unlink(self._socket_path)
        except FileNotFoundError:
            pass

    def handle_error(self, request, client_address):  # noqa: ANN001 - stdlib signature
        exc = sys.exc_info()[1]
        if isinstance(exc, (ConnectionResetError, ConnectionAbortedError, BrokenPipeError)):
            return
        if isinstance(exc, OSError) and exc.errno in (
            errno.ECONNABORTED,
            errno.ECONNRESET,
            errno.EPIPE,
        ):
            return
        print(
            "[AI Backend][frame_error] "
            f"client={client_address!r} error={type(exc).__name__}: {exc}",
            flush=True,
        )
        traceback.print_exc()


def _build_handler(
    ctx: HandlerContext,
    pool: ThreadPoolExecutor,
    events: EventBus,
    backend_version: str,
):
    """Build a `BaseRequestHandler` subclass bound to the shared runtime objects."""

    class _Handler(socketserver.BaseRequestHandler):
        def handle(self) -> None:  # noqa: D401 - stdlib API
            # Buffered file objects over the accepted socket: `rfile.read(n)`
            # blocks until n bytes or EOF, which is exactly what `read_frame`
            # needs; `wfile` is buffered so we always `flush()` after a frame.
            rfile = self.request.makefile("rb", buffering=0)
            wfile = self.request.makefile("wb", buffering=0)
            try:
                serve_connection(
                    rfile,
                    wfile,
                    ctx,
                    pool,
                    events,
                    backend_version=backend_version,
                    # The accepted socket lets the event bus bound per-write
                    # blocking so a slow/dead client can't stall events to the
                    # rest (FIX-4).
                    sock=self.request,
                )
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                pass
            finally:
                try:
                    wfile.close()
                except Exception:  # noqa: BLE001
                    pass
                try:
                    rfile.close()
                except Exception:  # noqa: BLE001
                    pass

    return _Handler


def run_frame_server(
    state: Any,
    socket_path: str,
    stop_event: threading.Event,
    *,
    backend_version: str,
    get_health_snapshot: Callable[[], dict[str, Any]],
    events: EventBus | None = None,
    max_workers: int = _DEFAULT_MAX_WORKERS,
) -> EventBus:
    """Serve the v2 frame protocol on `socket_path` until `stop_event` is set.

    Builds the shared `EventBus` (returned so the health worker can publish to
    it), the worker `ThreadPoolExecutor`, and the `HandlerContext`, then runs
    `serve_forever` on a `FrameUnixServer`. A background thread watches
    `stop_event` and calls `server.shutdown()` so this function returns cleanly.

    Returns the `EventBus` used by the server (the same object passed in via
    `events`, or a freshly created one).
    """
    if not hasattr(socket, "AF_UNIX"):
        raise RuntimeError(
            "This Python build lacks AF_UNIX support; Windows 10 1803+ with a "
            "modern CPython is required."
        )

    bus = events if events is not None else EventBus()
    pool = ThreadPoolExecutor(
        max_workers=max_workers, thread_name_prefix="ipc-frame-worker"
    )
    ctx = HandlerContext(
        state=state,
        events=bus,
        get_health_snapshot=get_health_snapshot,
    )
    handler_cls = _build_handler(ctx, pool, bus, backend_version)
    server = FrameUnixServer(os.fspath(socket_path), handler_cls)

    # Watch the stop event and trigger an orderly shutdown of serve_forever.
    def _watch_stop() -> None:
        stop_event.wait()
        try:
            server.shutdown()
        except Exception:  # noqa: BLE001
            pass

    watcher = threading.Thread(
        target=_watch_stop, name="ipc-frame-stop-watch", daemon=True
    )
    watcher.start()

    print(f"[AI Backend] frame server on unix socket {socket_path}", flush=True)
    try:
        server.serve_forever(poll_interval=0.25)
    finally:
        server.server_close()
        pool.shutdown(wait=False)
    return bus
