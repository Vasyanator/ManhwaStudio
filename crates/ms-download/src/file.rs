/*
File: crates/ms-download/src/file.rs

Purpose:
One verified file end to end: wait for the destination's busy guard, keep an already-correct
destination, otherwise resume the `.part` (rehashing its prefix when a sha256 is pinned),
stream the rest through `ResumableFetch::fetch_part`, verify, fsync and replace-rename it into
place. Also the publish / hashing helpers `unpack` shares.

Key items:
- `Expected`: what the content is checked against (server-announced size, pinned size, or
  pinned size + sha256).
- `FileRequest`, `FileEvent`: input and progress of `fetch_verified`.
- `fetch_verified()`: the operation.

Notes:
- Two callers that publish the same `dest` are serialized by `busy::dest_key(dest)`; the
  second one then finds the file already verified and returns at once (no transfer).
- With `Expected::Announced` an existing `dest` cannot be verified, so it is downloaded again
  and replaced; callers that use it decide presence before calling.
- Resume is bound to the remote object: the `RemoteIdentity` of the response that started
  the `.part` is kept in `<part>.identity`; a later response with another identity (or a
  `.part` without a recorded identity while the server now names one) restarts the file
  from zero, so a file replaced upstream is never spliced onto an older prefix. Weak ETags
  are no identity. Without a pinned sha256 and without a strong identity, a `.part` left by
  an EARLIER run restarts from zero; retries within one run still resume. Without a pinned sha256, an LFS sha256 the
  server announced (`RemoteIdentity::lfs_sha256`) is checked before publishing.
- Publishing is `std::fs::rename`, which replaces an existing target on every supported
  platform (`MoveFileExW` + `MOVEFILE_REPLACE_EXISTING` on Windows); on unix the parent
  directory is fsynced afterwards so the rename itself is durable.
*/

use std::cell::{Cell, RefCell};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::Digest;

use crate::busy::{BusyGuard, dest_key};
use crate::error::DownloadError;
use crate::resumable::{CHUNK, FetchError, FetchResponse, PartRequest, RangeFetcher, RemoteIdentity, ResumableFetch, StreamEvent, chunk_len};

/// What the downloaded (or unpacked) content must match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected<'a> {
    /// No pin: the size the server announces (or, for an archive member, the size its header
    /// declares) is the only check.
    Announced,
    /// Exactly this many bytes.
    Size(u64),
    /// Exactly `size` bytes hashing to `sha256` (64 hex digits, either case).
    SizeSha256 { size: u64, sha256: &'a str },
}

impl Expected<'_> {
    /// The pinned size, if any.
    #[must_use]
    pub fn size(&self) -> Option<u64> {
        match self {
            Self::Announced => None,
            Self::Size(size) | Self::SizeSha256 { size, .. } => Some(*size),
        }
    }

    /// The pinned sha256 as given, if any.
    #[must_use]
    pub fn sha256(&self) -> Option<&str> {
        match self {
            Self::Announced | Self::Size(_) => None,
            Self::SizeSha256 { sha256, .. } => Some(sha256),
        }
    }

    /// The pinned sha256 lowercased, after checking it is 64 hex digits.
    ///
    /// # Errors
    /// `InvalidRequest` for a malformed pin (it could never match, so every run would
    /// download and discard the whole file).
    pub(crate) fn checked_sha256(&self) -> Result<Option<String>, DownloadError> {
        match self.sha256() {
            None => Ok(None),
            Some(sha256) if sha256.len() == 64 && sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) => Ok(Some(sha256.to_ascii_lowercase())),
            Some(sha256) => Err(DownloadError::InvalidRequest { detail: format!("sha256 pin '{sha256}' is not 64 hex digits") }),
        }
    }
}

/// One file to download and publish.
#[derive(Debug, Clone, Copy)]
pub struct FileRequest<'a> {
    /// Resolve URL of the file (never logged by this crate; `log_context` names the file).
    pub url: &'a str,
    /// The `.part` staging file; its parent directory is created when missing. Callers that
    /// share `dest` must share this path too, or the second one restarts from zero.
    pub part: &'a Path,
    /// The published file; its parent directory is created when missing.
    pub dest: &'a Path,
    /// What the content is checked against.
    pub expected: Expected<'a>,
}

