/*
File: crates/ms-native-package/src/tests.rs

Purpose:
Unit tests of the package transaction over `ms_download::test_support::ScriptedFetcher`: fresh
install (marker bytes, member order, cleanup), the fast path, resume of an interrupted
download, repair of a crash between member renames from the staged archive, a foreign `.part`
discarded, a foreign marker not trusted, the busy wait, cancel, removal, name validation and
the all-or-nothing extraction step.

Notes:
- No network; every file lives under a self-removing temp directory.
*/

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ms_download::test_support::{ScriptedFetcher, Step};
use sha2::Digest;
use zip::write::{SimpleFileOptions, ZipWriter};

use super::*;

const URL: &str = "https://example.invalid/pkg.zip";
const MARKER: &str = ".pkg-complete";
const FORMAT: &str = "test package v1";

/// A stored zip holding `members`.
fn zip_bytes(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, body) in members {
        zip.start_file(*name, opts).expect("start member");
        zip.write_all(body).expect("write member");
    }
    zip.finish().expect("finish zip").into_inner()
}

fn sha_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    sha2::Sha256::digest(bytes).iter().flat_map(|byte| [DIGITS[usize::from(byte >> 4)], DIGITS[usize::from(byte & 0x0f)]]).map(char::from).collect()
}

fn len_u64(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).expect("test sizes fit u64")
}

/// The test archive: `lib/main.so` of `main_len` bytes, `lib/side.so` and an unlisted README.
fn archive(main_len: usize) -> (Vec<u8>, Vec<u8>) {
    let main: Vec<u8> = (0..main_len).map(|index| u8::try_from(index % 251).expect("below 251")).collect();
    (zip_bytes(&[("lib/main.so", main.as_slice()), ("lib/side.so", b"side".as_slice()), ("README", b"skip".as_slice())]), main)
}

/// Retry schedule without real waits (busy waits poll every millisecond).
fn policy(max_retries: u32) -> RetryPolicy<'static> {
    RetryPolicy { max_retries, first_delay: Duration::ZERO, max_delay: Duration::ZERO, retry_after_cap: Duration::ZERO, wait_slice: Duration::from_millis(1), sleep: &std::thread::sleep }
}

const MEMBERS: [PackageMember<'static>; 2] = [PackageMember { archive_path: "lib/side.so", file_name: "side.so" }, PackageMember { archive_path: "lib/main.so", file_name: "main.so.1" }];
const FIELDS: [(&str, &str); 2] = [("name", "test"), ("version", "1.0")];

/// Runs `ensure_installed` (no-wait retry policy, no retries) for a one-source recipe pinned
/// to `body` (or `sha256`) in `dir`.
fn install(fetcher: &ScriptedFetcher, cancel: &AtomicBool, dir: &Path, body: &[u8], sha256: Option<&str>, busy_key: &str, progress: &mut dyn FnMut(PackageProgress)) -> Result<InstallOutcome, PackageError> {
    let sha = sha256.map_or_else(|| sha_hex(body), str::to_owned);
    let sources = [PackageSource { url: URL, file_name: "pkg.zip", size: len_u64(body), sha256: &sha, archive: ArchiveKind::Zip, members: &MEMBERS }];
    let recipe = PackageRecipe { fields: &FIELDS, sources: &sources };
    let retry = policy(0);
    let env = FetchEnv { fetcher, retry: &retry, cancel, log_tag: "[test]", log_context: "pkg=test" };
    ensure_installed(&env, &layout(dir), &recipe, busy_key, progress)
}

fn layout(dir: &Path) -> PackageLayout<'_> {
    PackageLayout { dir, marker_file: MARKER, marker_format: FORMAT }
}

fn installed(dir: &Path, body: &[u8]) -> bool {
    let sha = sha_hex(body);
    let sources = [PackageSource { url: URL, file_name: "pkg.zip", size: len_u64(body), sha256: &sha, archive: ArchiveKind::Zip, members: &MEMBERS }];
    is_installed(&layout(dir), &PackageRecipe { fields: &FIELDS, sources: &sources })
}

