"""
File: modules/ai_backend/ipc/dispatcher.py

Purpose:
Per-connection request dispatcher for the v2 IPC protocol. Owns the read loop
for one socket: performs the `hello` handshake, routes `request` frames to
worker threads, tracks per-id cancellation, and serializes all outbound frames
(response/progress/event/error) through the connection's write lock.

Main responsibilities:
- handshake: read the client `hello`, validate `v` == PROTOCOL_VERSION, reply
  `hello{v, backend_version}` or an `error` frame + close on mismatch;
- per `request{id, method}`: register a cancel `threading.Event` for that id,
  submit the handler to a thread pool, let it emit `progress{id}` frames, then
  emit the terminal `response{id, status, ...}` (+ blob) or an `error` frame;
- `cancel{id}`: set that id's cancel event so the running handler stops and
  finishes with `status:"interrupted"`; unknown/finished ids are a no-op;
- robust to client disconnect: BrokenPipe/ConnectionReset/clean EOF drop the
  connection quietly (quiet disconnect handling).

Notes:
One `Dispatcher` instance serves one connection. It is constructed with the
shared `HandlerContext`, a shared `ThreadPoolExecutor`, and the shared
`EventBus`. The connection registers with the bus for the duration so health
events fan out to it.
"""

from __future__ import annotations

import dataclasses
import threading
import traceback
from concurrent.futures import ThreadPoolExecutor
from typing import Any

from .events import EventBus, EventSink
from .framing import FrameError, FrameWriteLock, StreamClosed, read_frame, write_frame
from .protocol import (
    HEADER_BACKEND_VERSION,
    HEADER_ERROR,
    HEADER_ID,
    HEADER_KIND,
    HEADER_METHOD,
    HEADER_STATUS,
    HEADER_VERSION,
    KIND_ERROR,
    KIND_HELLO,
    KIND_PROGRESS,
    KIND_REQUEST,
    KIND_RESPONSE,
    KIND_CANCEL,
    PROTOCOL_VERSION,
    STATUS_ERROR,
    STATUS_INTERRUPTED,
    STATUS_OK,
)
from .registry import HandlerContext, Interrupted, get_handler

# Connection-drop errors that must be swallowed quietly (a client closing
# mid-request is normal, not a bug — quiet disconnect handling).
_DISCONNECT_ERRORS = (BrokenPipeError, ConnectionResetError, ConnectionAbortedError)

# Per-connection cap on in-flight (submitted-but-not-yet-terminal) requests.
# Beyond this the dispatcher replies `response{status:error}` instead of queueing
# the request, so a single flooding client cannot grow the shared pool queue (and
# its up-to-32-MiB blobs) without bound (FIX-5).
_MAX_IN_FLIGHT_PER_CONN = 32


class ProgressEmitter:
    """Lets a running handler push `progress{id}` frames for its request id.

    Passed to streaming handlers (e.g. SDXL in a later phase). Writes go through
    the connection write lock so progress never interleaves with other frames.
    """

    def __init__(self, dispatcher: "Dispatcher", request_id: int) -> None:
        self._dispatcher = dispatcher
        self._id = request_id

    def emit(self, fields: dict[str, Any], blob: bytes = b"") -> None:
        header: dict[str, Any] = dict(fields)
        header[HEADER_VERSION] = PROTOCOL_VERSION
        header[HEADER_ID] = self._id
        header[HEADER_KIND] = KIND_PROGRESS
        self._dispatcher._write(header, blob)


