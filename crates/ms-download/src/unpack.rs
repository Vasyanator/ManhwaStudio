/*
File: crates/ms-download/src/unpack.rs

Purpose:
Extraction of members of a downloaded archive:
- atomic extraction of ONE member of a zip archive (e.g. a model weight shipped as a GitHub
  release zip): stream it into a temporary sibling of the destination while hashing, verify
  size and sha256, fsync, then replace-rename it into place;
- all-or-nothing STAGING of several members of a zip / tar.gz / tar.zst archive (a native
  library package): every wanted member is written, hashed and fsynced to `<dest>.tmp`; the
  caller renames the temporaries once the whole set was found.

Key items:
- `zip_member_atomic()`: the single-member operation.
- `ArchiveKind`, `WantedMember`, `StagedMember`, `stage_members()`, `staging_path()`: the
  multi-member staging step.
- `UnpackError`: the typed failure of `stage_members` (each variant names its operation so a
  consumer can map it onto its own localized text).

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
- `stage_members` publishes nothing: renaming the staged temporaries (in the caller's order)
  and any completion marker are the caller's transaction (`ms-native-package`). It skips
  directory and symlink entries that carry a wanted path (only the real regular file is
  extracted) and normalizes tar keys (`\` -> `/`, a leading `./` stripped), so one clean
  member path matches GNU-tar-packed archives too.
*/

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use sha2::Digest;
use tar::Archive as TarArchive;
use zip::ZipArchive;

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

/// How a downloaded archive is packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    /// PKZIP archive.
    Zip,
    /// gzip-compressed tar.
    TarGz,
    /// zstd-compressed tar.
    TarZst,
}

/// One member [`stage_members`] must extract: its exact path inside the archive (tar keys are
/// compared after normalization, see the file header) and its final destination, whose
/// [`staging_path`] receives the bytes.
#[derive(Debug, Clone, Copy)]
pub struct WantedMember<'a> {
    /// Member path inside the archive.
    pub archive_path: &'a str,
    /// Final destination; only its `<file name>.tmp` sibling is written here.
    pub dest: &'a Path,
}

/// A member [`stage_members`] wrote to the staging path of its destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedMember {
    /// Member path inside the archive (as wanted).
    pub archive_path: String,
    /// Exact byte size written.
    pub size: u64,
    /// Lowercase hex sha256 of the bytes written.
    pub sha256: String,
}

/// Failure of [`stage_members`]. Every variant names the operation that failed and the
/// underlying cause, so a consumer can render its own (localized) text per variant; the
/// `Display` here is English diagnostics. Every staged temporary is already removed when one
/// of these is returned.
#[derive(Debug, thiserror::Error)]
pub enum UnpackError {
    /// The archive file could not be opened.
    #[error("cannot open archive {}: {source}", path.display())]
    OpenArchive { path: PathBuf, source: std::io::Error },
    /// The zip directory could not be read (not a zip, truncated, corrupt).
    #[error("cannot read the zip archive: {source}")]
    ZipRead { source: zip::result::ZipError },
    /// Zip entry number `index` could not be read.
    #[error("cannot read zip entry {index}: {source}")]
    ZipEntry { index: usize, source: zip::result::ZipError },
    /// The zstd decoder could not be created.
    #[error("cannot open the zstd decoder: {source}")]
    ZstdOpen { source: std::io::Error },
    /// The tar entry list could not be read.
    #[error("cannot read the tar archive: {source}")]
    TarRead { source: std::io::Error },
    /// A tar entry header could not be read.
    #[error("cannot read a tar entry: {source}")]
    TarEntry { source: std::io::Error },
    /// A tar entry path is not valid.
    #[error("invalid tar entry path: {source}")]
    TarInvalidPath { source: std::io::Error },
    /// The parent directory `path` of a staging file could not be created.
    #[error("cannot create directory {}: {source}", path.display())]
    CreateDir { path: PathBuf, source: std::io::Error },
    /// The staging file `path` could not be created.
    #[error("cannot create file {}: {source}", path.display())]
    CreateFile { path: PathBuf, source: std::io::Error },
    /// Copying a member's data into the staging file `path` failed (corrupt member data or a
    /// write error).
    #[error("cannot extract a member to {}: {source}", path.display())]
    CopyMember { path: PathBuf, source: std::io::Error },
    /// The staging file `path` could not be fsynced.
    #[error("cannot fsync {}: {source}", path.display())]
    SyncFile { path: PathBuf, source: std::io::Error },
    /// A wanted member is not in the archive (the first missing one in `wanted` order).
    #[error("member '{member}' is not in the archive")]
    MemberMissing { member: String },
}

/// The temporary sibling `<dest file name>.tmp` that [`stage_members`] writes `dest` to.
#[must_use]
pub fn staging_path(dest: &Path) -> PathBuf {
    // A destination without a file name still gets a distinct `.tmp` sibling (callers pass
    // `dir / <plain file name>`, which always has one).
    let mut name = dest.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    name.push(".tmp");
    dest.with_file_name(name)
}

