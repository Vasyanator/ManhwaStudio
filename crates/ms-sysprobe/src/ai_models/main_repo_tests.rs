/*
File: crates/ms-sysprobe/src/ai_models/main_repo_tests.rs

Purpose:
Unit tests of the main-repository downloader (`ai_models::ensure_files_with` /
`download_hf_file_with`, built on `ms_download::fetch_verified`): publish and `.part`
removal, resume of a `.part` left by an earlier run, a failure that publishes nothing and
keeps the `.part`, progress events, and cancel mid-transfer, mid-backoff and while waiting for
another download of the same file.

Notes:
- The scripted in-memory fetcher comes from `ms_download::test_support` (dev-dependency
  feature `test-support`); the backoff sleep returns at once. No network, no real waiting;
  scratch data lives in a self-removing `tempfile` directory.
*/

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ms_download::busy::BusyGuard;
use ms_download::resumable::{RemoteIdentity, RetryPolicy};
use ms_download::test_support::{ScriptedFetcher, Step};
use sha2::{Digest, Sha256};

use crate::ai_models::{
    EnsureCtx, ModelDownloadEvent, ModelDownloadPhase, ModelEnsureError, PROGRESS_STEP_BYTES, VerifiedMarker, busy_key, ensure_files_with,
    ensure_pinned_file_with, progress_due,
};

/// Deterministic test content of `len` bytes, varied by `seed`.
fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|index| u8::try_from(index % 251).unwrap_or(0).wrapping_add(seed)).collect()
}

