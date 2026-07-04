/*
File: lib.rs

Purpose:
Public entry point of the `ms-storage` crate. Declares the synchronous,
object-safe `Storage` trait that abstracts ManhwaStudio's persistence away from
`std::fs`, plus the shared error/metadata/dir-entry types and the two concrete
backends.

Key structures:
- Storage (trait)      : the persistence contract used by the application
- StorageError         : typed error surface (thiserror)
- Metadata / DirEntry  : lightweight result records
- NativeStorage        : rooted std::fs backend (desktop)
- MemStorage           : in-memory virtual filesystem (web session store / tests)

Notes:
Paths are virtual, root-relative, and always use '/'; see `path::normalize`.
The trait is object-safe so the application can hold `Arc<dyn Storage>` and pick
the backend at startup (native folder vs. browser IndexedDB-backed memory).
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]
// The crate is intentionally named after the domain concept it exports.
#![allow(clippy::module_name_repetitions)]

mod mem;
mod path;
// `std::fs`-backed backends are desktop-only: they read real mtimes
// (`std::time::SystemTime`) and have no purpose in the browser, where `MemStorage`
// (hydrated from IndexedDB) is the store.
#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
mod passthrough;

pub use mem::MemStorage;
pub use path::normalize;
#[cfg(not(target_arch = "wasm32"))]
pub use native::{NativeStorage, real_path};
#[cfg(not(target_arch = "wasm32"))]
pub use passthrough::PassthroughStorage;

/// Result alias for storage operations.
pub type Result<T> = std::result::Result<T, StorageError>;

/// Typed error surface for every storage backend.
///
/// Backends map their native failures onto these variants so callers can react
/// uniformly regardless of whether the store is a real filesystem or an
/// in-memory tree. `Io` carries the original `std::io::Error` for diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The requested path does not exist.
    #[error("path not found: {0}")]
    NotFound(String),
    /// A directory operation targeted a file (or a file op targeted a dir).
    #[error("not a directory: {0}")]
    NotADirectory(String),
    /// A file operation targeted a directory.
    #[error("is a directory: {0}")]
    IsADirectory(String),
    /// The virtual path is malformed (empty component, illegal characters).
    #[error("invalid path: {0}")]
    InvalidPath(String),
    /// The virtual path used `..` to escape above the storage root.
    #[error("path escapes storage root: {0}")]
    Escape(String),
    /// A create operation found an existing entry it must not overwrite.
    #[error("already exists: {0}")]
    AlreadyExists(String),
    /// Underlying OS I/O error, tagged with the virtual path it happened on.
    #[error("io error at {path}: {source}")]
    Io {
        /// Virtual path the operation was performed on.
        path: String,
        /// Original OS error.
        source: std::io::Error,
    },
    /// Backend-specific failure that has no more precise variant.
    #[error("{0}")]
    Backend(String),
}

/// Minimal stat record returned by [`Storage::metadata`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    /// Length in bytes; `0` for directories.
    pub len: u64,
    /// Whether the entry is a directory.
    pub is_dir: bool,
    /// Last modification time if the backend tracks one. `None` for backends
    /// without modification timestamps (e.g. the in-memory web store), which
    /// is why external-change watchers must tolerate a missing value.
    pub modified: Option<web_time::SystemTime>,
}

/// One entry of a [`Storage::read_dir`] listing.
///
/// `name` is only the final path component, not a full path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Final path component (no separators).
    pub name: String,
    /// Whether this entry is a directory.
    pub is_dir: bool,
}

/// Synchronous, root-relative persistence contract.
///
/// All `path` arguments are virtual paths relative to the backend's root and
/// use '/' as the separator (backslashes are accepted and normalized). `.` and
/// `..` are resolved logically; a `..` that would leave the root yields
/// [`StorageError::Escape`]. Every method is non-generic so the trait stays
/// object-safe (`Arc<dyn Storage>`).
///
/// Semantics deliberately mirror `std::fs`: [`Storage::write`] and
/// [`Storage::rename`] do **not** create missing parent directories — callers
/// must [`Storage::create_dir_all`] first, exactly as the desktop code already
/// does.
pub trait Storage: Send + Sync + std::fmt::Debug {
    /// Reads the whole file at `path` into a byte vector.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if it does not exist, [`StorageError::IsADirectory`]
    /// if it is a directory, or [`StorageError::Io`] on backend failure.
    fn read(&self, path: &str) -> Result<Vec<u8>>;

    /// Writes `data` to `path`, truncating any existing file.
    ///
    /// The parent directory must already exist.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if the parent is missing,
    /// [`StorageError::IsADirectory`] if `path` names a directory, or
    /// [`StorageError::Io`] on backend failure.
    fn write(&self, path: &str, data: &[u8]) -> Result<()>;

    /// Returns whether any entry (file or directory) exists at `path`.
    fn exists(&self, path: &str) -> bool;

    /// Returns whether `path` exists and is a directory.
    fn is_dir(&self, path: &str) -> bool;

    /// Creates `path` and all missing parent directories. Idempotent.
    ///
    /// # Errors
    /// [`StorageError::NotADirectory`] if a path component is an existing file,
    /// or [`StorageError::Io`] on backend failure.
    fn create_dir_all(&self, path: &str) -> Result<()>;

    /// Lists the immediate children of the directory at `path`.
    ///
    /// Ordering is unspecified; callers that need determinism must sort.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if missing, [`StorageError::NotADirectory`] if
    /// `path` is a file, or [`StorageError::Io`] on backend failure.
    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>>;

    /// Removes the file at `path`.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if missing, [`StorageError::IsADirectory`] if
    /// it is a directory, or [`StorageError::Io`] on backend failure.
    fn remove_file(&self, path: &str) -> Result<()>;

    /// Recursively removes the directory at `path` and its contents.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if missing, [`StorageError::NotADirectory`] if
    /// `path` is a file, or [`StorageError::Io`] on backend failure.
    fn remove_dir_all(&self, path: &str) -> Result<()>;

    /// Renames/moves `from` to `to`. The destination's parent must exist.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if `from` or the destination parent is
    /// missing, or [`StorageError::Io`] on backend failure.
    fn rename(&self, from: &str, to: &str) -> Result<()>;

    /// Returns [`Metadata`] for the entry at `path`.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if missing, or [`StorageError::Io`] on backend
    /// failure.
    fn metadata(&self, path: &str) -> Result<Metadata>;

    /// Convenience: reads a file and decodes it as UTF-8.
    ///
    /// # Errors
    /// As [`Storage::read`], plus [`StorageError::Backend`] if the bytes are not
    /// valid UTF-8.
    fn read_to_string(&self, path: &str) -> Result<String> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes)
            .map_err(|e| StorageError::Backend(format!("invalid utf-8 in {path}: {e}")))
    }
}
