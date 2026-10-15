/*
File: crates/ms-download/src/file/tests.rs

Purpose:
Unit tests of `fetch_verified`: fresh and resumed downloads (prefix rehash), hash mismatch
deleting the `.part`, an existing destination kept or replaced, a shared destination
fetched once, cancel during the transfer, the backoff and the busy wait, malformed pins,
and the resume binding to the remote identity (same identity resumes, a changed or unknown
one restarts, an announced LFS sha256 is enforced).

Notes:
- `ScriptedFetcher` serves the content; the backoff sleep only records. No network, no real
  waiting; scratch data lives in self-removing temp directories.
*/

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::resumable::{FetchError, FetchResponse, RangeFetcher, RemoteIdentity};
use crate::test_fs::{SleepLog, content, len_u64, policy, read, sha256_hex, temp_root, write};
use crate::test_support::{ScriptedFetcher, Step};

/// Runs `fetch_verified` over `fetcher` with a recording sleep, collecting the events.
fn run(fetcher: &dyn RangeFetcher, sleep: &SleepLog<'_>, cancel: &AtomicBool, req: &FileRequest<'_>) -> (Result<u64, DownloadError>, Vec<FileEvent>) {
    let sleep_fn = |slice| sleep.sleep(slice);
    let retry = policy(&sleep_fn);
    let engine = ResumableFetch { fetcher, retry: &retry, cancel, log_tag: "[test]", log_context: "file=test" };
    let mut events = Vec::new();
    let result = fetch_verified(&engine, req, &mut |event| events.push(event));
    (result, events)
}

#[test]
fn fresh_download_is_verified_and_published() {
    let root = temp_root("ms-download-file-");
    let body = content(300_000, 1);
    let sha = sha256_hex(&body);
    let (part, dest) = (root.path().join("staging/w.part"), root.path().join("models/w.bin"));
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::SizeSha256 { size: len_u64(&body), sha256: &sha } };
    let (result, _) = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert_eq!(result.ok(), Some(len_u64(&body)));
    assert_eq!(read(&dest), body);
    assert!(!part.exists());
}

#[test]
fn resume_rehashes_the_staged_prefix() {
    let root = temp_root("ms-download-file-");
    let body = content(400_000, 2);
    let sha = sha256_hex(&body);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    write(&part, &body[..150_000]);
    // A drop after the resume point proves the hasher still covers prefix + both bodies.
    let fetcher = ScriptedFetcher::new(&body, &[Step::Drop(100_000)]);
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::SizeSha256 { size: len_u64(&body), sha256: &sha } };
    let (result, events) = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert_eq!(result.ok(), Some(len_u64(&body)));
    assert_eq!(fetcher.offsets(), vec![150_000, 250_000]);
    assert!(events.contains(&FileEvent::Verifying { done: 150_000, total: 150_000 }));
    assert_eq!(read(&dest), body);
}

#[test]
fn corrupt_prefix_fails_the_hash_and_deletes_the_part() {
    let root = temp_root("ms-download-file-");
    let body = content(200_000, 3);
    let sha = sha256_hex(&body);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    write(&part, &content(50_000, 99));
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::SizeSha256 { size: len_u64(&body), sha256: &sha } };
    let (result, _) = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert!(matches!(result, Err(DownloadError::HashMismatch { ref expected, .. }) if *expected == sha), "{result:?}");
    assert!(!part.exists());
    assert!(!dest.exists());
}

#[test]
fn matching_destination_is_kept_without_a_request() {
    let root = temp_root("ms-download-file-");
    let body = content(10_000, 4);
    let sha = sha256_hex(&body).to_ascii_uppercase();
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    write(&dest, &body);
    write(&part, &body[..10]);
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::SizeSha256 { size: len_u64(&body), sha256: &sha } };
    let (result, _) = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert_eq!(result.ok(), Some(len_u64(&body)));
    assert!(fetcher.offsets().is_empty());
    assert!(!part.exists(), "a stale part next to a verified file is removed");
}