fn sorted_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir).expect("list dir").map(|item| item.expect("dir entry").file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

/// A fresh install in a new temp dir with a 1000-byte main library.
fn installed_package(busy_key: &str) -> (tempfile::TempDir, Vec<u8>) {
    let temp = tempfile::tempdir().expect("temp dir");
    let (body, _) = archive(1000);
    install(&ScriptedFetcher::new(&body, &[]), &AtomicBool::new(false), temp.path(), &body, None, busy_key, &mut |_| {}).expect("first install");
    (temp, body)
}

#[test]
fn fresh_install_publishes_members_and_the_marker() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (body, main) = archive(1000);
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let mut stages = Vec::new();
    let outcome = install(&fetcher, &AtomicBool::new(false), temp.path(), &body, None, "pkg:fresh", &mut |update| stages.push(update.stage)).expect("installs");
    assert_eq!(outcome, InstallOutcome::Installed);
    assert_eq!(fs::read(temp.path().join("main.so.1")).expect("main"), main);
    assert_eq!(fs::read(temp.path().join("side.so")).expect("side"), b"side");
    assert_eq!(sorted_names(temp.path()), vec![".download", MARKER, "main.so.1", "side.so"]);
    assert!(sorted_names(&temp.path().join(DOWNLOAD_DIR)).is_empty(), "archive, .part or identity left behind");
    let expected = format!("{FORMAT}\nname test\nversion 1.0\nsource {URL} {}\nmember side.so 4 {}\nmember main.so.1 1000 {}\n", sha_hex(&body), sha_hex(b"side"), sha_hex(&main));
    assert_eq!(fs::read_to_string(temp.path().join(MARKER)).expect("marker"), expected);
    stages.dedup();
    assert_eq!(stages, vec![PackageStage::Probing, PackageStage::Downloading, PackageStage::Extracting, PackageStage::Done]);
    assert!(installed(temp.path(), &body));

    // The fast path: no request at all.
    let offline = ScriptedFetcher::new(&body, &[Step::Status(404, None)]);
    let outcome = install(&offline, &AtomicBool::new(false), temp.path(), &body, None, "pkg:fresh", &mut |_| {}).expect("cached");
    assert_eq!(outcome, InstallOutcome::AlreadyInstalled);
    assert!(offline.offsets().is_empty(), "the fast path makes no request");
}

#[test]
fn two_sources_are_fetched_and_recorded_in_order() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (body, main) = archive(500);
    let sha = sha_hex(&body);
    let extra_members = [PackageMember { archive_path: "README", file_name: "README" }];
    let sources = [
        PackageSource { url: "https://example.invalid/extra.zip", file_name: "extra.zip", size: len_u64(&body), sha256: &sha, archive: ArchiveKind::Zip, members: &extra_members },
        PackageSource { url: URL, file_name: "pkg.zip", size: len_u64(&body), sha256: &sha, archive: ArchiveKind::Zip, members: &MEMBERS },
    ];
    let recipe = PackageRecipe { fields: &FIELDS, sources: &sources };
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let retry = policy(0);
    let cancel = AtomicBool::new(false);
    let env = FetchEnv { fetcher: &fetcher, retry: &retry, cancel: &cancel, log_tag: "[test]", log_context: "pkg=two" };
    ensure_installed(&env, &layout(temp.path()), &recipe, "pkg:two", &mut |_| {}).expect("installs");
    assert_eq!(fetcher.urls(), vec!["https://example.invalid/extra.zip".to_owned(), URL.to_owned()]);
    let expected = format!(
        "{FORMAT}\nname test\nversion 1.0\nsource https://example.invalid/extra.zip {sha}\nsource {URL} {sha}\nmember README 4 {}\nmember side.so 4 {}\nmember main.so.1 500 {}\n",
        sha_hex(b"skip"),
        sha_hex(b"side"),
        sha_hex(&main)
    );
    assert_eq!(fs::read_to_string(temp.path().join(MARKER)).expect("marker"), expected);
    assert!(is_installed(&layout(temp.path()), &recipe));
}

