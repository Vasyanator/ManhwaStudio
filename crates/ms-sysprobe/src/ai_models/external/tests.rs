/*
File: crates/ms-sysprobe/src/ai_models/external/tests.rs

Purpose:
Unit tests of the external-model downloader core (`download_with`), its transient-failure
retry, and the stat-only status probe, driven by in-memory `RangeFetcher`s (one plain, one
scripted with drops / short bodies / error statuses) and an injected backoff sleep that only
records — no network, no real waiting, scratch data only under a unique `tempfile`
directory removed on drop. One opt-in `#[ignore]` test downloads the real Baberu
spec when `MS_TEST_EXTERNAL_DOWNLOAD=1`.

Notes:
- Each test builds its own leaked `'static` spec with a unique id, because the busy guard is
  process-wide and tests run in parallel.
*/

use std::collections::{BTreeMap, VecDeque};
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::*;

/// How the fake server answers a range request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RangeMode {
    /// 206 with the requested suffix.
    Honour,
    /// 200 with the whole body.
    Ignore,
}

/// Rendezvous for the busy test: the first fetch announces itself, then waits for release.
struct FetchGate {
    entered: Mutex<mpsc::Sender<()>>,
    release: Mutex<mpsc::Receiver<()>>,
}

/// In-memory server keyed by URL; records every `(url, offset)` it was asked for.
struct FakeFetcher {
    bodies: BTreeMap<String, Vec<u8>>,
    mode: RangeMode,
    calls: Mutex<Vec<(String, u64)>>,
    /// When set, the first fetch announces itself and then blocks until released.
    gate: Option<FetchGate>,
}

impl FakeFetcher {
    fn new(bodies: BTreeMap<String, Vec<u8>>, mode: RangeMode) -> Self {
        Self { bodies, mode, calls: Mutex::new(Vec::new()), gate: None }
    }

    fn calls(&self) -> Vec<(String, u64)> {
        self.calls.lock().map(|calls| calls.clone()).unwrap_or_default()
    }
}

impl RangeFetcher for FakeFetcher {
    fn fetch(&self, url: &str, offset: u64) -> Result<FetchResponse, FetchError> {
        if let Some(FetchGate { entered, release }) = &self.gate {
            entered.lock().map_err(permanent)?.send(()).map_err(permanent)?;
            release.lock().map_err(permanent)?.recv().map_err(permanent)?;
        }
        self.calls.lock().map_err(permanent)?.push((url.to_owned(), offset));
        let body = self.bodies.get(url).ok_or_else(|| classify_status(404, None))?;
        if self.mode == RangeMode::Honour && offset > 0 {
            let start = usize::try_from(offset).map_err(permanent)?;
            let suffix = body.get(start..).ok_or_else(|| classify_status(416, None))?.to_vec();
            return Ok(FetchResponse { status: FetchStatus::Partial, body: Box::new(Cursor::new(suffix)) });
        }
        Ok(FetchResponse { status: FetchStatus::Full, body: Box::new(Cursor::new(body.clone())) })
    }
}

/// A fake-server internal failure as a permanent fetch error.
fn permanent(err: impl std::fmt::Display) -> FetchError {
    FetchError::Permanent { detail: err.to_string() }
}

fn url_of(file: &ExternalFile) -> String {
    format!("mem://{}", file.path)
}

fn sha_hex(bytes: &[u8]) -> String {
    hex_lower(&Sha256::digest(bytes))
}

/// Deterministic test content of `len` bytes, varied by `seed`.
fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|index| u8::try_from(index % 251).unwrap_or(0).wrapping_add(seed)).collect()
}

fn leak(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

/// Builds a leaked spec over `(path, body)` pairs plus the matching fake-server body map.
fn make_spec(id: &str, revision: &str, files: &[(&str, &[u8])]) -> (&'static ExternalModelSpec, BTreeMap<String, Vec<u8>>) {
    let entries: Vec<ExternalFile> = files
        .iter()
        .map(|(path, body)| ExternalFile {
            path: leak((*path).to_owned()),
            size: u64::try_from(body.len()).unwrap_or(u64::MAX),
            sha256: leak(sha_hex(body)),
        })
        .collect();
    let bodies = entries.iter().zip(files).map(|(file, (_, body))| (url_of(file), body.to_vec())).collect();
    let spec = Box::leak(Box::new(ExternalModelSpec {
        id: leak(id.to_owned()),
        repo_id: "owner/repo",
        revision: leak(revision.to_owned()),
        dir: leak(format!("Test/{id}")),
        files: Box::leak(entries.into_boxed_slice()),
    }));
    (spec, bodies)
}

const REV_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REV_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn temp_root() -> tempfile::TempDir {
    match tempfile::Builder::new().prefix("ms-sysprobe-external-").tempdir_in(std::env::temp_dir()) {
        Ok(dir) => dir,
        Err(err) => panic!("cannot create a temp dir: {err}"),
    }
}

/// A backoff sleep for tests that must never wait.
fn no_sleep(_: Duration) {}

/// The production schedule with a sleep that returns at once.
fn quiet_policy() -> RetryPolicy<'static> {
    RetryPolicy { sleep: &no_sleep, ..RetryPolicy::standard() }
}

