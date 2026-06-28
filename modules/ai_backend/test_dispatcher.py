"""
File: modules/ai_backend/test_dispatcher.py

Purpose:
Unit tests for the v2 IPC per-connection dispatcher
(`modules/ai_backend/ipc/dispatcher.py`).

Main responsibilities:
- hello handshake: success reply and version-mismatch error + close;
- request -> response round-trip via a fake registered handler;
- cancel flips the per-id cancel event and yields `status:"interrupted"`;
- multiplexing: two concurrent ids are routed to and answered for the right id.

Notes:
A `socket.socketpair()` gives a faithful blocking stream pair. The dispatcher
runs on a background thread reading the server end; the test drives the client
end with the real frame codec.
"""

from __future__ import annotations

import socket
import threading
import time
from concurrent.futures import ThreadPoolExecutor

import pytest

from modules.ai_backend.ipc import registry
from modules.ai_backend.ipc.dispatcher import Dispatcher
from modules.ai_backend.ipc.events import EventBus
from modules.ai_backend.ipc.framing import read_frame, write_frame
from modules.ai_backend.ipc.registry import HandlerContext, Interrupted
from modules.ai_backend.ipc.protocol import PROTOCOL_VERSION

BACKEND_VERSION = "9.9.9-test"


class _Conn:
    """A connected client over a socketpair, with the dispatcher running server-side."""

    def __init__(self, ctx: HandlerContext, pool: ThreadPoolExecutor, events: EventBus) -> None:
        self._srv_sock, self._cli_sock = socket.socketpair()
        # Bound client reads so a missing/late frame fails fast instead of
        # hanging the test forever (e.g. after the server closes its end).
        self._cli_sock.settimeout(5.0)
        self._srv_r = self._srv_sock.makefile("rb", buffering=0)
        self._srv_w = self._srv_sock.makefile("wb", buffering=0)
        self.cli_r = self._cli_sock.makefile("rb", buffering=0)
        self.cli_w = self._cli_sock.makefile("wb", buffering=0)
        self.dispatcher = Dispatcher(
            self._srv_r,
            self._srv_w,
            ctx,
            pool,
            events,
            backend_version=BACKEND_VERSION,
        )
        self._thread = threading.Thread(target=self.dispatcher.serve, daemon=True)
        self._thread.start()

    def send(self, header: dict, blob: bytes = b"") -> None:
        write_frame(self.cli_w, header, blob)

    def recv(self) -> tuple[dict, bytes]:
        return read_frame(self.cli_r)

    def close(self) -> None:
        # Close the client socket (and its makefiles) so the server-side
        # read_frame sees EOF and the dispatcher thread exits.
        for f in (self.cli_w, self.cli_r):
            try:
                f.close()
            except Exception:
                pass
        try:
            self._cli_sock.close()
        except Exception:
            pass
        self._thread.join(timeout=2.0)
        for f in (self._srv_r, self._srv_w):
            try:
                f.close()
            except Exception:
                pass
        try:
            self._srv_sock.close()
        except Exception:
            pass


@pytest.fixture
def ctx() -> HandlerContext:
    return HandlerContext(
        state=None,
        events=EventBus(),
        get_health_snapshot=lambda: {"ok": True, "service": "mf_ai_backend"},
    )


@pytest.fixture
def pool():
    p = ThreadPoolExecutor(max_workers=4)
    yield p
    p.shutdown(wait=False)


@pytest.fixture
def events() -> EventBus:
    return EventBus()


@pytest.fixture(autouse=True)
def _restore_registry():
    """Snapshot and restore the global method registry around each test."""
    saved = dict(registry.METHOD_HANDLERS)
    yield
    registry.METHOD_HANDLERS.clear()
    registry.METHOD_HANDLERS.update(saved)


def _hello(conn: _Conn) -> dict:
    conn.send({"v": PROTOCOL_VERSION, "id": 0, "kind": "hello"})
    header, _ = conn.recv()
    return header


def test_hello_handshake_ok(ctx, pool, events) -> None:
    conn = _Conn(ctx, pool, events)
    try:
        header = _hello(conn)
        assert header["kind"] == "hello"
        assert header["v"] == PROTOCOL_VERSION
        assert header["backend_version"] == BACKEND_VERSION
    finally:
        conn.close()