/// Progress of [`fetch_verified`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEvent {
    /// Another download in this process is publishing the same `dest`; waiting for it (sent
    /// once per call).
    Waiting,
    /// Hashing an existing destination or a staged prefix: `done` of `total` bytes.
    Verifying { done: u64, total: u64 },
    /// Transfer progress and retry waits of the stream.
    Stream(StreamEvent),
}

/// Makes `req.dest` present and verified against `req.expected`, and returns its size.
///
/// Steps: take the busy guard of `dest` (waiting, cancellable, `FileEvent::Waiting`); keep an
/// existing `dest` that already matches a pinned size (and sha256); otherwise resume the
/// `.part` — rehashing its prefix when a sha256 is pinned, dropping it when it is longer than
/// the pin — stream the rest through `engine` (retry, resume and cancel per its policy),
/// check the sha256 (deleting the `.part` on a mismatch), and replace-rename the fsynced
/// `.part` onto `dest`, fsyncing the parent directory on unix.
///
/// The `.part` is bound to the [`RemoteIdentity`] of the response that started it (sidecar
/// `<part>.identity`, see the file header): a resume answered by another identity restarts
/// from zero. Without a pinned sha256, a `lfs_sha256` the server announced is verified (one
/// extra read of the staged file, reported as `Verifying`) before publishing.
///
/// # Errors
/// `InvalidRequest` (malformed sha256 pin), `Cancelled` (busy wait, hash pass, transfer or
/// backoff; `.part` kept), `Io`, `Http` (`.part` kept), `SizeMismatch` and `HashMismatch`
/// (`.part` deleted; also for a mismatching announced LFS sha256).
pub fn fetch_verified(engine: &ResumableFetch<'_>, req: &FileRequest<'_>, progress: &mut dyn FnMut(FileEvent)) -> Result<u64, DownloadError> {
    let pinned_sha = req.expected.checked_sha256()?;
    let pinned_size = req.expected.size();
    let key = dest_key(req.dest);
    // Held until return: no other caller in the process stages or publishes this `dest`.
    let _guard = BusyGuard::acquire_waiting(&key, engine.cancel, engine.retry, &mut || progress(FileEvent::Waiting))?;

    if let Some(size) = pinned_size
        && file_len(req.dest) == Some(size)
    {
        let matches = match &pinned_sha {
            None => true,
            Some(expected) => {
                let mut hasher = sha2::Sha256::new();
                hash_file(req.dest, &mut hasher, engine.cancel, &mut |done, total| progress(FileEvent::Verifying { done, total }))?;
                hex_lower(&hasher.finalize()) == *expected
            }
        };
        if matches {
            remove_file_if_exists(req.part)?;
            remove_file_if_exists(&identity_sidecar(req.part))?;
            return Ok(size);
        }
        ms_log::runtime_log::log_info(format!(
            "{} existing file differs from the pin, downloading it again: {}",
            engine.log_tag, engine.log_context
        ));
    }

    if let Some(parent) = req.part.parent() {
        create_dir_all(parent)?;
    }
    let mut offset = file_len(req.part).unwrap_or(0);
    if pinned_size.is_some_and(|size| offset > size) {
        remove_file_if_exists(req.part)?;
        offset = 0;
    }
    let mut hasher = pinned_sha.as_ref().map(|_| sha2::Sha256::new());
    if let Some(hasher) = hasher.as_mut()
        && offset > 0
    {
        hash_file(req.part, hasher, engine.cancel, &mut |done, total| progress(FileEvent::Verifying { done, total }))?;
    }

    let sidecar = identity_sidecar(req.part);
    // A missing `.part` has no identity yet; an unreadable sidecar reads as "unknown", which
    // only ever leads to a restart, never to trusting the prefix.
    let bound = if offset > 0 { read_identity(&sidecar) } else { None };
    let binder = IdentityBinder {
        inner: engine.fetcher,
        sidecar: &sidecar,
        log_tag: engine.log_tag,
        log_context: engine.log_context,
        bound: RefCell::new(bound),
        // A pinned sha256 catches any splice at the end; without one only an identity can.
        require_identity: pinned_sha.is_none(),
        observed: Cell::new(false),
    };
    let bound_engine = ResumableFetch { fetcher: &binder, ..*engine };
    let request = PartRequest { url: req.url, part: req.part, offset, expected_size: pinned_size };
    let size = bound_engine.fetch_part(&request, hasher.as_mut(), &mut |event| progress(FileEvent::Stream(event)))?;

    if let (Some(expected), Some(hasher)) = (pinned_sha.as_ref(), hasher) {
        let actual = hex_lower(&hasher.finalize());
        if actual != *expected {
            remove_file_if_exists(req.part)?;
            remove_file_if_exists(&sidecar)?;
            return Err(DownloadError::HashMismatch { expected: expected.clone(), actual });
        }
    }
    let announced_sha = if pinned_sha.is_none() { binder.bound.into_inner().and_then(|identity| identity.lfs_sha256) } else { None };
    if let Some(expected) = announced_sha {
        let mut hasher = sha2::Sha256::new();
        hash_file(req.part, &mut hasher, engine.cancel, &mut |done, total| progress(FileEvent::Verifying { done, total }))?;
        let actual = hex_lower(&hasher.finalize());
        if actual != expected {
            remove_file_if_exists(req.part)?;
            remove_file_if_exists(&sidecar)?;
            return Err(DownloadError::HashMismatch { expected, actual });
        }
    }
    publish_replace(req.part, req.dest)?;
    remove_file_if_exists(&sidecar)?;
    Ok(size)
}