fn temp_root() -> tempfile::TempDir {
    match tempfile::Builder::new().prefix("ms-sysprobe-main-repo-").tempdir_in(std::env::temp_dir()) {
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

/// A backoff sleep for tests that must never wait.
fn no_sleep(_: Duration) {}

/// The production schedule with a sleep that returns at once.
fn quiet_policy() -> RetryPolicy<'static> {
    RetryPolicy { sleep: &no_sleep, ..RetryPolicy::standard() }
}

/// Runs `ensure_files_with` for `remotes` under `root`; `on_event` sees every event (and may
/// set `cancel`). Returns the result and every event.
fn ensure(
    fetcher: &ScriptedFetcher,
    root: &Path,
    remotes: &[&str],
    cancel: &AtomicBool,
    on_event: &dyn Fn(&ModelDownloadEvent),
) -> (Result<(), ModelEnsureError>, Vec<ModelDownloadEvent>) {
    let mut events = Vec::new();
    let mut progress = |event: &ModelDownloadEvent| {
        on_event(event);
        events.push(event.clone());
    };
    let mut ctx = EnsureCtx { cancel, progress: &mut progress };
    let result = ensure_files_with(fetcher, &quiet_policy(), root, remotes, &mut ctx);
    (result, events)
}

fn ignore(_: &ModelDownloadEvent) {}

#[test]
fn main_repo_download_publishes_the_file_and_removes_the_part() {
    let body = content(200_000, 20);
    let root = temp_root();
    let fetcher = ScriptedFetcher::new(&body, &[Step::Drop(70_000)]);
    let (result, events) = ensure(&fetcher, root.path(), &["ONNX/det.onnx"], &AtomicBool::new(false), &ignore);
    assert_eq!(result, Ok(()));
    assert_eq!(read(&root.path().join("ONNX").join("det.onnx")), body);
    assert!(!root.path().join("ONNX").join("det.onnx.part").exists());
    assert_eq!(fetcher.offsets(), vec![0, 70_000]);
    // The drop is a retry (progress), and every transfer event names the announced size.
    assert!(events.iter().any(|event| matches!(event.phase, ModelDownloadPhase::Retrying { attempt: 1, .. })));
    let last = events.last().expect("events");
    assert_eq!((last.phase, last.staged, last.total), (ModelDownloadPhase::Downloading, 200_000, Some(200_000)));
    assert_eq!((last.file.as_str(), last.file_index, last.file_count), ("ONNX/det.onnx", 0, 1));
}

#[test]
fn present_files_are_neither_requested_nor_counted() {
    let body = content(3000, 23);
    let root = temp_root();
    write(&root.path().join("config.json"), b"{}");
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let (result, events) = ensure(&fetcher, root.path(), &["config.json", "model.onnx"], &AtomicBool::new(false), &ignore);
    assert_eq!(result, Ok(()));
    assert_eq!(fetcher.offsets(), vec![0]);
    assert!(events.iter().all(|event| event.file == "model.onnx" && event.file_count == 1));
    assert_eq!(read(&root.path().join("config.json")), b"{}");
}

#[test]
fn a_part_of_an_earlier_run_without_any_identity_restarts() {
    let body = content(10_000, 21);
    let root = temp_root();
    write(&root.path().join("config.json.part"), &body[..2500]);
    // The unpinned repository and a server without a strong ETag: nothing proves the staged
    // bytes are the current file, so the earlier run's prefix is not trusted.
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let (result, _) = ensure(&fetcher, root.path(), &["config.json"], &AtomicBool::new(false), &ignore);
    assert_eq!(result, Ok(()));
    assert_eq!(fetcher.offsets(), vec![2500, 0]);
    assert_eq!(read(&root.path().join("config.json")), body);
}

#[test]
fn main_repo_download_failure_publishes_nothing_and_keeps_the_part() {
    let body = content(10_000, 22);
    let root = temp_root();
    let mut script = vec![Step::Short(4000)];
    script.extend([Step::Status(503, None); 6]);
    let fetcher = ScriptedFetcher::new(&body, &script);
    let (result, _) = ensure(&fetcher, root.path(), &["model.onnx"], &AtomicBool::new(false), &ignore);
    assert!(matches!(result, Err(ModelEnsureError::Failed(_))));
    assert!(!root.path().join("model.onnx").exists());
    assert_eq!(file_len(&root.path().join("model.onnx.part")), Some(4000));
}

#[test]
fn cancel_mid_transfer_ends_as_cancelled_and_keeps_the_part() {
    let body = content(600 * 1024, 24);
    let root = temp_root();
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let cancel = AtomicBool::new(false);
    let stop = |event: &ModelDownloadEvent| {
        if event.phase == ModelDownloadPhase::Downloading && event.staged > 0 {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    let (result, _) = ensure(&fetcher, root.path(), &["big.onnx", "second.onnx"], &cancel, &stop);
    assert_eq!(result, Err(ModelEnsureError::Cancelled));
    assert!(!root.path().join("big.onnx").exists());
    assert!(file_len(&root.path().join("big.onnx.part")).is_some_and(|len| len > 0 && len < 600 * 1024));
    // The second file is never started.
    assert_eq!(fetcher.offsets(), vec![0]);
}

#[test]
fn cancel_during_the_backoff_ends_as_cancelled() {
    let body = content(10_000, 25);
    let root = temp_root();
    let fetcher = ScriptedFetcher::new(&body, &[Step::Short(1000)]);
    let cancel = AtomicBool::new(false);
    let stop = |event: &ModelDownloadEvent| {
        if matches!(event.phase, ModelDownloadPhase::Retrying { .. }) {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    let (result, _) = ensure(&fetcher, root.path(), &["model.onnx"], &cancel, &stop);
    assert_eq!(result, Err(ModelEnsureError::Cancelled));
    assert_eq!(fetcher.offsets(), vec![0]);
    assert_eq!(file_len(&root.path().join("model.onnx.part")), Some(1000));
}

#[test]
fn cancel_while_waiting_for_another_download_of_the_file() {
    let body = content(1000, 26);
    let root = temp_root();
    let held = BusyGuard::try_acquire(&busy_key(&root.path().join("model.onnx"))).expect("free key");
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let cancel = AtomicBool::new(false);
    let stop = |event: &ModelDownloadEvent| {
        if event.phase == ModelDownloadPhase::WaitingForOtherDownload {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    let (result, events) = ensure(&fetcher, root.path(), &["model.onnx"], &cancel, &stop);
    drop(held);
    assert_eq!(result, Err(ModelEnsureError::Cancelled));
    assert_eq!(events.len(), 1, "the wait is reported once");
    assert!(fetcher.offsets().is_empty(), "nothing is requested while another holder runs");
}

/// Transfer events are thinned to one per step, while phase changes, restarts and the last
/// byte always go through.
#[test]
fn progress_is_thinned_but_keeps_phase_changes_and_the_last_byte() {
    let down = ModelDownloadPhase::Downloading;
    let retry = ModelDownloadPhase::Retrying { attempt: 1, max_attempts: 5, delay_secs: 2 };
    assert!(progress_due(None, down, 10, None));
    assert!(!progress_due(Some((down, 10)), down, 20, Some(100 * PROGRESS_STEP_BYTES)));
    assert!(progress_due(Some((down, 10)), down, 10 + PROGRESS_STEP_BYTES, None));
    assert!(progress_due(Some((down, 10)), down, 50, Some(50)));
    assert!(progress_due(Some((down, 500)), down, 0, None));
    assert!(progress_due(Some((down, 10)), retry, 10, None));
    let next = ModelDownloadPhase::Retrying { attempt: 1, max_attempts: 5, delay_secs: 1 };
    assert!(progress_due(Some((retry, 10)), next, 10, None));
    assert!(!progress_due(Some((retry, 10)), retry, 10, None));
}

/// A long transfer reports about one event per step, not one per read.
#[test]
fn a_long_transfer_reports_per_step_not_per_read() {
    let len = usize::try_from(3 * PROGRESS_STEP_BYTES).unwrap_or(usize::MAX);
    let body = content(len, 27);
    let root = temp_root();
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let (result, events) = ensure(&fetcher, root.path(), &["model.onnx"], &AtomicBool::new(false), &ignore);
    assert_eq!(result, Ok(()));
    assert!(events.len() <= 5, "{} events", events.len());
    assert_eq!(events.last().map(|event| event.staged), Some(3 * PROGRESS_STEP_BYTES));
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn etag(tag: &str) -> Option<RemoteIdentity> {
    RemoteIdentity::from_headers(None, Some(tag))
}

#[test]
fn a_part_of_another_upstream_version_is_restarted_not_spliced() {
    let old = content(50_000, 40);
    let new = content(50_000, 41);
    let root = temp_root();
    let first = ScriptedFetcher::new(&old, &[Step::Short(20_000), Step::Status(404, None)]).with_identity(etag("\"blob-old\""));
    let (result, _) = ensure(&first, root.path(), &["model.onnx"], &AtomicBool::new(false), &ignore);
    assert!(matches!(result, Err(ModelEnsureError::Failed(_))));
    assert_eq!(file_len(&root.path().join("model.onnx.part")), Some(20_000));

    let replaced = ScriptedFetcher::new(&new, &[]).with_identity(etag("\"blob-new\""));
    let (result, _) = ensure(&replaced, root.path(), &["model.onnx"], &AtomicBool::new(false), &ignore);
    assert_eq!(result, Ok(()));
    assert_eq!(replaced.offsets(), vec![20_000, 0]);
    assert_eq!(read(&root.path().join("model.onnx")), new);
}

#[test]
fn a_part_of_the_same_upstream_version_resumes() {
    let body = content(50_000, 42);
    let root = temp_root();
    let first = ScriptedFetcher::new(&body, &[Step::Short(20_000), Step::Status(404, None)]).with_identity(etag("\"blob\""));
    let (result, _) = ensure(&first, root.path(), &["model.onnx"], &AtomicBool::new(false), &ignore);
    assert!(result.is_err());
    let again = ScriptedFetcher::new(&body, &[]).with_identity(etag("\"blob\""));
    let (result, _) = ensure(&again, root.path(), &["model.onnx"], &AtomicBool::new(false), &ignore);
    assert_eq!(result, Ok(()));
    assert_eq!(again.offsets(), vec![20_000]);
    assert_eq!(read(&root.path().join("model.onnx")), body);
}

#[test]
fn an_announced_lfs_sha256_that_does_not_match_publishes_nothing() {
    let body = content(30_000, 43);
    let root = temp_root();
    let lying = RemoteIdentity::from_headers(Some(&format!("\"{}\"", sha_hex(b"other"))), None);
    let fetcher = ScriptedFetcher::new(&body, &[]).with_identity(lying);
    let (result, _) = ensure(&fetcher, root.path(), &["model.onnx"], &AtomicBool::new(false), &ignore);
    assert!(matches!(result, Err(ModelEnsureError::Failed(_))), "{result:?}");
    assert!(!root.path().join("model.onnx").exists());
    assert!(!root.path().join("model.onnx.part").exists());
}

/// Runs the pinned ensure for `remote` with `body`'s size and sha256 as the pin.
fn ensure_pinned(fetcher: &ScriptedFetcher, root: &Path, remote: &str, body: &[u8]) -> Result<(), ModelEnsureError> {
    let mut progress = |_: &ModelDownloadEvent| {};
    let cancel = AtomicBool::new(false);
    let mut ctx = EnsureCtx { cancel: &cancel, progress: &mut progress };
    let size = u64::try_from(body.len()).unwrap_or(u64::MAX);
    ensure_pinned_file_with(fetcher, &quiet_policy(), root, remote, size, &sha_hex(body), &mut ctx)
}

/// Lets the coarse filesystem clock tick, so a later write gets a later ctime than an
/// earlier one (the verified marker refuses "racy" same-tick stamps).
fn let_the_clock_tick() {
    std::thread::sleep(Duration::from_millis(50));
}

#[test]
fn a_wrong_pinned_file_of_the_right_size_is_downloaded_again() {
    let body = content(40_000, 44);
    let root = temp_root();
    let local = root.path().join("lama.ckpt");
    write(&local, &content(40_000, 45));
    let fetcher = ScriptedFetcher::new(&body, &[]);
    assert_eq!(ensure_pinned(&fetcher, root.path(), "lama.ckpt", &body), Ok(()));
    assert_eq!(fetcher.offsets(), vec![0]);
    assert_eq!(read(&local), body);
    // The repaired file is kept by the next call without any request.
    let again = ScriptedFetcher::new(&body, &[Step::Status(500, None)]);
    assert_eq!(ensure_pinned(&again, root.path(), "lama.ckpt", &body), Ok(()));
    assert!(again.offsets().is_empty());
}

#[test]
fn a_correct_pinned_file_is_kept_and_then_trusted_by_its_marker() {
    let body = content(40_000, 46);
    let root = temp_root();
    let local = root.path().join("lama.ckpt");
    write(&local, &body);
    let_the_clock_tick();
    let fetcher = ScriptedFetcher::new(&body, &[]);
    assert_eq!(ensure_pinned(&fetcher, root.path(), "lama.ckpt", &body), Ok(()));
    assert!(fetcher.offsets().is_empty(), "a file that hashes to the pin needs no request");
    if cfg!(unix) {
        assert!(VerifiedMarker::confirms(&local, 40_000, &sha_hex(&body)));
    } else {
        assert!(!VerifiedMarker::confirms(&local, 40_000, &sha_hex(&body)), "no cheap identity: always hashed");
    }
    // A pin change is not confirmed by the old marker.
    assert!(!VerifiedMarker::confirms(&local, 40_000, &sha_hex(b"new pin")));
}

#[test]
fn a_same_size_rewrite_with_restored_mtime_is_no_longer_trusted() {
    let body = content(40_000, 47);
    let root = temp_root();
    let local = root.path().join("lama.ckpt");
    write(&local, &body);
    let_the_clock_tick();
    let fetcher = ScriptedFetcher::new(&body, &[]);
    assert_eq!(ensure_pinned(&fetcher, root.path(), "lama.ckpt", &body), Ok(()));
    let original_mtime = std::fs::metadata(&local).and_then(|metadata| metadata.modified()).expect("mtime");
    let_the_clock_tick();
    // What `cp -p` / a backup restore does: other bytes, same size, same mtime.
    write(&local, &content(40_000, 48));
    let handle = std::fs::OpenOptions::new().write(true).open(&local).expect("open");
    handle.set_modified(original_mtime).expect("set mtime");
    drop(handle);
    assert!(!VerifiedMarker::confirms(&local, 40_000, &sha_hex(&body)));
    let again = ScriptedFetcher::new(&body, &[]);
    assert_eq!(ensure_pinned(&again, root.path(), "lama.ckpt", &body), Ok(()));
    assert_eq!(again.offsets(), vec![0]);
    assert_eq!(read(&local), body);
}