#[test]
fn an_interrupted_download_resumes_with_a_prefix_hash() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (body, _) = archive(4000);
    let err = install(&ScriptedFetcher::new(&body, &[Step::Drop(1500)]), &AtomicBool::new(false), temp.path(), &body, None, "pkg:resume", &mut |_| {}).expect_err("retries exhausted");
    assert!(matches!(&err, PackageError::Download { url, .. } if url == URL), "{err:?}");
    let part = temp.path().join(DOWNLOAD_DIR).join("pkg.zip.part");
    assert_eq!(fs::metadata(&part).expect("part kept").len(), 1500);
    assert_eq!(fs::read(temp.path().join(DOWNLOAD_DIR).join("pkg.zip.part.id")).expect("identity"), format!("{URL}\n{}\n", sha_hex(&body)).into_bytes());

    let second = ScriptedFetcher::new(&body, &[]);
    let mut stages = Vec::new();
    install(&second, &AtomicBool::new(false), temp.path(), &body, None, "pkg:resume", &mut |update| stages.push(update.stage)).expect("resumes");
    assert_eq!(second.offsets(), vec![1500]);
    stages.dedup();
    assert_eq!(stages, vec![PackageStage::Probing, PackageStage::Verifying, PackageStage::Downloading, PackageStage::Extracting, PackageStage::Done]);
}

#[test]
fn a_crash_between_member_renames_is_repaired_from_the_staged_archive() {
    let (temp, body) = installed_package("pkg:crash");
    // State after a crash mid-publication: marker removed first, verified archive still
    // staged, one member from the old generation.
    fs::remove_file(temp.path().join(MARKER)).expect("marker removed");
    fs::write(temp.path().join(DOWNLOAD_DIR).join("pkg.zip"), &body).expect("staged archive");
    fs::write(temp.path().join("main.so.1"), b"old generation").expect("old member");
    assert!(!installed(temp.path(), &body));

    let offline = ScriptedFetcher::new(&body, &[Step::Status(404, None)]);
    install(&offline, &AtomicBool::new(false), temp.path(), &body, None, "pkg:crash", &mut |_| {}).expect("repaired");
    assert!(offline.offsets().is_empty(), "the staged verified archive must be reused");
    assert_eq!(fs::read(temp.path().join("main.so.1")).expect("main"), archive(1000).1);
    assert!(installed(temp.path(), &body));
}

#[test]
fn a_truncated_member_is_not_installed() {
    let (temp, body) = installed_package("pkg:truncated");
    fs::write(temp.path().join("side.so"), b"s").expect("truncate member");
    assert!(!installed(temp.path(), &body));
    let fetcher = ScriptedFetcher::new(&body, &[]);
    install(&fetcher, &AtomicBool::new(false), temp.path(), &body, None, "pkg:truncated", &mut |_| {}).expect("repaired");
    assert_eq!(fetcher.offsets(), vec![0]);
}

#[test]
fn a_part_staged_for_another_archive_is_discarded() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (body, _) = archive(1000);
    let staging = temp.path().join(DOWNLOAD_DIR);
    fs::create_dir_all(&staging).expect("download dir");
    fs::write(staging.join("pkg.zip.part"), b"foreign bytes").expect("foreign part");
    fs::write(staging.join("pkg.zip.part.id"), b"https://example.invalid/other.zip\nabc\n").expect("foreign identity");
    let fetcher = ScriptedFetcher::new(&body, &[]);
    install(&fetcher, &AtomicBool::new(false), temp.path(), &body, None, "pkg:foreign", &mut |_| {}).expect("installs");
    assert_eq!(fetcher.offsets(), vec![0], "the foreign .part must not be resumed");
}

