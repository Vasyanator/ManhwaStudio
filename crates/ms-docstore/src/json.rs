/*
File: json.rs

Purpose:
The optimistic-concurrency vocabulary of every owned document. The JSON codec's write recipe
itself (atomic temp + fsync + rename, temp naming, step journal) lives in `ms-atomic-file`
and is re-exported from the crate root unchanged.

Main responsibilities:
- `Fingerprint` / `SaveBaseline`: the "is the file still what I last read?" check two
  running instances of the app need in order not to overwrite each other silently.

Key types:
- `Fingerprint` (byte length + 64-bit digest of one exact document state)
- `SaveBaseline` (what the caller expects to find on disk: Unchecked / Absent / Matching)
*/

/// Identity of one exact on-disk state of a document: its byte length plus a 64-bit digest
/// of its bytes.
///
/// It exists so a writer can tell "the file is still what I last read/wrote" from "another
/// process replaced it", WITHOUT depending on a filesystem timestamp (whose resolution is
/// coarse enough that two writes inside one second are indistinguishable, and which a copy
/// or a restore can move backwards). Never persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint {
    /// Length of the document in bytes.
    len: u64,
    /// First 8 bytes of the SHA-256 digest of the document bytes, read big-endian.
    digest: u64,
}

/// What a caller expects to find on disk when it saves — the optimistic-concurrency check
/// that keeps two running app instances from silently overwriting each other's data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SaveBaseline {
    /// No expectation at all: overwrite whatever is there. Used before this process has
    /// read the document even once, and by documents without a conflict contract.
    #[default]
    Unchecked,
    /// The document is expected to be ABSENT (this process saw no file).
    Absent,
    /// The document is expected to hash exactly to this fingerprint.
    Matching(Fingerprint),
}

impl SaveBaseline {
    /// Whether a document currently hashing to `found` satisfies this expectation.
    /// `Absent` never does — an existing file is by definition not the absence we expected.
    /// (A file that is absent at write time never blocks a write; that rule lives in the
    /// store's write path, not here.)
    #[must_use]
    pub fn accepts(self, found: Fingerprint) -> bool {
        match self {
            Self::Unchecked => true,
            Self::Absent => false,
            Self::Matching(expected) => expected == found,
        }
    }
}

/// 64-bit fingerprint of `contents`: the first 8 bytes of its SHA-256, read big-endian,
/// plus the byte length.
#[must_use]
pub fn fingerprint(contents: &[u8]) -> Fingerprint {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(contents);
    // SHA-256 is 32 bytes, so the first 8 always exist; the fixed-size copy cannot panic.
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    Fingerprint {
        // The length is a cheap first discriminator beside the digest; a hypothetical
        // >2^64-byte document would saturate rather than wrap, which only ever makes the
        // comparison stricter (never falsely "unchanged").
        len: u64::try_from(contents.len()).unwrap_or(u64::MAX),
        digest: u64::from_be_bytes(head),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The baseline vocabulary: only an exact match (or "no expectation") accepts a
    /// document, and `Absent` never accepts an existing one.
    #[test]
    fn baselines_accept_exactly_what_they_promise() {
        let one = fingerprint(b"{}\n");
        let other = fingerprint(b"{ }\n");
        assert!(SaveBaseline::Unchecked.accepts(one));
        assert!(!SaveBaseline::Absent.accepts(one));
        assert!(SaveBaseline::Matching(one).accepts(one));
        assert!(!SaveBaseline::Matching(one).accepts(other));
    }
}
