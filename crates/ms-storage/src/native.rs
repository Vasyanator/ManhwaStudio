/*
File: native.rs

Purpose:
Desktop backend for the `Storage` trait. Maps virtual paths onto a real
`std::fs` directory rooted at a fixed base path and translates OS errors into
typed `StorageError`s.

Key structures:
- NativeStorage: rooted std::fs backend.

Notes:
Because `path::normalize` strips every `..`, the joined real path can never
escape `root`; there is no TOCTOU traversal risk from the path string itself.
*/

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::{DirEntry, Metadata, Storage, StorageError, path};

/// `std::fs` backed [`Storage`] rooted at a base directory.
///
/// All virtual paths are resolved relative to `root`; the backend never reads or
/// writes outside it. Cloning is cheap and shares the same root (backed by the
/// OS filesystem, so all clones observe the same state).
#[derive(Debug, Clone)]
pub struct NativeStorage {
    root: PathBuf,
}

impl NativeStorage {
    /// Creates a backend rooted at `root`.
    ///
    /// The directory is not required to exist yet; callers can
    /// [`Storage::create_dir_all`] `""` to materialize it.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolves a virtual path to a real filesystem path under `root`.
    fn resolve(&self, vpath: &str) -> Result<PathBuf, StorageError> {
        let comps = path::normalize(vpath)?;
        let mut p = self.root.clone();
        p.extend(comps.iter().map(String::as_str));
        Ok(p)
    }

    /// Wraps an `std::io::Error` as a typed [`StorageError`] for `vpath`.
    fn io_err(vpath: &str, e: std::io::Error) -> StorageError {
        match e.kind() {
            std::io::ErrorKind::NotFound => StorageError::NotFound(vpath.to_string()),
            std::io::ErrorKind::AlreadyExists => StorageError::AlreadyExists(vpath.to_string()),
            _ => StorageError::Io {
                path: vpath.to_string(),
                source: e,
            },
        }
    }
}

impl Storage for NativeStorage {
    fn read(&self, path: &str) -> Result<Vec<u8>, StorageError> {
        let real = self.resolve(path)?;
        if real.is_dir() {
            return Err(StorageError::IsADirectory(path.to_string()));
        }
        std::fs::read(&real).map_err(|e| Self::io_err(path, e))
    }

    fn read_prefix(&self, path: &str, max_len: usize) -> Result<Vec<u8>, StorageError> {
        let real = self.resolve(path)?;
        if real.is_dir() {
            return Err(StorageError::IsADirectory(path.to_string()));
        }
        let file = std::fs::File::open(&real).map_err(|e| Self::io_err(path, e))?;
        // `take` bounds the read so a huge file costs only `max_len` bytes of I/O.
        // The length saturates instead of overflowing, keeping the method total.
        let mut buf = Vec::new();
        file.take(u64::try_from(max_len).unwrap_or(u64::MAX))
            .read_to_end(&mut buf)
            .map_err(|e| Self::io_err(path, e))?;
        Ok(buf)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), StorageError> {
        let real = self.resolve(path)?;
        if real.is_dir() {
            return Err(StorageError::IsADirectory(path.to_string()));
        }
        std::fs::write(&real, data).map_err(|e| Self::io_err(path, e))
    }

    fn exists(&self, path: &str) -> bool {
        self.resolve(path).is_ok_and(|p| p.exists())
    }

    fn is_dir(&self, path: &str) -> bool {
        self.resolve(path).is_ok_and(|p| p.is_dir())
    }

    fn create_dir_all(&self, path: &str) -> Result<(), StorageError> {
        let real = self.resolve(path)?;
        std::fs::create_dir_all(&real).map_err(|e| Self::io_err(path, e))
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        let real = self.resolve(path)?;
        let iter = std::fs::read_dir(&real).map_err(|e| Self::io_err(path, e))?;
        let mut out = Vec::new();
        for entry in iter {
            let entry = entry.map_err(|e| Self::io_err(path, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // `file_type` avoids an extra stat when the OS already knows the kind.
            let is_dir = entry
                .file_type()
                .map(|t| t.is_dir())
                .map_err(|e| Self::io_err(path, e))?;
            out.push(DirEntry { name, is_dir });
        }
        Ok(out)
    }

    fn remove_file(&self, path: &str) -> Result<(), StorageError> {
        let real = self.resolve(path)?;
        if real.is_dir() {
            return Err(StorageError::IsADirectory(path.to_string()));
        }
        std::fs::remove_file(&real).map_err(|e| Self::io_err(path, e))
    }

    fn remove_dir_all(&self, path: &str) -> Result<(), StorageError> {
        let real = self.resolve(path)?;
        if real.is_file() {
            return Err(StorageError::NotADirectory(path.to_string()));
        }
        std::fs::remove_dir_all(&real).map_err(|e| Self::io_err(path, e))
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let from_real = self.resolve(from)?;
        let to_real = self.resolve(to)?;
        std::fs::rename(&from_real, &to_real).map_err(|e| Self::io_err(from, e))
    }

    fn metadata(&self, path: &str) -> Result<Metadata, StorageError> {
        let real = self.resolve(path)?;
        let md = std::fs::metadata(&real).map_err(|e| Self::io_err(path, e))?;
        Ok(Metadata {
            len: md.len(),
            is_dir: md.is_dir(),
            modified: md.modified().ok(),
        })
    }
}

/// Returns the real on-disk path a [`NativeStorage`] would use for `vpath`.
///
/// Exposed for migration shims that still need a concrete `&Path` (e.g. passing
/// to `image::open`) while the caller is being ported to byte-oriented I/O.
///
/// # Errors
/// Propagates [`StorageError::Escape`]/[`StorageError::InvalidPath`] from
/// normalization.
pub fn real_path(root: &Path, vpath: &str) -> Result<PathBuf, StorageError> {
    let comps = path::normalize(vpath)?;
    let mut p = root.to_path_buf();
    p.extend(comps.iter().map(String::as_str));
    Ok(p)
}