#[test]
fn a_marker_of_another_recipe_is_not_trusted() {
    let (temp, body) = installed_package("pkg:recipe");
    // Same members and sizes, recorded for another pin of the archive.
    let marker = fs::read_to_string(temp.path().join(MARKER)).expect("marker");
    fs::write(temp.path().join(MARKER), marker.replace(&sha_hex(&body), &"0".repeat(64))).expect("foreign marker");
    assert!(!installed(temp.path(), &body));
    let fetcher = ScriptedFetcher::new(&body, &[]);
    install(&fetcher, &AtomicBool::new(false), temp.path(), &body, None, "pkg:recipe", &mut |_| {}).expect("rebuilt");
    assert_eq!(fetcher.offsets(), vec![0], "a foreign marker must not take the fast path");
    assert!(installed(temp.path(), &body));
}

#[test]
fn hash_mismatch_deletes_the_part_and_publishes_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (body, _) = archive(1000);
    let zeros = "0".repeat(64);
    let err = install(&ScriptedFetcher::new(&body, &[]), &AtomicBool::new(false), temp.path(), &body, Some(&zeros), "pkg:hash", &mut |_| {}).expect_err("mismatch");
    assert!(matches!(&err, PackageError::ChecksumMismatch { expected, actual, .. } if *expected == zeros && *actual == sha_hex(&body)), "{err:?}");
    assert!(!temp.path().join(DOWNLOAD_DIR).join("pkg.zip.part").exists());
    assert!(!temp.path().join("main.so.1").exists());
    assert!(!temp.path().join(MARKER).exists());
}

#[test]
fn a_missing_member_is_an_unpack_error_and_nothing_is_published() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (body, _) = archive(1000);
    let sha = sha_hex(&body);
    let members = [PackageMember { archive_path: "lib/side.so", file_name: "side.so" }, PackageMember { archive_path: "lib/absent.so", file_name: "absent.so" }];
    let sources = [PackageSource { url: URL, file_name: "pkg.zip", size: len_u64(&body), sha256: &sha, archive: ArchiveKind::Zip, members: &members }];
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let retry = policy(0);
    let cancel = AtomicBool::new(false);
    let env = FetchEnv { fetcher: &fetcher, retry: &retry, cancel: &cancel, log_tag: "[test]", log_context: "pkg=missing" };
    let err = ensure_installed(&env, &layout(temp.path()), &PackageRecipe { fields: &FIELDS, sources: &sources }, "pkg:missing", &mut |_| {}).expect_err("missing member");
    assert!(matches!(&err, PackageError::Unpack { source, .. } if matches!(source.as_ref(), ms_download::unpack::UnpackError::MemberMissing { member } if member == "lib/absent.so")), "{err:?}");
    assert_eq!(sorted_names(temp.path()), vec![".download"]);
    // The verified archive stays staged for the next attempt.
    assert!(temp.path().join(DOWNLOAD_DIR).join("pkg.zip").is_file());
}

#[test]
fn cancel_during_the_transfer_keeps_the_part() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (body, _) = archive(1_600_000);
    let cancel = AtomicBool::new(false);
    let err = install(&ScriptedFetcher::new(&body, &[]), &cancel, temp.path(), &body, None, "pkg:cancel", &mut |update| {
        if update.stage == PackageStage::Downloading && update.downloaded > 0 {
            cancel.store(true, Ordering::Relaxed);
        }
    })
    .expect_err("cancelled");
    assert!(matches!(err, PackageError::Cancelled), "{err:?}");
    let staged = fs::metadata(temp.path().join(DOWNLOAD_DIR).join("pkg.zip.part")).expect("part kept").len();
    assert!(staged > 0 && staged < len_u64(&body), "staged {staged}");
}