fn run(fetcher: &dyn RangeFetcher, root: &Path, spec: &'static ExternalModelSpec, cancel: &AtomicBool) -> Result<PathBuf, ExternalModelError> {
    download_with(fetcher, &url_of, root, spec, cancel, &quiet_policy(), &mut |_| {})
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

fn staged_part(root: &Path, spec: &ExternalModelSpec, file: &ExternalFile) -> PathBuf {
    part_path(&spec.local_dir(root).join(STAGING_DIR), file)
}

#[test]
fn fresh_download_publishes_files_and_marker() {
    let big = content(300 * 1024, 1);
    let small = content(17, 2);
    let (spec, bodies) = make_spec("fresh", REV_A, &[("onnx/big.bin", &big), ("small.txt", &small)]);
    let root = temp_root();
    let fetcher = FakeFetcher::new(bodies, RangeMode::Honour);
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Missing);
    assert!(matches!(installed_dir(root.path(), spec), Err(ExternalModelError::NotDownloaded { .. })));

    let mut last = None;
    let dir = download_with(&fetcher, &url_of, root.path(), spec, &AtomicBool::new(false), &quiet_policy(), &mut |progress| last = Some(*progress));
    let dir = dir.unwrap_or_else(|err| panic!("download failed: {err}"));

    assert_eq!(read(&dir.join("onnx").join("big.bin")), big);
    assert_eq!(read(&dir.join("small.txt")), small);
    assert!(dir.join(COMPLETE_MARKER).is_file());
    assert!(!dir.join(STAGING_DIR).exists());
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Installed);
    assert_eq!(installed_dir(root.path(), spec).ok(), Some(dir));
    let last = last.unwrap_or_else(|| panic!("no progress reported"));
    assert_eq!(last.total_done, spec.total_bytes());
    assert_eq!(last.file_index, 1);
}

