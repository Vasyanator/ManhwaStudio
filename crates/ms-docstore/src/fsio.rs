/*
File: fsio.rs

Purpose:
The store's file primitives outside the atomic write recipe: existence probe, whole-file
read, metadata, removal, rename and copy. Every other module of the crate addresses files
through here, so reads, stats and writes of one document always name the SAME file.

Key functions:
- exists() / read() / metadata() / remove() / rename() / copy()

Notes:
- Native: `std::fs` on the real `Path`. The storage seam takes `&str` paths, and a lossy
  UTF-8 conversion of a non-UTF-8 path (possible in Linux home directories) would probe a
  different name than the one the native atomic write replaces. The native global backend
  is the passthrough (`ms-storage`), so this is the same file system the seam would reach.
- wasm32: the `ms_storage::global::storage()` seam (there is no `std::fs`; the web store's
  virtual paths are UTF-8 by construction).
- Errors keep the seam's vocabulary on both targets (`StorageError::NotFound` / `Io` with
  the original `io::Error`), so callers match one error shape regardless of the target.
*/

use std::path::Path;

use ms_storage::StorageError;

/// Modification data of one file: byte length and modification time, when available.
pub(crate) struct FileMeta {
    /// File length in bytes.
    pub(crate) len: u64,
    /// Modification time, when the backend has one.
    pub(crate) modified: Option<web_time::SystemTime>,
}

/// Maps a native I/O error to the seam's vocabulary, tagged with `path`.
#[cfg(not(target_arch = "wasm32"))]
fn storage_err(path: &Path, err: std::io::Error) -> StorageError {
    match err.kind() {
        std::io::ErrorKind::NotFound => StorageError::NotFound(path.display().to_string()),
        std::io::ErrorKind::AlreadyExists => StorageError::AlreadyExists(path.display().to_string()),
        _ => StorageError::Io { path: path.display().to_string(), source: err },
    }
}

/// Whether a file (or directory) exists at `path`. A probe failure reads as absent.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn exists(path: &Path) -> bool {
    path.exists()
}

/// Whether a file (or directory) exists at `path`. A probe failure reads as absent.
#[cfg(target_arch = "wasm32")]
pub(crate) fn exists(path: &Path) -> bool {
    ms_storage::global::storage().exists(path.to_string_lossy().as_ref())
}

/// The whole file at `path`; `None` when it does not exist.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn read(path: &Path) -> Result<Option<Vec<u8>>, StorageError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(storage_err(path, err)),
    }
}

/// The whole file at `path`; `None` when it does not exist.
#[cfg(target_arch = "wasm32")]
pub(crate) fn read(path: &Path) -> Result<Option<Vec<u8>>, StorageError> {
    match ms_storage::global::storage().read(path.to_string_lossy().as_ref()) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(StorageError::NotFound(_)) => Ok(None),
        Err(err) => Err(err),
    }
}

/// Length and modification time of the file at `path`; `None` when it does not exist.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn metadata(path: &Path) -> Result<Option<FileMeta>, StorageError> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(FileMeta { len: meta.len(), modified: meta.modified().ok() })),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(storage_err(path, err)),
    }
}

/// Length and modification time of the file at `path`; `None` when it does not exist.
#[cfg(target_arch = "wasm32")]
pub(crate) fn metadata(path: &Path) -> Result<Option<FileMeta>, StorageError> {
    match ms_storage::global::storage().metadata(path.to_string_lossy().as_ref()) {
        Ok(meta) => Ok(Some(FileMeta { len: meta.len, modified: meta.modified })),
        Err(StorageError::NotFound(_)) => Ok(None),
        Err(err) => Err(err),
    }
}

/// Removes the file at `path`. Absent is not an error.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn remove(path: &Path) -> Result<(), StorageError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(storage_err(path, err)),
    }
}

/// Removes the file at `path`. Absent is not an error.
#[cfg(target_arch = "wasm32")]
pub(crate) fn remove(path: &Path) -> Result<(), StorageError> {
    match ms_storage::global::storage().remove_file(path.to_string_lossy().as_ref()) {
        Ok(()) | Err(StorageError::NotFound(_)) => Ok(()),
        Err(err) => Err(err),
    }
}

/// Renames `from` to `to`, replacing an existing `to` file.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn rename(from: &Path, to: &Path) -> Result<(), StorageError> {
    std::fs::rename(from, to).map_err(|err| storage_err(from, err))
}

/// Renames `from` to `to`, replacing an existing `to` file.
#[cfg(target_arch = "wasm32")]
pub(crate) fn rename(from: &Path, to: &Path) -> Result<(), StorageError> {
    ms_storage::global::storage().rename(from.to_string_lossy().as_ref(), to.to_string_lossy().as_ref())
}

/// Copies the file `from` to `to`, replacing an existing `to` file.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn copy(from: &Path, to: &Path) -> Result<(), StorageError> {
    std::fs::copy(from, to).map(|_| ()).map_err(|err| storage_err(from, err))
}

/// Copies the file `from` to `to`, replacing an existing `to` file (read + write: the seam
/// has no copy primitive).
#[cfg(target_arch = "wasm32")]
pub(crate) fn copy(from: &Path, to: &Path) -> Result<(), StorageError> {
    let store = ms_storage::global::storage();
    let bytes = store.read(from.to_string_lossy().as_ref())?;
    store.write(to.to_string_lossy().as_ref(), &bytes)
}
