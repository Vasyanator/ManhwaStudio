"""
File: modules/ai_backend/ipc/framing.py

Purpose:
Pure wire codec for the framed, multiplexed Rust <-> Python IPC protocol (v2).
Each frame is `[u32 BE header_len][header_json][u32 BE blob_len][blob]`; this
module reads and writes that exact layout over any blocking byte stream (an
AF_UNIX socket's `makefile()` object, a `BytesIO`, or a test fake).

Main responsibilities:
- `read_frame(reader)` -> `(header: dict, blob: bytes)`, reassembling partial
  reads and treating EOF in the middle of a frame as a fatal `FrameError`;
- `write_frame(writer, header, blob)` -> serialize one frame atomically;
- enforce the `MAX_HEADER_BYTES` / `MAX_BLOB_BYTES` size guards before
  allocating or reading either segment;
- `FrameWriteLock`, a per-connection lock so multiple worker threads can write
  whole frames to one socket without interleaving.

Notes:
This module is transport-agnostic and has no knowledge of kinds, methods, or
dispatch. It only moves bytes. The authoritative wire spec lives in
`PROTOCOL.md`; the shared constants live in `protocol.py`.
"""

from __future__ import annotations

import json
import struct
import threading
from typing import Any, Protocol

from .protocol import MAX_BLOB_BYTES, MAX_HEADER_BYTES

# u32 big-endian length prefix shared by the header and blob segments.
_LEN_STRUCT = struct.Struct(">I")
_LEN_PREFIX_BYTES = _LEN_STRUCT.size


class FrameError(Exception):
    """Fatal protocol/codec error: malformed frame, size-guard breach, or EOF.

    Raised by `read_frame`/`write_frame`. The caller treats it as a connection
    that can no longer be trusted (it emits a `kind:"error"` frame and/or closes
    the socket, per PROTOCOL.md "Error model").
    """


class _Reader(Protocol):
    def read(self, n: int) -> bytes: ...


class _Writer(Protocol):
    def write(self, data: bytes) -> Any: ...

    def flush(self) -> Any: ...


def _write_all(writer: _Writer, data: bytes) -> None:
    """Write *all* of `data` to `writer`, looping over short writes.

    A stream `write()` may legally consume fewer bytes than offered and return
    the count actually written (this is how a raw `socket.makefile(... )` / a
    `socket.send()`-backed writer can short-write on non-Linux platforms). A
    single `writer.write(data)` would then silently drop the tail and corrupt
    the frame stream. We loop until every byte is accepted.

    A writer that returns `None` from `write()` (e.g. `io.BytesIO`, buffered
    file objects) is treated as having consumed the whole chunk, matching the
    standard `BufferedWriter.write` contract.
    """
    view = memoryview(data)
    total = len(view)
    sent = 0
    while sent < total:
        written = writer.write(view[sent:])
        if written is None:
            # The writer follows the buffered-stream contract: it accepts the
            # whole chunk (or raises). Nothing left to re-offer.
            return
        if written < 0:
            raise FrameError(f"Writer returned negative write count {written}.")
        sent += written


def _read_exactly(reader: _Reader, n: int) -> bytes:
    """Read exactly `n` bytes from `reader`, looping over partial reads.

    A blocking stream's `read(n)` may legally return fewer than `n` bytes; we
    loop until the buffer is full. An empty read means EOF: zero bytes at a
    frame boundary is a clean stream close, but a short read mid-frame is fatal.
    The two cases are distinguished by the caller, so here we simply raise
    `FrameError` whenever the stream ends before `n` bytes arrive.
    """
    if n == 0:
        return b""
    chunks: list[bytes] = []
    remaining = n
    while remaining > 0:
        chunk = reader.read(remaining)
        if not chunk:
            raise FrameError(
                f"Unexpected EOF: wanted {n} bytes, got {n - remaining} before close."
            )
        chunks.append(chunk)
        remaining -= len(chunk)
    if len(chunks) == 1:
        return chunks[0]
    return b"".join(chunks)


class StreamClosed(FrameError):
    """The peer closed the stream cleanly at a frame boundary (no partial frame).

    Subclasses `FrameError` so existing `except FrameError` handlers still catch
    it, but lets the dispatcher distinguish an orderly disconnect (drop quietly)
    from a corrupt/truncated frame.
    """


