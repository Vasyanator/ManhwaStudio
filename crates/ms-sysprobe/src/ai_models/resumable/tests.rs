/*
File: crates/ms-sysprobe/src/ai_models/resumable/tests.rs

Purpose:
Unit tests of the shared HTTP mechanics: `hf_resolve_url_at` / `endpoint_from_env_value`
(encoding, `HF_ENDPOINT` override as a parameter — no process environment is touched), the
resumable per-file fetch without a pinned size (size from the server's announcement), and the
main-repository downloader `download_hf_file_with` that is built on it. The pinned
(size + sha256) path is covered by `external/tests.rs`.

Notes:
- A scripted in-memory fetcher replays drops, short bodies, error statuses and size
  announcements; the backoff sleep only records. No network, no real waiting; scratch data
  lives in a self-removing `tempfile` directory.
*/

use std::collections::VecDeque;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::*;

const REPO: &str = "Vasyanator2/ManhwaStudio_AI_Models";

#[test]
fn resolve_url_encodes_every_segment_and_keeps_separators() {
    assert_eq!(
        hf_resolve_url_at(DEFAULT_HF_ENDPOINT, REPO, "main", "ONNX/PaddleOCR/detection/v5/det.onnx"),
        "https://huggingface.co/Vasyanator2/ManhwaStudio_AI_Models/resolve/main/ONNX/PaddleOCR/detection/v5/det.onnx"
    );
    assert_eq!(
        hf_resolve_url_at("https://hf.example", "owner/repo", "main", "dir name/a+b#c?.pt"),
        "https://hf.example/owner/repo/resolve/main/dir%20name/a%2Bb%23c%3F.pt"
    );
    // Non-ASCII is encoded byte by byte; unreserved punctuation stays.
    assert_eq!(hf_resolve_url_at("https://hf.example", "o/r", "main", "ä/x-y_z.~1"), "https://hf.example/o/r/resolve/main/%C3%A4/x-y_z.~1");
    // The revision is one segment: a branch-name slash is encoded as the Hub expects.
    assert_eq!(hf_resolve_url_at("https://hf.example", "o/r", "refs/pr/1", "f.bin"), "https://hf.example/o/r/resolve/refs%2Fpr%2F1/f.bin");
}

#[test]
fn hf_endpoint_value_overrides_the_default() {
    assert_eq!(endpoint_from_env_value(None), DEFAULT_HF_ENDPOINT);
    assert_eq!(endpoint_from_env_value(Some("")), DEFAULT_HF_ENDPOINT);
    assert_eq!(endpoint_from_env_value(Some("   ")), DEFAULT_HF_ENDPOINT);
    assert_eq!(endpoint_from_env_value(Some(" https://hf-mirror.example/ ")), "https://hf-mirror.example");
    assert_eq!(endpoint_from_env_value(Some("http://127.0.0.1:8080//")), "http://127.0.0.1:8080");
    let url = hf_resolve_url_at(endpoint_from_env_value(Some("https://hf-mirror.example/")), REPO, "main", "a.json");
    assert_eq!(url, "https://hf-mirror.example/Vasyanator2/ManhwaStudio_AI_Models/resolve/main/a.json");
}

/// One scripted answer; once the script is empty the server answers normally.
#[derive(Clone, Copy)]
enum Step {
    /// Serve `n` bytes from the requested offset, then fail the body read (connection reset).
    Drop(usize),
    /// Serve `n` bytes from the requested offset, then end the body cleanly.
    Short(usize),
    /// Answer with an HTTP error status and an optional `Retry-After`.
    Status(u16, Option<&'static str>),
    /// Serve the requested range normally but announce this whole-file size.
    Announce(Option<u64>),
}

/// Single-body server: honours ranges (416 with the real total at or past the end) and
/// replays a script of failures.
struct ScriptedFetcher {
    body: Vec<u8>,
    script: Mutex<VecDeque<Step>>,
    offsets: Mutex<Vec<u64>>,
}

impl ScriptedFetcher {
    fn new(body: &[u8], script: &[Step]) -> Self {
        Self { body: body.to_vec(), script: Mutex::new(script.iter().copied().collect()), offsets: Mutex::new(Vec::new()) }
    }

