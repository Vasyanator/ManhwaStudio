"""
File: modules/ai_backend/ipc/events.py

Purpose:
Publish/subscribe event bus for the v2 IPC protocol. Server-initiated `event`
frames (id=0) are fanned out to every live connection. The health worker
publishes `TOPIC_HEALTH` snapshots; later phases publish `TOPIC_DEVICE` /
`TOPIC_MODEL_LOAD` / `TOPIC_LOG`.

Main responsibilities:
- track the set of live connections (register on connect, unregister on close);
- `publish(topic, payload)` -> encode one `event` frame and write it to every
  subscriber, best-effort (a dead/broken socket is dropped, never raised).

Notes:
Each subscriber is an `EventSink`: a `(writer, write_lock)` pair, optionally
carrying the underlying socket so the bus can bound how long a single write may
block. Writes are serialized through the connection's own `FrameWriteLock` so an
event never interleaves with a response/progress frame on the same socket. The
bus is thread-safe; publishing happens from the health worker thread while
connections register/unregister from their own dispatcher threads.

Slow/dead-client isolation (FIX-4): `publish()` writes to each sink under that
sink's write lock. A client whose kernel send-buffer is full would otherwise
block `publish()` indefinitely, stalling events to *every* other client and the
health cycle. To prevent one slow/dead peer from stalling the others we set a
bounded socket send timeout (`_PUBLISH_WRITE_TIMEOUT_S`) around each event write:
a stuck send raises `socket.timeout`/`OSError`, that sink is dropped as dead, and
the loop moves on to the next subscriber. This is chosen over a per-connection
outbound queue + writer thread because it needs no extra threads or buffering,
reuses the existing per-connection `FrameWriteLock`, and a wedged client is
already useless to us — dropping it is the correct outcome.
"""

from __future__ import annotations

import threading
from typing import Any

from .framing import FrameWriteLock, _write_all, encode_frame
from .protocol import (
    HEADER_ID,
    HEADER_KIND,
    HEADER_TOPIC,
    HEADER_VERSION,
    KIND_EVENT,
    PROTOCOL_VERSION,
    VALID_TOPICS,
)


# Max seconds a single event write may block before the sink is declared dead.
# A client whose send-buffer stays full this long is treated as gone so it can
# never stall the publisher (and thus every other client) indefinitely.
_PUBLISH_WRITE_TIMEOUT_S = 2.0


class EventSink:
    """One subscriber: a writable stream plus its per-connection write lock.

    `publish` writes pre-encoded event frames through `write_lock` so they cannot
    interleave with the connection's response/progress frames.

    `sock` is the underlying connection socket, supplied so the event bus can
    bound how long a single event write may block (FIX-4). It is optional: when
    absent (e.g. a `BytesIO`-backed test sink) the timeout guard is skipped.
    """

    __slots__ = ("writer", "write_lock", "sock")

    def __init__(
        self,
        writer: Any,
        write_lock: FrameWriteLock,
        sock: Any = None,
    ) -> None:
        self.writer = writer
        self.write_lock = write_lock
        self.sock = sock


class EventBus:
    """Thread-safe fan-out of `event` frames to all live connections."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._sinks: set[EventSink] = set()

    def register(self, sink: EventSink) -> None:
        """Add a connection as an event subscriber."""
        with self._lock:
            self._sinks.add(sink)

    def unregister(self, sink: EventSink) -> None:
        """Remove a connection (on close); idempotent."""
        with self._lock:
            self._sinks.discard(sink)

    def subscriber_count(self) -> int:
        with self._lock:
            return len(self._sinks)

    def publish(self, topic: str, payload: dict[str, Any], blob: bytes = b"") -> None:
        """Fan a single `event{id:0, topic, ...payload}` frame out to all sinks.

        Best-effort: a sink whose socket is broken/closed -- or whose write
        blocks past `_PUBLISH_WRITE_TIMEOUT_S` because its send-buffer is full --
        is dropped silently and skipped, so one slow/dead client can never stall
        the publisher (and thus every other client; FIX-4). `payload` fields are
        merged inline into the event header; the reserved frame fields
        (`v`/`id`/`kind`/`topic`) always win.
        """
        if topic not in VALID_TOPICS:
            raise ValueError(f"Unknown event topic: {topic!r}")

        header: dict[str, Any] = dict(payload)
        header[HEADER_VERSION] = PROTOCOL_VERSION
        header[HEADER_ID] = 0
        header[HEADER_KIND] = KIND_EVENT
        header[HEADER_TOPIC] = topic

        # Encode once; the same bytes go to every subscriber.
        frame = encode_frame(header, blob)

        with self._lock:
            sinks = list(self._sinks)

        dead: list[EventSink] = []
        for sink in sinks:
            try:
                with sink.write_lock:
                    self._write_to_sink(sink, frame)
            except Exception:  # noqa: BLE001 - broken/stuck socket -> drop sink
                dead.append(sink)

        if dead:
            with self._lock:
                for sink in dead:
                    self._sinks.discard(sink)

    @staticmethod
    def _write_to_sink(sink: EventSink, frame: bytes) -> None:
        """Write one pre-encoded event frame to `sink`, bounded by a timeout.

        When the sink carries its socket, a non-blocking-with-deadline send
        timeout is applied for the duration of this write so a full send-buffer
        raises `socket.timeout`/`OSError` instead of blocking forever; the caller
        treats any raise as a dead sink and drops it. The socket's previous
        timeout is always restored. Sinks without a socket (e.g. a `BytesIO`
        test sink) are written directly.
        """
        if sink.sock is None:
            _write_all(sink.writer, frame)
            sink.writer.flush()
            return

        previous_timeout = sink.sock.gettimeout()
        try:
            sink.sock.settimeout(_PUBLISH_WRITE_TIMEOUT_S)
            _write_all(sink.writer, frame)
            sink.writer.flush()
        finally:
            try:
                sink.sock.settimeout(previous_timeout)
            except OSError:
                # Socket already closed mid-write; nothing to restore.
                pass
