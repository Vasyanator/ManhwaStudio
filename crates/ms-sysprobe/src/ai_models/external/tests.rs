/*
File: crates/ms-sysprobe/src/ai_models/external/tests.rs

Purpose:
Unit tests of the external-model downloader core (`download_with`), its transient-failure
retry, and the stat-only status probe, driven by in-memory `RangeFetcher`s (one plain, one
scripted with drops / short bodies / error statuses) and an injected backoff sleep that only
records — plus the repair path `verify_with` (shared-file corruption invalidating every
spec that lists it) and adoption of present-but-unmarked files (`unmarked_files_present`,
`adopt_external_model`; a fetcher that panics proves no request is made) — no network, no real waiting, scratch data only under a unique `tempfile`
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
            return Ok(FetchResponse { status: FetchStatus::Partial, total_len: Some(total_len(body)), identity: None, body: Box::new(Cursor::new(suffix)) });
        }
        Ok(FetchResponse { status: FetchStatus::Full, total_len: Some(total_len(body)), identity: None, body: Box::new(Cursor::new(body.clone())) })
    }
}

/// A fake-server internal failure as a permanent fetch error.
fn permanent(err: impl std::fmt::Display) -> FetchError {
    FetchError::Permanent { detail: err.to_string() }
}

/// The whole-file size a fake server announces.
fn total_len(body: &[u8]) -> u64 {
    u64::try_from(body.len()).unwrap_or(u64::MAX)
}

fn url_of(file: &ExternalFile) -> String {
    format!("mem://{}", file.remote_path())
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
        .map(|(path, body)| ExternalFile::direct(leak((*path).to_owned()), u64::try_from(body.len()).unwrap_or(u64::MAX), leak(sha_hex(body))))
        .collect();
    let bodies = entries.iter().zip(files).map(|(file, (_, body))| (url_of(file), body.to_vec())).collect();
    let spec = Box::leak(Box::new(ExternalModelSpec {
        id: leak(id.to_owned()),
        source: ExternalSource::HuggingFace { repo_id: "owner/repo", revision: leak(revision.to_owned()) },
        dir: leak(format!("Test/{id}")),
        layout: DirLayout::Exclusive,
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
    let good = ExternalFile::direct("a.bin", 1, leak("0".repeat(64)));
    let cases: Vec<(&str, &str, &str, ExternalFile)> = vec![
        ("bad-rev", "main", "Dir", good),
        ("upper-rev", leak("A".repeat(40)), "Dir", good),
        ("bad-sha", REV_A, "Dir", ExternalFile { sha256: "abc", ..good }),
        ("dotdot", REV_A, "Dir", ExternalFile { path: "../evil", ..good }),
        ("absolute", REV_A, "Dir", ExternalFile { path: "/etc/passwd", ..good }),
        ("backslash", REV_A, "Dir", ExternalFile { path: "a\\b", ..good }),
        ("drive", REV_A, "Dir", ExternalFile { path: "C:evil", ..good }),
        ("reserved", REV_A, "Dir", ExternalFile { path: ".download/x", ..good }),
        ("marker", REV_A, "Dir", ExternalFile { path: ".ms_model_complete.other.json", ..good }),
        ("bad-dir", REV_A, "../Dir", good),
    ];
    let root = temp_root();
    let fetcher = FakeFetcher::new(BTreeMap::new(), RangeMode::Honour);
    let hf = |revision: &'static str| ExternalSource::HuggingFace { repo_id: "owner/repo", revision };
    let leak_spec = |id: &'static str, source: ExternalSource, layout: DirLayout, files: &[ExternalFile]| -> &'static ExternalModelSpec {
        Box::leak(Box::new(ExternalModelSpec { id, source, dir: "Dir", layout, files: Box::leak(files.to_vec().into_boxed_slice()) }))
    };
    for (id, revision, dir, file) in cases {
        let spec = Box::leak(Box::new(ExternalModelSpec { id, source: hf(revision), dir, layout: DirLayout::Exclusive, files: Box::leak(Box::new([file])) }));
        let result = run(&fetcher, root.path(), spec, &AtomicBool::new(false));
        assert!(matches!(result, Err(ExternalModelError::InvalidSpec(_))), "{id}: {result:?}");
    }
    let duplicate = leak_spec("dup", hf(REV_A), DirLayout::Exclusive, &[good, good]);
    assert!(matches!(validate_spec(duplicate), Err(ExternalModelError::InvalidSpec(_))));
    let bad_repo = leak_spec("repo", ExternalSource::HuggingFace { repo_id: "no-slash", revision: REV_A }, DirLayout::Exclusive, &[good]);
    assert!(matches!(validate_spec(bad_repo), Err(ExternalModelError::InvalidSpec(_))));
    for base_url in ["http://h.example/x", "https://", "https://h.example/x/", "https://h.example/x?y=1", "https://h.example/../x", "https://u@h.example", "https://h.example/a b"] {
        let spec = leak_spec("url", ExternalSource::Url { base_url }, DirLayout::Exclusive, &[good]);
        assert!(matches!(validate_spec(spec), Err(ExternalModelError::InvalidSpec(_))), "{base_url}");
    }
    let fine_url = leak_spec("url_ok", ExternalSource::Url { base_url: "https://h.example:8443/a/b" }, DirLayout::Exclusive, &[good]);
    assert!(validate_spec(fine_url).is_ok());
    let shared_bad_id = leak_spec("Bad-Id", hf(REV_A), DirLayout::Shared, &[good]);
    assert!(matches!(validate_spec(shared_bad_id), Err(ExternalModelError::InvalidSpec(_))));
    let member = |archive: &'static str, archive_size: u64, archive_sha256: &'static str, member: &'static str| ExternalFile {
        origin: FileOrigin::ZipMember { archive, archive_size, archive_sha256, member },
        ..good
    };
    let sha = leak("1".repeat(64));
    for file in [member("../a.zip", 1, sha, "a"), member("a.zip", 0, sha, "a"), member("a.zip", 1, "abc", "a"), member("a.zip", 1, sha, ""), member(".download/a.zip", 1, sha, "a")] {
        let spec = leak_spec("zip", hf(REV_A), DirLayout::Exclusive, &[file]);
        assert!(matches!(validate_spec(spec), Err(ExternalModelError::InvalidSpec(_))), "{file:?}");
    }
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
        Ok(FetchResponse { status, total_len: Some(total_len(&self.body)), identity: None, body })
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

/// CRC-32 (IEEE) of `bytes`, for the hand-built test archives.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A minimal single-member STORED zip archive (no dependency on a zip writer).
fn stored_zip(name: &str, data: &[u8]) -> Vec<u8> {
    let crc = crc32(data);
    let size = u32::try_from(data.len()).unwrap_or(u32::MAX);
    let name_len = u16::try_from(name.len()).unwrap_or(u16::MAX);
    let mut out = Vec::new();
    // Local file header.
    out.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
    out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // version, flags, method=stored, time, date
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&name_len.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(data);
    let central_offset = u32::try_from(out.len()).unwrap_or(u32::MAX);
    // Central directory entry.
    out.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
    out.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // made by, needed, flags, method, time, date
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&name_len.to_le_bytes());
    out.extend_from_slice(&[0; 12]); // extra, comment, disk, internal attrs, external attrs
    out.extend_from_slice(&0_u32.to_le_bytes()); // local header offset
    out.extend_from_slice(name.as_bytes());
    let central_size = u32::try_from(out.len()).unwrap_or(u32::MAX) - central_offset;
    // End of central directory.
    out.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0]);
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out
}

/// A zip-member file pinned to `data` inside the archive `archive_body` served at `archive`.
fn member_file(path: &str, data: &[u8], archive: &str, archive_body: &[u8]) -> ExternalFile {
    ExternalFile {
        path: leak(path.to_owned()),
        size: u64::try_from(data.len()).unwrap_or(u64::MAX),
        sha256: leak(sha_hex(data)),
        origin: FileOrigin::ZipMember {
            archive: leak(archive.to_owned()),
            archive_size: u64::try_from(archive_body.len()).unwrap_or(u64::MAX),
            archive_sha256: leak(sha_hex(archive_body)),
            member: leak(path.to_owned()),
        },
    }
}

/// A leaked `Url`-source spec in the shared directory `dir`.
fn shared_spec(id: &str, dir: &str, files: Vec<ExternalFile>) -> &'static ExternalModelSpec {
    Box::leak(Box::new(ExternalModelSpec {
        id: leak(id.to_owned()),
        source: ExternalSource::Url { base_url: "https://models.example/releases" },
        dir: leak(dir.to_owned()),
        layout: DirLayout::Shared,
        files: Box::leak(files.into_boxed_slice()),
    }))
}

#[test]
fn zip_members_are_fetched_verified_extracted_and_their_archives_removed() {
    let detector = content(9000, 50);
    let recognizer = content(7000, 51);
    let detector_zip = stored_zip("det.pth", &detector);
    let recognizer_zip = stored_zip("rec_a.pth", &recognizer);
    let files = vec![member_file("det.pth", &detector, "v1/det.zip", &detector_zip), member_file("rec_a.pth", &recognizer, "v2/rec_a.zip", &recognizer_zip)];
    let spec = shared_spec("zip_flow", "Shared/zipflow", files);
    let bodies = BTreeMap::from([("mem://v1/det.zip".to_owned(), detector_zip.clone()), ("mem://v2/rec_a.zip".to_owned(), recognizer_zip.clone())]);
    let root = temp_root();
    let fetcher = FakeFetcher::new(bodies, RangeMode::Honour);
    let mut last = None;
    let dir = download_with(&fetcher, &url_of, root.path(), spec, &AtomicBool::new(false), &quiet_policy(), &mut |progress| {
        assert!(progress.file_done <= progress.file_total);
        last = Some(*progress);
    })
    .unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(read(&dir.join("det.pth")), detector);
    assert_eq!(read(&dir.join("rec_a.pth")), recognizer);
    assert!(dir.join(".ms_model_complete.zip_flow.json").is_file());
    assert!(!dir.join(COMPLETE_MARKER).exists(), "a shared directory never writes the exclusive marker");
    assert!(!dir.join(STAGING_DIR).exists(), "archives and staging are removed");
    assert_eq!(spec.total_bytes(), total_len(&detector_zip) + total_len(&recognizer_zip));
    assert_eq!(last.map(|progress| progress.total_done), Some(spec.total_bytes()));
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Installed);
}

#[test]
fn a_corrupt_archive_is_rejected_and_nothing_is_published() {
    let data = content(5000, 52);
    let archive = stored_zip("w.pth", &data);
    let file = member_file("w.pth", &data, "w.zip", &archive);
    let spec = shared_spec("zip_corrupt", "Shared/zipcorrupt", vec![file]);
    let mut served = archive.clone();
    served[40] ^= 0xFF;
    let root = temp_root();
    let fetcher = FakeFetcher::new(BTreeMap::from([("mem://w.zip".to_owned(), served)]), RangeMode::Honour);
    let result = run(&fetcher, root.path(), spec, &AtomicBool::new(false));
    assert!(matches!(result, Err(ExternalModelError::HashMismatch { expected, .. }) if expected == sha_hex(&archive)), "{result:?}");
    assert!(!spec.local_dir(root.path()).join("w.pth").exists());
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Missing);
}

#[test]
fn specs_sharing_a_directory_keep_independent_status_and_share_the_common_file() {
    let detector = content(8000, 53);
    let rec_a = content(3000, 54);
    let rec_b = content(3500, 55);
    let (det_zip, a_zip, b_zip) = (stored_zip("det.pth", &detector), stored_zip("a.pth", &rec_a), stored_zip("b.pth", &rec_b));
    let det = member_file("det.pth", &detector, "det.zip", &det_zip);
    let spec_a = shared_spec("shared_a", "Shared/common", vec![det, member_file("a.pth", &rec_a, "a.zip", &a_zip)]);
    let spec_b = shared_spec("shared_b", "Shared/common", vec![det, member_file("b.pth", &rec_b, "b.zip", &b_zip)]);
    let bodies = BTreeMap::from([("mem://det.zip".to_owned(), det_zip), ("mem://a.zip".to_owned(), a_zip), ("mem://b.zip".to_owned(), b_zip)]);
    let root = temp_root();
    let fetcher = FakeFetcher::new(bodies, RangeMode::Honour);

    run(&fetcher, root.path(), spec_a, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(external_model_status(root.path(), spec_a), ExternalModelStatus::Installed);
    assert_eq!(external_model_status(root.path(), spec_b), ExternalModelStatus::Missing);

    let before = fetcher.calls().len();
    let dir = run(&fetcher, root.path(), spec_b, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    let fetched: Vec<String> = fetcher.calls()[before..].iter().map(|(url, _)| url.clone()).collect();
    assert_eq!(fetched, vec!["mem://b.zip".to_owned()], "the shared detector is found verified, not downloaded again");
    // Installing B neither retired A's marker nor deleted A's recognizer.
    assert_eq!(external_model_status(root.path(), spec_a), ExternalModelStatus::Installed);
    assert_eq!(external_model_status(root.path(), spec_b), ExternalModelStatus::Installed);
    assert_eq!(read(&dir.join("a.pth")), rec_a);
    assert_eq!(read(&dir.join("det.pth")), detector);

    // A corrupted recognizer of A breaks only A.
    std::fs::write(dir.join("a.pth"), b"x").unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(external_model_status(root.path(), spec_a), ExternalModelStatus::Missing);
    assert_eq!(external_model_status(root.path(), spec_b), ExternalModelStatus::Installed);
}

/// The completion marker exactly as releases before `Url` sources / zip members wrote it for
/// a Hugging Face spec (`serde_json::to_vec_pretty` of `{id, repo_id, revision, files}`).
const LEGACY_MARKER: &str = r#"{
  "id": "legacy",
  "repo_id": "owner/repo",
  "revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "files": [
    {
      "path": "model.bin",
      "size": 3,
      "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    }
  ]
}"#;

#[test]
fn hugging_face_markers_stay_byte_identical_and_old_installs_read_installed() {
    let (spec, bodies) = make_spec("legacy", REV_A, &[("model.bin", b"abc")]);
    assert_eq!(serde_json::to_string_pretty(&Marker::for_spec(spec)).ok().as_deref(), Some(LEGACY_MARKER));

    // An install written by an older build: the files plus today's marker bytes.
    let root = temp_root();
    let dir = spec.local_dir(root.path());
    std::fs::create_dir_all(&dir).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(dir.join("model.bin"), b"abc").unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(dir.join(COMPLETE_MARKER), LEGACY_MARKER).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Installed);

    // And a fresh download writes exactly those bytes.
    let fresh = temp_root();
    let dir = run(&FakeFetcher::new(bodies, RangeMode::Honour), fresh.path(), spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(std::fs::read_to_string(dir.join(COMPLETE_MARKER)).ok().as_deref(), Some(LEGACY_MARKER));
}

#[test]
fn url_and_zip_pins_are_part_of_the_marker_identity() {
    let data = content(100, 56);
    let archive = stored_zip("w.pth", &data);
    let spec = shared_spec("marker_zip", "Shared/markerzip", vec![member_file("w.pth", &data, "w.zip", &archive)]);
    let json = serde_json::to_value(Marker::for_spec(spec)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(json["url"], "https://models.example/releases");
    assert!(json.get("repo_id").is_none() && json.get("revision").is_none());
    assert_eq!(json["files"][0]["archive"]["path"], "w.zip");
    assert_eq!(json["files"][0]["archive"]["member"], "w.pth");
}

/// Runs `verify_with` with `related` as the catalog.
fn verify(root: &Path, spec: &'static ExternalModelSpec, related: &[&'static ExternalModelSpec]) -> Result<VerifyReport, ExternalModelError> {
    verify_with(root, spec, related, &AtomicBool::new(false), &quiet_policy(), &mut |progress| assert!(progress.file_done <= progress.file_total))
}

#[test]
fn verify_repairs_a_same_size_corruption_of_a_shared_file_for_every_spec() {
    let detector = content(6000, 60);
    let (rec_a, rec_b) = (content(2000, 61), content(2100, 62));
    let (det_zip, a_zip, b_zip) = (stored_zip("det.pth", &detector), stored_zip("a.pth", &rec_a), stored_zip("b.pth", &rec_b));
    let det = member_file("det.pth", &detector, "det.zip", &det_zip);
    let spec_a = shared_spec("verify_a", "Shared/verify", vec![det, member_file("a.pth", &rec_a, "a.zip", &a_zip)]);
    let spec_b = shared_spec("verify_b", "Shared/verify", vec![det, member_file("b.pth", &rec_b, "b.zip", &b_zip)]);
    let bodies = BTreeMap::from([("mem://det.zip".to_owned(), det_zip), ("mem://a.zip".to_owned(), a_zip), ("mem://b.zip".to_owned(), b_zip)]);
    let root = temp_root();
    let fetcher = FakeFetcher::new(bodies, RangeMode::Honour);
    let dir = run(&fetcher, root.path(), spec_a, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    run(&fetcher, root.path(), spec_b, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));

    // Same length, other bytes: the stat-only probe cannot see it.
    std::fs::write(dir.join("det.pth"), content(6000, 99)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(external_model_status(root.path(), spec_a), ExternalModelStatus::Installed);

    let report = verify(root.path(), spec_a, &[spec_a, spec_b]).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(report.removed, vec!["det.pth"]);
    assert_eq!(report.verified, vec!["a.pth"]);
    assert_eq!(report.invalidated, vec!["verify_a", "verify_b"]);
    assert!(!report.is_intact());
    assert!(!dir.join("det.pth").exists());
    assert_eq!(external_model_status(root.path(), spec_a), ExternalModelStatus::Missing);
    assert_eq!(external_model_status(root.path(), spec_b), ExternalModelStatus::Missing);
    assert_eq!(read(&dir.join("b.pth")), rec_b, "the other spec's own file is untouched");

    // A normal download repairs both.
    run(&fetcher, root.path(), spec_a, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    let before = fetcher.calls().len();
    run(&fetcher, root.path(), spec_b, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fetcher.calls().len(), before, "B finds both of its files verified");
    assert_eq!(read(&dir.join("det.pth")), detector);
    assert_eq!(external_model_status(root.path(), spec_b), ExternalModelStatus::Installed);
    assert!(verify(root.path(), spec_b, &[spec_a, spec_b]).is_ok_and(|report| report.is_intact()));
}

#[test]
fn verify_reports_missing_files_and_keeps_intact_ones() {
    let (spec, bodies) = make_spec("verify_missing", REV_A, &[("a.bin", &content(500, 63)), ("b.bin", &content(600, 64))]);
    let root = temp_root();
    let dir = run(&FakeFetcher::new(bodies, RangeMode::Honour), root.path(), spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    std::fs::remove_file(dir.join("b.bin")).unwrap_or_else(|err| panic!("{err}"));
    let report = verify(root.path(), spec, &[spec]).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!((report.verified, report.missing, report.removed, report.invalidated), (vec!["a.bin"], vec!["b.bin"], Vec::new(), Vec::new()));
    assert_eq!(read(&dir.join("a.bin")), content(500, 63));
}

#[test]
fn verify_removes_a_wrong_size_file_and_its_marker() {
    let (spec, bodies) = make_spec("verify_size", REV_A, &[("a.bin", &content(500, 65))]);
    let root = temp_root();
    let dir = run(&FakeFetcher::new(bodies, RangeMode::Honour), root.path(), spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(dir.join("a.bin"), b"short").unwrap_or_else(|err| panic!("{err}"));
    let report = verify(root.path(), spec, &[spec]).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!((report.removed, report.invalidated), (vec!["a.bin"], vec!["verify_size"]));
    assert!(!dir.join(COMPLETE_MARKER).exists() && !dir.join("a.bin").exists());
}

#[test]
fn verify_is_busy_while_the_spec_downloads_and_honours_cancel() {
    let (spec, bodies) = make_spec("verify_busy", REV_A, &[("a.bin", &content(300_000, 66))]);
    let root = temp_root();
    run(&FakeFetcher::new(bodies, RangeMode::Honour), root.path(), spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    let held = BusyGuard::try_acquire(spec.id).unwrap_or_else(|_| panic!("free id"));
    assert!(matches!(verify(root.path(), spec, &[spec]), Err(ExternalModelError::Busy { .. })));
    drop(held);
    let cancel = AtomicBool::new(true);
    let result = verify_with(root.path(), spec, &[spec], &cancel, &quiet_policy(), &mut |_| {});
    assert!(matches!(result, Err(ExternalModelError::Cancelled)), "{result:?}");
    assert!(spec.local_dir(root.path()).join("a.bin").exists(), "cancel deletes nothing");
}

/// A server that must never be asked: adoption works on local files only, and a download
/// right after a successful adoption keeps every file without a request.
struct NoNetworkFetcher;

impl RangeFetcher for NoNetworkFetcher {
    fn fetch(&self, url: &str, offset: u64) -> Result<FetchResponse, FetchError> {
        panic!("adoption must not touch the network: fetch {url} at {offset}")
    }
}

/// Writes every file of `spec` with the given bodies at its catalog path, as an older
/// downloader would have left them (no marker, no staging).
fn place_files(root: &Path, spec: &ExternalModelSpec, bodies: &BTreeMap<String, Vec<u8>>) {
    let dir = spec.local_dir(root);
    for file in spec.files {
        let path = join_relative(&dir, file.path);
        std::fs::create_dir_all(path.parent().unwrap_or(&dir)).unwrap_or_else(|err| panic!("{err}"));
        let body = bodies.get(&url_of(file)).unwrap_or_else(|| panic!("no body for {}", file.path));
        std::fs::write(&path, body).unwrap_or_else(|err| panic!("{err}"));
    }
}

#[test]
fn adoption_of_present_unmarked_files_writes_the_marker_without_network() {
    let (spec, bodies) = make_spec("adopt_ok", REV_A, &[("transformer/w.safetensors", &content(300_000, 70)), ("config.json", &content(40, 71))]);
    let root = temp_root();
    assert!(!unmarked_files_present(root.path(), spec), "nothing on disk");
    place_files(root.path(), spec, &bodies);
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Missing);
    assert!(unmarked_files_present(root.path(), spec));

    let mut reports = Vec::new();
    let dir = adopt_external_model(root.path(), spec, &AtomicBool::new(false), &mut |progress| reports.push(*progress))
        .unwrap_or_else(|err| panic!("adoption failed: {err}"));
    assert_eq!(dir, spec.local_dir(root.path()));
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Installed);
    assert!(!unmarked_files_present(root.path(), spec), "an installed spec is not adoptable");
    assert!(!dir.join(STAGING_DIR).exists(), "adoption creates no staging");
    assert!(reports.iter().all(|progress| progress.phase == ExternalDownloadPhase::Adopting && progress.file_done <= progress.file_total));
    assert_eq!(reports.last().map(|progress| progress.total_done), Some(spec.total_bytes()));

    // Already installed: a second adoption hashes nothing; a download keeps every file without
    // a single request.
    let mut again = 0_usize;
    adopt_external_model(root.path(), spec, &AtomicBool::new(false), &mut |_| again += 1).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(again, 0);
    run(&NoNetworkFetcher, root.path(), spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Installed);
}

#[test]
fn a_same_size_mismatch_refuses_adoption_deletes_nothing_and_a_download_repairs_only_that_file() {
    let good = content(200_000, 72);
    let other = content(5000, 73);
    let (spec, bodies) = make_spec("adopt_mismatch", REV_A, &[("a.bin", &good), ("b.bin", &other)]);
    let root = temp_root();
    place_files(root.path(), spec, &bodies);
    let dir = spec.local_dir(root.path());
    let mut damaged = good.clone();
    damaged[1234] ^= 0x5A;
    std::fs::write(dir.join("a.bin"), &damaged).unwrap_or_else(|err| panic!("{err}"));
    assert!(unmarked_files_present(root.path(), spec), "same size: the stat-only probe cannot tell");

    let result = adopt_external_model(root.path(), spec, &AtomicBool::new(false), &mut |_| {});
    assert!(matches!(&result, Err(ExternalModelError::HashMismatch { file: "a.bin", expected, actual }) if *expected == sha_hex(&good) && *actual == sha_hex(&damaged)), "{result:?}");
    assert_eq!(read(&dir.join("a.bin")), damaged, "the mismatching file is kept as it is");
    assert_eq!(read(&dir.join("b.bin")), other);
    assert!(!dir.join(COMPLETE_MARKER).exists());
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Missing);

    let fetcher = FakeFetcher::new(bodies, RangeMode::Honour);
    run(&fetcher, root.path(), spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fetcher.calls(), vec![(url_of(&spec.files[0]), 0)], "only the differing file is downloaded");
    assert_eq!(read(&dir.join("a.bin")), good);
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Installed);
}

#[test]
fn absent_or_wrong_size_files_are_not_adoptable_and_adoption_reports_them_untouched() {
    let (spec, bodies) = make_spec("adopt_sizes", REV_A, &[("a.bin", &content(3000, 74)), ("b.bin", &content(10, 75))]);
    let root = temp_root();
    place_files(root.path(), spec, &bodies);
    let dir = spec.local_dir(root.path());
    std::fs::write(dir.join("b.bin"), b"short").unwrap_or_else(|err| panic!("{err}"));
    assert!(!unmarked_files_present(root.path(), spec));
    let result = adopt_external_model(root.path(), spec, &AtomicBool::new(false), &mut |_| panic!("no hashing before the size check"));
    assert!(matches!(result, Err(ExternalModelError::SizeMismatch { file: "b.bin", expected: 10, actual: 5 })), "{result:?}");
    assert_eq!(read(&dir.join("b.bin")), b"short");

    std::fs::remove_file(dir.join("b.bin")).unwrap_or_else(|err| panic!("{err}"));
    let result = adopt_external_model(root.path(), spec, &AtomicBool::new(false), &mut |_| panic!("no hashing before the size check"));
    assert!(matches!(result, Err(ExternalModelError::NotDownloaded { .. })), "{result:?}");
    assert!(dir.join("a.bin").is_file());
    assert!(!dir.join(COMPLETE_MARKER).exists());
}

/// A shared directory with NO marker at all (the shape of the FLUX.2 klein 4B folder the
/// backend used to download) is adoptable and gets its per-spec marker; a marker of ANOTHER
/// identity (an older pin of the same id) is an older install, never adoptable: the adoption
/// refuses and leaves the old marker and the files untouched.
#[test]
fn a_markerless_shared_directory_is_adopted_but_a_foreign_marker_is_never_adopted() {
    let body = content(4000, 76);
    let file = ExternalFile::direct("model.gguf", total_len(&body), leak(sha_hex(&body)));
    let spec = shared_spec("adopt_shared", "Shared/adopt", vec![file]);
    let root = temp_root();
    let dir = spec.local_dir(root.path());
    std::fs::create_dir_all(&dir).unwrap_or_else(|err| panic!("{err}"));
    std::fs::write(dir.join("model.gguf"), &body).unwrap_or_else(|err| panic!("{err}"));
    assert!(unmarked_files_present(root.path(), spec), "no marker at all: adoptable");
    adopt_external_model(root.path(), spec, &AtomicBool::new(false), &mut |_| {}).unwrap_or_else(|err| panic!("{err}"));
    assert!(dir.join(".ms_model_complete.adopt_shared.json").is_file());
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Installed);

    let old_marker = br#"{"id":"adopt_shared","url":"https://old.example","files":[]}"#;
    std::fs::write(spec.marker_path(root.path()), old_marker).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Missing);
    assert!(!unmarked_files_present(root.path(), spec), "a foreign marker is an older install");
    let result = adopt_external_model(root.path(), spec, &AtomicBool::new(false), &mut |_| panic!("no hashing over a foreign marker"));
    assert!(matches!(result, Err(ExternalModelError::NotDownloaded { .. })), "{result:?}");
    assert_eq!(read(&spec.marker_path(root.path())), old_marker.to_vec(), "the old marker is kept for the download's upgrade");
    assert_eq!(read(&dir.join("model.gguf")), body);
}

/// An Exclusive repin with the SAME file sizes: the old install's marker makes the new spec
/// non-adoptable, so the upgrade goes through the download, which removes the file only the
/// old pin listed.
#[test]
fn a_same_size_repin_is_upgraded_by_the_download_never_adopted() {
    let shared = content(3000, 78);
    let (old_spec, old_bodies) = make_spec("repin_adopt", REV_A, &[("shared.json", &shared), ("stale.txt", &content(50, 79))]);
    let root = temp_root();
    run(&FakeFetcher::new(old_bodies, RangeMode::Honour), root.path(), old_spec, &AtomicBool::new(false)).unwrap_or_else(|err| panic!("{err}"));

    let (new_spec, new_bodies) = make_spec("repin_adopt", REV_B, &[("shared.json", &shared)]);
    assert_eq!(external_model_status(root.path(), new_spec), ExternalModelStatus::Missing);
    assert!(!unmarked_files_present(root.path(), new_spec), "same sizes, but an older version is installed");

    let dir = run(&FakeFetcher::new(new_bodies, RangeMode::Honour), root.path(), new_spec, &AtomicBool::new(false))
        .unwrap_or_else(|err| panic!("{err}"));
    assert!(!dir.join("stale.txt").exists(), "the download path cleans what only the old pin listed");
    assert_eq!(external_model_status(root.path(), new_spec), ExternalModelStatus::Installed);
}

#[test]
fn adoption_is_busy_while_the_spec_has_another_job_and_honours_cancel() {
    let (spec, bodies) = make_spec("adopt_busy", REV_A, &[("a.bin", &content(300_000, 77))]);
    let root = temp_root();
    place_files(root.path(), spec, &bodies);
    let held = BusyGuard::try_acquire(spec.id).unwrap_or_else(|_| panic!("free id"));
    let result = adopt_external_model(root.path(), spec, &AtomicBool::new(false), &mut |_| {});
    assert!(matches!(result, Err(ExternalModelError::Busy { .. })), "{result:?}");
    drop(held);
    let result = adopt_external_model(root.path(), spec, &AtomicBool::new(true), &mut |_| {});
    assert!(matches!(result, Err(ExternalModelError::Cancelled)), "{result:?}");
    assert!(spec.local_dir(root.path()).join("a.bin").is_file(), "cancel deletes nothing");
    assert_eq!(external_model_status(root.path(), spec), ExternalModelStatus::Missing);
    assert!(unmarked_files_present(root.path(), spec), "a cancelled adoption stays adoptable");
}
