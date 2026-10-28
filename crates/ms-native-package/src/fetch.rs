/*
File: crates/ms-native-package/src/fetch.rs

Purpose:
One source archive of a package: claim its `.part` (identity sidecar), download + verify it
through `ms_download::fetch_verified`, stage the wanted members, rename them into the package
directory in member order, fsync the directory, delete the archive. Plus the progress throttle
that turns `ms-download` events into `PackageProgress`.

Key items:
- fetch_source()         : the per-source step of `ensure_installed`
- claim_part()           : `.part.id` = `<url>\n<sha256>\n`; a foreign `.part` is deleted
- extract_and_publish()  : `stage_members` + renames + directory fsync
- ProgressThrottle / forward_file_event(): 512 KiB report interval per stage

Notes:
The `.download/<archive>`, `<archive>.part` and `<archive>.part.id` names and the identity
bytes are a persistence contract shared with caches written by earlier program versions.
*/

use std::fs;
use std::path::{Path, PathBuf};

use ms_download::resumable::{ResumableFetch, StreamEvent};
use ms_download::unpack::{ArchiveKind, WantedMember, stage_members, staging_path};
use ms_download::{DownloadError, Expected, FileEvent, FileRequest, fetch_verified};

use crate::error::{PackageError, PackageIoOp};
use crate::marker::{PublishedMember, sync_dir};
use crate::{FetchEnv, PackageMember, PackageProgress, PackageSource, PackageStage, emit};

/// Minimum number of bytes between throttled progress reports (transfer and hashing).
const PROGRESS_EMIT_INTERVAL_BYTES: u64 = 512 * 1024;

/// Downloads, verifies and extracts one source archive into `dir`.
///
/// Claims the `.part` for this source ([`claim_part`]), lets `ms_download::fetch_verified`
/// resume / stream / verify (exact size + sha256) / publish it as `download_dir/<file name>`,
/// reports `Extracting`, stages and publishes exactly the source's members in their order
/// ([`extract_and_publish`]) and removes the archive. Returns the published members with
/// their size and sha256 (for the completion marker).
///
/// # Errors
/// `Cancelled` (`.part` kept), `ChecksumMismatch` (`.part` deleted), `Download`, `Unpack`,
/// `Io` (`RemovePart` / `WriteIdentity`), `Publish`, `SyncDir`.
pub(crate) fn fetch_source(env: &FetchEnv<'_>, source: &PackageSource<'_>, dir: &Path, download_dir: &Path, progress: &mut dyn FnMut(PackageProgress)) -> Result<Vec<PublishedMember>, PackageError> {
    let archive_path = download_dir.join(source.file_name);
    let part_path = download_dir.join(format!("{}.part", source.file_name));
    let identity_path = download_dir.join(format!("{}.part.id", source.file_name));
    claim_part(&part_path, &identity_path, source, env.log_tag)?;

    let log_context = format!("{} archive={}", env.log_context, source.file_name);
    ms_log::runtime_log::log_info(format!("{} downloading {} ({log_context})", env.log_tag, source.url));
    let engine = ResumableFetch { fetcher: env.fetcher, retry: env.retry, cancel: env.cancel, log_tag: env.log_tag, log_context: &log_context };
    let request = FileRequest {
        url: source.url,
        part: &part_path,
        dest: &archive_path,
        expected: Expected::SizeSha256 { size: source.size, sha256: source.sha256 },
    };
    let mut throttle = ProgressThrottle::default();
    let fetched = fetch_verified(&engine, &request, &mut |event| forward_file_event(event, source.size, &mut throttle, progress));
    if let Err(err) = fetched {
        match &err {
            DownloadError::Cancelled => ms_log::runtime_log::log_info(format!("{} download cancelled ({log_context}); the staged bytes are kept", env.log_tag)),
            DownloadError::Busy { .. }
            | DownloadError::InvalidRequest { .. }
            | DownloadError::Io { .. }
            | DownloadError::Http { .. }
            | DownloadError::SizeMismatch { .. }
            | DownloadError::HashMismatch { .. }
            | DownloadError::Archive { .. } => ms_log::runtime_log::log_error(format!("{} download failed ({log_context}): {err}", env.log_tag)),
        }
        return Err(PackageError::from_download(source.url, err));
    }
    ms_log::runtime_log::log_info(format!("{} size and SHA256 verified for '{}'", env.log_tag, archive_path.display()));
    // The `.part` became the archive: its identity file is stale now. A leftover one is
    // harmless (the next `claim_part` rewrites it), so a failure only warns.
    remove_file_logged(&identity_path, "stale download identity", env.log_tag);

    emit(progress, PackageStage::Extracting, 0, None);
    let published = extract_and_publish(&archive_path, source.archive, dir, source.members)?;

    // The archive is no longer needed once its members are published; a cleanup failure is
    // non-fatal (the package directory already holds the extracted files).
    remove_file_logged(&archive_path, "archive", env.log_tag);
    Ok(published)
}