#[test]
fn a_second_installer_waits_then_takes_the_fast_path() {
    let temp = tempfile::tempdir().expect("temp dir");
    let dir = temp.path().to_path_buf();
    let (body, _) = archive(1000);
    let holder = BusyGuard::try_acquire("pkg:concurrent").expect("key free");
    let (stage_tx, stage_rx) = std::sync::mpsc::channel();
    let worker_dir = dir.clone();
    let worker_body = body.clone();
    let worker = std::thread::spawn(move || {
        let fetcher = ScriptedFetcher::new(&worker_body, &[Step::Status(404, None)]);
        // A send fails only once the test stopped listening; the stage is then moot.
        let result = install(&fetcher, &AtomicBool::new(false), &worker_dir, &worker_body, None, "pkg:concurrent", &mut |update| {
            stage_tx.send(update.stage).ok();
        });
        (result, fetcher.offsets())
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let stage = stage_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).expect("worker reports waiting");
        if stage == PackageStage::WaitingForOtherDownload {
            break;
        }
    }
    // Publish the package as the other holder would, then release the key.
    let other = tempfile::tempdir().expect("temp dir");
    install(&ScriptedFetcher::new(&body, &[]), &AtomicBool::new(false), other.path(), &body, None, "pkg:concurrent-other", &mut |_| {}).expect("other install");
    for name in ["main.so.1", "side.so", MARKER] {
        fs::copy(other.path().join(name), dir.join(name)).expect("publish");
    }
    drop(holder);
    let (result, offsets) = worker.join().expect("worker thread");
    assert_eq!(result.expect("fast path after the wait"), InstallOutcome::InstalledByOther);
    assert!(offsets.is_empty(), "no request after the other holder finished");
}

#[test]
fn removal_drops_the_package_and_honours_the_busy_key() {
    let temp = tempfile::tempdir().expect("temp dir");
    let pkg_dir: PathBuf = temp.path().join("pkg");
    let (body, _) = archive(1000);
    install(&ScriptedFetcher::new(&body, &[]), &AtomicBool::new(false), &pkg_dir, &body, None, "pkg:remove", &mut |_| {}).expect("install");
    assert!(installed(&pkg_dir, &body));

    // Busy + cancel: nothing is removed.
    let holder = BusyGuard::try_acquire("pkg:remove").expect("key free");
    let err = remove_installed(&layout(&pkg_dir), "pkg:remove", &AtomicBool::new(true), &policy(0)).expect_err("cancelled");
    assert!(matches!(err, PackageError::Cancelled), "{err:?}");
    assert!(installed(&pkg_dir, &body));
    drop(holder);

    remove_installed(&layout(&pkg_dir), "pkg:remove", &AtomicBool::new(false), &policy(0)).expect("removed");
    assert!(!pkg_dir.exists());
    assert!(!installed(&pkg_dir, &body));
    // A missing directory is success.
    remove_installed(&layout(&pkg_dir), "pkg:remove", &AtomicBool::new(false), &policy(0)).expect("nothing to remove");
}

#[test]
fn invalid_file_names_are_rejected_before_any_io() {
    let temp = tempfile::tempdir().expect("temp dir");
    let dir = temp.path().join("pkg");
    let (body, _) = archive(10);
    let sha = sha_hex(&body);
    for bad in ["", "a/b", "..", MARKER, DOWNLOAD_DIR] {
        let members = [PackageMember { archive_path: "lib/main.so", file_name: bad }];
        let sources = [PackageSource { url: URL, file_name: "pkg.zip", size: len_u64(&body), sha256: &sha, archive: ArchiveKind::Zip, members: &members }];
        let recipe = PackageRecipe { fields: &FIELDS, sources: &sources };
        let fetcher = ScriptedFetcher::new(&body, &[]);
        let retry = policy(0);
        let cancel = AtomicBool::new(false);
        let env = FetchEnv { fetcher: &fetcher, retry: &retry, cancel: &cancel, log_tag: "[test]", log_context: "pkg=bad" };
        let err = ensure_installed(&env, &layout(&dir), &recipe, "pkg:bad", &mut |_| {}).expect_err("invalid name");
        assert!(matches!(&err, PackageError::InvalidFileName { name } if name == bad), "{err:?}");
        assert!(!is_installed(&layout(&dir), &recipe));
        assert!(fetcher.offsets().is_empty() && !dir.exists());
    }
}