#[test]
fn resume_appends_after_a_206() {
    let body = content(200_000, 3);
    let (spec, bodies) = make_spec("resume", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    let part = staged_part(root.path(), spec, &spec.files[0]);
    std::fs::create_dir_all(part.parent().unwrap_or(root.path())).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(&part, &body[..1000]).unwrap_or_else(|err| panic!("{err}"));

    let fetcher = FakeFetcher::new(bodies, RangeMode::Honour);
    let dir = run(&fetcher, root.path(), spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fetcher.calls(), vec![(url_of(&spec.files[0]), 1000)]);
    assert_eq!(read(&dir.join("model.bin")), body);
}

#[test]
fn a_200_answer_to_a_range_request_restarts_from_zero() {
    let body = content(5000, 4);
    let (spec, bodies) = make_spec("restart200", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    let part = staged_part(root.path(), spec, &spec.files[0]);
    std::fs::create_dir_all(part.parent().unwrap_or(root.path())).unwrap_or_else(|err| panic!("{err}"));
    // A prefix that does NOT match the real bytes: only a true restart yields the right hash.
    std::fs::write(&part, vec![0xEE; 700]).unwrap_or_else(|err| panic!("{err}"));

    let fetcher = FakeFetcher::new(bodies, RangeMode::Ignore);
    let dir = run(&fetcher, root.path(), spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fetcher.calls(), vec![(url_of(&spec.files[0]), 700)]);
    assert_eq!(read(&dir.join("model.bin")), body);
}

#[test]
fn an_overflowing_body_deletes_the_part_and_writes_no_marker() {
    let body = content(4000, 5);
    let (spec, mut bodies) = make_spec("sizemismatch", REV_A, &[("model.bin", &body)]);
    for served in bodies.values_mut() {
        served.extend_from_slice(b"extra");
    }
    let root = temp_root();
    let fetcher = FakeFetcher::new(bodies, RangeMode::Honour);
    let result = run(&fetcher, root.path(), spec, &AtomicBool::new(false));
    assert!(matches!(result, Err(ExternalModelError::SizeMismatch { expected: 4000, actual: 4005, .. })), "{result:?}");
    assert!(!staged_part(root.path(), spec, &spec.files[0]).exists());
    assert!(!spec.local_dir(root.path()).join(COMPLETE_MARKER).exists());
}

#[test]
fn hash_mismatch_deletes_the_part_and_writes_no_marker() {
    let body = content(4000, 6);
    let (spec, mut bodies) = make_spec("hashmismatch", REV_A, &[("model.bin", &body)]);
    for served in bodies.values_mut() {
        served[10] ^= 0xFF;
    }
    let root = temp_root();
    let fetcher = FakeFetcher::new(bodies, RangeMode::Honour);
    let result = run(&fetcher, root.path(), spec, &AtomicBool::new(false));
    assert!(matches!(result, Err(ExternalModelError::HashMismatch { .. })), "{result:?}");
    assert!(!staged_part(root.path(), spec, &spec.files[0]).exists());
    assert!(!spec.local_dir(root.path()).join("model.bin").exists());
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Missing);
}

#[test]
fn cancel_keeps_the_part_and_a_later_call_resumes() {
    let body = content(600 * 1024, 7);
    let (spec, bodies) = make_spec("cancel", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    let fetcher = FakeFetcher::new(bodies, RangeMode::Honour);
    let cancel = AtomicBool::new(false);
    let result = download_with(&fetcher, &url_of, root.path(), spec, &cancel, &quiet_policy(), &mut |progress| {
        if progress.phase == ExternalDownloadPhase::Downloading && progress.file_done >= 128 * 1024 {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    assert!(matches!(result, Err(ExternalModelError::Cancelled)), "{result:?}");
    let dir = spec.local_dir(root.path());
    assert!(!dir.join(COMPLETE_MARKER).exists());
    let staged = file_len(&staged_part(root.path(), spec, &spec.files[0])).unwrap_or(0);
    assert!(staged > 0 && staged < spec.total_bytes(), "staged {staged}");
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Partial { bytes_present: staged });

    let dir = run(&fetcher, root.path(), spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fetcher.calls().last(), Some(&(url_of(&spec.files[0]), staged)));
    assert_eq!(read(&dir.join("model.bin")), body);
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Installed);
}

#[test]
fn changed_pin_reads_missing_keeps_matching_files_and_removes_stale_ones() {
    let shared = content(3000, 8);
    let old_weights = content(3000, 9);
    let new_weights = content(3100, 10);
    let stale = content(50, 11);
    let added = content(60, 12);
    let (old_spec, old_bodies) = make_spec("repin", REV_A, &[("shared.json", &shared), ("weights.bin", &old_weights), ("stale.txt", &stale)]);
    let root = temp_root();
    run(&FakeFetcher::new(old_bodies, RangeMode::Honour), root.path(), old_spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));

    // Same id and directory, new revision and file set.
    let (new_spec, new_bodies) = make_spec("repin", REV_B, &[("shared.json", &shared), ("weights.bin", &new_weights), ("added.txt", &added)]);
    assert_eq!(external_model_status(root.path(), new_spec), ExternalModelStatus::Missing);
    assert_eq!(external_model_status(root.path(), old_spec), ExternalModelStatus::Installed);

    let fetcher = FakeFetcher::new(new_bodies, RangeMode::Honour);
    let dir = run(&fetcher, root.path(), new_spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    let fetched: Vec<String> = fetcher.calls().into_iter().map(|(url, _)| url).collect();
    assert_eq!(fetched, vec!["mem://weights.bin".to_owned(), "mem://added.txt".to_owned()]);
    assert_eq!(read(&dir.join("weights.bin")), new_weights);
    assert!(!dir.join("stale.txt").exists());
    assert_eq!(external_model_status(root.path(), new_spec), ExternalModelStatus::Installed);
    assert_eq!(external_model_status(root.path(), old_spec), ExternalModelStatus::Missing);
}

#[test]
fn staged_parts_of_another_pin_are_discarded() {
    let body = content(5000, 13);
    let (old_spec, old_bodies) = make_spec("repinpart", REV_A, &[("model.bin", &content(5000, 14))]);
    let root = temp_root();
    let cancel = AtomicBool::new(false);
    let result = download_with(&FakeFetcher::new(old_bodies, RangeMode::Honour), &url_of, root.path(), old_spec, &cancel, &quiet_policy(), &mut |progress| {
        if progress.phase == ExternalDownloadPhase::Downloading {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    assert!(matches!(result, Err(ExternalModelError::Cancelled)), "{result:?}");

    let (new_spec, new_bodies) = make_spec("repinpart", REV_B, &[("model.bin", &body)]);
    assert_eq!(external_model_status(root.path(), new_spec), ExternalModelStatus::Missing);
    let fetcher = FakeFetcher::new(new_bodies, RangeMode::Honour);
    let dir = run(&fetcher, root.path(), new_spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fetcher.calls(), vec![("mem://model.bin".to_owned(), 0)]);
    assert_eq!(read(&dir.join("model.bin")), body);
}

#[test]
fn files_published_by_an_abandoned_pin_are_removed_by_the_next_install() {
    let shared = content(2000, 20);
    let (spec_a, bodies_a) = make_spec("abandon", REV_A, &[("shared.json", &shared), ("a_only.txt", &content(40, 21))]);
    let root = temp_root();
    run(&FakeFetcher::new(bodies_a, RangeMode::Honour), root.path(), spec_a, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));

    // Pin B publishes `b_only.txt`, then is cancelled while streaming its last file.
    let (spec_b, bodies_b) = make_spec("abandon", REV_B, &[("shared.json", &shared), ("b_only.txt", &content(50, 22)), ("b_big.bin", &content(300 * 1024, 23))]);
    let cancel = AtomicBool::new(false);
    let result = download_with(&FakeFetcher::new(bodies_b, RangeMode::Honour), &url_of, root.path(), spec_b, &cancel, &quiet_policy(), &mut |progress| {
        if progress.file_path == "b_big.bin" && progress.phase == ExternalDownloadPhase::Downloading {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    assert!(matches!(result, Err(ExternalModelError::Cancelled)), "{result:?}");
    let dir = spec_a.local_dir(root.path());
    assert!(dir.join("b_only.txt").is_file(), "B must have published b_only.txt before the cancel");

    // Pin C lists neither A's nor B's own files.
    let rev_c = "cccccccccccccccccccccccccccccccccccccccc";
    let (spec_c, bodies_c) = make_spec("abandon", rev_c, &[("shared.json", &shared), ("c_only.txt", &content(60, 24))]);
    run(&FakeFetcher::new(bodies_c, RangeMode::Honour), root.path(), spec_c, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    assert!(!dir.join("b_only.txt").exists(), "a file published by the abandoned pin B was left behind");
    assert!(!dir.join("b_big.bin").exists());
    assert!(!dir.join("a_only.txt").exists());
    assert_eq!(read(&dir.join("shared.json")), shared);
    assert!(dir.join("c_only.txt").is_file());
    assert!(!dir.join(STAGING_DIR).exists());
    assert_eq!(external_model_status(root.path(), spec_c), ExternalModelStatus::Installed);
}

#[test]
fn an_over_long_part_is_deleted_and_the_file_restarts_from_zero() {
    let body = content(3000, 25);
    let (spec, bodies) = make_spec("overlong", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    let part = staged_part(root.path(), spec, &spec.files[0]);
    std::fs::create_dir_all(part.parent().unwrap_or(root.path())).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(&part, vec![0xAB; 3500]).unwrap_or_else(|err| panic!("{err}"));

    let fetcher = FakeFetcher::new(bodies, RangeMode::Honour);
    let dir = run(&fetcher, root.path(), spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fetcher.calls(), vec![(url_of(&spec.files[0]), 0)]);
    assert_eq!(read(&dir.join("model.bin")), body);
}

#[test]
fn a_malformed_in_progress_marker_is_treated_as_foreign() {
    let body = content(3000, 26);
    let (spec, bodies) = make_spec("malformedprogress", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    let part = staged_part(root.path(), spec, &spec.files[0]);
    let staging = spec.local_dir(root.path()).join(STAGING_DIR);
    std::fs::create_dir_all(part.parent().unwrap_or(root.path())).unwrap_or_else(|err| panic!("{err}"));
    // A wrong prefix: resuming it instead of discarding would end in a hash mismatch.
    std::fs::write(&part, vec![0xCD; 700]).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(staging.join(IN_PROGRESS_MARKER), b"{not json").unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Missing);

    let fetcher = FakeFetcher::new(bodies, RangeMode::Honour);
    let dir = run(&fetcher, root.path(), spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fetcher.calls(), vec![(url_of(&spec.files[0]), 0)]);
    assert_eq!(read(&dir.join("model.bin")), body);
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Installed);
}

/// A staging directory that cannot be removed after the marker is written must not turn an
/// installed model into a failed download. Unix-only: a read-only subdirectory makes
/// `remove_dir_all` fail (when running as root it succeeds and the test still holds).
#[cfg(unix)]
#[test]
fn a_staging_removal_failure_after_the_marker_still_reports_success() {
    use std::os::unix::fs::PermissionsExt;

    let body = content(1500, 27);
    let (spec, bodies) = make_spec("stagingstuck", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    let locked = spec.local_dir(root.path()).join(STAGING_DIR).join("locked");
    std::fs::create_dir_all(&locked).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(locked.join("held"), b"x").unwrap_or_else(|err| panic!("{err}"));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap_or_else(|err| panic!("{err}"));

    let result = run(&FakeFetcher::new(bodies, RangeMode::Honour), root.path(), spec, &AtomicBool::new(false));
    // Restore write access first, so the temp dir can always be removed on drop.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap_or_else(|err| panic!("{err}"));
    let dir = result.unwrap_or_else(|err| panic!("an installed model must not be reported as failed: {err}"));
    assert_eq!(read(&dir.join("model.bin")), body);
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Installed);
}

#[test]
fn a_second_concurrent_download_of_the_same_spec_is_busy() {
    let body = content(1000, 15);
    let (spec, bodies) = make_spec("busy", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mut fetcher = FakeFetcher::new(bodies, RangeMode::Honour);
    fetcher.gate = Some(FetchGate { entered: Mutex::new(entered_tx), release: Mutex::new(release_rx) });
    let fetcher = Arc::new(fetcher);
    let root_path = root.path().to_path_buf();
    let worker_fetcher = Arc::clone(&fetcher);
    let worker = std::thread::spawn(move || run(&*worker_fetcher, &root_path, spec, &AtomicBool::new(false)));

    entered_rx.recv().unwrap_or_else(|err| panic!("worker never fetched: {err}"));
    let second = run(&FakeFetcher::new(BTreeMap::new(), RangeMode::Honour), root.path(), spec, &AtomicBool::new(false));
    assert!(matches!(second, Err(ExternalModelError::Busy { id: "busy" })), "{second:?}");
    release_tx.send(()).unwrap_or_else(|err| panic!("{err}"));
    let first = worker.join().unwrap_or_else(|_| panic!("worker panicked"));
    assert!(first.is_ok(), "{first:?}");
    // The guard is released afterwards: a re-verify of the installed spec succeeds.
    let calls_before = fetcher.calls().len();
    let fetcher_after = FakeFetcher::new(BTreeMap::new(), RangeMode::Honour);
    assert!(run(&fetcher_after, root.path(), spec, &AtomicBool::new(false)).is_ok());
    assert!(fetcher_after.calls().is_empty());
    assert_eq!(fetcher.calls().len(), calls_before);
}

#[test]
fn invalid_specs_are_rejected_before_any_io() {
    let good = ExternalFile { path: "a.bin", size: 1, sha256: leak("0".repeat(64)) };
    let cases: Vec<(&str, &str, &str, ExternalFile)> = vec![
        ("bad-rev", "main", "Dir", good),
        ("upper-rev", leak("A".repeat(40)), "Dir", good),
        ("bad-sha", REV_A, "Dir", ExternalFile { sha256: "abc", ..good }),
        ("dotdot", REV_A, "Dir", ExternalFile { path: "../evil", ..good }),
        ("absolute", REV_A, "Dir", ExternalFile { path: "/etc/passwd", ..good }),
        ("backslash", REV_A, "Dir", ExternalFile { path: "a\\b", ..good }),
        ("drive", REV_A, "Dir", ExternalFile { path: "C:evil", ..good }),
        ("reserved", REV_A, "Dir", ExternalFile { path: ".download/x", ..good }),
        ("bad-dir", REV_A, "../Dir", good),
    ];
    let root = temp_root();
    let fetcher = FakeFetcher::new(BTreeMap::new(), RangeMode::Honour);
    for (id, revision, dir, file) in cases {
        let spec = Box::leak(Box::new(ExternalModelSpec { id, repo_id: "owner/repo", revision, dir, files: Box::leak(Box::new([file])) }));
        let result = run(&fetcher, root.path(), spec, &AtomicBool::new(false));
        assert!(matches!(result, Err(ExternalModelError::InvalidSpec(_))), "{id}: {result:?}");
    }
    let duplicate = Box::leak(Box::new(ExternalModelSpec { id: "dup", repo_id: "owner/repo", revision: REV_A, dir: "Dir", files: Box::leak(Box::new([good, good])) }));
    assert!(matches!(validate_spec(duplicate), Err(ExternalModelError::InvalidSpec(_))));
    let bad_repo = Box::leak(Box::new(ExternalModelSpec { id: "repo", repo_id: "no-slash", revision: REV_A, dir: "Dir", files: Box::leak(Box::new([good])) }));
    assert!(matches!(validate_spec(bad_repo), Err(ExternalModelError::InvalidSpec(_))));
    assert!(fetcher.calls().is_empty());
    let entries = std::fs::read_dir(root.path()).map(Iterator::count).unwrap_or(usize::MAX);
    assert_eq!(entries, 0, "an invalid spec must not touch the filesystem");
}

#[test]
fn status_requires_exact_sizes_besides_the_marker() {
    let body = content(2000, 16);
    let (spec, bodies) = make_spec("statussize", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    let dir = run(&FakeFetcher::new(bodies, RangeMode::Honour), root.path(), spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(dir.join("model.bin"), b"truncated").unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Missing);
    assert!(matches!(installed_dir(root.path(), spec), Err(ExternalModelError::NotDownloaded { .. })));
}

#[test]
fn progress_is_monotonic_within_a_phase() {
    let (spec, bodies) = make_spec("progress", REV_A, &[("a.bin", &content(400 * 1024, 17)), ("b.bin", &content(10, 18))]);
    let root = temp_root();
    let reports = AtomicUsize::new(0);
    let mut last_total = 0_u64;
    let result = download_with(&FakeFetcher::new(bodies, RangeMode::Honour), &url_of, root.path(), spec, &AtomicBool::new(false), &quiet_policy(), &mut |progress| {
        reports.fetch_add(1, Ordering::Relaxed);
        assert!(progress.total_done >= last_total);
        assert!(progress.file_done <= progress.file_total);
        assert_eq!(progress.total_bytes, spec.total_bytes());
        last_total = progress.total_done;
    });
    assert!(result.is_ok(), "{result:?}");
    assert!(reports.load(Ordering::Relaxed) >= 5);
}

/// One scripted answer of [`ScriptedFetcher`]; once the script is empty it serves normally.
#[derive(Clone, Copy)]
enum Step {
    /// Serve `n` bytes from the requested offset, then fail the body read (connection reset).
    Drop(usize),
    /// Serve `n` bytes from the requested offset, then end the body cleanly (truncated
    /// chunked response).
    Short(usize),
    /// Answer with an HTTP error status and an optional `Retry-After`.
    Status(u16, Option<&'static str>),
}

/// Single-body server that honours ranges and replays a script of failures.
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

impl RangeFetcher for ScriptedFetcher {
    fn fetch(&self, _url: &str, offset: u64) -> Result<FetchResponse, FetchError> {
        self.offsets.lock().map_err(permanent)?.push(offset);
        let step = self.script.lock().map_err(permanent)?.pop_front();
        let start = usize::try_from(offset).map_err(permanent)?;
        let rest = self.body.get(start..).ok_or_else(|| classify_status(416, None))?;
        let status = if offset > 0 { FetchStatus::Partial } else { FetchStatus::Full };
        let body: Box<dyn Read + Send> = match step {
            None => Box::new(Cursor::new(rest.to_vec())),
            Some(Step::Drop(len)) => Box::new(Cursor::new(rest[..len.min(rest.len())].to_vec()).chain(ResetReader)),
            Some(Step::Short(len)) => Box::new(Cursor::new(rest[..len.min(rest.len())].to_vec())),
            Some(Step::Status(code, retry_after)) => return Err(classify_status(code, retry_after)),
        };
        Ok(FetchResponse { status, body })
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

/// Runs a scripted single-file download with a recording sleep; returns the result and
/// every `Retrying` phase reported.
fn run_scripted(
    fetcher: &ScriptedFetcher,
    root: &Path,
    spec: &'static ExternalModelSpec,
    cancel: &AtomicBool,
    sleeps: &SleepLog<'_>,
) -> (Result<PathBuf, ExternalModelError>, Vec<(ExternalDownloadPhase, u64)>) {
    let sleep = |slice: Duration| sleeps.sleep(slice);
    let policy = RetryPolicy { sleep: &sleep, ..RetryPolicy::standard() };
    let mut retries = Vec::new();
    let result = download_with(fetcher, &url_of, root, spec, cancel, &policy, &mut |progress| {
        if matches!(progress.phase, ExternalDownloadPhase::Retrying { .. }) {
            retries.push((progress.phase, progress.file_done));
        }
    });
    (result, retries)
}

#[test]
fn a_dropped_connection_resumes_from_the_staged_length() {
    let body = content(300 * 1024, 30);
    let (spec, _) = make_spec("retrydrop", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    let fetcher = ScriptedFetcher::new(&body, &[Step::Drop(100_000)]);
    let sleeps = SleepLog::new(None);
    let (result, retries) = run_scripted(&fetcher, root.path(), spec, &AtomicBool::new(false), &sleeps);
    let dir = result.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fetcher.offsets(), vec![0, 100_000]);
    assert_eq!(read(&dir.join("model.bin")), body);
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Installed);
    // First retry waits 2 s in 100 ms slices, counting the seconds down to zero.
    assert_eq!(sleeps.total(), Duration::from_secs(2));
    assert_eq!(sleeps.count(), 20);
    let phases: Vec<ExternalDownloadPhase> = retries.iter().map(|(phase, _)| *phase).collect();
    assert_eq!(
        phases,
        [2, 1, 0].map(|delay_secs| ExternalDownloadPhase::Retrying { attempt: 1, max_attempts: 5, delay_secs }).to_vec()
    );
    assert!(retries.iter().all(|(_, staged)| *staged == 100_000));
}

#[test]
fn a_short_body_keeps_the_part_and_resumes() {
    let body = content(5000, 31);
    let (spec, _) = make_spec("retryshort", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    // Two truncated bodies in a row, then a complete one.
    let fetcher = ScriptedFetcher::new(&body, &[Step::Short(1000), Step::Short(1500)]);
    let sleeps = SleepLog::new(None);
    let (result, _) = run_scripted(&fetcher, root.path(), spec, &AtomicBool::new(false), &sleeps);
    let dir = result.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fetcher.offsets(), vec![0, 1000, 2500]);
    assert_eq!(read(&dir.join("model.bin")), body);
}

#[test]
fn a_503_is_retried_and_retry_after_is_honoured() {
    let body = content(3000, 32);
    let (spec, _) = make_spec("retry503", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    let fetcher = ScriptedFetcher::new(&body, &[Step::Status(503, None), Step::Status(429, Some("7"))]);
    let sleeps = SleepLog::new(None);
    let (result, _) = run_scripted(&fetcher, root.path(), spec, &AtomicBool::new(false), &sleeps);
    let dir = result.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fetcher.offsets(), vec![0, 0, 0]);
    assert_eq!(read(&dir.join("model.bin")), body);
    // 2 s backoff, then the server's 7 s (longer than the 4 s backoff).
    assert_eq!(sleeps.total(), Duration::from_secs(9));
}

#[test]
fn a_404_is_not_retried() {
    let body = content(3000, 33);
    let (spec, _) = make_spec("retry404", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    let fetcher = ScriptedFetcher::new(&body, &[Step::Status(404, None)]);
    let sleeps = SleepLog::new(None);
    let (result, retries) = run_scripted(&fetcher, root.path(), spec, &AtomicBool::new(false), &sleeps);
    assert!(matches!(result, Err(ExternalModelError::Http { file: "model.bin", .. })), "{result:?}");
    assert_eq!(fetcher.offsets(), vec![0]);
    assert_eq!(sleeps.count(), 0);
    assert!(retries.is_empty());
}

#[test]
fn exhausted_retries_fail_with_the_part_kept_and_partial_status() {
    let body = content(5000, 34);
    let (spec, _) = make_spec("retryexhaust", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    let mut script = vec![Step::Short(1000)];
    script.extend([Step::Status(503, None); 6]);
    let fetcher = ScriptedFetcher::new(&body, &script);
    let sleeps = SleepLog::new(None);
    let (result, retries) = run_scripted(&fetcher, root.path(), spec, &AtomicBool::new(false), &sleeps);
    assert!(matches!(&result, Err(ExternalModelError::Http { detail, .. }) if detail.contains("503")), "{result:?}");
    // The short body counts as failure 1, then four 503s are retried and the fifth gives up.
    assert_eq!(fetcher.offsets(), vec![0, 1000, 1000, 1000, 1000, 1000]);
    assert_eq!(sleeps.total(), Duration::from_secs(2 + 4 + 8 + 16 + 30));
    let attempts: Vec<u32> = retries
        .iter()
        .filter_map(|(phase, _)| match phase {
            ExternalDownloadPhase::Retrying { attempt, delay_secs: 0, .. } => Some(*attempt),
            _ => None,
        })
        .collect();
    assert_eq!(attempts, vec![1, 2, 3, 4, 5]);
    assert_eq!(file_len(&staged_part(root.path(), spec, &spec.files[0])), Some(1000));
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Partial { bytes_present: 1000 });
}

#[test]
fn progress_resets_the_retry_counter() {
    let body = content(20_000, 35);
    let (spec, _) = make_spec("retryreset", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    // Eight drops, each after new bytes: more than `max_retries`, yet never in a row.
    let fetcher = ScriptedFetcher::new(&body, &[Step::Drop(1000); 8]);
    let sleeps = SleepLog::new(None);
    let (result, retries) = run_scripted(&fetcher, root.path(), spec, &AtomicBool::new(false), &sleeps);
    let dir = result.unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(read(&dir.join("model.bin")), body);
    assert_eq!(fetcher.offsets(), (0..=8).map(|drop| drop * 1000).collect::<Vec<u64>>());
    assert!(retries.iter().all(|(phase, _)| matches!(phase, ExternalDownloadPhase::Retrying { attempt: 1, .. })));
    assert_eq!(sleeps.total(), Duration::from_secs(2 * 8));
}

#[test]
fn cancel_during_the_backoff_wait_returns_promptly() {
    let body = content(3000, 36);
    let (spec, _) = make_spec("retrycancel", REV_A, &[("model.bin", &body)]);
    let root = temp_root();
    let fetcher = ScriptedFetcher::new(&body, &[Step::Drop(500)]);
    let cancel = AtomicBool::new(false);
    let sleeps = SleepLog::new(Some(&cancel));
    let (result, _) = run_scripted(&fetcher, root.path(), spec, &cancel, &sleeps);
    assert!(matches!(result, Err(ExternalModelError::Cancelled)), "{result:?}");
    // The flag is seen after the first 100 ms slice, not after the whole 2 s wait.
    assert_eq!(sleeps.count(), 1);
    assert_eq!(fetcher.offsets(), vec![0]);
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Partial { bytes_present: 500 });
}

#[test]
fn status_classification_and_backoff_schedule() {
    for code in [408, 429, 500, 502, 503, 504, 599] {
        assert!(matches!(classify_status(code, None), FetchError::Transient { retry_after: None, .. }), "{code}");
    }
    for code in [400, 401, 403, 404, 410, 416, 451] {
        assert!(matches!(classify_status(code, Some("5")), FetchError::Permanent { .. }), "{code}");
    }
    assert_eq!(classify_status(503, Some(" 12 ")), FetchError::Transient { detail: "HTTP status 503".to_owned(), retry_after: Some(Duration::from_secs(12)) });
    // An HTTP-date is not parsed; the backoff applies.
    assert!(matches!(classify_status(503, Some("Wed, 21 Oct 2015 07:28:00 GMT")), FetchError::Transient { retry_after: None, .. }));

    let policy = quiet_policy();
    let delays: Vec<u64> = (1..=7).map(|attempt| policy.delay_for(attempt, None).as_secs()).collect();
    assert_eq!(delays, vec![2, 4, 8, 16, 30, 30, 30]);
    assert_eq!(policy.delay_for(u32::MAX, None), Duration::from_secs(30));
    assert_eq!(policy.delay_for(1, Some(Duration::from_secs(1))), Duration::from_secs(2));
    assert_eq!(policy.delay_for(1, Some(Duration::from_secs(3600))), Duration::from_secs(60));
}

/// Opt-in real download of the pinned Baberu files (~243 MB) into a temp dir.
#[test]
#[ignore = "network: set MS_TEST_EXTERNAL_DOWNLOAD=1 and run with --ignored"]
fn real_baberu_download_when_opted_in() {
    if std::env::var("MS_TEST_EXTERNAL_DOWNLOAD").as_deref() != Ok("1") {
        eprintln!("MS_TEST_EXTERNAL_DOWNLOAD!=1, skipping the network download test");
        return;
    }
    let spec = &crate::ai_models::external_catalog::BABERU_OCR;
    let root = temp_root();
    let dir = download_external_model(root.path(), spec, &AtomicBool::new(false), &mut |_| {}).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Installed);
    assert_eq!(installed_dir(root.path(), spec).ok(), Some(dir));
}
