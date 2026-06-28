"""
File: modules/ai_backend/test_events.py

Purpose:
Unit tests for the v2 IPC event bus (`modules/ai_backend/ipc/events.py`),
focused on the FIX-4 guarantee: one slow/dead subscriber must never stall
`publish()` for the other (fast) subscribers, and a stuck/broken sink is dropped.

Notes:
A real `socket.socketpair()` provides a faithful blocking sink. A slow client is
simulated by never draining its receive end and filling the send-buffer so the
server-side write blocks; the bus's per-write timeout must then drop that sink
without holding up a fast sink registered alongside it.
"""

from __future__ import annotations

import socket
import threading

from modules.ai_backend.ipc import events as events_mod
from modules.ai_backend.ipc.events import EventBus, EventSink
from modules.ai_backend.ipc.framing import FrameWriteLock, read_frame


def _sink_from_socket(sock: socket.socket) -> EventSink:
    writer = sock.makefile("wb", buffering=0)
    return EventSink(writer, FrameWriteLock(), sock)


def test_slow_sink_does_not_block_fast_sink(monkeypatch) -> None:
    # Shrink the per-write timeout so the test is fast; the wedged sink should
    # be dropped after this, never blocking the fast sink's delivery.
    monkeypatch.setattr(events_mod, "_PUBLISH_WRITE_TIMEOUT_S", 0.2)

    bus = EventBus()

    # Fast sink: a live socketpair whose client end is actively drained.
    fast_srv, fast_cli = socket.socketpair()
    # Slow sink: a socketpair whose client end is NEVER read, so the server
    # send-buffer fills and the write eventually blocks -> times out -> dropped.
    slow_srv, slow_cli = socket.socketpair()
    slow_srv.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
    slow_cli.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)

    try:
        bus.register(_sink_from_socket(fast_srv))
        bus.register(_sink_from_socket(slow_srv))
        assert bus.subscriber_count() == 2

        fast_reader = fast_cli.makefile("rb", buffering=0)
        received: list[dict] = []

        def _drain_fast() -> None:
            # Read whatever the fast sink receives so its buffer never fills.
            for _ in range(20):
                try:
                    header, _blob = read_frame(fast_reader)
                except Exception:  # noqa: BLE001 - stream closed at test teardown
                    return
                received.append(header)

        drainer = threading.Thread(target=_drain_fast, daemon=True)
        drainer.start()

        # Publish a big-ish payload repeatedly: the slow sink's 4 KiB send-buffer
        # cannot absorb it, so its write blocks past the timeout and it is
        # dropped, while the fast sink keeps getting frames.
        big = "x" * 2000
        done = threading.Event()

        def _publish_many() -> None:
            for _ in range(10):
                bus.publish("log", {"level": "info", "message": big})
            done.set()

        pub = threading.Thread(target=_publish_many, daemon=True)
        pub.start()

        # If a slow sink could stall publish(), this would block far longer than
        # a handful of timeouts; bound it generously and assert it finished.
        assert done.wait(5.0), "publish() was stalled by the slow sink"

        # The slow sink must have been dropped; the fast sink survives.
        assert bus.subscriber_count() == 1
        # The fast client actually received events (delivery was not blocked).
        # Give the drain thread a moment to record at least one frame.
        for _ in range(50):
            if received:
                break
            threading.Event().wait(0.02)
        assert received, "fast sink received no events"
        assert received[0]["topic"] == "log"
    finally:
        for s in (fast_srv, fast_cli, slow_srv, slow_cli):
            try:
                s.close()
            except OSError:
                pass


def test_partial_writer_delivers_complete_frame() -> None:
    """A writer that accepts only 1 byte per call still delivers the full frame.

    This exercises the `_write_all` loop added in FIX-1: without it, a single
    `writer.write(frame)` that short-writes would silently drop the tail and
    corrupt the frame stream.
    """
    bus = EventBus()

    received_bytes = bytearray()

    class _OneBytewriterWriter:
        """Accepts exactly 1 byte per `write()` call."""

        def write(self, data: bytes) -> int:
            if data:
                received_bytes.append(data[0])
                return 1
            return 0

        def flush(self) -> None:
            pass

    sink = EventSink(_OneBytewriterWriter(), FrameWriteLock(), None)
    bus.register(sink)
    bus.publish("health", {"ok": True})
    # Sink must still be alive (no exception) and received_bytes must be non-empty.
    assert bus.subscriber_count() == 1
    assert len(received_bytes) > 0

    # The received bytes must form a valid frame decodable by read_frame.
    import io
    from modules.ai_backend.ipc.framing import read_frame
    header, blob = read_frame(io.BytesIO(bytes(received_bytes)))
    assert header["topic"] == "health"
    assert header["ok"] is True
    assert blob == b""


def test_dead_sink_is_dropped() -> None:
    # A sink whose writer raises on write is treated as dead and removed, and
    # publish() does not raise.
    bus = EventBus()

    class _BrokenWriter:
        def write(self, data):  # noqa: ANN001
            raise BrokenPipeError("peer gone")

        def flush(self) -> None:
            pass

    sink = EventSink(_BrokenWriter(), FrameWriteLock(), None)
    bus.register(sink)
    bus.publish("health", {"ok": True})
    assert bus.subscriber_count() == 0
