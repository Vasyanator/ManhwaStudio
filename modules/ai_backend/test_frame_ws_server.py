"""
File: modules/ai_backend/test_frame_ws_server.py

Purpose:
Tests for the WebSocket fallback transport
(`modules/ai_backend/ipc/frame_ws_server.py`).

Covers:
- smoke: WS handshake with the correct token is accepted, then the framed
  `hello` handshake + a `health` request/response round-trips over WS binary
  messages (proving the existing dispatcher runs unchanged over the WS
  transport);
- auth: a token mismatch (and a missing token) is rejected at the WS handshake;
- stream reassembly: a single frame split across two WS binary messages is still
  parsed, proving the receiver treats inbound binary payloads as one ordered
  byte stream rather than one-message-per-frame;
- concurrency (INV-A): PINGs interleaved with many response frames still all
  parse round-trip, proving the single writer thread + encoder lock keep the
  shared wsproto encoder/socket free of interleaved/corrupt frames;
- clean auth on hostile input (MINOR): a non-ASCII `?token=` yields a proper
  `RejectConnection` (401) rather than a `TypeError` traceback;
- slow/dead-client isolation (INV-B): once the bounded outbound queue fills, a
  producer `write` raises `BrokenPipeError` within the put timeout and tears the
  connection down instead of blocking the publisher forever.

All tests are hermetic: the server binds an ephemeral TCP port (0) and the
`health` method only reads the injected snapshot getter, so no torch / AI model
stack and no real AF_UNIX backend are involved. The bound port is discovered by
capturing the server's `MS_BACKEND_WS_PORT=<port>` stdout line.
"""

from __future__ import annotations

import socket
import sys
import threading
import time

import pytest

from modules.ai_backend.ipc import frame_ws_server
from modules.ai_backend.ipc.framing import encode_frame, read_frame
from modules.ai_backend.ipc.frame_ws_server import run_frame_ws_server
from modules.ai_backend.ipc.protocol import PROTOCOL_VERSION

from wsproto import ConnectionType, WSConnection
from wsproto.events import (
    AcceptConnection,
    BytesMessage,
    CloseConnection,
    Ping,
    Pong,
    RejectConnection,
    Request,
)

BACKEND_VERSION = "ws-smoke-1.0"
WS_TOKEN = "s3cr3t-handshake-token"


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------


def _make_snapshot() -> dict:
    return {
        "ok": True,
        "service": "mf_ai_backend",
        "backend_version": BACKEND_VERSION,
        "is_torch_available": False,
    }


class _PortCapture:
    """stdout tee that extracts the bound port from `MS_BACKEND_WS_PORT=<port>`.

    Passes every write through to the wrapped stream and, on the first line that
    announces the bound port, records it and sets `ready`.
    """

    def __init__(self, real) -> None:  # noqa: ANN001 - wraps any text stream
        self._real = real
        self.port: int | None = None
        self.ready = threading.Event()

    def write(self, s: str) -> int:
        if self.port is None and "MS_BACKEND_WS_PORT=" in s:
            for line in s.splitlines():
                line = line.strip()
                if line.startswith("MS_BACKEND_WS_PORT="):
                    try:
                        self.port = int(line.split("=", 1)[1])
                        self.ready.set()
                    except ValueError:
                        pass
        return self._real.write(s)

    def flush(self) -> None:
        self._real.flush()