#[test]
fn member_names_flatten_and_reject_empty() {
    assert_eq!(flatten_member_name("onnxruntime/capi/dxil.dll").expect("has a file name"), "dxil.dll");
    assert_eq!(flatten_member_name("plain.so").expect("has a file name"), "plain.so");
    assert!(matches!(flatten_member_name("/"), Err(PackageError::InvalidMemberPath { member }) if member == "/"));
    assert!(matches!(flatten_member_name("a/.."), Err(PackageError::InvalidMemberPath { .. })));
}

#[test]
fn extraction_stages_every_member_before_publishing_any() {
    let temp = tempfile::tempdir().expect("temp dir");
    let zip_path = temp.path().join("fixture.zip");
    fs::write(&zip_path, zip_bytes(&[("capi/onnxruntime.dll", b"primary".as_slice()), ("capi/dxil.dll", b"dxil".as_slice())])).expect("write zip");
    let out = temp.path().join("out");
    fs::create_dir_all(&out).expect("out dir");

    // One wanted member is absent: nothing may be published, no temporary left.
    let incomplete = [PackageMember { archive_path: "capi/dxil.dll", file_name: "dxil.dll" }, PackageMember { archive_path: "capi/absent.dll", file_name: "absent.dll" }];
    let err = fetch::extract_and_publish(&zip_path, ArchiveKind::Zip, &out, &incomplete).expect_err("missing member");
    assert!(matches!(&err, PackageError::Unpack { source, .. } if matches!(source.as_ref(), ms_download::unpack::UnpackError::MemberMissing { member } if member == "capi/absent.dll")), "{err:?}");
    assert!(sorted_names(&out).is_empty(), "a member or a temporary was left before the set was complete");

    let complete = [PackageMember { archive_path: "capi/dxil.dll", file_name: "dxil.dll" }, PackageMember { archive_path: "capi/onnxruntime.dll", file_name: "onnxruntime.dll" }];
    let published = fetch::extract_and_publish(&zip_path, ArchiveKind::Zip, &out, &complete).expect("extract");
    assert_eq!(published.iter().map(|member| member.file_name.as_str()).collect::<Vec<_>>(), vec!["dxil.dll", "onnxruntime.dll"]);
    assert_eq!(fs::read(out.join("onnxruntime.dll")).expect("primary"), b"primary");
    assert_eq!(fs::read(out.join("dxil.dll")).expect("dxil"), b"dxil");
    assert_eq!(sorted_names(&out), vec!["dxil.dll", "onnxruntime.dll"]);
}

#[test]
fn a_marker_outside_the_package_directory_is_rejected_before_any_io() {
    let temp = tempfile::tempdir().expect("temp dir");
    let dir = temp.path().join("pkg");
    let outside = temp.path().join("outside-marker");
    fs::write(&outside, b"keep me").expect("outside file");
    let absolute = outside.to_string_lossy().into_owned();
    let (body, _) = archive(10);
    let sha = sha_hex(&body);
    for bad in [absolute.as_str(), "../outside-marker", "..", "", DOWNLOAD_DIR, "a/b"] {
        let bad_layout = PackageLayout { dir: &dir, marker_file: bad, marker_format: FORMAT };
        let sources = [PackageSource { url: URL, file_name: "pkg.zip", size: len_u64(&body), sha256: &sha, archive: ArchiveKind::Zip, members: &MEMBERS }];
        let recipe = PackageRecipe { fields: &FIELDS, sources: &sources };
        let fetcher = ScriptedFetcher::new(&body, &[]);
        let retry = policy(0);
        let cancel = AtomicBool::new(false);
        let env = FetchEnv { fetcher: &fetcher, retry: &retry, cancel: &cancel, log_tag: "[test]", log_context: "pkg=marker" };
        let err = ensure_installed(&env, &bad_layout, &recipe, "pkg:bad-marker", &mut |_| {}).expect_err("invalid marker");
        assert!(matches!(&err, PackageError::InvalidFileName { name } if name == bad), "{err:?}");
        assert!(!is_installed(&bad_layout, &recipe), "an invalid marker is never trusted");
        let err = remove_installed(&bad_layout, "pkg:bad-marker", &cancel, &retry).expect_err("invalid marker");
        assert!(matches!(&err, PackageError::InvalidFileName { name } if name == bad), "{err:?}");
        assert!(fetcher.offsets().is_empty() && !dir.exists(), "nothing touched for {bad:?}");
    }
    assert_eq!(fs::read(&outside).expect("outside file kept"), b"keep me");
}