def test_hello_version_mismatch(ctx, pool, events) -> None:
    conn = _Conn(ctx, pool, events)
    try:
        conn.send({"v": PROTOCOL_VERSION + 99, "id": 0, "kind": "hello"})
        header, _ = conn.recv()
        assert header["kind"] == "error"
        assert "mismatch" in header["error"].lower()
        # After a mismatch the dispatcher stops reading and returns (the
        # connection is abandoned); the handshake never completes.
        assert not conn.dispatcher._handshake_done
    finally:
        conn.close()


def test_request_response_round_trip(ctx, pool, events) -> None:
    def echo_handler(ctx_, header, blob, cancel_event):
        return {"echoed": header.get("payload"), "blob_len": len(blob)}, b"out:" + blob

    registry.register("test.echo", echo_handler)

    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        conn.send(
            {"v": 1, "id": 5, "kind": "request", "method": "test.echo", "payload": "hi"},
            b"abc",
        )
        header, blob = conn.recv()
        assert header["id"] == 5
        assert header["kind"] == "response"
        assert header["status"] == "ok"
        assert header["echoed"] == "hi"
        assert header["blob_len"] == 3
        assert blob == b"out:abc"
    finally:
        conn.close()


def test_unknown_method_yields_response_error(ctx, pool, events) -> None:
    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        conn.send({"v": 1, "id": 11, "kind": "request", "method": "does.not.exist"})
        header, _ = conn.recv()
        assert header["id"] == 11
        assert header["kind"] == "response"
        assert header["status"] == "error"
        assert "Unknown method" in header["error"]
    finally:
        conn.close()


def test_cancel_flips_event_and_interrupts(ctx, pool, events) -> None:
    started = threading.Event()

    def slow_handler(ctx_, header, blob, cancel_event):
        started.set()
        # Wait until canceled (or time out as a safety net).
        for _ in range(200):
            if cancel_event.is_set():
                raise Interrupted("canceled by client")
            time.sleep(0.01)
        return {"never": True}, b""

    registry.register("test.slow", slow_handler)

    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        conn.send({"v": 1, "id": 8, "kind": "request", "method": "test.slow"})
        assert started.wait(2.0), "handler did not start"
        conn.send({"v": 1, "id": 8, "kind": "cancel"})
        header, _ = conn.recv()
        assert header["id"] == 8
        assert header["kind"] == "response"
        assert header["status"] == "interrupted"
    finally:
        conn.close()


def test_multiplexing_two_concurrent_ids(ctx, pool, events) -> None:
    # id 1 blocks until id 2 has answered, proving independent routing.
    gate = threading.Event()

    def blocker(ctx_, header, blob, cancel_event):
        gate.wait(2.0)
        return {"who": 1}, b""

    def quick(ctx_, header, blob, cancel_event):
        return {"who": 2}, b""

    registry.register("test.blocker", blocker)
    registry.register("test.quick", quick)

    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        conn.send({"v": 1, "id": 1, "kind": "request", "method": "test.blocker"})
        conn.send({"v": 1, "id": 2, "kind": "request", "method": "test.quick"})

        # id 2 should answer first while id 1 is still blocked.
        h_first, _ = conn.recv()
        assert h_first["id"] == 2
        assert h_first["who"] == 2

        gate.set()
        h_second, _ = conn.recv()
        assert h_second["id"] == 1
        assert h_second["who"] == 1
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# Streaming: the dispatcher hands each request a live per-id ProgressEmitter on
# a per-REQUEST context copy, so a streaming handler emits progress{id} frames
# followed by the terminal response{id, status:ok} — and concurrent requests on
# one connection never cross-route progress to the wrong id.
# ---------------------------------------------------------------------------


def test_streaming_handler_emits_progress_then_response(ctx, pool, events) -> None:
    n = 4

    def streamer(ctx_, header, blob, cancel_event):
        emitter = getattr(ctx_, "progress_emitter", None)
        assert emitter is not None, "dispatcher must attach a live emitter"
        for step in range(1, n + 1):
            # Half the frames carry a blob to prove blobs ride along.
            payload = b"preview-%d" % step if step % 2 == 0 else b""
            emitter.emit({"step": step, "total": n}, payload)
        return {"done": True}, b"final"

    registry.register("test.stream", streamer)

    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        conn.send({"v": 1, "id": 7, "kind": "request", "method": "test.stream"})

        # Exactly N progress{id} frames, in order, correct fields/blobs.
        for step in range(1, n + 1):
            header, blob = conn.recv()
            assert header["kind"] == "progress"
            assert header["id"] == 7
            assert header["v"] == PROTOCOL_VERSION
            assert header["step"] == step
            assert header["total"] == n
            assert blob == (b"preview-%d" % step if step % 2 == 0 else b"")

        # Then the terminal response.
        header, blob = conn.recv()
        assert header["kind"] == "response"
        assert header["id"] == 7
        assert header["status"] == "ok"
        assert header["done"] is True
        assert blob == b"final"
    finally:
        conn.close()


