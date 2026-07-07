"""
File: modules/ai_backend/ipc/frame_ws_server.py

Purpose:
WebSocket fallback transport for the framed, multiplexed Rust <-> Python IPC
protocol. Python is the WS SERVER (bound to `127.0.0.1:<ephemeral port>`); Rust
is the WS CLIENT. This is an alternative to the AF_UNIX transport in
`frame_server.py`; it exists for platforms/configs where AF_UNIX is unavailable
or undesired. The exact same `serve_connection`/`Dispatcher`/handler stack runs
on top of it — only the byte transport differs.

Wire contract (shared with the Rust client, do not deviate):
- Handshake URL `ws://127.0.0.1:<port>/?token=<token>`. The server extracts the
  `token` query param from the WS handshake request target and compares it with
  `hmac.compare_digest` to the configured `ws_token`; a mismatch (or a missing
  token) is rejected with HTTP 401 (`RejectConnection`) and the connection is
  closed. A match accepts the upgrade.
- After the upgrade, the EXISTING frame codec bytes
  `[u32 BE header_len][header_json][u32 BE blob_len][blob]` (see `framing.py`)
  travel as the payload of WS BINARY messages. The sender sends one full frame
  per binary message, but the receiver treats the concatenation of ALL inbound
  binary-message payloads as one ordered byte stream and relies on
  `read_frame`'s length prefixes to delimit frames (it does NOT assume one WS
  message == one frame). PING is answered with PONG; CLOSE/EOF ends the stream.
- After the listener binds, exactly one line `MS_BACKEND_WS_PORT=<port>` is
  printed to stdout and flushed so the Rust supervisor can parse the bound port.

Key structures:
- `FrameWsServer`: `ThreadingMixIn + TCPServer` bound to `(ws_host, ws_port)`.
- `_WsStreamAdapter`: per-connection byte-stream adapter exposing the duck-typed
  `read(n)` / `write(data)` / `flush()` interface `framing.py` needs, backed by
  a `wsproto.WSConnection` over the accepted TCP socket.

Outbound writer thread (concurrency contract):
Each connection runs ONE dedicated outbound writer thread. It is the ONLY place
that calls `self._ws.send(...)` and `self._sock.sendall(...)` after the
handshake, so the shared `wsproto` encoder and the raw socket are touched by a
single thread — no interleaved/corrupt WS frames even when a response is being
written while the read thread must reply to a PING or echo a CLOSE. Callers
(`write` on pool/publisher threads, the read thread's PING/CLOSE handling) only
ENQUEUE an intent onto a bounded queue; they never touch the encoder or socket
themselves. The read thread's `receive_data`/`events` still shares the one
non-thread-safe `wsproto.WSConnection` with the writer's `send`, so a small
encoder lock (`_ws_lock`) guards every `WSConnection` touch. It is held only
around the non-blocking encode/parse — never across a blocking `recv`/`sendall`
or a queue put — so it serializes the encoder without ever coupling the two
threads' blocking I/O. `write` uses a bounded put with a timeout: if a wedged client keeps
the queue full past the timeout, the connection is torn down (socket closed)
instead of blocking the publisher forever — this restores the slow/dead-client
isolation the AF_UNIX path gets from `EventSink`'s per-write timeout (FIX-4).

Key functions:
- `_build_ws_handler(...)`: builds the `BaseRequestHandler` subclass.
- `run_frame_ws_server(...)`: builds the shared runtime objects and serves until
  `stop_event` is set (mirrors `frame_server.run_frame_server`).

Notes:
This module is intentionally free of any heavy/torch imports (like
`frame_server.py`) so the framing, dispatcher, and smoke tests can import it
without the AI model stack. `wsproto` (pure-Python, pulls `h11`) is required.
"""

from __future__ import annotations

import errno
import hmac
import queue
import socket
import socketserver
import sys
import threading
import traceback
from concurrent.futures import ThreadPoolExecutor
from typing import Any, Callable
from urllib.parse import parse_qs, urlsplit