#[test]
fn colliding_names_across_sources_are_rejected_before_any_io() {
    const LIB: [PackageMember<'static>; 1] = [PackageMember { archive_path: "lib/main.so", file_name: "main.so" }];
    const SIDE: [PackageMember<'static>; 1] = [PackageMember { archive_path: "lib/side.so", file_name: "side.so" }];
    const SAME_NAME: [PackageMember<'static>; 1] = [PackageMember { archive_path: "other/main.so", file_name: "main.so" }];
    const TMP_NAME: [PackageMember<'static>; 1] = [PackageMember { archive_path: "lib/x", file_name: "main.so.tmp" }];
    const BOTH: [PackageMember<'static>; 2] = [LIB[0], SAME_NAME[0]];
    fn source(file_name: &'static str, members: &'static [PackageMember<'static>]) -> PackageSource<'static> {
        PackageSource { url: URL, file_name, size: 10, sha256: "", archive: ArchiveKind::Zip, members }
    }
    let temp = tempfile::tempdir().expect("temp dir");
    let dir = temp.path().join("pkg");
    let (body, _) = archive(10);
    let sha = sha_hex(&body);
    let cases: Vec<(Vec<PackageSource<'static>>, &str)> = vec![
        // The same archive name twice: one `.part` / `.part.id` for two sources.
        (vec![source("a.zip", &LIB), source("a.zip", &SIDE)], "a.zip"),
        // An archive named like another archive's `.part`.
        (vec![source("a.zip", &LIB), source("a.zip.part", &SIDE)], "a.zip.part"),
        // The same published member name from two sources.
        (vec![source("a.zip", &LIB), source("b.zip", &SAME_NAME)], "main.so"),
        // A member named like another member's staging temporary.
        (vec![source("a.zip", &LIB), source("b.zip", &TMP_NAME)], "main.so.tmp"),
        // The same member twice inside one source.
        (vec![source("a.zip", &BOTH)], "main.so"),
    ];
    for (sources, colliding) in &cases {
        let sources: Vec<PackageSource<'_>> = sources.iter().map(|source| PackageSource { sha256: &sha, ..*source }).collect();
        let recipe = PackageRecipe { fields: &FIELDS, sources: &sources };
        let fetcher = ScriptedFetcher::new(&body, &[]);
        let retry = policy(0);
        let cancel = AtomicBool::new(false);
        let env = FetchEnv { fetcher: &fetcher, retry: &retry, cancel: &cancel, log_tag: "[test]", log_context: "pkg=dup" };
        let err = ensure_installed(&env, &layout(&dir), &recipe, "pkg:dup", &mut |_| {}).expect_err("colliding names");
        assert!(matches!(&err, PackageError::DuplicateFileName { name } if name == colliding), "{err:?}");
        assert!(!is_installed(&layout(&dir), &recipe));
        assert!(fetcher.offsets().is_empty() && !dir.exists());
    }
}