    fn offsets(&self) -> Vec<u64> {
        self.offsets.lock().map(|offsets| offsets.clone()).unwrap_or_default()
    }
}

/// A body reader whose every read fails like a reset connection.
struct ResetReader;

impl Read for ResetReader {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset by peer"))
    }
}

fn permanent(err: impl std::fmt::Display) -> FetchError {
    FetchError::Permanent { detail: err.to_string() }
}

fn len_u64(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

impl RangeFetcher for ScriptedFetcher {
    fn fetch(&self, _url: &str, offset: u64) -> Result<FetchResponse, FetchError> {
        self.offsets.lock().map_err(permanent)?.push(offset);
        let step = self.script.lock().map_err(permanent)?.pop_front();
        let total = len_u64(&self.body);
        if offset > 0 && offset >= total {
            return Err(FetchError::RangeNotSatisfiable { total: Some(total) });
        }
        let start = usize::try_from(offset).map_err(permanent)?;
        let rest = self.body.get(start..).ok_or_else(|| permanent("offset past the body"))?;
        let status = if offset > 0 { FetchStatus::Partial } else { FetchStatus::Full };
        let (total_len, body): (Option<u64>, Box<dyn Read + Send>) = match step {
            None => (Some(total), Box::new(Cursor::new(rest.to_vec()))),
            Some(Step::Drop(len)) => (Some(total), Box::new(Cursor::new(rest[..len.min(rest.len())].to_vec()).chain(ResetReader))),
            Some(Step::Short(len)) => (Some(total), Box::new(Cursor::new(rest[..len.min(rest.len())].to_vec()))),
            Some(Step::Status(code, retry_after)) => return Err(classify_status(code, retry_after)),
            Some(Step::Announce(announced)) => (announced, Box::new(Cursor::new(rest.to_vec()))),
        };
        Ok(FetchResponse { status, total_len, body })
    }
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
    match tempfile::Builder::new().prefix("ms-sysprobe-resumable-").tempdir_in(std::env::temp_dir()) {
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

// --- main-repository downloader (`ai_models::download_hf_file_with`) -------------------

#[test]
fn main_repo_download_publishes_the_file_and_removes_the_part() {
    let body = content(200_000, 20);
    let root = temp_root();
    let local = root.path().join("ONNX").join("det.onnx");
    let fetcher = ScriptedFetcher::new(&body, &[Step::Drop(70_000)]);
    let result = crate::ai_models::download_hf_file_with(&fetcher, &quiet_policy(), "mem://det.onnx", &local, "ONNX/det.onnx");
    assert_eq!(result, Ok(()));
    assert_eq!(read(&local), body);
    assert!(!root.path().join("ONNX").join("det.onnx.part").exists());
    assert_eq!(fetcher.offsets(), vec![0, 70_000]);
}

#[test]
fn main_repo_download_resumes_a_part_left_by_an_earlier_run() {
    let body = content(10_000, 21);
    let root = temp_root();
    let local = root.path().join("config.json");
    write(&root.path().join("config.json.part"), &body[..2500]);
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let result = crate::ai_models::download_hf_file_with(&fetcher, &quiet_policy(), "mem://config.json", &local, "config.json");
    assert_eq!(result, Ok(()));
    assert_eq!(fetcher.offsets(), vec![2500]);
    assert_eq!(read(&local), body);
}

#[test]
fn main_repo_download_failure_publishes_nothing_and_keeps_the_part() {
    let body = content(10_000, 22);
    let root = temp_root();
    let local = root.path().join("model.onnx");
    let mut script = vec![Step::Short(4000)];
    script.extend([Step::Status(503, None); 6]);
    let fetcher = ScriptedFetcher::new(&body, &script);
    let result = crate::ai_models::download_hf_file_with(&fetcher, &quiet_policy(), "mem://model.onnx", &local, "model.onnx");
    assert!(result.is_err());
    assert!(!local.exists());
    assert_eq!(file_len(&root.path().join("model.onnx.part")), Some(4000));
}