/// Sidecar next to `part` that records the [`RemoteIdentity`] its bytes belong to.
fn identity_sidecar(part: &Path) -> PathBuf {
    let mut name = part.as_os_str().to_os_string();
    name.push(".identity");
    PathBuf::from(name)
}

/// Reads a sidecar written by [`write_identity`]; `None` when it is absent, unreadable or
/// malformed.
fn read_identity(sidecar: &Path) -> Option<RemoteIdentity> {
    let text = std::fs::read_to_string(sidecar).ok()?;
    let mut etag = None;
    let mut lfs_sha256 = None;
    for line in text.lines() {
        match line.split_once(' ') {
            Some(("etag", value)) if !value.is_empty() => etag = Some(value.to_owned()),
            Some(("lfs_sha256", value)) if !value.is_empty() => lfs_sha256 = Some(value.to_owned()),
            _ => return None,
        }
    }
    Some(RemoteIdentity { etag: etag?, lfs_sha256 })
}

/// Records `identity` in `sidecar` (`etag <tag>` and an optional `lfs_sha256 <hex>` line), or
/// removes the sidecar for `None`. A torn write reads back as malformed, i.e. "unknown".
fn write_identity(sidecar: &Path, identity: Option<&RemoteIdentity>) -> std::io::Result<()> {
    let Some(identity) = identity else {
        return match std::fs::remove_file(sidecar) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    };
    let mut text = format!("etag {}\n", identity.etag);
    if let Some(sha) = &identity.lfs_sha256 {
        text.push_str(&format!("lfs_sha256 {sha}\n"));
    }
    std::fs::write(sidecar, text)
}

/// The engine's fetcher with the resume of one `.part` bound to a remote identity.
///
/// A resume request (`offset > 0`) is replaced by a request from byte 0 — whose 200 makes the
/// engine restart the file — when its answer names another identity than the one the staged
/// bytes belong to (`bound`), or, for an unpinned file (`require_identity`), when neither has
/// a (strong) identity and the staged bytes come from an EARLIER run: across runs only an
/// identity can prove the remote file unchanged. Within one run a tagless server still
/// resumes (the transient-failure retry rule; the engine's size checks still apply). Every
/// identity change is recorded in the sidecar before any byte of the new response is staged.
struct IdentityBinder<'a> {
    inner: &'a dyn RangeFetcher,
    sidecar: &'a Path,
    log_tag: &'a str,
    log_context: &'a str,
    /// Identity of the bytes in the `.part`; `None` = unknown (or a server without tags).
    bound: RefCell<Option<RemoteIdentity>>,
    /// No pinned sha256: a staged prefix of unknown identity from an earlier run is not kept.
    require_identity: bool,
    /// Whether a response of THIS run has been seen (the staged bytes are then this run's).
    observed: Cell<bool>,
}

