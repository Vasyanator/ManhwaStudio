/*
File: crates/ms-download/src/unpack.rs

Purpose:
Atomic extraction of ONE member of a downloaded zip archive (e.g. a model weight shipped as
a GitHub release zip): stream it into a temporary sibling of the destination while hashing,
verify size and sha256, fsync, then replace-rename it into place.

Key items:
- `zip_member_atomic()`: the operation.

Notes:
- The destination is never written in place, so a crash or a corrupt member leaves either
  the previous file or nothing — never a truncated one.
- Shared destinations: an existing `dest` that already matches a pinned size (and sha256) is
  a no-op success; otherwise the busy id `busy::dest_key(dest)` is taken with `try_acquire`
  and a concurrent extraction of the same `dest` fails with `DownloadError::Busy` (this call
  has no cancel flag to wait with). Callers whose downloads may race on one shared member
  serialize their fetch + unpack under a key of their own via `BusyGuard::acquire_waiting`
  (a key other than the `dest:` ids, which these functions take themselves).
- No cancel polling: one member is bounded by its pinned size and extracts in seconds.
*/

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::Digest;

use crate::busy::{BusyGuard, dest_key};
use crate::error::DownloadError;
use crate::file::{Expected, file_len, hash_file, hex_lower, io_error, publish_replace, remove_file_if_exists};
use crate::resumable::{CHUNK, chunk_len};

/// Extracts `member` (its exact name inside the archive) of the zip at `archive` to `dest`
/// and returns its size.
///
/// With a pinned size the member's declared size is checked before anything is written and
/// the streamed bytes are capped at it; with a pinned sha256 the extracted bytes are hashed.
/// The temporary file `<dest file name>.unpack` next to `dest` is deleted on every content
/// failure. An existing `dest` that already matches the pin is kept without opening the
/// archive.
///
/// # Errors
/// `InvalidRequest` (malformed sha256 pin, `dest` without a file name), `Busy` (another
/// extraction of `dest` is running), `Archive` (unreadable archive, missing or directory
/// member, corrupt member data), `SizeMismatch`, `HashMismatch`, `Io`.
pub fn zip_member_atomic(archive: &Path, member: &str, dest: &Path, expected: Expected<'_>) -> Result<u64, DownloadError> {
    let pinned_sha = expected.checked_sha256()?;
    let tmp = temp_path(dest)?;
    if let Some(size) = existing_match(dest, expected.size(), pinned_sha.as_deref())? {
        return Ok(size);
    }
    let key = dest_key(dest);
    // Held until return: no other extraction in the process writes `tmp` or `dest`.
    let _guard = BusyGuard::try_acquire(&key).map_err(|_| DownloadError::Busy { key: key.clone() })?;
    // Another holder may have published it between the check above and taking the guard.
    if let Some(size) = existing_match(dest, expected.size(), pinned_sha.as_deref())? {
        return Ok(size);
    }

    let file = std::fs::File::open(archive).map_err(|source| io_error(archive, source))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|err| archive_error(archive, member, &err))?;
    let mut entry = zip.by_name(member).map_err(|err| archive_error(archive, member, &err))?;
    if entry.is_dir() {
        return Err(DownloadError::Archive { detail: format!("member '{member}' of {} is a directory", archive.display()) });
    }
    let declared = entry.size();
    let limit = expected.size().unwrap_or(declared);
    if declared != limit {
        return Err(DownloadError::SizeMismatch { expected: limit, actual: declared });
    }

    let (size, digest) = match extract_to(&mut entry, &tmp, limit, archive, member) {
        Ok(extracted) => extracted,
        Err(error) => {
            remove_file_if_exists(&tmp)?;
            return Err(error);
        }
    };
    if size != limit {
        remove_file_if_exists(&tmp)?;
        return Err(DownloadError::SizeMismatch { expected: limit, actual: size });
    }
    if let Some(expected_sha) = pinned_sha
        && digest != expected_sha
    {
        remove_file_if_exists(&tmp)?;
        return Err(DownloadError::HashMismatch { expected: expected_sha, actual: digest });
    }
    publish_replace(&tmp, dest)?;
    Ok(size)
}

/// Streams `entry` into a fresh `tmp`, at most `limit` bytes, and fsyncs it; returns the
/// byte count and the lowercase sha256.
fn extract_to(entry: &mut impl Read, tmp: &Path, limit: u64, archive: &Path, member: &str) -> Result<(u64, String), DownloadError> {
    if let Some(parent) = tmp.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }
    let mut out = std::fs::File::create(tmp).map_err(|source| io_error(tmp, source))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0_u8; CHUNK];
    let mut written = 0_u64;
    loop {
        // A read error here is the member's data (decompression or CRC failure), not the disk.
        let read = entry.read(&mut buffer).map_err(|err| DownloadError::Archive {
            detail: format!("reading member '{member}' of {} failed: {err}", archive.display()),
        })?;
        if read == 0 {
            break;
        }
        let next = written.saturating_add(chunk_len(read));
        if next > limit {
            return Err(DownloadError::SizeMismatch { expected: limit, actual: next });
        }
        out.write_all(&buffer[..read]).map_err(|source| io_error(tmp, source))?;
        hasher.update(&buffer[..read]);
        written = next;
    }
    out.sync_all().map_err(|source| io_error(tmp, source))?;
    Ok((written, hex_lower(&hasher.finalize())))
}

/// `Some(size)` when `dest` already holds the pinned content. Without a pinned size nothing
/// can be trusted (`None`); a pinned sha256 is checked by hashing the file.
fn existing_match(dest: &Path, size: Option<u64>, sha256: Option<&str>) -> Result<Option<u64>, DownloadError> {
    let Some(size) = size else { return Ok(None) };
    if file_len(dest) != Some(size) {
        return Ok(None);
    }
    let Some(expected) = sha256 else { return Ok(Some(size)) };
    let mut hasher = sha2::Sha256::new();
    // This operation has no cancel flag (see the file header): the hash pass runs to the end.
    let never = std::sync::atomic::AtomicBool::new(false);
    hash_file(dest, &mut hasher, &never, &mut |_, _| {})?;
    Ok((hex_lower(&hasher.finalize()) == expected).then_some(size))
}

/// `<dest file name>.unpack` next to `dest`.
fn temp_path(dest: &Path) -> Result<PathBuf, DownloadError> {
    let Some(name) = dest.file_name() else {
        return Err(DownloadError::InvalidRequest { detail: format!("destination {} has no file name", dest.display()) });
    };
    let mut tmp_name = name.to_os_string();
    tmp_name.push(".unpack");
    Ok(dest.with_file_name(tmp_name))
}

fn archive_error(archive: &Path, member: &str, err: &zip::result::ZipError) -> DownloadError {
    DownloadError::Archive { detail: format!("cannot read member '{member}' of {}: {err}", archive.display()) }
}

#[cfg(test)]
mod tests;