def read_frame(reader: _Reader) -> tuple[dict[str, Any], bytes]:
    """Read one frame from `reader` and return `(header_dict, blob_bytes)`.

    Layout (big-endian u32 length prefixes):
        [header_len][header_json][blob_len][blob]

    Raises:
        StreamClosed: the stream ended cleanly at the start of a frame (the peer
            disconnected between frames). The caller drops the connection.
        FrameError: a size guard was exceeded, the header is not a JSON object,
            or the stream ended in the middle of a frame.
    """
    # Header length prefix. A clean EOF *here* (before any byte of a new frame)
    # means the peer closed between frames -> StreamClosed, not corruption.
    prefix = reader.read(_LEN_PREFIX_BYTES)
    if not prefix:
        raise StreamClosed("Peer closed the connection at a frame boundary.")
    if len(prefix) < _LEN_PREFIX_BYTES:
        prefix += _read_exactly(reader, _LEN_PREFIX_BYTES - len(prefix))
    (header_len,) = _LEN_STRUCT.unpack(prefix)

    if header_len == 0:
        raise FrameError("Frame header_len must be non-zero (header is required).")
    if header_len > MAX_HEADER_BYTES:
        raise FrameError(
            f"Frame header_len {header_len} exceeds MAX_HEADER_BYTES {MAX_HEADER_BYTES}."
        )

    header_bytes = _read_exactly(reader, header_len)
    try:
        header = json.loads(header_bytes.decode("utf-8"))
    except Exception as exc:  # noqa: BLE001 - any decode/parse failure is fatal
        raise FrameError(f"Frame header is not valid UTF-8 JSON: {exc}") from exc
    if not isinstance(header, dict):
        raise FrameError("Frame header JSON must be an object.")

    blob_prefix = _read_exactly(reader, _LEN_PREFIX_BYTES)
    (blob_len,) = _LEN_STRUCT.unpack(blob_prefix)
    if blob_len > MAX_BLOB_BYTES:
        raise FrameError(
            f"Frame blob_len {blob_len} exceeds MAX_BLOB_BYTES {MAX_BLOB_BYTES}."
        )
    blob = _read_exactly(reader, blob_len) if blob_len else b""
    return header, blob


def encode_frame(header: dict[str, Any], blob: bytes = b"") -> bytes:
    """Serialize one frame to a single bytes object (no I/O).

    Useful for the event bus, which encodes a frame once and fans the same bytes
    out to every subscriber. Enforces the same size guards as `write_frame`.
    """
    header_bytes = json.dumps(header, ensure_ascii=False).encode("utf-8")
    if len(header_bytes) > MAX_HEADER_BYTES:
        raise FrameError(
            f"Encoded header {len(header_bytes)} bytes exceeds MAX_HEADER_BYTES "
            f"{MAX_HEADER_BYTES}."
        )
    if len(blob) > MAX_BLOB_BYTES:
        raise FrameError(
            f"Blob {len(blob)} bytes exceeds MAX_BLOB_BYTES {MAX_BLOB_BYTES}."
        )
    return b"".join(
        (
            _LEN_STRUCT.pack(len(header_bytes)),
            header_bytes,
            _LEN_STRUCT.pack(len(blob)),
            blob,
        )
    )


def write_frame(writer: _Writer, header: dict[str, Any], blob: bytes = b"") -> None:
    """Encode and write one whole frame to `writer`, then flush.

    The whole frame is built in memory and written with a true write-all loop
    (`_write_all`) so a short write never truncates the frame, then flushed so
    buffered writers actually push the bytes out. A frame is never split across
    worker threads at the byte level (callers must still serialize concurrent
    writers with a `FrameWriteLock`).

    Raises FrameError on a size-guard breach. A broken/closed socket surfaces as
    the underlying OSError (BrokenPipeError/ConnectionResetError); the caller
    treats that as a dropped connection.
    """
    data = encode_frame(header, blob)
    _write_all(writer, data)
    writer.flush()


class FrameWriteLock:
    """Per-connection write lock guarding whole-frame writes to one socket.

    Multiple worker threads (responses, progress frames, events) may target the
    same connection. Each acquires this lock around its `write_frame` call so the
    bytes of two frames never interleave. Use as a context manager:

        with conn.write_lock:
            write_frame(conn.writer, header, blob)
    """

    __slots__ = ("_lock",)

    def __init__(self) -> None:
        self._lock = threading.Lock()

    def __enter__(self) -> "FrameWriteLock":
        self._lock.acquire()
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self._lock.release()
