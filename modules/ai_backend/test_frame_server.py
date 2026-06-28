"""
File: modules/ai_backend/test_frame_server.py

Purpose:
Tests for the v2 frame server (`modules/ai_backend/ipc/frame_server.py`).

Covers:
- smoke: hello handshake + health request/response round-trip;
- single-instance guard: binding an already-live socket raises
  `FrameBackendInstanceError`;
- stale-socket: a dead socket file is unlinked and bind succeeds;
- permissions: the bound socket file has `0o600` perms on POSIX;
- stop: `stop_event.set()` causes `run_frame_server` to return promptly;
- concurrency: two concurrent client connections each complete a
  `hello` + `health` round trip.

All tests run without torch or any AI model dependency (the `health` method
only reads the injected snapshot getter).
"""

from __future__ import annotations

import os
import socket
import stat
import tempfile
import threading
import time
from pathlib import Path

import pytest

from modules.ai_backend.ipc.framing import read_frame, write_frame
from modules.ai_backend.ipc.frame_server import FrameBackendInstanceError, run_frame_server
from modules.ai_backend.ipc.protocol import PROTOCOL_VERSION

BACKEND_VERSION = "smoke-1.0"


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


def _start_server(sock_path: str, stop_event: threading.Event) -> threading.Thread:
    """Start run_frame_server in a daemon thread; return the thread."""
    t = threading.Thread(
        target=run_frame_server,
        args=(None, sock_path, stop_event),
        kwargs={
            "backend_version": BACKEND_VERSION,
            "get_health_snapshot": _make_snapshot,
        },
        daemon=True,
    )
    t.start()
    return t


