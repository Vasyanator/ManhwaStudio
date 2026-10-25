/*
File: crates/ms-atomic-file/src/error.rs

Purpose:
The typed failure of the atomic write recipe (`AtomicWriteError`) and its conversion into
`std::io::Error` for callers whose own error vocabulary is I/O-shaped.

Notes:
The variant set and field names are public API re-exported by `ms-docstore` and destructured
by its dependents (e.g. `DirSync { dir, reason }` without `..`); adding a field or a variant is a
breaking change for them.
*/

use std::path::PathBuf;

/// Typed failure of the atomic write recipe. Every variant names the path it was working on
/// and the OS reason, so the log line and the user-facing message carry the same facts.
///
/// In EVERY variant the previous file is left untouched, except `DirSync`, where the new file
/// IS in place but its directory entry is not known to be durable yet — which is why that case
/// is still an error: the caller must not delete the data's old home.
///
/// Nothing is logged by this crate: the caller logs with its own context. `DirSync` in
/// particular has a caller-dependent severity (tolerable for a marker, fatal before deleting a
/// source).
#[derive(Debug, thiserror::Error)]
pub enum AtomicWriteError {
    /// The sibling temp file could not be created, written or fsynced — or, with
    /// `AtomicWriteOptions::create_parent`, the parent directory could not be created (then
    /// `path` is that directory). On wasm32 every write ends here with kind `Unsupported`.
    #[error("cannot write temp file {}: {reason}", path.display())]
    TempWrite {
        /// The temp file (or the parent directory) involved.
        path: PathBuf,
        /// OS reason.
        reason: String,
        /// Kind of the underlying I/O error (`InvalidData` for a failed read-back
        /// validation), so a caller can tell e.g. a permission problem from a full disk
        /// without parsing the (possibly localized) `reason`.
        kind: std::io::ErrorKind,
    },
    /// The finished temp file could not be renamed over the target.
    #[error("cannot replace {}: {reason}", path.display())]
    Rename {
        /// The target file.
        path: PathBuf,
        /// OS reason.
        reason: String,
        /// Kind of the underlying I/O error of the (last) rename attempt.
        kind: std::io::ErrorKind,
    },
    /// The rename succeeded but the containing directory could not be fsynced.
    #[error("the new file is written but directory {} could not be fsynced: {reason}", dir.display())]
    DirSync {
        /// The directory that could not be fsynced.
        dir: PathBuf,
        /// OS reason.
        reason: String,
    },
}

impl AtomicWriteError {
    /// The I/O error kind behind this failure; `Other` for `DirSync`, which keeps no kind.
    #[must_use]
    pub fn kind(&self) -> std::io::ErrorKind {
        match self {
            Self::TempWrite { kind, .. } | Self::Rename { kind, .. } => *kind,
            Self::DirSync { .. } => std::io::ErrorKind::Other,
        }
    }
}

/// For callers whose error type wraps `std::io::Error`: the kind is kept (see
/// [`AtomicWriteError::kind`]) and the full message (path + reason) becomes the error text.
impl From<AtomicWriteError> for std::io::Error {
    fn from(err: AtomicWriteError) -> Self {
        std::io::Error::new(err.kind(), err)
    }
}