class _WsClient:
    """Minimal in-process wsproto CLIENT used to drive the WS server.

    Performs the WS Upgrade handshake with `token` in the query string, then
    exposes a blocking `read(n)` (so `framing.read_frame` can consume the framed
    protocol carried in WS binary messages) plus `send_frame`/`send_raw_binary`
    to push frames. `accepted` is True/False after the handshake resolves.
    """

    def __init__(self, host: str, port: int, token: str, timeout: float = 5.0) -> None:
        self._ws = WSConnection(ConnectionType.CLIENT)
        self._sock = socket.create_connection((host, port), timeout=timeout)
        self._sock.settimeout(timeout)
        self._buf = bytearray()
        self.accepted: bool | None = None
        self.closed = False
        self._sock.sendall(
            self._ws.send(Request(host=host, target=f"/?token={token}"))
        )
        self._pump_until(lambda: self.accepted is not None)

    def _pump_once(self) -> bool:
        data = self._sock.recv(65536)
        if not data:
            self.closed = True
            return False
        self._ws.receive_data(data)
        for event in self._ws.events():
            if isinstance(event, AcceptConnection):
                self.accepted = True
            elif isinstance(event, RejectConnection):
                self.accepted = False
            elif isinstance(event, BytesMessage):
                self._buf += event.data
            elif isinstance(event, Ping):
                self._sock.sendall(self._ws.send(event.response()))
            elif isinstance(event, Pong):
                pass
            elif isinstance(event, CloseConnection):
                self.closed = True
        return True

    def _pump_until(self, pred) -> None:  # noqa: ANN001 - simple predicate
        while not pred():
            if not self._pump_once():
                break

    def read(self, n: int) -> bytes:
        while not self._buf and not self.closed:
            if not self._pump_once():
                break
        if not self._buf:
            return b""
        take = min(n, len(self._buf))
        chunk = bytes(self._buf[:take])
        del self._buf[:take]
        return chunk

    def send_frame(self, header: dict, blob: bytes = b"") -> None:
        self.send_raw_binary(encode_frame(header, blob))

    def send_raw_binary(self, data: bytes) -> None:
        self._sock.sendall(self._ws.send(BytesMessage(data=data)))

    def send_ping(self, payload: bytes = b"") -> None:
        """Send a WS control PING; the server answers it via its writer thread."""
        self._sock.sendall(self._ws.send(Ping(payload=payload)))

    def close(self) -> None:
        try:
            self._sock.close()
        except OSError:
            pass


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def ws_server():
    """Start run_frame_ws_server on an ephemeral port; yield the bound port."""
    stop_event = threading.Event()
    cap = _PortCapture(sys.stdout)
    old_stdout = sys.stdout
    # Redirect stdout only long enough to capture the announced port; the server
    # keeps printing after that (harmlessly) to the restored stream.
    sys.stdout = cap
    t = threading.Thread(
        target=run_frame_ws_server,
        args=(None, "127.0.0.1", 0, WS_TOKEN, stop_event),
        kwargs={
            "backend_version": BACKEND_VERSION,
            "get_health_snapshot": _make_snapshot,
        },
        daemon=True,
    )
    t.start()
    try:
        assert cap.ready.wait(5.0), "server did not print MS_BACKEND_WS_PORT in time"
    finally:
        sys.stdout = old_stdout
    assert cap.port is not None
    yield cap.port
    stop_event.set()
    t.join(timeout=5.0)


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_correct_token_hello_then_health_round_trip(ws_server) -> None:
    """Correct token: WS upgrade accepted, then hello+health round-trips."""
    client = _WsClient("127.0.0.1", ws_server, WS_TOKEN)
    try:
        assert client.accepted is True

        # Framed hello handshake over WS binary messages.
        client.send_frame({"v": PROTOCOL_VERSION, "id": 0, "kind": "hello"})
        hello_hdr, _ = read_frame(client)
        assert hello_hdr["kind"] == "hello"
        assert hello_hdr["v"] == PROTOCOL_VERSION
        assert hello_hdr["backend_version"] == BACKEND_VERSION

        # Health request -> valid response, proving the shared dispatcher runs.
        client.send_frame(
            {"v": PROTOCOL_VERSION, "id": 1, "kind": "request", "method": "health"}
        )
        resp_hdr, resp_blob = read_frame(client)
        assert resp_hdr["id"] == 1
        assert resp_hdr["kind"] == "response"
        assert resp_hdr["status"] == "ok"
        assert resp_hdr["service"] == "mf_ai_backend"
        assert resp_hdr["backend_version"] == BACKEND_VERSION
        assert resp_blob == b""
    finally:
        client.close()


def test_wrong_token_is_rejected(ws_server) -> None:
    """A mismatched handshake token is rejected (HTTP 401) at the WS upgrade."""
    client = _WsClient("127.0.0.1", ws_server, "definitely-wrong-token")
    try:
        assert client.accepted is False
    finally:
        client.close()


def test_missing_token_is_rejected(ws_server) -> None:
    """A handshake with a blank/missing `token` param is rejected."""
    # token="" yields target "/?token=", whose empty value parse_qs drops, so the
    # server sees no token and must reject.
    client = _WsClient("127.0.0.1", ws_server, "")
    try:
        assert client.accepted is False
    finally:
        client.close()