#[test]
fn differing_destination_is_replaced() {
    // The replace-rename path: `dest` exists with the right size but other bytes, so the new
    // file must replace it (on Windows `rename` is `MoveFileExW` + REPLACE_EXISTING).
    let root = temp_root("ms-download-file-");
    let body = content(20_000, 5);
    let sha = sha256_hex(&body);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    write(&dest, &content(20_000, 77));
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::SizeSha256 { size: len_u64(&body), sha256: &sha } };
    let (result, _) = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert_eq!(result.ok(), Some(len_u64(&body)));
    assert_eq!(read(&dest), body);
}

#[test]
fn shared_destination_is_fetched_once() {
    let root = temp_root("ms-download-file-");
    let body = content(30_000, 6);
    let sha = sha256_hex(&body);
    let (part, dest) = (root.path().join("w.part"), root.path().join("shared.bin"));
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::SizeSha256 { size: len_u64(&body), sha256: &sha } };
    let (first, _) = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), &req);
    let (second, _) = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert_eq!(first.ok(), Some(len_u64(&body)));
    assert_eq!(second.ok(), Some(len_u64(&body)));
    assert_eq!(fetcher.offsets(), vec![0], "the second model finds the file already verified");
}

#[test]
fn size_pin_without_hash_and_announced_size() {
    let root = temp_root("ms-download-file-");
    let body = content(5_000, 7);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let sized = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::Size(len_u64(&body)) };
    let (result, _) = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), &sized);
    assert_eq!(result.ok(), Some(len_u64(&body)));
    // An existing file cannot be verified without a pin: `Announced` downloads it again.
    let announced = FileRequest { expected: Expected::Announced, ..sized };
    let (result, _) = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), &announced);
    assert_eq!(result.ok(), Some(len_u64(&body)));
    assert_eq!(fetcher.offsets(), vec![0, 0]);
    assert_eq!(read(&dest), body);
}

#[test]
fn malformed_pin_is_rejected_before_any_io() {
    let root = temp_root("ms-download-file-");
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    let fetcher = ScriptedFetcher::new(b"x", &[]);
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::SizeSha256 { size: 1, sha256: "abc" } };
    let (result, _) = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert!(matches!(result, Err(DownloadError::InvalidRequest { .. })), "{result:?}");
    assert!(fetcher.offsets().is_empty());
}

/// Serves the body of `inner` but sets `cancel` once the first bytes were read.
struct CancelAfterFirstRead {
    inner: ScriptedFetcher,
    cancel: Arc<AtomicBool>,
}

struct CancellingReader {
    inner: Box<dyn Read + Send>,
    cancel: Arc<AtomicBool>,
}

impl Read for CancellingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.cancel.store(true, Ordering::Relaxed);
        Ok(read)
    }
}

impl RangeFetcher for CancelAfterFirstRead {
    fn fetch(&self, url: &str, offset: u64) -> Result<FetchResponse, FetchError> {
        let response = self.inner.fetch(url, offset)?;
        let body = Box::new(CancellingReader { inner: response.body, cancel: Arc::clone(&self.cancel) });
        Ok(FetchResponse { body, ..response })
    }
}

#[test]
fn cancel_during_the_transfer_keeps_the_part() {
    let root = temp_root("ms-download-file-");
    let body = content(600_000, 8);
    let sha = sha256_hex(&body);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    let cancel = Arc::new(AtomicBool::new(false));
    let fetcher = CancelAfterFirstRead { inner: ScriptedFetcher::new(&body, &[]), cancel: Arc::clone(&cancel) };
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::SizeSha256 { size: len_u64(&body), sha256: &sha } };
    let (result, _) = run(&fetcher, &SleepLog::new(None), &cancel, &req);
    assert!(matches!(result, Err(DownloadError::Cancelled)), "{result:?}");
    assert!(part.exists(), "staged bytes survive a cancel");
    assert!(!dest.exists());
}

