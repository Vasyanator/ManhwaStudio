/*
File: crates/ms-download/src/resumable/tests.rs

Purpose:
Unit tests of the resumable per-file fetch without a pinned size (size from the server's
announcement): resume after drops and short bodies, retry with backoff and `Retry-After`,
permanent statuses, proven-wrong staged bytes, cancel during the backoff, 416 handling.
The pinned (size + sha256) path is covered by `ms_sysprobe::ai_models::external` tests.

Notes:
- The scripted in-memory fetcher (`crate::test_support`) replays drops, short bodies, error
  statuses and size announcements; the backoff sleep only records. No network, no real
  waiting; scratch data lives in a self-removing `tempfile` directory.
*/

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::*;
use crate::test_support::{ScriptedFetcher, Step};

fn len_u64(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

/// Records every backoff sleep slice; optionally sets `cancel` on the first one.
struct SleepLog<'a> {
    slices: Mutex<Vec<Duration>>,
    cancel_on_sleep: Option<&'a AtomicBool>,
}

impl<'a> SleepLog<'a> {
    fn new(cancel_on_sleep: Option<&'a AtomicBool>) -> Self {
        Self { slices: Mutex::new(Vec::new()), cancel_on_sleep }
    }

    fn sleep(&self, slice: Duration) {
        if let Ok(mut slices) = self.slices.lock() {
            slices.push(slice);
        }
        if let Some(cancel) = self.cancel_on_sleep {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    fn count(&self) -> usize {
        self.slices.lock().map(|slices| slices.len()).unwrap_or(usize::MAX)
    }

    fn total(&self) -> Duration {
        self.slices.lock().map(|slices| slices.iter().sum()).unwrap_or(Duration::MAX)
    }
}

/// Deterministic test content of `len` bytes, varied by `seed`.
fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|index| u8::try_from(index % 251).unwrap_or(0).wrapping_add(seed)).collect()
}

fn temp_root() -> tempfile::TempDir {
    match tempfile::Builder::new().prefix("ms-download-resumable-").tempdir_in(std::env::temp_dir()) {
        Ok(dir) => dir,
        Err(err) => panic!("cannot create a temp dir: {err}"),
    }
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap_or_else(|err| panic!("write {}: {err}", path.display()));
}

fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|metadata| metadata.len())
}

/// Runs an unpinned `fetch_part` of `part` from its current length with a recording sleep;
/// returns the result and every `Retrying` event.
fn fetch_unpinned(
    fetcher: &ScriptedFetcher,
    part: &Path,
    cancel: &AtomicBool,
    sleeps: &SleepLog<'_>,
) -> (Result<u64, StreamError>, Vec<StreamEvent>) {
    let sleep = |slice: Duration| sleeps.sleep(slice);
    let policy = RetryPolicy { sleep: &sleep, ..RetryPolicy::standard() };
    let engine = ResumableFetch { fetcher, retry: &policy, cancel, log_tag: "[test]", log_context: "file=model.bin" };
    let request = PartRequest { url: "mem://model.bin", part, offset: file_len(part).unwrap_or(0), expected_size: None };
    let mut retries = Vec::new();
    let result = engine.fetch_part(&request, None, &mut |event| {
        if matches!(event, StreamEvent::Retrying { .. }) {
            retries.push(event);
        }
    });
    (result, retries)
}

/// A backoff sleep for tests that must never wait.
fn no_sleep(_: Duration) {}

/// The production schedule with a sleep that returns at once.
fn quiet_policy() -> RetryPolicy<'static> {
    RetryPolicy { sleep: &no_sleep, ..RetryPolicy::standard() }
}

fn part_in(root: &tempfile::TempDir) -> PathBuf {
    root.path().join("model.bin.part")
}

#[test]
fn unpinned_download_takes_its_size_from_the_announcement() {
    let body = content(300 * 1024, 1);
    let root = temp_root();
    let part = part_in(&root);
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let (result, retries) = fetch_unpinned(&fetcher, &part, &AtomicBool::new(false), &SleepLog::new(None));
    assert_eq!(result.ok(), Some(len_u64(&body)));
    assert_eq!(read(&part), body);
    assert!(retries.is_empty());
}