from wsproto import ConnectionType, WSConnection
from wsproto.events import (
    AcceptConnection,
    BytesMessage,
    CloseConnection,
    Ping,
    Pong,
    RejectConnection,
    Request,
    TextMessage,
)

from .dispatcher import serve_connection
from .events import EventBus
from .registry import HandlerContext

# Worker pool size for concurrent request handlers across all connections. Kept
# identical to `frame_server._DEFAULT_MAX_WORKERS` so both transports behave the
# same under load.
_DEFAULT_MAX_WORKERS = 16

# Size of each raw `recv` from the accepted TCP socket before feeding wsproto.
_RECV_CHUNK_BYTES = 65536

# HTTP status returned when the handshake token is missing or does not match.
_REJECT_STATUS_UNAUTHORIZED = 401

# Bound on the per-connection outbound queue. A healthy client drains far faster
# than frames are produced, so this stays near-empty; it only fills when a client
# stops reading, at which point the bounded put below detects the wedge.
_OUTBOUND_QUEUE_MAXSIZE = 512

# Max seconds a producer (a pool/publisher thread in `write`, or the read thread
# enqueuing a PONG/CLOSE) may block waiting for room in a full outbound queue
# before the connection is declared wedged and torn down. Kept equal to the
# AF_UNIX sink timeout (`events._PUBLISH_WRITE_TIMEOUT_S`) so both transports
# isolate a slow/dead client on the same bound (FIX-4 parity).
_OUTBOUND_PUT_TIMEOUT_S = 2.0

# Outbound-intent tags placed on the queue. The writer thread turns each into a
# wsproto send; the producer never touches the encoder or socket.
_OUT_DATA = "data"  # payload: bytes (one full frame) -> BytesMessage
_OUT_PONG = "pong"  # payload: bytes (ping payload) -> Pong
_OUT_CLOSE = "close"  # payload: tuple[int | None, str | None] (code, reason)

# Sentinel pushed to stop the writer thread after it has drained the queue.
_WRITER_STOP = object()


class FrameWsServer(socketserver.ThreadingMixIn, socketserver.TCPServer):
    """Threaded WebSocket-transport frame server bound to a TCP host/port.

    Bound to `(ws_host, ws_port)`; pass `ws_port == 0` for an ephemeral port and
    read the actual port back from `server_address[1]` after construction. Each
    accepted connection is served on its own daemon thread by the handler from
    `_build_ws_handler`. Connection-reset/abort/broken-pipe errors are swallowed
    (mirrors `frame_server.FrameUnixServer.handle_error`).
    """

    daemon_threads = True
    # WS clients reconnect on the same ephemeral/fixed port across restarts;
    # allow immediate rebind so a quick restart is not blocked by TIME_WAIT.
    allow_reuse_address = True

    def handle_error(self, request, client_address):  # noqa: ANN001 - stdlib signature
        """Swallow benign client-disconnect errors; log anything else.

        A client that resets/aborts/breaks the pipe mid-request is normal, not a
        server bug, so those are silenced. Any other exception is reported with a
        traceback, matching the AF_UNIX server's behavior.
        """
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
            "[AI Backend][ws_frame_error] "
            f"client={client_address!r} error={type(exc).__name__}: {exc}",
            flush=True,
        )
        traceback.print_exc()