class Dispatcher:
    """Serves one v2 connection: read loop + worker dispatch + write serialization."""

    def __init__(
        self,
        reader: Any,
        writer: Any,
        ctx: HandlerContext,
        pool: ThreadPoolExecutor,
        events: EventBus,
        *,
        backend_version: str,
        sock: Any = None,
    ) -> None:
        self._reader = reader
        self._writer = writer
        self._ctx = ctx
        self._pool = pool
        self._events = events
        self._backend_version = backend_version

        self._write_lock = FrameWriteLock()
        # `sock` (when supplied) lets the event bus bound how long a single event
        # write may block on this connection, so a slow/dead client cannot stall
        # the publisher for everyone else (FIX-4).
        self._sink = EventSink(writer, self._write_lock, sock)
        self._handshake_done = False

        # Per-id cancellation registry. Guarded by `_cancels_lock`.
        self._cancels_lock = threading.Lock()
        self._cancels: dict[int, threading.Event] = {}

    # -- outbound framing -----------------------------------------------------

    def _write(self, header: dict[str, Any], blob: bytes = b"") -> None:
        """Write one whole frame under the connection write lock (atomic)."""
        with self._write_lock:
            write_frame(self._writer, header, blob)

    def _send_error_frame(self, request_id: int, message: str) -> None:
        """Send a protocol-level `kind:"error"` frame (§6). Best-effort."""
        header = {
            HEADER_VERSION: PROTOCOL_VERSION,
            HEADER_ID: int(request_id),
            HEADER_KIND: KIND_ERROR,
            HEADER_ERROR: message,
        }
        try:
            self._write(header)
        except Exception:  # noqa: BLE001 - peer already gone; nothing to do
            pass

    # -- main loop ------------------------------------------------------------

    def serve(self) -> None:
        """Run the read loop until the connection closes or a fatal error.

        Registers with the event bus only after a successful handshake, and
        always unregisters on exit so a closed socket stops receiving events.
        """
        try:
            while True:
                try:
                    header, blob = read_frame(self._reader)
                except StreamClosed:
                    return  # clean disconnect between frames
                except FrameError as exc:
                    # Corrupt/oversized frame: tell the client, then close.
                    self._send_error_frame(0, str(exc))
                    return
                except _DISCONNECT_ERRORS:
                    return
                except OSError:
                    return

                if not self._handshake_done:
                    if not self._do_handshake(header):
                        return
                    continue

                self._route(header, blob)
        except _DISCONNECT_ERRORS:
            return
        except Exception:  # noqa: BLE001 - never let one connection crash the server
            traceback.print_exc()
        finally:
            self._events.unregister(self._sink)

    def _do_handshake(self, header: dict[str, Any]) -> bool:
        """Validate and answer the client `hello`. Returns False to close."""
        if header.get(HEADER_KIND) != KIND_HELLO:
            self._send_error_frame(0, "Expected a hello frame before any request.")
            return False

        client_version = header.get(HEADER_VERSION)
        if client_version != PROTOCOL_VERSION:
            self._send_error_frame(
                0,
                f"Protocol version mismatch: server={PROTOCOL_VERSION}, "
                f"client={client_version!r}.",
            )
            return False

        reply = {
            HEADER_VERSION: PROTOCOL_VERSION,
            HEADER_ID: 0,
            HEADER_KIND: KIND_HELLO,
            HEADER_BACKEND_VERSION: self._backend_version,
        }
        try:
            self._write(reply)
        except Exception:  # noqa: BLE001 - peer vanished during handshake
            return False

        self._handshake_done = True
        # Now that the client is ready, subscribe it to server events.
        self._events.register(self._sink)
        return True

    def _route(self, header: dict[str, Any], blob: bytes) -> None:
        """Dispatch a post-handshake frame by kind."""
        kind = header.get(HEADER_KIND)
        if kind == KIND_REQUEST:
            self._dispatch_request(header, blob)
        elif kind == KIND_CANCEL:
            self._handle_cancel(header)
        elif kind == KIND_HELLO:
            # A non-request protocol error: PROTOCOL.md §6 mandates id=0 so the
            # client never misroutes it as a response to some request id the
            # client-supplied header happened to carry.
            self._send_error_frame(0, "Duplicate hello after handshake.")
        elif kind is None:
            self._send_error_frame(0, "Frame missing required 'kind' field.")
        else:
            # Unknown kind is likewise not attributable to a request -> id=0 (§6).
            self._send_error_frame(0, f"Unexpected frame kind: {kind!r}")

    def _handle_cancel(self, header: dict[str, Any]) -> None:
        """Set the cancel event for `id`. Unknown/finished id is a no-op (§3.3)."""
        request_id = header.get(HEADER_ID)
        if not isinstance(request_id, int):
            return
        with self._cancels_lock:
            event = self._cancels.get(request_id)
        if event is not None:
            event.set()

    def _dispatch_request(self, header: dict[str, Any], blob: bytes) -> None:
        """Validate a `request`, register cancellation, submit to the pool."""
        request_id = header.get(HEADER_ID)
        if not isinstance(request_id, int) or request_id <= 0:
            self._send_error_frame(0, "request frame requires a positive integer id.")
            return

        method = header.get(HEADER_METHOD)
        handler = get_handler(method) if isinstance(method, str) else None
        if handler is None:
            # Unknown method is a request failure, echoing the id (§6).
            self._send_response(
                request_id, STATUS_ERROR, {HEADER_ERROR: f"Unknown method: {method!r}"}
            )
            return

        cancel_event = threading.Event()
        with self._cancels_lock:
            if request_id in self._cancels:
                self._send_error_frame(request_id, f"Duplicate in-flight id: {request_id}.")
                return
            # Per-connection in-flight cap: reject (don't queue) once a client has
            # too many requests outstanding, so a flooder can't grow the shared
            # pool queue with up-to-32-MiB blobs without bound (FIX-5).
            if len(self._cancels) >= _MAX_IN_FLIGHT_PER_CONN:
                self._send_response(
                    request_id,
                    STATUS_ERROR,
                    {HEADER_ERROR: "too many in-flight requests"},
                )
                return
            self._cancels[request_id] = cancel_event

        # Submitting can raise if the pool is shut down (e.g. during teardown).
        # Without this guard the id would leak in `_cancels` and the client would
        # get no terminal frame (FIX-2). On failure, undo the registration and
        # report a terminal error (best-effort; the peer may already be gone).
        try:
            self._pool.submit(
                self._run_handler, handler, request_id, header, blob, cancel_event
            )
        except Exception as exc:  # noqa: BLE001 - pool rejected the submit
            with self._cancels_lock:
                self._cancels.pop(request_id, None)
            self._send_response(
                request_id, STATUS_ERROR, {HEADER_ERROR: f"backend unavailable: {exc}"}
            )

    def _run_handler(
        self,
        handler: Any,
        request_id: int,
        header: dict[str, Any],
        blob: bytes,
        cancel_event: threading.Event,
    ) -> None:
        """Worker-thread body: run the handler, emit the terminal frame."""
        # Give THIS request its own emitter, bound to its own id, on a per-request
        # SHALLOW COPY of the shared connection context (never mutate `self._ctx`).
        # `dataclasses.replace` copies the field references (`state`, `events`,
        # `get_health_snapshot`) verbatim, so everything else a handler reads
        # still resolves; only `progress_emitter` differs per request. Because
        # each worker thread sees its own copy, two concurrent requests on this
        # connection can never observe each other's emitter (no misrouted ids).
        request_ctx = dataclasses.replace(
            self._ctx, progress_emitter=ProgressEmitter(self, request_id)
        )
        try:
            try:
                fields, resp_blob = handler(request_ctx, header, blob, cancel_event)
            except Interrupted as exc:
                self._send_response(
                    request_id, STATUS_INTERRUPTED, {HEADER_ERROR: str(exc)} if str(exc) else {}
                )
                return
            except Exception as exc:  # noqa: BLE001 - handler failure -> response error
                if cancel_event.is_set():
                    self._send_response(request_id, STATUS_INTERRUPTED, {})
                else:
                    traceback.print_exc()
                    self._send_response(
                        request_id, STATUS_ERROR, {HEADER_ERROR: str(exc)}
                    )
                return

            if cancel_event.is_set():
                self._send_response(request_id, STATUS_INTERRUPTED, {})
                return

            self._send_response(request_id, STATUS_OK, fields, resp_blob)
        finally:
            with self._cancels_lock:
                self._cancels.pop(request_id, None)

    def _send_response(
        self,
        request_id: int,
        status: str,
        fields: dict[str, Any],
        blob: bytes = b"",
    ) -> None:
        """Emit the terminal `response{id, status, ...fields}` (+ optional blob).

        The reserved frame fields override any same-named handler result keys.
        """
        header: dict[str, Any] = dict(fields)
        header[HEADER_VERSION] = PROTOCOL_VERSION
        header[HEADER_ID] = int(request_id)
        header[HEADER_KIND] = KIND_RESPONSE
        header[HEADER_STATUS] = status
        try:
            self._write(header, blob)
        except _DISCONNECT_ERRORS:
            return
        except Exception:  # noqa: BLE001 - peer gone; the id is retired anyway
            return


def serve_connection(
    reader: Any,
    writer: Any,
    ctx: HandlerContext,
    pool: ThreadPoolExecutor,
    events: EventBus,
    *,
    backend_version: str,
    sock: Any = None,
) -> None:
    """Convenience entry point: build a `Dispatcher` and run its read loop.

    `sock` is the underlying connection socket (optional); when supplied it is
    handed to the event sink so the bus can bound per-write blocking (FIX-4).
    """
    Dispatcher(
        reader,
        writer,
        ctx,
        pool,
        events,
        backend_version=backend_version,
        sock=sock,
    ).serve()