def test_two_concurrent_streams_get_distinct_ids(ctx, pool, events) -> None:
    # Two streaming requests run at the same time on ONE connection. Each must
    # receive progress frames stamped with ITS OWN id — never the other's.
    release = threading.Event()
    both_running = threading.Semaphore(0)

    def streamer(ctx_, header, blob, cancel_event):
        emitter = getattr(ctx_, "progress_emitter", None)
        assert emitter is not None
        both_running.release()
        # Hold until both handlers are in-flight, forcing real concurrency so a
        # shared/clobbered emitter would misroute ids.
        release.wait(3.0)
        for step in range(1, 4):
            emitter.emit({"step": step, "total": 3})
        return {"ok": True}, b""

    registry.register("test.cstream", streamer)

    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        conn.send({"v": 1, "id": 100, "kind": "request", "method": "test.cstream"})
        conn.send({"v": 1, "id": 200, "kind": "request", "method": "test.cstream"})

        # Both handlers are running before either emits a frame.
        assert both_running.acquire(timeout=3.0)
        assert both_running.acquire(timeout=3.0)
        release.set()

        # Collect every frame until both ids have produced their terminal
        # response. Bucket frames by id and assert no progress frame ever
        # carries the wrong id.
        progress_by_id: dict[int, list[int]] = {100: [], 200: []}
        responses: set[int] = set()
        while len(responses) < 2:
            header, _ = conn.recv()
            fid = header["id"]
            assert fid in (100, 200)
            if header["kind"] == "progress":
                # The emitter's id MUST match the request that owns this stream.
                progress_by_id[fid].append(header["step"])
            elif header["kind"] == "response":
                assert header["status"] == "ok"
                responses.add(fid)

        # Each stream emitted exactly its own 3 steps under its own id.
        assert progress_by_id[100] == [1, 2, 3]
        assert progress_by_id[200] == [1, 2, 3]
    finally:
        release.set()
        conn.close()


# ---------------------------------------------------------------------------
# FIX-3: protocol-error frames not tied to a request must use id=0 (§6).
# ---------------------------------------------------------------------------

def test_duplicate_hello_error_uses_id_zero(ctx, pool, events) -> None:
    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        # A second hello carrying a non-zero id must NOT be echoed as that id;
        # it is a protocol error -> id=0, or the client would misroute it.
        conn.send({"v": 1, "id": 5, "kind": "hello"})
        header, _ = conn.recv()
        assert header["kind"] == "error"
        assert header["id"] == 0
        assert "Duplicate hello" in header["error"]
    finally:
        conn.close()


def test_unknown_kind_error_uses_id_zero(ctx, pool, events) -> None:
    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        conn.send({"v": 1, "id": 7, "kind": "bogus"})
        header, _ = conn.recv()
        assert header["kind"] == "error"
        assert header["id"] == 0
        assert "bogus" in header["error"]
    finally:
        conn.close()


def test_missing_kind_error_message_and_id_zero(ctx, pool, events) -> None:
    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        conn.send({"v": 1, "id": 9})  # no 'kind'
        header, _ = conn.recv()
        assert header["kind"] == "error"
        assert header["id"] == 0
        assert header["error"] == "Frame missing required 'kind' field."
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# request validation: missing/non-int/non-positive id, missing method.
# ---------------------------------------------------------------------------

def test_request_without_id_rejected_with_id_zero(ctx, pool, events) -> None:
    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        conn.send({"v": 1, "kind": "request", "method": "health"})  # no id
        header, _ = conn.recv()
        assert header["kind"] == "error"
        assert header["id"] == 0
        assert "positive integer id" in header["error"]
    finally:
        conn.close()


def test_request_with_non_int_id_rejected(ctx, pool, events) -> None:
    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        conn.send({"v": 1, "id": "5", "kind": "request", "method": "health"})
        header, _ = conn.recv()
        assert header["kind"] == "error"
        assert header["id"] == 0
        assert "positive integer id" in header["error"]
    finally:
        conn.close()


