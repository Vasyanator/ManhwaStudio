/*
File: crates/ms-browser-ext/src/install.rs

Purpose:
The unpacked extension folder: where it lives, which bundle it holds, and the atomic,
version-stamped unpack that keeps Chrome's one-time "Load unpacked" registration valid across
app updates (the path never changes, only its files).

Key items:
- install_dir() — `<data_dir>/browser_extension/chromium`, the one owner of the folder name
- installed_build_id() — the stamp's `build_id`, `None` when nothing complete is installed
- install() — write every bundle file (temp + fsync + rename through `ms_atomic_file`), drop
  files the OLD stamp listed that the new bundle lacks, sync each touched folder once, write the
  new stamp LAST
- InstallReport, InstallError

Contracts:
- The folder is complete for a bundle exactly when its stamp names that bundle's `build_id`.
  A crash mid-install leaves the old stamp or none, which reads as outdated / not installed.
- Only files listed in the previous stamp are ever deleted: `bridge.json` (written by the
  bridge) and any user file in the folder survive every install.
- One install per folder at a time, across processes: `install` holds an exclusive advisory
  lock on `INSTALL_LOCK_FILE_NAME` inside the folder from the old stamp's read through the final
  stamp write and directory sync. A second installer (another app process, or another thread)
  WAITS for it, then re-reads the stamp, so two app versions cannot interleave their files under
  one stamp. The OS drops the lock when its holder dies; the lock file is never deleted.
- Blocking file I/O (and the lock wait): call from a worker thread, never the GUI thread.
*/

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use ms_atomic_file::{AtomicWriteOptions, Durability, sync_directory, write_atomic_with};
use serde::{Deserialize, Serialize};

use crate::bundle::Bundle;
use crate::names::STAMP_FILE_NAME;

/// File name of the install transaction's inter-process lock inside the folder (see the file
/// header). A dot file: never a bundle path, ignored by the browser.
const INSTALL_LOCK_FILE_NAME: &str = ".ms-install.lock";

/// Folder of the unpacked Chromium extension under the app's data directory
/// (`ms_config::data_dir()`): `<data_dir>/browser_extension/chromium`. Stable forever: the
/// user registers this path once in the browser.
#[must_use]
pub fn install_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("browser_extension").join("chromium")
}

/// What `install` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// `build_id` now installed (the bundle's).
    pub build_id: String,
    /// `build_id` of the previous complete install, if its stamp was readable.
    pub previous_build_id: Option<String>,
    /// Files written because they were missing or differed.
    pub written: usize,
    /// Files already byte-identical, left untouched.
    pub unchanged: usize,
    /// Files of the previous bundle that the new one lacks, deleted.
    pub removed: usize,
    /// True when the previous stamp was corrupt, so stale files of that install could not be
    /// identified and were left in place.
    pub previous_stamp_corrupt: bool,
}

/// Failure of `installed_build_id` / `install`.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// A bundle path is absolute, escapes the folder, is a reserved name or repeats.
    #[error("invalid bundle path {path:?}: {reason}")]
    InvalidBundlePath {
        /// The offending relative path.
        path: String,
        /// Why it was refused.
        reason: &'static str,
    },
    /// A file-system operation failed.
    #[error("{op} failed for {}: {source}", path.display())]
    Io {
        /// The operation, e.g. "create directory", "write", "rename".
        op: &'static str,
        /// The path it was applied to.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// The new stamp could not be serialized (not expected for plain strings; kept typed).
    #[error("the extension stamp could not be encoded: {0}")]
    StampEncode(#[source] serde_json::Error),
    /// The stamp exists but is not valid JSON of the expected shape.
    #[error("the extension stamp {} is corrupt: {source}", path.display())]
    StampCorrupt {
        /// The stamp file.
        path: PathBuf,
        /// The parse error.
        #[source]
        source: serde_json::Error,
    },
}

/// On-disk stamp `ms-extension-stamp.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Stamp {
    build_id: String,
    files: Vec<String>,
}

