/*
File: crates/ms-native-package/src/error.rs

Purpose:
`PackageError`, the typed failure of installing, probing names of, and removing a pinned
native package, and `PackageIoOp`, the filesystem step an `Io` failure belongs to.

Notes:
- The large cause payloads (`DownloadError`, `UnpackError`, `AtomicWriteError`) are boxed so
  every `Result<_, PackageError>` stays small (`clippy::result_large_err`; `PathBuf` is wider on
  Windows).
- `Display` is English diagnostic text without secrets. Every variant keeps the operation, the
  path(s) and the underlying cause as separate fields, so a consumer maps each one onto its own
  localized text (e.g. `ms_onnx_runtime::map_package_error`) instead of parsing a message.
- Which staged files survive is stated per variant; no member is published on an error
  before the failing step, and the completion marker is never present after one.
*/

use std::path::PathBuf;

use ms_atomic_file::AtomicWriteError;
use ms_download::DownloadError;
use ms_download::unpack::UnpackError;

/// The filesystem step of a [`PackageError::Io`] failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageIoOp {
    /// Creating the package directory (`path`).
    CreateDir,
    /// Creating the `.download` staging directory (`path`).
    CreateDownloadDir,
    /// Removing a `.part` staged for another archive (`path`).
    RemovePart,
    /// Writing the `.part.id` identity sidecar (`path`).
    WriteIdentity,
    /// Removing the completion marker (`path`).
    RemoveMarker,
    /// Removing the package directory tree (`path`) in `remove_installed`.
    RemoveDir,
}

impl std::fmt::Display for PackageIoOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::CreateDir => "creating the package directory",
            Self::CreateDownloadDir => "creating the download directory",
            Self::RemovePart => "removing a foreign partial download",
            Self::WriteIdentity => "writing the download identity",
            Self::RemoveMarker => "removing the completion marker",
            Self::RemoveDir => "removing the package directory",
        })
    }
}

/// Failure of [`crate::ensure_installed`], [`crate::remove_installed`] or
/// [`crate::flatten_member_name`].
#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    /// The cancel flag stopped the busy wait, the transfer, a retry wait or the hash pass;
    /// the staged `.part` is kept so the next call resumes it.
    #[error("the package download was cancelled")]
    Cancelled,
    /// The archive at `url` does not hash to its pinned sha256; its `.part` was deleted and
    /// nothing was extracted.
    #[error("sha256 mismatch for {url}: expected {expected}, got {actual}")]
    ChecksumMismatch { url: String, expected: String, actual: String },
    /// Any other failure of the download of `url` (permanent HTTP failure, retries exhausted,
    /// size mismatch, I/O while staging, …), as `ms-download` reported it. A partial `.part`
    /// is kept unless its bytes were proven wrong. For a busy wait that failed for a reason
    /// other than cancel, `url` is the LAST source's URL.
    #[error("downloading {url} failed: {source}")]
    Download { url: String, source: Box<DownloadError> },
    /// Staging the members of the verified archive `archive` failed; every temporary is
    /// removed, no member of that archive is published, the archive stays staged.
    #[error("unpacking {} failed: {source}", archive.display())]
    Unpack { archive: PathBuf, source: Box<UnpackError> },
    /// A filesystem step `op` on `path` failed.
    #[error("{op} failed on {}: {source}", path.display())]
    Io { op: PackageIoOp, path: PathBuf, source: std::io::Error },
    /// Renaming the staged member `from` onto `to` failed; the members renamed before it stay
    /// published, the completion marker stays absent, so the next call repairs the set.
    #[error("cannot publish {} as {}: {source}", from.display(), to.display())]
    Publish { from: PathBuf, to: PathBuf, source: std::io::Error },
    /// The package directory `dir` could not be fsynced after a removal or the member renames.
    #[error("cannot fsync directory {}: {source}", dir.display())]
    SyncDir { dir: PathBuf, source: Box<AtomicWriteError> },
    /// The completion marker `path` (inside the package directory `dir`) could not be written
    /// atomically; the package stays untrusted.
    #[error("cannot write the completion marker {}: {source}", path.display())]
    WriteMarker { path: PathBuf, dir: PathBuf, source: Box<AtomicWriteError> },
    /// An archive member path has no file-name component (e.g. `/` or `a/..`).
    #[error("invalid archive member path '{member}'")]
    InvalidMemberPath { member: String },
    /// A caller-supplied file name (the marker's, a member's or an archive's) is not one plain
    /// path component, or a member / archive name collides with the marker or the `.download`
    /// directory. Rejected before any I/O.
    #[error("invalid package file name '{name}'")]
    InvalidFileName { name: String },
    /// Two files of one recipe would share a path: an archive name (or one of its staging
    /// names) used twice across sources, or a member name (or its `.tmp` temporary) used twice
    /// across all sources. Rejected before any I/O.
    #[error("package file name '{name}' collides with another file of the recipe")]
    DuplicateFileName { name: String },
}

impl PackageError {
    /// Maps an `ms-download` failure of `url`: cancel and a sha256 mismatch get their own
    /// variants, everything else is [`PackageError::Download`].
    pub(crate) fn from_download(url: &str, err: DownloadError) -> Self {
        match err {
            DownloadError::Cancelled => Self::Cancelled,
            DownloadError::HashMismatch { expected, actual } => Self::ChecksumMismatch { url: url.to_owned(), expected, actual },
            source @ (DownloadError::Busy { .. }
            | DownloadError::InvalidRequest { .. }
            | DownloadError::Io { .. }
            | DownloadError::Http { .. }
            | DownloadError::SizeMismatch { .. }
            | DownloadError::Archive { .. }) => Self::Download { url: url.to_owned(), source: Box::new(source) },
        }
    }
}