def _wait_for_socket(sock_path: str, timeout: float = 5.0) -> None:
    """Block until the socket file exists and accepts connections."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if Path(sock_path).exists():
            probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                probe.connect(sock_path)
                probe.close()
                return
            except OSError:
                probe.close()
        time.sleep(0.02)
    raise TimeoutError(f"Socket {sock_path!r} did not become ready within {timeout}s.")


def _connect(sock_path: str) -> socket.socket:
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    deadline = time.time() + 5.0
    while True:
        try:
            client.connect(sock_path)
            return client
        except OSError:
            if time.time() > deadline:
                raise
            time.sleep(0.02)


def _hello_health(sock_path: str) -> dict:
    """Connect, do hello+health, return the health response header."""
    client = _connect(sock_path)
    try:
        r = client.makefile("rb", buffering=0)
        w = client.makefile("wb", buffering=0)
        write_frame(w, {"v": PROTOCOL_VERSION, "id": 0, "kind": "hello"})
        hello_hdr, _ = read_frame(r)
        assert hello_hdr["kind"] == "hello"
        write_frame(w, {"v": PROTOCOL_VERSION, "id": 1, "kind": "request", "method": "health"})
        resp_hdr, resp_blob = read_frame(r)
        assert resp_hdr["kind"] == "response"
        assert resp_hdr["status"] == "ok"
        assert resp_blob == b""
        return resp_hdr
    finally:
        client.close()


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def tmp_sock(tmp_path):
    """Return a fresh socket path inside a temp dir (socket not yet created)."""
    return str(tmp_path / "backend_v2.sock")


@pytest.fixture
def server_socket(tmp_sock):
    """Start a live server; yield the socket path; stop cleanly on teardown."""
    stop_event = threading.Event()
    t = _start_server(tmp_sock, stop_event)
    _wait_for_socket(tmp_sock)
    yield tmp_sock
    stop_event.set()
    t.join(timeout=5.0)


# ---------------------------------------------------------------------------
# Original smoke test (preserved)
# ---------------------------------------------------------------------------


def test_hello_then_health_round_trip(server_socket) -> None:
    client = _connect(server_socket)
    try:
        r = client.makefile("rb", buffering=0)
        w = client.makefile("wb", buffering=0)

        # Handshake.
        write_frame(w, {"v": PROTOCOL_VERSION, "id": 0, "kind": "hello"})
        hello_header, _ = read_frame(r)
        assert hello_header["kind"] == "hello"
        assert hello_header["v"] == PROTOCOL_VERSION
        assert hello_header["backend_version"] == BACKEND_VERSION

        # Health request.
        write_frame(w, {"v": 1, "id": 1, "kind": "request", "method": "health"})
        resp_header, resp_blob = read_frame(r)
        assert resp_header["id"] == 1
        assert resp_header["kind"] == "response"
        assert resp_header["status"] == "ok"
        assert resp_header["service"] == "mf_ai_backend"
        assert resp_header["backend_version"] == BACKEND_VERSION
        assert resp_blob == b""
    finally:
        client.close()


# ---------------------------------------------------------------------------
# FIX-5: lifecycle tests
# ---------------------------------------------------------------------------


def test_live_socket_raises_instance_error(server_socket, tmp_path) -> None:
    """Binding while a live peer owns the socket raises FrameBackendInstanceError."""
    from modules.ai_backend.ipc.frame_server import FrameUnixServer

    # A dummy handler class (never actually invoked here).
    import socketserver

    with pytest.raises(FrameBackendInstanceError):
        FrameUnixServer(server_socket, socketserver.BaseRequestHandler)


def test_stale_socket_is_unlinked_and_bind_succeeds(tmp_sock) -> None:
    """A stale (non-listening) socket file is removed and bind succeeds."""
    # Create a dead socket file that no live peer owns.
    dead = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    dead.bind(tmp_sock)
    dead.close()
    assert Path(tmp_sock).exists(), "precondition: stale file must exist"

    # run_frame_server should unlink it and bind successfully.
    stop_event = threading.Event()
    t = _start_server(tmp_sock, stop_event)
    try:
        _wait_for_socket(tmp_sock)  # proves bind succeeded
        hdr = _hello_health(tmp_sock)
        assert hdr["service"] == "mf_ai_backend"
    finally:
        stop_event.set()
        t.join(timeout=5.0)


@pytest.mark.skipif(os.name != "posix", reason="permission bits are POSIX-only")
def test_socket_permissions_are_0o600(tmp_sock) -> None:
    """The bound socket file must be chmod 0o600 on POSIX."""
    stop_event = threading.Event()
    t = _start_server(tmp_sock, stop_event)
    try:
        _wait_for_socket(tmp_sock)
        mode = stat.S_IMODE(os.stat(tmp_sock).st_mode)
        assert mode == 0o600, f"Expected 0o600, got {mode:#o}"
    finally:
        stop_event.set()
        t.join(timeout=5.0)


def test_stop_event_causes_server_to_return_promptly(tmp_sock) -> None:
    """Setting stop_event must cause run_frame_server to return within ~2 s."""
    stop_event = threading.Event()
    t = _start_server(tmp_sock, stop_event)
    _wait_for_socket(tmp_sock)

    t0 = time.monotonic()
    stop_event.set()
    t.join(timeout=10.0)
    elapsed = time.monotonic() - t0

    assert not t.is_alive(), "run_frame_server thread did not exit after stop_event"
    assert elapsed < 5.0, f"Server took too long to stop: {elapsed:.1f}s"


def test_two_concurrent_clients_both_complete(server_socket) -> None:
    """Two simultaneous connections each finish hello+health without interference."""
    results: list[dict] = []
    errors: list[Exception] = []

    def _client_task() -> None:
        try:
            hdr = _hello_health(server_socket)
            results.append(hdr)
        except Exception as exc:  # noqa: BLE001
            errors.append(exc)

    t1 = threading.Thread(target=_client_task, daemon=True)
    t2 = threading.Thread(target=_client_task, daemon=True)
    t1.start()
    t2.start()
    t1.join(timeout=10.0)
    t2.join(timeout=10.0)

    assert not errors, f"Client errors: {errors}"
    assert len(results) == 2, f"Expected 2 successful responses, got {len(results)}"
    for hdr in results:
        assert hdr["service"] == "mf_ai_backend"
        assert hdr["backend_version"] == BACKEND_VERSION