#[test]
fn cancel_during_the_backoff() {
    let root = temp_root("ms-download-file-");
    let body = content(1_000, 9);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    let cancel = AtomicBool::new(false);
    let sleep = SleepLog::new(Some(&cancel));
    let fetcher = ScriptedFetcher::new(&body, &[Step::Status(503, None)]);
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::Size(len_u64(&body)) };
    let (result, events) = run(&fetcher, &sleep, &cancel, &req);
    assert!(matches!(result, Err(DownloadError::Cancelled)), "{result:?}");
    assert_eq!(sleep.count(), 1);
    assert!(events.iter().any(|event| matches!(event, FileEvent::Stream(StreamEvent::Retrying { attempt: 1, .. }))));
}

#[test]
fn busy_destination_waits_and_can_be_cancelled() {
    let root = temp_root("ms-download-file-");
    let body = content(1_000, 10);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    let held = BusyGuard::try_acquire(&dest_key(&dest)).unwrap_or_else(|err| panic!("{err}"));
    let cancel = AtomicBool::new(false);
    let sleep = SleepLog::new(Some(&cancel));
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::Size(len_u64(&body)) };
    let (result, events) = run(&fetcher, &sleep, &cancel, &req);
    drop(held);
    assert!(matches!(result, Err(DownloadError::Cancelled)), "{result:?}");
    assert_eq!(events, vec![FileEvent::Waiting]);
    assert!(fetcher.offsets().is_empty());
}

#[test]
fn busy_destination_proceeds_once_released() {
    let root = temp_root("ms-download-file-");
    let body = content(1_000, 11);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    let held = std::sync::Mutex::new(Some(BusyGuard::try_acquire(&dest_key(&dest)).unwrap_or_else(|err| panic!("{err}"))));
    // The "other download" finishes during the first wait slice.
    let sleep = SleepLog::with_hook(|| {
        if let Ok(mut guard) = held.lock() {
            guard.take();
        }
    });
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::Size(len_u64(&body)) };
    let (result, events) = run(&fetcher, &sleep, &AtomicBool::new(false), &req);
    assert_eq!(result.ok(), Some(len_u64(&body)));
    assert_eq!(events.first(), Some(&FileEvent::Waiting));
    assert_eq!(sleep.count(), 1);
}

/// An identity as a Hugging Face resolve hop announces it (`lfs_sha256` from `X-Linked-Etag`).
fn lfs_identity(sha: &str) -> Option<RemoteIdentity> {
    RemoteIdentity::from_headers(Some(&format!("\"{sha}\"")), Some("\"cdn-etag\""))
}

/// A non-LFS identity (`ETag` only, never read as a hash).
fn plain_identity(tag: &str) -> Option<RemoteIdentity> {
    RemoteIdentity::from_headers(None, Some(tag))
}

#[test]
fn a_resume_answered_by_the_same_identity_extends_the_part() {
    let root = temp_root("ms-download-file-");
    let body = content(300_000, 30);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    let fetcher = ScriptedFetcher::new(&body, &[Step::Short(120_000), Step::Status(404, None)]).with_identity(plain_identity("\"v1\""));
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::Announced };
    let (first, _) = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert!(matches!(first, Err(DownloadError::Http { .. })), "{first:?}");
    assert_eq!(std::fs::read_to_string(identity_sidecar(&part)).ok().as_deref(), Some("etag v1\n"));

    let again = ScriptedFetcher::new(&body, &[]).with_identity(plain_identity("\"v1\""));
    let (second, _) = run(&again, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert_eq!(second.ok(), Some(len_u64(&body)));
    assert_eq!(again.offsets(), vec![120_000]);
    assert_eq!(read(&dest), body);
    assert!(!identity_sidecar(&part).exists());
}