class _WsStreamAdapter:
    """Blocking byte-stream over one accepted WS connection for `framing.py`.

    Wraps a `wsproto.WSConnection(SERVER)` on the accepted TCP socket and exposes
    exactly the duck-typed interface the frame codec needs: `read(n)`,
    `write(data)`, and `flush()`. Inbound WS BINARY-message payloads are
    concatenated into one ordered byte stream (`read_frame`'s length prefixes
    delimit frames), so message fragmentation / CONTINUATION is transparent.
    PING is answered with PONG; a CLOSE or socket EOF ends the stream (`read`
    returns `b""` so `read_frame` observes a clean `StreamClosed`).

    Threading contract: the read thread (`read`/`perform_handshake`) is the only
    caller of `self._ws.receive_data(...)`; the dedicated writer thread
    (`start_writer`) is the only caller of `self._ws.send(...)` and
    `self._sock.sendall(...)` once the handshake is done. Every outbound path —
    `write` (pool/publisher threads) and the read thread's PING/CLOSE handling —
    only enqueues an intent; it never encodes or writes. This makes the socket
    single-writer, so concurrent producers cannot corrupt the wire. The read and
    writer threads still share the one non-thread-safe `WSConnection`, so
    `_ws_lock` serializes every access to it (`receive_data`/`events` on the read
    side, `send` on the writer side). That lock is held only around the
    non-blocking encode/parse, never across a blocking `recv`/`sendall`.

    The same instance also performs the HTTP Upgrade handshake and token check
    via `perform_handshake` before any frame bytes flow (and before the writer
    thread starts, so the `AcceptConnection`/`RejectConnection` bytes are sent
    inline on the read thread while no other thread can touch the encoder).
    """

    def __init__(self, sock: socket.socket, ws_token: str) -> None:
        self._sock = sock
        self._token = ws_token
        self._ws = WSConnection(ConnectionType.SERVER)
        # Serializes access to the non-thread-safe `wsproto` state machine between
        # the read thread (`receive_data` + `events`) and the writer thread
        # (`send`). A `WSConnection` shares mutable state across its send/receive
        # paths, so concurrent calls corrupt the encoder. Held ONLY around the
        # non-blocking encode/parse — never across a blocking `recv`/`sendall` or
        # a bounded queue put — so a wedged socket cannot stall the other thread.
        self._ws_lock = threading.Lock()
        # Inbound application byte stream awaiting `read` consumption.
        self._inbuf = bytearray()
        # True once a CLOSE frame, socket EOF, or a teardown was observed; no
        # more app bytes flow and further `write`s are refused.
        self._closed = False
        # Handshake result: None until the client `Request` is processed, then
        # True (accepted) / False (rejected or missing/mismatched token).
        self._handshake_ok: bool | None = None

        # Peer address captured up front for backpressure diagnostics (it is
        # unavailable once the socket is torn down). Never contains secrets.
        try:
            self._peer: Any = sock.getpeername()
        except OSError:
            self._peer = None

        # Bounded outbound queue drained by the single writer thread. Read module
        # globals here so tests can shrink them before a connection is accepted.
        self._outq: queue.Queue[object] = queue.Queue(maxsize=_OUTBOUND_QUEUE_MAXSIZE)
        self._put_timeout = _OUTBOUND_PUT_TIMEOUT_S
        self._writer_thread: threading.Thread | None = None
        self._writer_started = False

    # -- handshake ------------------------------------------------------------

    def perform_handshake(self) -> bool:
        """Drive the WS Upgrade handshake; return True iff the token matched.

        Reads raw bytes from the socket, feeds wsproto, and on the `Request`
        event validates the `token` query param against the configured token
        (constant-time). On match sends `AcceptConnection`; otherwise sends a
        `RejectConnection` (HTTP 401). Returns False on a mismatch or on socket
        EOF before the handshake completed.
        """
        while self._handshake_ok is None:
            if not self._recv_into_ws():
                # Socket closed before the client sent a complete Upgrade
                # request: treat as a failed handshake.
                return False
        return self._handshake_ok

    def _on_request(self, event: Request) -> None:
        """Validate the handshake token and accept or reject the upgrade."""
        target = event.target
        if isinstance(target, bytes):
            # wsproto normally decodes this to str; decode defensively otherwise.
            target = target.decode("latin-1")
        token = _extract_token(target)
        # Constant-time compare on the UTF-8 BYTES of both tokens. A missing
        # token is an immediate reject and must never reach `compare_digest`.
        # Comparing as bytes (not str) is deliberate: `hmac.compare_digest` on
        # two `str`s raises `TypeError` for any non-ASCII code point, so a
        # hostile/garbled percent-decoded token would otherwise emit a noisy
        # traceback. Encoding both sides keeps the check failing closed with no
        # traceback for any input.
        matched = token is not None and hmac.compare_digest(
            token.encode("utf-8"), self._token.encode("utf-8")
        )
        if matched:
            self._sock.sendall(self._ws.send(AcceptConnection()))
            self._handshake_ok = True
        else:
            self._sock.sendall(
                self._ws.send(
                    RejectConnection(status_code=_REJECT_STATUS_UNAUTHORIZED)
                )
            )
            self._handshake_ok = False

    # -- inbound pump ---------------------------------------------------------

    def _recv_into_ws(self) -> bool:
        """Read one chunk from the socket, feed wsproto, process its events.

        Returns False on socket EOF (clean peer close at the TCP level); True
        otherwise. Application BINARY payloads are appended to `_inbuf`, PINGs
        are answered with PONGs, and a WS CLOSE marks the stream closed.
        """
        data = self._sock.recv(_RECV_CHUNK_BYTES)
        if not data:
            # TCP EOF: no more bytes will ever arrive.
            self._closed = True
            return False
        # Feed wsproto and drain its event list under the encoder lock: the writer
        # thread may be calling `self._ws.send(...)` concurrently and a
        # `WSConnection` is not safe for concurrent send/receive. The lock is held
        # ONLY around the non-blocking parse — not across the `recv` above nor the
        # event handling below, because `_process_event` may enqueue a PONG/CLOSE
        # with a bounded wait, and blocking on a full queue while holding this lock
        # would deadlock the writer thread (which needs the lock to encode/drain).
        with self._ws_lock:
            self._ws.receive_data(data)
            events = list(self._ws.events())
        for event in events:
            self._process_event(event)
        return True

    def _process_event(self, event: Any) -> None:
        """Dispatch one wsproto inbound event.

        Only events a SERVER can receive are handled explicitly; anything else
        (e.g. a stray client-only event) is ignored rather than crashing the
        connection.
        """
        if isinstance(event, Request):
            self._on_request(event)
        elif isinstance(event, BytesMessage):
            # Concatenate every binary payload chunk into the ordered byte
            # stream; `read_frame` length prefixes delimit frames, so we do not
            # care about WS message/fragment boundaries here.
            self._inbuf += event.data
        elif isinstance(event, TextMessage):
            # The wire protocol only uses BINARY messages; a TEXT message is not
            # part of the contract. Ignore its payload rather than corrupt the
            # binary frame stream with it.
            pass
        elif isinstance(event, Ping):
            # Do NOT send inline: only the writer thread may touch the encoder
            # and socket. Enqueue the PONG intent (the ping payload); the writer
            # thread builds and sends the `Pong`.
            self._enqueue_outbound((_OUT_PONG, event.payload), "pong")
        elif isinstance(event, Pong):
            # Unsolicited/keepalive pong: nothing to do.
            pass
        elif isinstance(event, CloseConnection):
            # Enqueue the close echo (carrying the peer's code/reason) for the
            # writer thread, then mark the stream done so `read` reports a clean
            # EOF once buffered bytes are drained. Enqueue BEFORE setting closed
            # so the intent is not dropped by the closed-stream guard.
            self._enqueue_outbound((_OUT_CLOSE, (event.code, event.reason)), "close")
            self._closed = True
        else:
            # AcceptConnection / RejectConnection / RejectData are client-side
            # inbound events; a server never receives them. Ignore defensively.
            pass

    # -- framing.py duck-typed stream interface -------------------------------

    def read(self, n: int) -> bytes:
        """Return up to `n` bytes from the inbound stream, blocking as needed.

        Serves buffered bytes first; when the buffer is empty it pumps the
        socket until data arrives or the stream closes. Returns `b""` only at a
        true stream end (WS CLOSE or TCP EOF with an empty buffer), which
        `read_frame` interprets as a clean `StreamClosed` at a frame boundary.
        """
        if n <= 0:
            return b""
        while not self._inbuf and not self._closed:
            if not self._recv_into_ws():
                break
        if not self._inbuf:
            return b""
        take = min(n, len(self._inbuf))
        chunk = bytes(self._inbuf[:take])
        del self._inbuf[:take]
        return chunk

    def write(self, data: bytes) -> int:
        """Enqueue `data` as one outbound WS BINARY message; return its length.

        Called by `write_frame` on pool/publisher threads. The frame codec builds
        a whole frame in memory and hands it here in one call, so one `write`
        maps to exactly one binary message (one full frame per message, per the
        wire contract). The bytes are NOT sent here: they are enqueued for the
        single writer thread, so producers never touch the shared encoder/socket.
        Always reports the full length so the codec's write-all loop completes in
        one iteration.

        The put is bounded by `_OUTBOUND_PUT_TIMEOUT_S`. If the queue stays full
        past the timeout the client is wedged (not draining); the connection is
        torn down and `BrokenPipeError` is raised so the caller (a dispatcher
        write or an `EventSink` publish) treats this sink as dead — a slow/dead
        peer can never block the publisher for everyone else (FIX-4 parity).

        # Errors
        Raises `BrokenPipeError` when the stream is already closed or when the
        outbound queue stays full past the timeout (wedged client).
        """
        if self._closed:
            raise BrokenPipeError("WS outbound stream is closed")
        payload = bytes(data)
        self._enqueue_outbound((_OUT_DATA, payload), "data", payload_bytes=len(payload))
        return len(payload)

    def flush(self) -> None:
        """No-op: the FIFO queue preserves order and the writer thread sends.

        A blocking flush would let a wedged client stall the caller, defeating
        the queue's isolation; ordering is already guaranteed by the FIFO queue.
        """
        return None

    # -- outbound writer thread ----------------------------------------------

    def start_writer(self) -> None:
        """Start the single outbound writer thread. Call once, after handshake.

        Idempotent. The writer thread is the ONLY caller of `self._ws.send(...)`
        and `self._sock.sendall(...)` from this point on, so the wsproto encoder
        and the socket are written by exactly one thread.
        """
        if self._writer_started:
            return
        self._writer_started = True
        self._writer_thread = threading.Thread(
            target=self._run_writer, name="ipc-ws-writer", daemon=True
        )
        self._writer_thread.start()

    def _run_writer(self) -> None:
        """Drain the outbound queue, encoding+sending each intent, until stopped.

        Exits on the `_WRITER_STOP` sentinel (clean stop, after draining queued
        frames such as a CLOSE echo) or on a socket `OSError` (peer gone). On an
        `OSError` it tears the connection down so the read loop unblocks; it does
        not log, because a peer dropping mid-stream is a normal disconnect.
        """
        while True:
            item = self._outq.get()
            if item is _WRITER_STOP:
                return
            try:
                self._send_item(item)  # type: ignore[arg-type] - not the sentinel
            except OSError:
                # Client socket broke mid-send: stop writing and force teardown
                # so the read thread's recv unblocks and serve_connection ends.
                self._closed = True
                self._close_socket()
                return

    def _send_item(self, item: tuple[str, Any]) -> None:
        """Encode and send one outbound intent. Writer-thread only.

        `item` is `(tag, payload)`; the tag selects the wsproto event built and
        sent here (never on a producer thread). May raise `OSError` if the socket
        is broken; the caller (`_run_writer`) handles that as a teardown.
        """
        tag, payload = item
        if tag == _OUT_DATA:
            self._sock.sendall(self._encode(BytesMessage(data=payload)))
        elif tag == _OUT_PONG:
            self._sock.sendall(self._encode(Pong(payload=payload)))
        elif tag == _OUT_CLOSE:
            code, reason = payload
            self._sock.sendall(self._encode(CloseConnection(code=code, reason=reason)))
        else:
            # Only the three tags above are ever enqueued; ignore anything else
            # defensively rather than crash the connection's writer thread.
            pass

    def _encode(self, event: Any) -> bytes:
        """Encode one outbound wsproto event to bytes under the encoder lock.

        Serializes the shared `WSConnection` with the read thread's
        `receive_data`/`events` (wsproto is not concurrent-safe). The lock is held
        only around this non-blocking encode, never across the following blocking
        `sendall`, so a wedged socket can never stall the read thread.
        """
        with self._ws_lock:
            return self._ws.send(event)

    def _enqueue_outbound(
        self, item: tuple[str, Any], what: str, *, payload_bytes: int | None = None
    ) -> None:
        """Put one outbound intent on the queue with a bounded wait.

        `what` is a short label for the backpressure log. On a full queue past
        the timeout the client is wedged: the connection is torn down (socket
        closed, stream marked closed) and `BrokenPipeError` is raised so the
        producer (dispatcher write or `EventSink` publish) sees a dead sink.

        # Errors
        Raises `BrokenPipeError` when the bounded put times out (wedged client).
        """
        try:
            self._outq.put(item, timeout=self._put_timeout)
        except queue.Full:
            self._backpressure_teardown(what, payload_bytes)
            raise BrokenPipeError(
                "WS outbound queue full; client not draining"
            ) from None

    def _backpressure_teardown(self, what: str, payload_bytes: int | None) -> None:
        """Isolate a wedged client: mark closed, close the socket, log once.

        Mirrors the AF_UNIX `EventSink` timeout behavior (FIX-4): a client that
        keeps the outbound queue full is dropped so it cannot block the publisher
        (and thus every other client) indefinitely. Closing the socket also
        unblocks the writer thread's stuck `sendall` and the read thread's
        `recv`, ending `serve_connection`. The log carries no secrets.
        """
        already = self._closed
        self._closed = True
        self._close_socket()
        if not already:
            detail = "" if payload_bytes is None else f" payload_bytes={payload_bytes}"
            print(
                "[AI Backend][ws_backpressure_teardown] "
                f"client={self._peer!r} intent={what}{detail} "
                f"timeout_s={self._put_timeout}: client not draining, "
                "connection dropped",
                flush=True,
            )

    def _close_socket(self) -> None:
        """Close the TCP socket, swallowing an already-closed error."""
        try:
            self._sock.close()
        except OSError:
            pass

    def shutdown(self) -> None:
        """Stop the writer thread and release the connection. Idempotent.

        Marks the stream closed, pushes the stop sentinel so the writer drains
        any already-queued frames (e.g. a CLOSE echo) and exits, then joins it
        with a bound. If the writer is wedged in `sendall` to a dead client, the
        socket is force-closed so the blocking send raises and the thread exits,
        guaranteeing no thread leak on any exit path.
        """
        self._closed = True
        # Best-effort sentinel: a full queue means the writer is busy in sendall,
        # and the socket close below is what unblocks it in that case.
        try:
            self._outq.put_nowait(_WRITER_STOP)
        except queue.Full:
            pass
        thread = self._writer_thread
        if thread is not None and thread.is_alive():
            thread.join(timeout=self._put_timeout)
            if thread.is_alive():
                # Writer stuck sending to a wedged client: force the socket shut
                # so the blocking sendall raises, then join again.
                self._close_socket()
                thread.join(timeout=self._put_timeout)
        self._close_socket()