/// Every `Downloading` event of an unpinned stream carries the size the server announced, so
/// a consumer can draw a determinate bar without a pin.
#[test]
fn downloading_events_carry_the_announced_total() {
    let body = content(300 * 1024, 7);
    let root = temp_root();
    let part = part_in(&root);
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let policy = quiet_policy();
    let cancel = AtomicBool::new(false);
    let engine = ResumableFetch { fetcher: &fetcher, retry: &policy, cancel: &cancel, log_tag: "[test]", log_context: "file=model.bin" };
    let request = PartRequest { url: "mem://model.bin", part: &part, offset: 0, expected_size: None };
    let mut totals = Vec::new();
    let result = engine.fetch_part(&request, None, &mut |event| {
        if let StreamEvent::Downloading { total, .. } = event {
            totals.push(total);
        }
    });
    assert_eq!(result.ok(), Some(len_u64(&body)));
    assert!(!totals.is_empty());
    assert!(totals.iter().all(|total| *total == Some(len_u64(&body))));
}

#[test]
fn a_short_body_keeps_the_part_and_resumes_from_its_length() {
    let body = content(5000, 2);
    let root = temp_root();
    let part = part_in(&root);
    let fetcher = ScriptedFetcher::new(&body, &[Step::Short(1000), Step::Drop(1500)]);
    let sleeps = SleepLog::new(None);
    let (result, retries) = fetch_unpinned(&fetcher, &part, &AtomicBool::new(false), &sleeps);
    assert_eq!(result.ok(), Some(5000));
    assert_eq!(fetcher.offsets(), vec![0, 1000, 2500]);
    assert_eq!(read(&part), body);
    // Each failure staged new bytes, so both retries are "attempt 1" with the 2 s backoff.
    assert_eq!(sleeps.total(), Duration::from_secs(4));
    assert!(retries.iter().all(|event| matches!(event, StreamEvent::Retrying { attempt: 1, .. })));
}

#[test]
fn a_5xx_is_retried_with_backoff_and_retry_after() {
    let body = content(3000, 3);
    let root = temp_root();
    let part = part_in(&root);
    let fetcher = ScriptedFetcher::new(&body, &[Step::Status(502, None), Step::Status(503, Some("7"))]);
    let sleeps = SleepLog::new(None);
    let (result, _) = fetch_unpinned(&fetcher, &part, &AtomicBool::new(false), &sleeps);
    assert_eq!(result.ok(), Some(3000));
    assert_eq!(fetcher.offsets(), vec![0, 0, 0]);
    assert_eq!(sleeps.total(), Duration::from_secs(2 + 7));
}

#[test]
fn a_404_is_not_retried() {
    let body = content(3000, 4);
    let root = temp_root();
    let part = part_in(&root);
    let fetcher = ScriptedFetcher::new(&body, &[Step::Status(404, None)]);
    let sleeps = SleepLog::new(None);
    let (result, retries) = fetch_unpinned(&fetcher, &part, &AtomicBool::new(false), &sleeps);
    assert!(matches!(&result, Err(StreamError::Http { detail }) if detail.contains("404")), "{result:?}");
    assert_eq!(fetcher.offsets(), vec![0]);
    assert_eq!(sleeps.count(), 0);
    assert!(retries.is_empty());
}

#[test]
fn more_bytes_than_announced_deletes_the_part() {
    let body = content(3000, 5);
    let root = temp_root();
    let part = part_in(&root);
    let fetcher = ScriptedFetcher::new(&body, &[Step::Announce(Some(2000))]);
    let (result, _) = fetch_unpinned(&fetcher, &part, &AtomicBool::new(false), &SleepLog::new(None));
    assert!(matches!(result, Err(StreamError::SizeMismatch { expected: 2000, .. })), "{result:?}");
    assert!(!part.exists());
}

#[test]
fn a_206_announcing_another_total_deletes_the_part() {
    let body = content(3000, 6);
    let root = temp_root();
    let part = part_in(&root);
    // The first body is cut after 1000 bytes (total 3000 learned); the resume announces 4000:
    // the remote file changed, so the staged prefix belongs to another version.
    let fetcher = ScriptedFetcher::new(&body, &[Step::Short(1000), Step::Announce(Some(4000))]);
    let (result, _) = fetch_unpinned(&fetcher, &part, &AtomicBool::new(false), &SleepLog::new(None));
    assert!(matches!(result, Err(StreamError::SizeMismatch { expected: 3000, actual: 4000 })), "{result:?}");
    assert!(!part.exists());
}

