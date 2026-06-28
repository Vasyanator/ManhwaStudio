"""
File: modules/ai_backend/test_framing.py

Purpose:
Unit tests for the v2 IPC wire codec (`modules/ai_backend/ipc/framing.py`).

Main responsibilities:
- round-trip a header + blob through encode/read;
- accept an empty blob (blob_len == 0);
- reject frames that breach the header/blob size guards;
- reassemble a frame fed in small chunks (partial reads);
- treat EOF in the middle of a frame as a fatal error and a clean EOF at a
  frame boundary as `StreamClosed`.
"""

from __future__ import annotations

import io

import pytest

from modules.ai_backend.ipc import framing
from modules.ai_backend.ipc.framing import (
    FrameError,
    StreamClosed,
    encode_frame,
    read_frame,
    write_frame,
)


class _ChunkReader:
    """A reader that hands out at most `chunk` bytes per `read()` call.

    Exercises the partial-read reassembly path in `_read_exactly`/`read_frame`.
    """

    def __init__(self, data: bytes, chunk: int) -> None:
        self._data = data
        self._chunk = chunk
        self._pos = 0

    def read(self, n: int) -> bytes:
        if self._pos >= len(self._data):
            return b""
        take = min(n, self._chunk, len(self._data) - self._pos)
        out = self._data[self._pos : self._pos + take]
        self._pos += take
        return out


def test_round_trip_header_and_blob() -> None:
    header = {"v": 1, "id": 7, "kind": "response", "status": "ok", "text": "héllo"}
    blob = b"\x89PNG\r\n\x1a\n binary \x00\x01\x02"
    buf = io.BytesIO()
    write_frame(buf, header, blob)
    buf.seek(0)
    got_header, got_blob = read_frame(buf)
    assert got_header == header
    assert got_blob == blob


def test_empty_blob() -> None:
    header = {"v": 1, "id": 0, "kind": "hello", "backend_version": "3.4.2"}
    buf = io.BytesIO(encode_frame(header, b""))
    got_header, got_blob = read_frame(buf)
    assert got_header == header
    assert got_blob == b""


def test_header_size_guard_on_read() -> None:
    # Hand-craft a frame whose declared header_len exceeds MAX_HEADER_BYTES.
    import struct

    oversized = framing.MAX_HEADER_BYTES + 1
    raw = struct.pack(">I", oversized) + b"{}"
    with pytest.raises(FrameError, match="MAX_HEADER_BYTES"):
        read_frame(io.BytesIO(raw))


def test_blob_size_guard_on_read() -> None:
    import struct

    header_bytes = b'{"v":1,"id":1,"kind":"request"}'
    oversized = framing.MAX_BLOB_BYTES + 1
    raw = (
        struct.pack(">I", len(header_bytes))
        + header_bytes
        + struct.pack(">I", oversized)
    )
    with pytest.raises(FrameError, match="MAX_BLOB_BYTES"):
        read_frame(io.BytesIO(raw))


def test_blob_size_guard_on_encode() -> None:
    with pytest.raises(FrameError, match="MAX_BLOB_BYTES"):
        encode_frame({"v": 1, "kind": "response"}, b"x" * (framing.MAX_BLOB_BYTES + 1))


def test_partial_read_reassembly() -> None:
    header = {"v": 1, "id": 42, "kind": "request", "method": "ocr.manga"}
    blob = bytes(range(256)) * 4  # 1 KiB of binary
    frame = encode_frame(header, blob)
    # Feed the frame one byte at a time.
    reader = _ChunkReader(frame, chunk=1)
    got_header, got_blob = read_frame(reader)
    assert got_header == header
    assert got_blob == blob


def test_eof_at_frame_boundary_is_stream_closed() -> None:
    # Empty stream: a clean disconnect, not a corrupt frame.
    with pytest.raises(StreamClosed):
        read_frame(io.BytesIO(b""))


def test_eof_mid_frame_is_error() -> None:
    header = {"v": 1, "id": 1, "kind": "request", "method": "health"}
    frame = encode_frame(header, b"abcdef")
    truncated = frame[: len(frame) - 3]  # drop part of the blob
    with pytest.raises(FrameError):
        read_frame(io.BytesIO(truncated))
    # StreamClosed is only for a clean boundary EOF, not a truncated frame.
    with pytest.raises(FrameError):
        read_frame(io.BytesIO(frame[:2]))  # truncated header-length prefix


class _ByteAtATimeWriter:
    """A writer that accepts at most 1 byte per `write()` and reports the count.

    Mimics a `socket.send()`-backed writer that short-writes: each `write()`
    consumes one byte and returns `1`, forcing `_write_all` to loop. Records
    every byte so the full frame can be reassembled and compared.
    """

    def __init__(self) -> None:
        self.buf = bytearray()
        self.flushed = 0
        self.write_calls = 0

    def write(self, data) -> int:  # noqa: ANN001 - accepts bytes/memoryview
        self.write_calls += 1
        if len(data) == 0:
            return 0
        self.buf.append(data[0])
        return 1

    def flush(self) -> None:
        self.flushed += 1


def test_write_frame_handles_short_writes() -> None:
    # FIX-1: a writer that accepts 1 byte at a time must still get the whole
    # frame (write-all loop), and the frame must round-trip back.
    header = {"v": 1, "id": 3, "kind": "response", "status": "ok", "n": 5}
    blob = bytes(range(64))
    writer = _ByteAtATimeWriter()
    write_frame(writer, header, blob)

    expected = encode_frame(header, blob)
    assert bytes(writer.buf) == expected
    # One write() per byte (proves the short-write loop actually iterated).
    assert writer.write_calls >= len(expected)
    assert writer.flushed == 1  # flush actually ran exactly once

    got_header, got_blob = read_frame(io.BytesIO(bytes(writer.buf)))
    assert got_header == header
    assert got_blob == blob