fn io_error<'a>(op: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> InstallError + 'a {
    move |source| InstallError::Io { op, path: path.to_path_buf(), source }
}

/// Reads the stamp of `dir`; `Ok(None)` when there is none (or no folder at all).
fn read_stamp(dir: &Path) -> Result<Option<Stamp>, InstallError> {
    let path = dir.join(STAMP_FILE_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(InstallError::Io { op: "read", path, source }),
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|source| InstallError::StampCorrupt { path, source })
}

/// `build_id` of the complete install in `dir`, or `None` when nothing is installed there.
///
/// # Errors
/// `Io` when the stamp cannot be read, `StampCorrupt` when it is not a valid stamp (callers
/// treat that as "needs install"; `install` replaces it).
pub fn installed_build_id(dir: &Path) -> Result<Option<String>, InstallError> {
    Ok(read_stamp(dir)?.map(|stamp| stamp.build_id))
}

/// Checks that `path` is a plain relative `/`-separated path inside the folder.
fn validate_relative(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("empty path");
    }
    if path.starts_with('/') || path.contains('\\') || path.contains(':') {
        return Err("not a relative '/'-separated path");
    }
    if path.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
        return Err("empty, '.' or '..' component");
    }
    if path == STAMP_FILE_NAME {
        return Err("reserved for the installer stamp");
    }
    if path == INSTALL_LOCK_FILE_NAME {
        return Err("reserved for the installer lock");
    }
    Ok(())
}

fn validate_bundle(bundle: &Bundle) -> Result<(), InstallError> {
    let mut seen = BTreeSet::new();
    for (path, _) in bundle.files {
        validate_relative(path).map_err(|reason| InstallError::InvalidBundlePath { path: (*path).to_owned(), reason })?;
        if !seen.insert(*path) {
            return Err(InstallError::InvalidBundlePath { path: (*path).to_owned(), reason: "listed twice" });
        }
    }
    Ok(())
}

/// Joins a validated relative path onto `dir`, one component at a time.
fn target_path(dir: &Path, relative: &str) -> PathBuf {
    relative.split('/').fold(dir.to_path_buf(), |acc, part| acc.join(part))
}

/// Writes `bytes` to `path` through the workspace's one atomic recipe
/// (`ms_atomic_file::write_atomic`): sibling temp `.{name}.{pid}.tmp`, write, fsync, rename over
/// `path`; the temp is removed (best effort) on failure. The parent must exist. No
/// identical-bytes shortcut: `install` classifies identical files itself (`is_identical`) for
/// its report, so a file reaching this call always differs. The directory is NOT fsynced here:
/// `install` syncs each touched folder once (`sync_dir`).
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), InstallError> {
    let options = AtomicWriteOptions { skip_if_identical: false, ..AtomicWriteOptions::new(Durability::Contents) };
    write_atomic_with(path, bytes, &options).map_err(|err| InstallError::Io { op: "write", path: path.to_path_buf(), source: io::Error::from(err) })
}

/// Makes the renames inside `dir` durable before the stamp is written
/// (`ms_atomic_file::sync_directory`: an fsync on unix, a documented no-op on Windows, where
/// the filesystem journals the rename).
fn sync_dir(dir: &Path) -> Result<(), InstallError> {
    sync_directory(dir).map_err(|err| InstallError::Io { op: "sync directory", path: dir.to_path_buf(), source: io::Error::from(err) })
}

/// Holds the folder's install lock; released on drop (closing the handle releases it as well).
#[derive(Debug)]
struct InstallLock {
    file: File,
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        // The explicit unlock releases the lock at once (Windows may release a closed handle's
        // locks late). Its failure needs no handling: `drop` cannot report it (this crate does
        // not log), and closing the handle right after releases the lock in every case.
        match self.file.unlock() {
            Ok(()) | Err(_) => {}
        }
    }
}