def test_request_with_missing_method_rejected(ctx, pool, events) -> None:
    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        conn.send({"v": 1, "id": 4, "kind": "request"})  # no method
        header, _ = conn.recv()
        # Missing method is a request failure echoing the id (§6).
        assert header["id"] == 4
        assert header["kind"] == "response"
        assert header["status"] == "error"
        assert "Unknown method" in header["error"]
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# handler raising -> response{status:error}; cancel for unknown id is a no-op.
# ---------------------------------------------------------------------------

def test_handler_raising_yields_response_error(ctx, pool, events) -> None:
    def boom(ctx_, header, blob, cancel_event):
        raise RuntimeError("handler blew up")

    registry.register("test.boom", boom)

    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        conn.send({"v": 1, "id": 3, "kind": "request", "method": "test.boom"})
        header, _ = conn.recv()
        assert header["id"] == 3
        assert header["kind"] == "response"
        assert header["status"] == "error"
        assert "handler blew up" in header["error"]
    finally:
        conn.close()


def test_cancel_for_unknown_id_is_noop(ctx, pool, events) -> None:
    # Canceling an id that was never in-flight must do nothing (no frame, no
    # crash). Afterwards a normal request still works, proving the loop is live.
    def quick(ctx_, header, blob, cancel_event):
        return {"ok": True}, b""

    registry.register("test.quick2", quick)

    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        conn.send({"v": 1, "id": 999, "kind": "cancel"})  # unknown id -> no-op
        conn.send({"v": 1, "id": 1, "kind": "request", "method": "test.quick2"})
        header, _ = conn.recv()
        assert header["id"] == 1
        assert header["status"] == "ok"
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# FIX-2: submit failure cleans up the cancel registry and sends a terminal frame.
# ---------------------------------------------------------------------------

def test_submit_failure_cleans_registry_and_replies_error(ctx, events) -> None:
    def quick(ctx_, header, blob, cancel_event):
        return {"ok": True}, b""

    registry.register("test.quick3", quick)

    # A pool whose submit() always raises (mimics a pool shut down during
    # teardown). The id must NOT leak in _cancels and the client must get a
    # terminal response{status:error}.
    class _BrokenPool:
        def submit(self, *a, **k):
            raise RuntimeError("pool is shut down")

    conn = _Conn(ctx, _BrokenPool(), events)
    try:
        _hello(conn)
        conn.send({"v": 1, "id": 6, "kind": "request", "method": "test.quick3"})
        header, _ = conn.recv()
        assert header["id"] == 6
        assert header["kind"] == "response"
        assert header["status"] == "error"
        assert "backend unavailable" in header["error"]
        # The id was popped back out of the cancel registry (no leak).
        with conn.dispatcher._cancels_lock:
            assert 6 not in conn.dispatcher._cancels
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# FIX-5: per-connection in-flight cap rejects excess requests instead of queueing.
# ---------------------------------------------------------------------------

def test_in_flight_cap_rejects_excess(ctx, events, monkeypatch) -> None:
    from modules.ai_backend.ipc import dispatcher as dispatcher_mod

    # Tiny cap so the test is small and deterministic.
    monkeypatch.setattr(dispatcher_mod, "_MAX_IN_FLIGHT_PER_CONN", 2)

    gate = threading.Event()
    started = threading.Semaphore(0)

    def blocker(ctx_, header, blob, cancel_event):
        started.release()
        gate.wait(3.0)
        return {"ok": True}, b""

    registry.register("test.hold", blocker)

    # Enough pool workers to actually run the 2 held requests concurrently.
    pool = ThreadPoolExecutor(max_workers=4)
    conn = _Conn(ctx, pool, events)
    try:
        _hello(conn)
        # Fill the cap: 2 long-running, in-flight requests.
        conn.send({"v": 1, "id": 1, "kind": "request", "method": "test.hold"})
        conn.send({"v": 1, "id": 2, "kind": "request", "method": "test.hold"})
        assert started.acquire(timeout=3.0)
        assert started.acquire(timeout=3.0)

        # The 3rd request is over the cap -> rejected immediately, not queued.
        conn.send({"v": 1, "id": 3, "kind": "request", "method": "test.hold"})
        header, _ = conn.recv()
        assert header["id"] == 3
        assert header["kind"] == "response"
        assert header["status"] == "error"
        assert header["error"] == "too many in-flight requests"

        # Release the two held requests; they terminate normally.
        gate.set()
        got = {conn.recv()[0]["id"], conn.recv()[0]["id"]}
        assert got == {1, 2}
    finally:
        gate.set()
        conn.close()
        pool.shutdown(wait=False)