#[test]
fn no_announced_size_is_refused_and_the_part_kept() {
    let body = content(3000, 7);
    let root = temp_root();
    let part = part_in(&root);
    let fetcher = ScriptedFetcher::new(&body, &[Step::Announce(None)]);
    let (result, _) = fetch_unpinned(&fetcher, &part, &AtomicBool::new(false), &SleepLog::new(None));
    assert!(matches!(&result, Err(StreamError::Http { detail }) if detail.contains("no file size")), "{result:?}");
    assert_eq!(file_len(&part), Some(3000));
}

#[test]
fn cancel_during_the_backoff_wait_returns_promptly_and_keeps_the_part() {
    let body = content(3000, 8);
    let root = temp_root();
    let part = part_in(&root);
    let fetcher = ScriptedFetcher::new(&body, &[Step::Drop(500)]);
    let cancel = AtomicBool::new(false);
    let sleeps = SleepLog::new(Some(&cancel));
    let (result, _) = fetch_unpinned(&fetcher, &part, &cancel, &sleeps);
    assert!(matches!(result, Err(StreamError::Cancelled)), "{result:?}");
    // The flag is seen after the first 100 ms slice, not after the whole 2 s wait.
    assert_eq!(sleeps.count(), 1);
    assert_eq!(fetcher.offsets(), vec![0]);
    assert_eq!(file_len(&part), Some(500));
}

#[test]
fn a_complete_part_from_an_earlier_run_is_accepted_on_a_matching_416() {
    let body = content(4000, 9);
    let root = temp_root();
    let part = part_in(&root);
    write(&part, &body);
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let (result, _) = fetch_unpinned(&fetcher, &part, &AtomicBool::new(false), &SleepLog::new(None));
    assert_eq!(result.ok(), Some(4000));
    assert_eq!(fetcher.offsets(), vec![4000]);
    assert_eq!(read(&part), body);
}

#[test]
fn a_part_longer_than_the_remote_file_restarts_from_zero() {
    let body = content(4000, 10);
    let root = temp_root();
    let part = part_in(&root);
    write(&part, &content(6000, 99));
    let sleeps = SleepLog::new(None);
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let (result, _) = fetch_unpinned(&fetcher, &part, &AtomicBool::new(false), &sleeps);
    assert_eq!(result.ok(), Some(4000));
    assert_eq!(fetcher.offsets(), vec![6000, 0]);
    assert_eq!(read(&part), body);
    // The restart is immediate, not a backoff.
    assert_eq!(sleeps.count(), 0);
}

#[test]
fn a_416_with_a_pinned_size_is_permanent() {
    let body = content(1000, 11);
    let root = temp_root();
    let part = part_in(&root);
    write(&part, &content(1500, 12));
    let policy = quiet_policy();
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let cancel = AtomicBool::new(false);
    let engine = ResumableFetch { fetcher: &fetcher, retry: &policy, cancel: &cancel, log_tag: "[test]", log_context: "file=model.bin" };
    let request = PartRequest { url: "mem://model.bin", part: &part, offset: 1500, expected_size: Some(2000) };
    let result = engine.fetch_part(&request, None, &mut |_| {});
    assert!(matches!(&result, Err(StreamError::Http { detail }) if detail.contains("416")), "{result:?}");
    assert_eq!(file_len(&part), Some(1500));
}