def _extract_token(target: str) -> str | None:
    """Return the `token` query param from a WS request target, or None.

    `target` is the handshake request-URI (e.g. `/?token=abc`). Returns the
    first `token` value, or None when the param is absent (which the caller
    treats as an authentication failure).
    """
    query = urlsplit(target).query
    values = parse_qs(query).get("token")
    if not values:
        return None
    return values[0]


def _build_ws_handler(
    ctx: HandlerContext,
    pool: ThreadPoolExecutor,
    events: EventBus,
    backend_version: str,
    ws_token: str,
):
    """Build a `BaseRequestHandler` subclass bound to the shared runtime objects.

    Each accepted connection performs the WS handshake + token check, then runs
    the SAME `serve_connection` used by the AF_UNIX transport over a
    `_WsStreamAdapter`. `sock=None` is passed to `serve_connection`: the raw TCP
    socket must NOT be handed to the event bus, because a per-write
    `settimeout` on it would corrupt WS framing (events.py explicitly supports
    `sock=None`).
    """

    class _WsHandler(socketserver.BaseRequestHandler):
        def handle(self) -> None:  # noqa: D401 - stdlib API
            adapter = _WsStreamAdapter(self.request, ws_token)
            try:
                if not adapter.perform_handshake():
                    # Rejected (bad/missing token) or a closed socket: the reject
                    # response was already sent; just drop the connection.
                    return
                # Start the single outbound writer thread only after the accept
                # bytes were sent inline, so the encoder becomes single-writer
                # exactly when the first frame could be produced.
                adapter.start_writer()
                serve_connection(
                    adapter,
                    adapter,
                    ctx,
                    pool,
                    events,
                    backend_version=backend_version,
                    # Never hand the raw TCP socket to the event bus: a per-write
                    # settimeout would corrupt the WS framing. events.py supports
                    # sock=None; slow/dead-client isolation is instead provided by
                    # the adapter's bounded outbound queue (FIX-4 parity).
                    sock=None,
                )
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                # Normal client disconnect mid-stream; nothing to clean up.
                pass
            finally:
                # Always stop the writer thread and close the socket, on every
                # exit path (clean close, EOF, error, or backpressure teardown),
                # so no writer thread is leaked.
                adapter.shutdown()

    return _WsHandler