impl RangeFetcher for IdentityBinder<'_> {
    fn fetch(&self, url: &str, offset: u64) -> Result<FetchResponse, FetchError> {
        let mut response = self.inner.fetch(url, offset)?;
        let bound = self.bound.borrow().clone();
        let unprovable = self.require_identity && !self.observed.get() && response.identity.is_none();
        self.observed.set(true);
        if offset > 0 && (response.identity != bound || unprovable) {
            ms_log::runtime_log::log_warn(format!(
                "{} remote file is not the one the staged part belongs to, restarting: {} offset={offset} staged_etag={:?} remote_etag={:?}",
                self.log_tag,
                self.log_context,
                bound.as_ref().map(|identity| identity.etag.as_str()),
                response.identity.as_ref().map(|identity| identity.etag.as_str())
            ));
            drop(response);
            response = self.inner.fetch(url, 0)?;
        }
        if response.identity != bound {
            write_identity(self.sidecar, response.identity.as_ref()).map_err(|err| FetchError::Permanent {
                detail: format!("cannot record the remote identity in {}: {err}", self.sidecar.display()),
            })?;
            *self.bound.borrow_mut() = response.identity.clone();
        }
        Ok(response)
    }
}

/// Feeds the whole of `path` into `hasher`, reporting `(done, total)` (first with `done = 0`)
/// and checking `cancel` before every read of at most [`CHUNK`] bytes.
///
/// # Errors
/// `Cancelled`, `Io`.
pub(crate) fn hash_file(path: &Path, hasher: &mut sha2::Sha256, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<(), DownloadError> {
    let mut handle = std::fs::File::open(path).map_err(|source| io_error(path, source))?;
    let total = handle.metadata().map_err(|source| io_error(path, source))?.len();
    let mut buffer = vec![0_u8; CHUNK];
    let mut done = 0_u64;
    progress(done, total);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(DownloadError::Cancelled);
        }
        let read = handle.read(&mut buffer).map_err(|source| io_error(path, source))?;
        if read == 0 {
            return Ok(());
        }
        hasher.update(&buffer[..read]);
        done = done.saturating_add(chunk_len(read));
        progress(done, total);
    }
}

/// Renames the complete, already fsynced `staged` file onto `dest` (replacing an existing
/// file), creating `dest`'s parent first and fsyncing it afterwards on unix.
///
/// # Errors
/// `Io` naming the failing path; `staged` is kept on failure.
pub(crate) fn publish_replace(staged: &Path, dest: &Path) -> Result<(), DownloadError> {
    let parent = dest.parent().filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        create_dir_all(parent)?;
    }
    std::fs::rename(staged, dest).map_err(|source| io_error(dest, source))?;
    #[cfg(unix)]
    {
        // A rename is durable only once its directory entry is: fsync the directory.
        let dir = parent.unwrap_or_else(|| Path::new("."));
        let handle = std::fs::File::open(dir).map_err(|source| io_error(dir, source))?;
        handle.sync_all().map_err(|source| io_error(dir, source))?;
    }
    Ok(())
}

/// Length of the regular file at `path`, `None` when it is missing or not a file.
pub(crate) fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().filter(std::fs::Metadata::is_file).map(|metadata| metadata.len())
}

/// Lowercase hex form of a digest.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    // Both nibbles are < 16, so the indexing is always in bounds.
    bytes.iter().flat_map(|byte| [DIGITS[usize::from(byte >> 4)], DIGITS[usize::from(byte & 0x0f)]]).map(char::from).collect()
}

pub(crate) fn remove_file_if_exists(path: &Path) -> Result<(), DownloadError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}

fn create_dir_all(path: &Path) -> Result<(), DownloadError> {
    std::fs::create_dir_all(path).map_err(|source| io_error(path, source))
}

pub(crate) fn io_error(path: &Path, source: std::io::Error) -> DownloadError {
    DownloadError::Io { path: PathBuf::from(path), source }
}

#[cfg(test)]
mod tests;
