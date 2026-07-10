/*
File: backend_ipc/frame.rs

Purpose:
Frame codec for the v2 Rust <-> Python AI-backend IPC protocol. Implements the
wire format from `modules/ai_backend/ipc/PROTOCOL.md` exactly:

    [u32 BE header_len][header_json][u32 BE blob_len][blob]

`header_json` is a UTF-8 JSON object; `blob` is raw binary (no base64). Both
length prefixes are big-endian u32. Size guards (`MAX_HEADER_BYTES` 1 MiB,
`MAX_BLOB_BYTES` 32 MiB) are enforced before any allocation/read so a malicious or
corrupt peer cannot make us allocate unbounded memory.

Key types:
- Frame: a decoded `{ header: serde_json::Value, blob: Vec<u8> }`.

Key functions:
- read_frame(): blocking decode of one frame from any `Read`.
- write_frame(): encode one frame to any `Write`.

Notes:
The reads use fill-to-length loops (`read` may return short), so partial socket
reads are handled correctly. Errors are human-readable strings, consistent with
the rest of `backend_ipc`.
*/

// `Frame::new` is part of the published codec surface for Phase 3 call sites that
// will construct frames directly; unused in Phase 2.
#![allow(dead_code)]

use std::io::{Read, Write};

use serde_json::Value;

use super::protocol::{MAX_BLOB_BYTES, MAX_HEADER_BYTES};

/// One decoded protocol frame: a JSON `header` object plus an optional raw binary
/// `blob` (empty `Vec` when `blob_len == 0`).
#[derive(Debug, Clone)]
pub struct Frame {
    /// The `header_json` segment, parsed. Always a JSON object on a valid frame.
    pub header: Value,
    /// The raw blob bytes (no base64). Empty when the frame carried no blob.
    pub blob: Vec<u8>,
}

impl Frame {
    /// Convenience constructor for a frame with the given header and blob.
    #[must_use]
    pub fn new(header: Value, blob: Vec<u8>) -> Self {
        Self { header, blob }
    }
}

/// Reads exactly `buf.len()` bytes from `r`, looping over short reads.
///
/// # Errors
/// Returns an error string on a hard read error or on EOF before the buffer is
/// full (a truncated frame).
fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8], what: &str) -> Result<(), String> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = r
            .read(&mut buf[filled..])
            .map_err(|err| tf!("backend_ipc.frame.read_error", what = what, err = err))?;
        if n == 0 {
            return Err(tf!("backend_ipc.frame.closed_mid_frame", what = what, filled = filled, buf = buf.len()));
        }
        filled += n;
    }
    Ok(())
}

/// Reads one big-endian `u32` length prefix from `r`.
fn read_len_be(r: &mut impl Read, what: &str) -> Result<u32, String> {
    let mut prefix = [0_u8; 4];
    read_exact_or_eof(r, &mut prefix, what)?;
    Ok(u32::from_be_bytes(prefix))
}

/// Reads one frame from `r`.
///
/// Wire format: `[u32 BE header_len][header_json][u32 BE blob_len][blob]`. Both
/// length prefixes are size-guarded before allocating; `header_json` must parse
/// as a JSON object.
///
/// # Errors
/// Returns a human-readable error string on read failure, truncated frame, a
/// size-guard violation, invalid UTF-8, non-object/invalid JSON header.
pub fn read_frame(r: &mut impl Read) -> Result<Frame, String> {
    let header_len = read_len_be(r, t!("backend_ipc.frame.what_header_len"))? as usize;
    if header_len > MAX_HEADER_BYTES {
        return Err(tf!("backend_ipc.frame.header_over_limit_read", header_len = header_len, max_header_bytes = MAX_HEADER_BYTES));
    }
    if header_len == 0 {
        return Err(t!("backend_ipc.frame.empty_header").to_string());
    }

    let mut header_bytes = vec![0_u8; header_len];
    read_exact_or_eof(r, &mut header_bytes, t!("backend_ipc.frame.what_header"))?;
    let header: Value = serde_json::from_slice(&header_bytes)
        .map_err(|err| tf!("backend_ipc.frame.header_json_parse_error", err = err))?;
    if !header.is_object() {
        return Err(t!("backend_ipc.frame.header_not_object").to_string());
    }

    let blob_len = read_len_be(r, t!("backend_ipc.frame.what_blob_len"))? as usize;
    if blob_len > MAX_BLOB_BYTES {
        return Err(tf!("backend_ipc.frame.blob_over_limit_read", blob_len = blob_len, max_blob_bytes = MAX_BLOB_BYTES));
    }

    let mut blob = vec![0_u8; blob_len];
    if blob_len > 0 {
        read_exact_or_eof(r, &mut blob, "blob")?;
    }

    Ok(Frame { header, blob })
}