/// Makes `part` belong to `source` before a download resumes it.
///
/// `identity` (`<archive>.part.id`) records the URL and sha256 the `.part` was staged for
/// (`<url>\n<sha256>\n`). A `.part` whose identity is missing, unreadable or names another
/// recipe (same archive name, e.g. a re-pinned source) is deleted, because its bytes cannot be
/// proven to belong to this archive; then the identity is (re)written. The identity file is
/// written non-atomically on purpose: a torn one only mismatches next time, which discards
/// the `.part` — never resumes foreign bytes.
///
/// # Errors
/// [`PackageError::Io`] (`RemovePart` / `WriteIdentity`).
pub(crate) fn claim_part(part: &Path, identity: &Path, source: &PackageSource<'_>, log_tag: &str) -> Result<(), PackageError> {
    let expected = format!("{}\n{}\n", source.url, source.sha256);
    let owned = match fs::read(identity) {
        Ok(bytes) => bytes == expected.as_bytes(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!("{log_tag} cannot read download identity '{}' ({err}); restarting that download", identity.display()));
            false
        }
    };
    if owned {
        return Ok(());
    }
    match fs::remove_file(part) {
        Ok(()) => ms_log::runtime_log::log_info(format!("{log_tag} discarded '{}': it was staged for another archive", part.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(PackageError::Io { op: PackageIoOp::RemovePart, path: part.to_path_buf(), source }),
    }
    fs::write(identity, expected.as_bytes()).map_err(|source| PackageError::Io { op: PackageIoOp::WriteIdentity, path: identity.to_path_buf(), source })
}

/// Stages every member of the `kind` archive at `archive` (`ms_download::unpack::stage_members`:
/// all or nothing, temporaries removed on failure), then renames each `<dir>/<file name>.tmp`
/// onto `<dir>/<file name>` in `members` order (the caller lists its most important file
/// last) and fsyncs `dir`. Several renames are not one atomic step: the caller removes the
/// completion marker before and writes it after.
///
/// # Errors
/// [`PackageError::Unpack`] (nothing published), [`PackageError::Publish`] (earlier members
/// stay renamed), [`PackageError::SyncDir`].
pub(crate) fn extract_and_publish(archive: &Path, kind: ArchiveKind, dir: &Path, members: &[PackageMember<'_>]) -> Result<Vec<PublishedMember>, PackageError> {
    let dests: Vec<PathBuf> = members.iter().map(|member| dir.join(member.file_name)).collect();
    let wanted: Vec<WantedMember<'_>> = members.iter().zip(&dests).map(|(member, dest)| WantedMember { archive_path: member.archive_path, dest }).collect();
    let staged = stage_members(archive, kind, &wanted).map_err(|source| PackageError::Unpack { archive: archive.to_path_buf(), source: Box::new(source) })?;
    let mut published = Vec::with_capacity(members.len());
    for ((member, dest), staged) in members.iter().zip(&dests).zip(staged) {
        let from = staging_path(dest);
        fs::rename(&from, dest).map_err(|source| PackageError::Publish { from: from.clone(), to: dest.clone(), source })?;
        published.push(PublishedMember { file_name: member.file_name.to_owned(), size: staged.size, sha256: staged.sha256 });
    }
    // Every destination is `dir / <plain file name>`: one directory fsync covers them all.
    sync_dir(dir)?;
    Ok(published)
}

/// Removes `path` if it exists; any other failure is logged as a warning naming `what`.
pub(crate) fn remove_file_logged(path: &Path, what: &str, log_tag: &str) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => ms_log::runtime_log::log_warn(format!("{log_tag} failed to remove {what} '{}': {err}", path.display())),
    }
}

/// Rate limit of byte-counting progress reports: a report is sent when the stage changes,
/// when [`PROGRESS_EMIT_INTERVAL_BYTES`] more bytes passed, or at the end.
#[derive(Debug, Default)]
struct ProgressThrottle {
    /// Stage and byte count of the last report sent.
    last: Option<(PackageStage, u64)>,
}

impl ProgressThrottle {
    /// Whether a `(stage, bytes)` report of a `total`-byte operation should be sent; records
    /// it when so.
    fn admit(&mut self, stage: PackageStage, bytes: u64, total: u64) -> bool {
        let send = match self.last {
            Some((last_stage, last_bytes)) => last_stage != stage || bytes.saturating_sub(last_bytes) >= PROGRESS_EMIT_INTERVAL_BYTES || bytes >= total,
            None => true,
        };
        if send {
            self.last = Some((stage, bytes));
        }
        send
    }
}

/// Maps one `fetch_verified` event of a `size`-byte archive onto the caller's progress.
fn forward_file_event(event: FileEvent, size: u64, throttle: &mut ProgressThrottle, progress: &mut dyn FnMut(PackageProgress)) {
    match event {
        // `fetch_verified`'s own destination guard: unreachable while the package busy key is
        // held (every writer of this archive holds it first), reported anyway.
        FileEvent::Waiting => emit(progress, PackageStage::WaitingForOtherDownload, 0, None),
        FileEvent::Verifying { done, total } => {
            if throttle.admit(PackageStage::Verifying, done, total) {
                emit(progress, PackageStage::Verifying, done, Some(total));
            }
        }
        FileEvent::Stream(StreamEvent::Downloading { staged, total }) => {
            let total = total.unwrap_or(size);
            if throttle.admit(PackageStage::Downloading, staged, total) {
                emit(progress, PackageStage::Downloading, staged, Some(total));
            }
        }
        FileEvent::Stream(StreamEvent::Retrying { attempt, max_attempts, delay_secs, staged }) => {
            let stage = PackageStage::Retrying { attempt, max_attempts, delay_secs };
            // Every countdown step is a distinct stage value, so each one is sent.
            if throttle.admit(stage, staged, size) {
                emit(progress, stage, staged, Some(size));
            }
        }
    }
}