def test_frame_split_across_two_ws_messages(ws_server) -> None:
    """A single frame split over two WS binary messages is still parsed.

    Proves the server treats inbound binary payloads as one ordered byte stream
    (delimited by read_frame length prefixes), not one-message-per-frame.
    """
    client = _WsClient("127.0.0.1", ws_server, WS_TOKEN)
    try:
        assert client.accepted is True
        client.send_frame({"v": PROTOCOL_VERSION, "id": 0, "kind": "hello"})
        hello_hdr, _ = read_frame(client)
        assert hello_hdr["kind"] == "hello"

        # Encode one health request frame and split it across two binary WS
        # messages at an arbitrary interior offset.
        frame = encode_frame(
            {"v": PROTOCOL_VERSION, "id": 7, "kind": "request", "method": "health"}
        )
        split = max(1, len(frame) // 2)
        client.send_raw_binary(frame[:split])
        client.send_raw_binary(frame[split:])

        resp_hdr, _ = read_frame(client)
        assert resp_hdr["id"] == 7
        assert resp_hdr["kind"] == "response"
        assert resp_hdr["status"] == "ok"
    finally:
        client.close()


def test_pings_interleaved_with_frames_do_not_corrupt_stream(ws_server) -> None:
    """Concurrency (INV-A): PINGs interleaved with response frames stay parseable.

    The read thread answers each PING (enqueuing a PONG) while worker threads
    enqueue `response` frames; a single writer thread encodes+sends all of them,
    and `_ws_lock` serializes the shared wsproto encoder against the read thread's
    `receive_data`. If any of those paths raced the encoder or the socket, the WS
    binary framing would corrupt and `read_frame` would fail to parse the stream.
    We fire many request/PING pairs, then assert every framed response round-trips
    with `status == "ok"` and all ids are accounted for.
    """
    # Stay strictly under the dispatcher's per-connection in-flight cap (32) so no
    # request is ever rejected with "too many in-flight requests"; every response
    # must be a clean ok.
    count = 25
    client = _WsClient("127.0.0.1", ws_server, WS_TOKEN)
    try:
        assert client.accepted is True
        client.send_frame({"v": PROTOCOL_VERSION, "id": 0, "kind": "hello"})
        hello_hdr, _ = read_frame(client)
        assert hello_hdr["kind"] == "hello"

        # Interleave a control PING before each request so the server's read
        # thread and writer thread are exercised concurrently.
        for i in range(1, count + 1):
            client.send_ping(f"ping-{i}".encode())
            client.send_frame(
                {"v": PROTOCOL_VERSION, "id": i, "kind": "request", "method": "health"}
            )

        # Responses may arrive in any order (worker pool); collect all ids.
        seen: set[int] = set()
        for _ in range(count):
            resp_hdr, resp_blob = read_frame(client)
            assert resp_hdr["kind"] == "response"
            assert resp_hdr["status"] == "ok"
            assert resp_blob == b""
            seen.add(resp_hdr["id"])
        assert seen == set(range(1, count + 1))
    finally:
        client.close()


def test_non_ascii_token_is_rejected_cleanly(ws_server) -> None:
    """A non-ASCII `?token=` yields a clean 401 reject, never a traceback (MINOR).

    `caf%C3%A9` percent-decodes (via `parse_qs`) to the non-ASCII str "café".
    Comparing two `str`s with `hmac.compare_digest` would raise `TypeError` on
    such input, which would escape the handshake as a logged traceback and never
    send a `RejectConnection`. The fixed server compares the UTF-8 BYTES of both
    tokens, so a garbled/hostile token simply fails closed: the client observes a
    proper `RejectConnection` (`accepted is False`), not a dropped socket.
    """
    client = _WsClient("127.0.0.1", ws_server, "caf%C3%A9")
    try:
        assert client.accepted is False
    finally:
        client.close()


class _NeverDrainingSocket:
    """Minimal socket stand-in whose peer never drains the outbound queue.

    Supports only what `_WsStreamAdapter.__init__`/`write`/teardown touch:
    `getpeername` (diagnostics), `close` (teardown), and inert `sendall`/`recv`.
    The writer thread is intentionally never started in the test, so the bounded
    outbound queue fills and stays full — modelling a client that has stopped
    reading. `close()` records that the connection was torn down.
    """

    def __init__(self) -> None:
        self.closed = False

    def getpeername(self):  # noqa: ANN201 - stdlib-shaped stub
        return ("127.0.0.1", 65000)

    def close(self) -> None:
        self.closed = True

    def sendall(self, data: bytes) -> None:  # pragma: no cover - writer not started
        return None

    def recv(self, n: int) -> bytes:  # pragma: no cover - read loop not started
        return b""


def test_write_isolates_wedged_client(monkeypatch) -> None:
    """Slow/dead-client isolation (INV-B): `write` returns bounded, not forever.

    A client that stops draining lets the bounded outbound queue fill. The next
    producer `write` must NOT block indefinitely (which would stall the publisher
    / health worker for every other client); it blocks at most the put timeout,
    then tears the connection down and raises `BrokenPipeError`. We shrink the
    queue and timeout, fill the queue without a draining writer thread, and assert
    the overflowing `write` raises within the bound and closed the socket.
    """
    # Shrink both bounds BEFORE constructing the adapter: `__init__` snapshots the
    # module globals, so tests can make the wedge deterministic.
    monkeypatch.setattr(frame_ws_server, "_OUTBOUND_QUEUE_MAXSIZE", 2)
    monkeypatch.setattr(frame_ws_server, "_OUTBOUND_PUT_TIMEOUT_S", 0.2)

    sock = _NeverDrainingSocket()
    adapter = frame_ws_server._WsStreamAdapter(sock, WS_TOKEN)
    # Deliberately do NOT call start_writer(): nothing drains the queue, exactly
    # like a client whose kernel send-buffer is wedged full.

    # Fill the queue to capacity (maxsize 2). These enqueue without blocking.
    assert adapter.write(b"frame-a") == len(b"frame-a")
    assert adapter.write(b"frame-b") == len(b"frame-b")

    # The third write finds the queue full; it must give up after ~the timeout and
    # raise instead of blocking forever.
    start = time.monotonic()
    with pytest.raises(BrokenPipeError):
        adapter.write(b"frame-c")
    elapsed = time.monotonic() - start

    # Bounded by the (shrunk) put timeout, not indefinite. Generous ceiling to
    # avoid CI flakiness while still proving it is not blocking forever.
    assert elapsed < 2.0
    # The wedged connection was torn down so producers stop targeting it.
    assert sock.closed is True
    assert adapter._closed is True
