/*
File: passthrough.rs

Purpose:
Native backend that addresses the real OS filesystem by absolute path verbatim,
without virtual-root remapping. It is the desktop `Storage` used by the app: the
application already builds absolute `PathBuf`s everywhere, so routing those exact
paths through `PassthroughStorage` is behaviorally identical to the previous
direct `std::fs` calls — the migration is a no-op on native.

Key structures:
- PassthroughStorage: identity-mapped std::fs backend.

Notes:
Unlike `NativeStorage`, this backend does NOT normalize or root-guard paths: the
vpath IS the real path (`PathBuf::from(vpath)`). That is safe here because every
path originates inside the app from trusted `config::*` roots, never from
untrusted input. Use `NativeStorage` (rooted, escape-guarded) for sandboxes.
*/

use std::path::PathBuf;

use crate::{DirEntry, Metadata, Storage, StorageError};

/// Identity-mapped native [`Storage`]: `vpath` is the real OS path.
///
/// Behaves exactly like calling `std::fs` on the given absolute path. Cloning is
/// trivial and shares the underlying OS filesystem.
#[derive(Debug, Clone, Default)]
pub struct PassthroughStorage;

impl PassthroughStorage {
    /// Creates the passthrough backend.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Maps a `std::io::Error` to a typed [`StorageError`], tagged with `path`.
    fn io_err(path: &str, e: std::io::Error) -> StorageError {
        match e.kind() {
            std::io::ErrorKind::NotFound => StorageError::NotFound(path.to_string()),
            std::io::ErrorKind::AlreadyExists => StorageError::AlreadyExists(path.to_string()),
            _ => StorageError::Io {
                path: path.to_string(),
                source: e,
            },
        }
    }
}

impl Storage for PassthroughStorage {
    fn read(&self, path: &str) -> Result<Vec<u8>, StorageError> {
        std::fs::read(PathBuf::from(path)).map_err(|e| Self::io_err(path, e))
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        std::fs::write(PathBuf::from(path), data).map_err(|e| Self::io_err(path, e))
    }

    fn exists(&self, path: &str) -> bool {
        PathBuf::from(path).exists()
    }

    fn is_dir(&self, path: &str) -> bool {
        PathBuf::from(path).is_dir()
    }

    fn create_dir_all(&self, path: &str) -> Result<(), StorageError> {
        std::fs::create_dir_all(PathBuf::from(path)).map_err(|e| Self::io_err(path, e))
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        let iter = std::fs::read_dir(PathBuf::from(path)).map_err(|e| Self::io_err(path, e))?;
        let mut out = Vec::new();
        for entry in iter {
            let entry = entry.map_err(|e| Self::io_err(path, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry
                .file_type()
                .map(|t| t.is_dir())
                .map_err(|e| Self::io_err(path, e))?;
            out.push(DirEntry { name, is_dir });
        }
        Ok(out)
    }

    fn remove_file(&self, path: &str) -> Result<(), StorageError> {
        std::fs::remove_file(PathBuf::from(path)).map_err(|e| Self::io_err(path, e))
    }

    fn remove_dir_all(&self, path: &str) -> Result<(), StorageError> {
        std::fs::remove_dir_all(PathBuf::from(path)).map_err(|e| Self::io_err(path, e))
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        std::fs::rename(PathBuf::from(from), PathBuf::from(to)).map_err(|e| Self::io_err(from, e))
    }

    fn metadata(&self, path: &str) -> Result<Metadata, StorageError> {
        let md = std::fs::metadata(PathBuf::from(path)).map_err(|e| Self::io_err(path, e))?;
        Ok(Metadata {
            len: md.len(),
            is_dir: md.is_dir(),
            modified: md.modified().ok(),
        })
    }
}