#[test]
fn an_identity_change_between_runs_restarts_from_zero() {
    let root = temp_root("ms-download-file-");
    let old = content(300_000, 31);
    let new = content(300_000, 32);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::Announced };
    let first = ScriptedFetcher::new(&old, &[Step::Short(100_000), Step::Status(404, None)]).with_identity(plain_identity("\"old\""));
    let (result, _) = run(&first, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert!(result.is_err());

    // The upstream file was replaced by one of the same size: the old prefix must not be kept.
    let replaced = ScriptedFetcher::new(&new, &[]).with_identity(plain_identity("\"new\""));
    let (result, _) = run(&replaced, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert_eq!(result.ok(), Some(len_u64(&new)));
    assert_eq!(replaced.offsets(), vec![100_000, 0]);
    assert_eq!(read(&dest), new);
}

#[test]
fn a_part_without_a_recorded_identity_restarts_when_the_server_names_one() {
    let root = temp_root("ms-download-file-");
    let body = content(200_000, 33);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    write(&part, &content(50_000, 34));
    let fetcher = ScriptedFetcher::new(&body, &[]).with_identity(plain_identity("\"v\""));
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::Announced };
    let (result, _) = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert_eq!(result.ok(), Some(len_u64(&body)));
    assert_eq!(fetcher.offsets(), vec![50_000, 0]);
    assert_eq!(read(&dest), body);
}

#[test]
fn an_announced_lfs_sha256_is_verified_before_publishing() {
    let root = temp_root("ms-download-file-");
    let body = content(150_000, 35);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::Announced };

    let wrong = sha256_hex(b"something else");
    let lying = ScriptedFetcher::new(&body, &[]).with_identity(lfs_identity(&wrong));
    let (result, _) = run(&lying, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert!(matches!(result, Err(DownloadError::HashMismatch { ref expected, .. }) if *expected == wrong), "{result:?}");
    assert!(!part.exists() && !dest.exists() && !identity_sidecar(&part).exists());

    let honest = ScriptedFetcher::new(&body, &[]).with_identity(lfs_identity(&sha256_hex(&body).to_ascii_uppercase()));
    let (result, events) = run(&honest, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert_eq!(result.ok(), Some(len_u64(&body)));
    assert!(events.contains(&FileEvent::Verifying { done: len_u64(&body), total: len_u64(&body) }));
    assert_eq!(read(&dest), body);
}

#[test]
fn identity_sidecars_round_trip_and_reject_garbage() {
    let root = temp_root("ms-download-file-");
    let sidecar = root.path().join("w.part.identity");
    let identity = lfs_identity(&sha256_hex(b"x"));
    assert!(write_identity(&sidecar, identity.as_ref()).is_ok());
    assert_eq!(read_identity(&sidecar), identity);
    write(&sidecar, b"etag v\ngarbage\n");
    assert_eq!(read_identity(&sidecar), None);
    assert!(write_identity(&sidecar, None).is_ok());
    assert!(!sidecar.exists());
    assert!(write_identity(&sidecar, None).is_ok());
}

#[test]
fn without_a_strong_identity_an_unpinned_part_of_an_earlier_run_restarts() {
    let root = temp_root("ms-download-file-");
    let body = content(200_000, 36);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::Announced };
    // Only a weak validator: no identity. A drop within the run still resumes in place.
    let first = ScriptedFetcher::new(&body, &[Step::Drop(50_000), Step::Short(30_000), Step::Status(404, None)]).with_identity(plain_identity("W/\"v\""));
    let (result, _) = run(&first, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert!(result.is_err());
    assert_eq!(first.offsets(), vec![0, 50_000, 80_000]);
    // The next run cannot prove the remote bytes unchanged: restart from zero.
    let second = ScriptedFetcher::new(&body, &[]).with_identity(plain_identity("W/\"v\""));
    let (result, _) = run(&second, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert_eq!(result.ok(), Some(len_u64(&body)));
    assert_eq!(second.offsets(), vec![80_000, 0]);
    assert_eq!(read(&dest), body);
}

#[test]
fn a_pinned_part_resumes_across_runs_without_an_identity() {
    let root = temp_root("ms-download-file-");
    let body = content(200_000, 37);
    let sha = sha256_hex(&body);
    let (part, dest) = (root.path().join("w.part"), root.path().join("w.bin"));
    write(&part, &body[..60_000]);
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let req = FileRequest { url: "u", part: &part, dest: &dest, expected: Expected::SizeSha256 { size: len_u64(&body), sha256: &sha } };
    let (result, _) = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), &req);
    assert_eq!(result.ok(), Some(len_u64(&body)));
    assert_eq!(fetcher.offsets(), vec![60_000]);
}