def run_frame_ws_server(
    state: Any,
    ws_host: str,
    ws_port: int,
    ws_token: str,
    stop_event: threading.Event,
    *,
    backend_version: str,
    get_health_snapshot: Callable[[], dict[str, Any]],
    events: EventBus | None = None,
    max_workers: int = _DEFAULT_MAX_WORKERS,
) -> EventBus:
    """Serve the frame protocol over WebSocket on `(ws_host, ws_port)` until stop.

    A near-clone of `frame_server.run_frame_server` for the WS transport: builds
    the shared `EventBus` (returned so the health worker can publish to it), the
    worker `ThreadPoolExecutor`, and the `HandlerContext`, constructs a
    `FrameWsServer`, prints the actual bound port as `MS_BACKEND_WS_PORT=<port>`
    (so the Rust supervisor can parse it), then runs `serve_forever` until
    `stop_event` is set by a stop-watcher thread.

    `ws_port == 0` binds an ephemeral port. `ws_token` must be a non-empty string
    (the handshake compares it constant-time); it is fixed for the process
    lifetime. Returns the `EventBus` used by the server (the same object passed
    via `events`, or a freshly created one).
    """
    if not ws_token:
        raise ValueError(
            "run_frame_ws_server requires a non-empty ws_token for the WS "
            "handshake; refusing to serve an unauthenticated WebSocket."
        )

    bus = events if events is not None else EventBus()
    pool = ThreadPoolExecutor(
        max_workers=max_workers, thread_name_prefix="ipc-ws-worker"
    )
    ctx = HandlerContext(
        state=state,
        events=bus,
        get_health_snapshot=get_health_snapshot,
    )
    handler_cls = _build_ws_handler(ctx, pool, bus, backend_version, ws_token)
    server = FrameWsServer((ws_host, ws_port), handler_cls)

    # Read the actual bound port (ephemeral when ws_port==0) and hand it to the
    # Rust supervisor via a single parseable stdout line.
    actual_port = server.server_address[1]
    print(f"MS_BACKEND_WS_PORT={actual_port}", flush=True)

    # Watch the stop event and trigger an orderly shutdown of serve_forever.
    def _watch_stop() -> None:
        stop_event.wait()
        try:
            server.shutdown()
        except Exception:  # noqa: BLE001 - shutdown races with teardown; ignore
            pass

    watcher = threading.Thread(
        target=_watch_stop, name="ipc-ws-frame-stop-watch", daemon=True
    )
    watcher.start()

    print(
        f"[AI Backend] frame server on ws://{ws_host}:{actual_port}", flush=True
    )
    try:
        server.serve_forever(poll_interval=0.25)
    finally:
        server.server_close()
        pool.shutdown(wait=False)
    return bus