/// Writes one frame to `w`: `[u32 BE header_len][header_json][u32 BE blob_len][blob]`.
///
/// `header` is serialized compactly to UTF-8 JSON. Both segments are size-guarded
/// before writing. The whole frame is assembled into one buffer and emitted with a
/// single `write_all`, but that alone does NOT guarantee atomicity against other
/// writers: a concurrent writer sharing the same stream could still interleave its
/// bytes. Frame atomicity comes from callers serializing writes behind the
/// `write_half` mutex in `client.rs`; this buffer just avoids partial-frame writes
/// from a single caller.
///
/// # Errors
/// Returns a human-readable error string on serialization failure, a size-guard
/// violation, or a write failure.
pub fn write_frame(w: &mut impl Write, header: &Value, blob: &[u8]) -> Result<(), String> {
    debug_assert!(
        header.is_object(),
        "frame header must be a JSON object, got: {header}"
    );
    let header_bytes = serde_json::to_vec(header)
        .map_err(|err| tf!("backend_ipc.frame.header_serialize_error", err = err))?;
    if header_bytes.len() > MAX_HEADER_BYTES {
        return Err(tf!("backend_ipc.frame.header_over_limit_write", header_bytes = header_bytes.len(), max_header_bytes = MAX_HEADER_BYTES));
    }
    if blob.len() > MAX_BLOB_BYTES {
        return Err(tf!("backend_ipc.frame.blob_over_limit_write", blob = blob.len(), max_blob_bytes = MAX_BLOB_BYTES));
    }

    let header_len = header_bytes.len() as u32;
    let blob_len = blob.len() as u32;
    let mut out = Vec::with_capacity(4 + header_bytes.len() + 4 + blob.len());
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(&blob_len.to_be_bytes());
    out.extend_from_slice(blob);

    w.write_all(&out)
        .map_err(|err| tf!("backend_ipc.frame.write_error", err = err))?;
    w.flush()
        .map_err(|err| tf!("backend_ipc.frame.flush_error", err = err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn round_trip_header_and_blob() {
        let header = json!({ "v": 1, "id": 42, "kind": "request", "method": "ocr.manga" });
        let blob = b"\x89PNG\r\n\x1a\nfake-png-bytes".to_vec();

        let mut buf = Vec::new();
        write_frame(&mut buf, &header, &blob).expect("write frame");

        let mut cursor = Cursor::new(buf);
        let frame = read_frame(&mut cursor).expect("read frame");
        assert_eq!(frame.header, header);
        assert_eq!(frame.blob, blob);
    }

    #[test]
    fn round_trip_empty_blob() {
        let header = json!({ "v": 1, "id": 0, "kind": "hello" });
        let mut buf = Vec::new();
        write_frame(&mut buf, &header, &[]).expect("write frame");

        let mut cursor = Cursor::new(buf);
        let frame = read_frame(&mut cursor).expect("read frame");
        assert_eq!(frame.header, header);
        assert!(frame.blob.is_empty());
    }

    #[test]
    fn rejects_oversized_header_guard() {
        // Craft a prefix declaring a header larger than MAX_HEADER_BYTES; the
        // reader must reject before allocating/reading the body.
        let mut buf = Vec::new();
        let bogus_len = (MAX_HEADER_BYTES as u32) + 1;
        buf.extend_from_slice(&bogus_len.to_be_bytes());
        let mut cursor = Cursor::new(buf);
        let err = read_frame(&mut cursor).expect_err("oversized header must be rejected");
        // Pin the exact catalog key + rendered value (not a substring marker).
        assert_eq!(
            err,
            tf!(
                "backend_ipc.frame.header_over_limit_read",
                header_len = MAX_HEADER_BYTES + 1,
                max_header_bytes = MAX_HEADER_BYTES
            ),
        );
    }

    #[test]
    fn rejects_oversized_blob_guard() {
        // Valid header, then a blob_len prefix that exceeds the guard.
        let header = json!({ "kind": "event" });
        let header_bytes = serde_json::to_vec(&header).unwrap();
        let mut buf = Vec::new();
        buf.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
        buf.extend_from_slice(&header_bytes);
        let bogus_blob_len = (MAX_BLOB_BYTES as u32) + 1;
        buf.extend_from_slice(&bogus_blob_len.to_be_bytes());
        let mut cursor = Cursor::new(buf);
        let err = read_frame(&mut cursor).expect_err("oversized blob must be rejected");
        assert_eq!(
            err,
            tf!(
                "backend_ipc.frame.blob_over_limit_read",
                blob_len = MAX_BLOB_BYTES + 1,
                max_blob_bytes = MAX_BLOB_BYTES
            ),
        );
    }

    #[test]
    fn rejects_non_object_header() {
        // `write_frame` debug-asserts the header is an object, so craft the frame
        // bytes manually (a non-object JSON header) to exercise the READER's guard.
        let header_bytes = serde_json::to_vec(&json!([1, 2, 3])).unwrap();
        let mut buf = Vec::new();
        buf.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
        buf.extend_from_slice(&header_bytes);
        buf.extend_from_slice(&0_u32.to_be_bytes()); // blob_len = 0
        let mut cursor = Cursor::new(buf);
        let err = read_frame(&mut cursor).expect_err("array header must be rejected");
        assert_eq!(err, t!("backend_ipc.frame.header_not_object"));
    }

    #[test]
    fn handles_partial_reads() {
        // A Reader that yields one byte per call exercises the fill-to-length
        // loops in read_exact_or_eof.
        struct DripReader {
            data: Vec<u8>,
            pos: usize,
        }
        impl Read for DripReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.pos >= self.data.len() || buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = self.data[self.pos];
                self.pos += 1;
                Ok(1)
            }
        }

        let header = json!({ "v": 1, "id": 5, "kind": "response", "status": "ok" });
        let blob = vec![1_u8, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let mut buf = Vec::new();
        write_frame(&mut buf, &header, &blob).expect("write frame");

        let mut drip = DripReader { data: buf, pos: 0 };
        let frame = read_frame(&mut drip).expect("read frame from drip reader");
        assert_eq!(frame.header, header);
        assert_eq!(frame.blob, blob);
    }

    #[test]
    fn truncated_frame_is_error() {
        let header = json!({ "kind": "hello" });
        let mut buf = Vec::new();
        write_frame(&mut buf, &header, &[]).expect("write frame");
        buf.truncate(buf.len() - 1); // drop last byte of the blob_len prefix
        let mut cursor = Cursor::new(buf);
        let err = read_frame(&mut cursor).expect_err("truncated frame must error");
        // The blob-length prefix (4 bytes) is truncated to 3, so the reader fills 3
        // of 4 bytes before EOF while reading the blob-length field.
        assert_eq!(
            err,
            tf!(
                "backend_ipc.frame.closed_mid_frame",
                what = t!("backend_ipc.frame.what_blob_len"),
                filled = 3,
                buf = 4
            ),
        );
    }
}