/// Opens (creating it) and locks `dir`'s install lock, WAITING while another installer holds it.
fn lock_install(dir: &Path) -> Result<InstallLock, InstallError> {
    let path = dir.join(INSTALL_LOCK_FILE_NAME);
    let file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).map_err(io_error("open install lock", &path))?;
    file.lock().map_err(io_error("lock", &path))?;
    Ok(InstallLock { file })
}

/// True when `path` already holds exactly `bytes`.
fn is_identical(path: &Path, bytes: &[u8]) -> Result<bool, InstallError> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() && u64::try_from(bytes.len()).is_ok_and(|len| len == meta.len()) => Ok(fs::read(path).map_err(io_error("read", path))? == bytes),
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(InstallError::Io { op: "inspect", path: path.to_path_buf(), source }),
    }
}

/// Unpacks `bundle` into `dir` (created if missing) atomically per file, removes the files of
/// the previous install that the bundle no longer has, then writes the stamp last — all under the
/// folder's inter-process install lock (blocks while another installer holds it).
///
/// # Errors
/// `InvalidBundlePath` before anything is touched; `Io` for the lock file (open / lock);
/// `Io` for a failed write, rename, delete or directory sync — the stamp is then
/// NOT updated, so the folder reads as outdated and a retry repairs it.
pub fn install(bundle: &Bundle, dir: &Path) -> Result<InstallReport, InstallError> {
    validate_bundle(bundle)?;
    fs::create_dir_all(dir).map_err(io_error("create directory", dir))?;
    // Held to the end of this function: covers the old stamp's read through the final sync.
    let _lock = lock_install(dir)?;
    let (previous, previous_stamp_corrupt) = match read_stamp(dir) {
        Ok(stamp) => (stamp, false),
        // A corrupt stamp cannot say which files are stale; they stay, the stamp is replaced.
        Err(InstallError::StampCorrupt { .. }) => (None, true),
        Err(other) => return Err(other),
    };

    let mut report = InstallReport {
        build_id: bundle.build_id.to_owned(),
        previous_build_id: previous.as_ref().map(|stamp| stamp.build_id.clone()),
        written: 0,
        unchanged: 0,
        removed: 0,
        previous_stamp_corrupt,
    };
    let mut touched_dirs = BTreeSet::new();
    touched_dirs.insert(dir.to_path_buf());
    for (relative, bytes) in bundle.files {
        let target = target_path(dir, relative);
        if is_identical(&target, bytes)? {
            report.unchanged += 1;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_error("create directory", parent))?;
            touched_dirs.insert(parent.to_path_buf());
        }
        write_atomic(&target, bytes)?;
        report.written += 1;
    }

    if let Some(previous) = &previous {
        let current: BTreeSet<&str> = bundle.files.iter().map(|(path, _)| *path).collect();
        // Only paths the previous stamp listed AND that are valid relative paths are ever
        // deleted, so a damaged or hostile stamp cannot reach outside the folder.
        for stale in previous.files.iter().filter(|path| !current.contains(path.as_str()) && validate_relative(path).is_ok()) {
            let target = target_path(dir, stale);
            match fs::remove_file(&target) {
                Ok(()) => report.removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => return Err(InstallError::Io { op: "remove stale file", path: target, source }),
            }
        }
    }

    for touched in &touched_dirs {
        sync_dir(touched)?;
    }
    let stamp = Stamp { build_id: bundle.build_id.to_owned(), files: bundle.files.iter().map(|(path, _)| (*path).to_owned()).collect() };
    let stamp_bytes = serde_json::to_vec_pretty(&stamp).map_err(InstallError::StampEncode)?;
    write_atomic(&dir.join(STAMP_FILE_NAME), &stamp_bytes)?;
    sync_dir(dir)?;
    Ok(report)
}

#[cfg(test)]
mod tests;