#[test]
fn redirect_targets_resolve_against_the_current_url() {
    let base = "https://huggingface.co/owner/repo/resolve/main/dir/file.bin?download=1";
    let ok = |location: &str| resolve_location(base, location).ok();
    assert_eq!(ok("https://cdn.example/x?sig=1").as_deref(), Some("https://cdn.example/x?sig=1"));
    assert_eq!(ok("//cdn.example/x").as_deref(), Some("https://cdn.example/x"));
    assert_eq!(ok("/api/resolve-cache/x").as_deref(), Some("https://huggingface.co/api/resolve-cache/x"));
    assert_eq!(ok("other.bin").as_deref(), Some("https://huggingface.co/owner/repo/resolve/main/dir/other.bin"));
    // Query-only: same resource, new query (RFC 3986 §5.2.2).
    assert_eq!(ok("?sig=2").as_deref(), Some("https://huggingface.co/owner/repo/resolve/main/dir/file.bin?sig=2"));
    // Empty reference keeps the base query; a fragment is dropped.
    assert_eq!(ok("#frag").as_deref(), Some(base));
    // Dot segments are removed.
    assert_eq!(ok("../up/./x.bin").as_deref(), Some("https://huggingface.co/owner/repo/resolve/main/up/x.bin"));
    assert_eq!(ok("/a/b/../../../c").as_deref(), Some("https://huggingface.co/c"));
    assert_eq!(resolve_location("https://host", "x").ok().as_deref(), Some("https://host/x"));
    assert_eq!(resolve_location("http://host/a", "HTTPS://secure/b").ok().as_deref(), Some("https://secure/b"));
}

#[test]
fn rfc3986_reference_resolution_examples() {
    // RFC 3986 §5.4.1 normal examples (fragments dropped, base made http(s)).
    let base = "http://a/b/c/d;p?q";
    for (reference, expected) in [
        ("g", "http://a/b/c/g"),
        ("./g", "http://a/b/c/g"),
        ("g/", "http://a/b/c/g/"),
        ("/g", "http://a/g"),
        ("//g", "http://g"),
        ("?y", "http://a/b/c/d;p?y"),
        ("g?y", "http://a/b/c/g?y"),
        ("", "http://a/b/c/d;p?q"),
        (".", "http://a/b/c/"),
        ("./", "http://a/b/c/"),
        ("..", "http://a/b/"),
        ("../g", "http://a/b/g"),
        ("../..", "http://a/"),
        ("../../g", "http://a/g"),
        ("../../../g", "http://a/g"),
        ("g;x=1/../y", "http://a/b/c/y"),
    ] {
        assert_eq!(resolve_location(base, reference).ok().as_deref(), Some(expected), "{reference}");
    }
}

#[test]
fn downgrades_and_foreign_schemes_are_refused() {
    let base = "https://huggingface.co/x";
    assert!(resolve_location(base, "http://cdn.example/x").is_err());
    assert!(resolve_location(base, "HTTP://cdn.example/x").is_err());
    assert!(resolve_location(base, "ftp://elsewhere/x").is_err());
    assert!(resolve_location(base, "file:/etc/passwd").is_err());
    assert!(resolve_location("file:///etc/x", "/y").is_err());
    assert!(resolve_location("http://plain/x", "http://other/y").is_ok());
}

#[test]
fn identities_prefer_the_linked_etag_and_read_only_it_as_a_hash() {
    let sha = "D625AA1B3E0D0408ACFD6928AA84F005867AA8DBB9162480346A4E20660786CC";
    let lfs = RemoteIdentity::from_headers(Some(&format!("\"{sha}\"")), Some("\"cdn\""));
    assert_eq!(lfs.as_ref().map(|identity| identity.etag.as_str()), Some(sha));
    assert_eq!(lfs.and_then(|identity| identity.lfs_sha256), Some(sha.to_ascii_lowercase()));

    // A 64-hex plain ETag is still only a version tag, never a content hash.
    let plain = RemoteIdentity::from_headers(None, Some(&format!("\"{sha}\"")));
    assert_eq!(plain, Some(RemoteIdentity { etag: sha.to_owned(), lfs_sha256: None }));
    // Weak validators never make an identity (no byte-equality promise).
    assert_eq!(RemoteIdentity::from_headers(None, Some("W/\"v1\"")), None);
    assert_eq!(RemoteIdentity::from_headers(Some("W/\"abc\""), Some("\"strong\"")), Some(RemoteIdentity { etag: "strong".to_owned(), lfs_sha256: None }));
    let blob = RemoteIdentity::from_headers(Some("\"abc\""), None);
    assert_eq!(blob, Some(RemoteIdentity { etag: "abc".to_owned(), lfs_sha256: None }));
    assert_eq!(RemoteIdentity::from_headers(Some("\"\""), Some(" ")), None);
}