/// Writes every `wanted` member of the `kind` archive at `archive` to the [`staging_path`] of
/// its destination (parent directories created, contents hashed and fsynced) and returns the
/// staged members in `wanted` order.
///
/// All or nothing: only when EVERY wanted member was found is anything returned; on any
/// failure every staging file of `wanted` is removed (a failed removal is logged) and no
/// destination is touched. Only the listed members are read (large unrelated members are
/// skipped without being written); directory and symlink entries are skipped.
///
/// # Errors
/// [`UnpackError`]: open / read failures of the archive, staging-file I/O, or
/// `MemberMissing` naming the first wanted member (in `wanted` order) the archive lacks.
pub fn stage_members(archive: &Path, kind: ArchiveKind, wanted: &[WantedMember<'_>]) -> Result<Vec<StagedMember>, UnpackError> {
    match stage_all(archive, kind, wanted) {
        Ok(staged) => Ok(staged),
        Err(err) => {
            for member in wanted {
                remove_staging_logged(&staging_path(member.dest));
            }
            Err(err)
        }
    }
}

/// The body of [`stage_members`] without the cleanup on failure.
fn stage_all(archive: &Path, kind: ArchiveKind, wanted: &[WantedMember<'_>]) -> Result<Vec<StagedMember>, UnpackError> {
    let lookup: HashMap<&str, &Path> = wanted.iter().map(|member| (member.archive_path, member.dest)).collect();
    let mut found: HashMap<String, (u64, String)> = HashMap::new();
    let open = || File::open(archive).map_err(|source| UnpackError::OpenArchive { path: archive.to_path_buf(), source });

    match kind {
        ArchiveKind::Zip => {
            let mut zip = ZipArchive::new(open()?).map_err(|source| UnpackError::ZipRead { source })?;
            for index in 0..zip.len() {
                let mut file_in_zip = zip.by_index(index).map_err(|source| UnpackError::ZipEntry { index, source })?;
                let name = file_in_zip.name().to_string();
                if let Some(dest) = lookup.get(name.as_str()) {
                    if file_in_zip.is_dir() {
                        continue;
                    }
                    let digest = write_stream(&mut file_in_zip, &staging_path(dest))?;
                    found.insert(name, digest);
                }
            }
        }
        ArchiveKind::TarGz => stage_tar_members(GzDecoder::new(open()?), &lookup, &mut found)?,
        ArchiveKind::TarZst => {
            let decoder = zstd::stream::read::Decoder::new(open()?).map_err(|source| UnpackError::ZstdOpen { source })?;
            stage_tar_members(decoder, &lookup, &mut found)?;
        }
    }

    // Every requested member must have been extracted; report the first miss in order.
    wanted
        .iter()
        .map(|member| match found.get(member.archive_path) {
            Some((size, sha256)) => Ok(StagedMember { archive_path: member.archive_path.to_owned(), size: *size, sha256: sha256.clone() }),
            None => Err(UnpackError::MemberMissing { member: member.archive_path.to_owned() }),
        })
        .collect()
}

/// Stages the wanted members of a tar stream `reader` (see [`stage_members`]), recording the
/// found keys with their `(size, sha256)`.
///
/// Directory and symlink entries are skipped, so only the real library files land in the
/// destination directory (a versioned `.so` / `.dylib` file, not its symlinks).
fn stage_tar_members<R: Read>(reader: R, wanted: &HashMap<&str, &Path>, found: &mut HashMap<String, (u64, String)>) -> Result<(), UnpackError> {
    let mut archive = TarArchive::new(reader);
    let entries = archive.entries().map_err(|source| UnpackError::TarRead { source })?;
    for entry in entries {
        let mut entry = entry.map_err(|source| UnpackError::TarEntry { source })?;
        let path = entry.path().map_err(|source| UnpackError::TarInvalidPath { source })?;
        // Normalize separators and strip a leading `./`: GNU-tar-packed archives store
        // members as `./name/lib/...` (e.g. the onnxruntime 1.27.0 macOS archives), while
        // consumers write member paths without the prefix, so one clean path matches both
        // archive styles.
        let normalized = path.to_string_lossy().replace('\\', "/");
        let key = normalized.strip_prefix("./").unwrap_or(&normalized);
        if let Some(dest) = wanted.get(key) {
            // Only the real regular file is wanted; the archive's symlinks point at it and
            // are intentionally ignored.
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let digest = write_stream(&mut entry, &staging_path(dest))?;
            found.insert(key.to_string(), digest);
        }
    }
    Ok(())
}

/// Copies a readable archive member into a fresh `out_path` (creating parent dirs), fsyncs it
/// and returns its `(size, lowercase sha256)`.
fn write_stream<R: Read>(reader: &mut R, out_path: &Path) -> Result<(u64, String), UnpackError> {
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| UnpackError::CreateDir { path: parent.to_path_buf(), source })?;
    }
    let mut output = File::create(out_path).map_err(|source| UnpackError::CreateFile { path: out_path.to_path_buf(), source })?;
    let mut hashing = HashingWriter { inner: &mut output, hasher: sha2::Sha256::new() };
    let size = std::io::copy(reader, &mut hashing).map_err(|source| UnpackError::CopyMember { path: out_path.to_path_buf(), source })?;
    let sha256 = hex_lower(&hashing.hasher.finalize());
    output.sync_all().map_err(|source| UnpackError::SyncFile { path: out_path.to_path_buf(), source })?;
    Ok((size, sha256))
}

/// A writer that hashes everything it forwards to `inner`.
struct HashingWriter<'a> {
    inner: &'a mut File,
    hasher: sha2::Sha256,
}

impl Write for HashingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        // Only the bytes the file accepted are hashed; `io::copy` resubmits the rest.
        self.hasher.update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Removes a staging file if it exists; any other failure is logged as a warning (the
/// operation already failed, the leftover is overwritten by the next attempt).
fn remove_staging_logged(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => ms_log::runtime_log::log_warn(format!("[download] failed to remove extraction temporary '{}': {err}", path.display())),
    }
}

#[cfg(test)]
mod tests;
