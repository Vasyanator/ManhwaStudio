/*
File: crates/ms-installer/src/download/tests.rs

Purpose:
Unit tests of the installer's network layer: an asset download resumed after a mid-body
disconnect, sha256-pinned assets (accepted, and a mismatch deleting the `.part`), the identity
sidecar (a matching `.part` resumes when the sha256 is pinned and restarts otherwise, a foreign
one restarts from zero), cancel, digest
parsing, a failed attempt keeping the old destination, the GitHub API GET retrying a transient status, and the progress fraction.

Notes:
- `ms_download::test_support::ScriptedFetcher` serves the content and the backoff sleep is a
  no-op, so nothing touches the network or waits; scratch data lives in self-removing temp
  directories.
*/

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use ms_download::resumable::RetryPolicy;
use ms_download::test_support::{ScriptedFetcher, Step};
use sha2::Digest;

use super::*;

/// A backoff sleep that returns at once.
fn no_sleep(_slice: Duration) {}

/// The production schedule without real waiting.
fn fast_policy() -> RetryPolicy<'static> {
    RetryPolicy { sleep: &no_sleep, ..RetryPolicy::standard() }
}

fn temp_root() -> tempfile::TempDir {
    match tempfile::Builder::new().prefix("ms-installer-download-").tempdir_in(std::env::temp_dir()) {
        Ok(dir) => dir,
        Err(err) => panic!("cannot create a temp dir: {err}"),
    }
}

/// Deterministic test content of `len` bytes, varied by `seed`.
fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|index| u8::try_from(index % 251).unwrap_or(0).wrapping_add(seed)).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn len_u64(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

fn asset(body: &[u8], url: &str, digest: Option<String>) -> GithubAsset {
    GithubAsset { name: "asset.bin".to_owned(), browser_download_url: url.to_owned(), size: len_u64(body), digest }
}

/// Runs `download_asset_with` for `asset` into `<root>/downloads/asset.bin`.
fn run(fetcher: &ScriptedFetcher, asset: &GithubAsset, dst_path: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let (tx, _rx) = mpsc::channel();
    let req = AssetDownload { asset, dst_path, label_prefix: "test-asset", overall_range: (0.0, 1.0) };
    download_asset_with(fetcher, &fast_policy(), &req, &tx, cancel)
}

fn write_identity(path: &Path, identity: &AssetIdentity) {
    let bytes = serde_json::to_vec(identity).unwrap_or_else(|err| panic!("serialize identity: {err}"));
    std::fs::write(path, bytes).unwrap_or_else(|err| panic!("write {}: {err}", path.display()));
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

#[test]
fn download_resumes_after_mid_body_disconnect() {
    let root = temp_root();
    let body = content(400_000, 1);
    let half = body.len() / 2;
    let dst = root.path().join("downloads/asset.bin");
    let fetcher = ScriptedFetcher::new(&body, &[Step::Drop(half)]);
    let result = run(&fetcher, &asset(&body, "https://example.invalid/a", None), &dst, &AtomicBool::new(false));
    assert_eq!(result, Ok(()));
    assert_eq!(fetcher.offsets(), vec![0, len_u64(&body[..half])], "the retry must resume exactly where the cut happened");
    assert_eq!(read(&dst), body);
    assert!(!root.path().join("downloads/asset.bin.part").exists(), "no partial file may remain after success");
}

#[test]
fn sha256_pinned_asset_is_published() {
    let root = temp_root();
    let body = content(200_000, 2);
    let dst = root.path().join("asset.bin");
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let digest = Some(format!("sha256:{}", sha256_hex(&body)));
    assert_eq!(run(&fetcher, &asset(&body, "https://example.invalid/a", digest), &dst, &AtomicBool::new(false)), Ok(()));
    assert_eq!(read(&dst), body);
}

#[test]
fn digest_mismatch_deletes_the_part_and_publishes_nothing() {
    let root = temp_root();
    let body = content(200_000, 3);
    let dst = root.path().join("asset.bin");
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let digest = Some(format!("sha256:{}", sha256_hex(b"other content")));
    let result = run(&fetcher, &asset(&body, "https://example.invalid/a", digest), &dst, &AtomicBool::new(false));
    assert!(result.is_err());
    assert!(!root.path().join("asset.bin.part").exists(), "a proven-wrong .part must be deleted");
    assert!(!dst.exists());
}

/// Stages `body[..120_000]` as a `.part` of an earlier run with a matching sidecar, downloads
/// `asset` from a server without a strong ETag and returns the requested offsets.
fn resume_offsets_with_matching_identity(body: &[u8], digest: Option<String>) -> Vec<u64> {
    let root = temp_root();
    let dst = root.path().join("asset.bin");
    let asset = asset(body, "https://example.invalid/v2/asset.bin", digest);
    std::fs::write(root.path().join("asset.bin.part"), &body[..120_000]).unwrap_or_else(|err| panic!("write part: {err}"));
    write_identity(&root.path().join("asset.bin.identity.json"), &AssetIdentity::of(&asset));
    let fetcher = ScriptedFetcher::new(body, &[]);
    assert_eq!(run(&fetcher, &asset, &dst, &AtomicBool::new(false)), Ok(()));
    assert_eq!(read(&dst), body);
    fetcher.offsets()
}

#[test]
fn digest_pinned_asset_resumes_the_part_of_an_earlier_run() {
    // The pinned sha256 catches any splice at the end, so the engine may trust the prefix.
    let body = content(300_000, 4);
    let digest = Some(format!("sha256:{}", sha256_hex(&body)));
    assert_eq!(resume_offsets_with_matching_identity(&body, digest), vec![120_000]);
}

#[test]
fn digestless_asset_without_a_strong_etag_restarts_an_earlier_part() {
    // The installer sidecar only names the asset; a release asset can be replaced with the same
    // size, so without a sha256 or a strong server identity the engine restarts from zero.
    let body = content(300_000, 4);
    let offsets = resume_offsets_with_matching_identity(&body, None);
    assert_eq!(offsets.last(), Some(&0), "a size-only part from an earlier run must restart: {offsets:?}");
}

#[test]
fn identity_mismatch_restarts_from_zero() {
    let root = temp_root();
    let body = content(300_000, 5);
    let dst = root.path().join("asset.bin");
    let asset = asset(&body, "https://example.invalid/v2/asset.bin", None);
    // A `.part` of an older release with the same file name, and a stale published file of
    // the same size: neither may be resumed or kept.
    std::fs::write(root.path().join("asset.bin.part"), content(120_000, 99)).unwrap_or_else(|err| panic!("write part: {err}"));
    std::fs::write(&dst, content(body.len(), 98)).unwrap_or_else(|err| panic!("write dest: {err}"));
    write_identity(&root.path().join("asset.bin.identity.json"), &AssetIdentity { url: "https://example.invalid/v1/asset.bin".to_owned(), size: asset.size, digest: None });
    let fetcher = ScriptedFetcher::new(&body, &[]);
    assert_eq!(run(&fetcher, &asset, &dst, &AtomicBool::new(false)), Ok(()));
    assert_eq!(fetcher.offsets(), vec![0]);
    assert_eq!(read(&dst), body, "a successful download replaces the old destination");
    for leftover in ["asset.bin.part", "asset.bin.download", "asset.bin.identity.json"] {
        assert!(!root.path().join(leftover).exists(), "{leftover} must not remain after success");
    }
}

#[test]
fn failed_attempt_with_a_foreign_identity_keeps_the_old_destination() {
    let root = temp_root();
    let body = content(300_000, 8);
    let dst = root.path().join("asset.bin");
    let asset = asset(&body, "https://example.invalid/v2/asset.bin", None);
    let old = content(250_000, 96);
    std::fs::write(&dst, &old).unwrap_or_else(|err| panic!("write dest: {err}"));
    std::fs::write(root.path().join("asset.bin.part"), content(10_000, 95)).unwrap_or_else(|err| panic!("write part: {err}"));
    write_identity(&root.path().join("asset.bin.identity.json"), &AssetIdentity { url: "https://example.invalid/v1/asset.bin".to_owned(), size: 1, digest: None });
    // Offline / refused: the request fails permanently.
    let fetcher = ScriptedFetcher::new(&body, &[Step::Status(404, None)]);
    assert!(run(&fetcher, &asset, &dst, &AtomicBool::new(false)).is_err());
    assert_eq!(read(&dst), old, "a failed attempt must not touch the published destination");
    assert!(!root.path().join("asset.bin.download").exists());
}

#[test]
fn missing_identity_discards_a_leftover_part() {
    let root = temp_root();
    let body = content(100_000, 6);
    let dst = root.path().join("asset.bin");
    std::fs::write(root.path().join("asset.bin.part"), content(50_000, 97)).unwrap_or_else(|err| panic!("write part: {err}"));
    let fetcher = ScriptedFetcher::new(&body, &[]);
    assert_eq!(run(&fetcher, &asset(&body, "https://example.invalid/a", None), &dst, &AtomicBool::new(false)), Ok(()));
    assert_eq!(fetcher.offsets(), vec![0]);
    assert_eq!(read(&dst), body);
}

#[test]
fn cancel_stops_the_download_with_the_cancel_message() {
    let root = temp_root();
    let body = content(100_000, 7);
    let dst = root.path().join("asset.bin");
    let cancel = AtomicBool::new(false);
    cancel.store(true, Ordering::Relaxed);
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let result = run(&fetcher, &asset(&body, "https://example.invalid/a", None), &dst, &cancel);
    assert_eq!(result, Err(t!("installer.utils.download_cancelled_error").to_string()));
    assert!(fetcher.offsets().is_empty(), "a cancelled download sends no request");
    assert!(!dst.exists());
}

#[test]
fn only_a_well_formed_sha256_digest_pins_the_hash() {
    let hex = "ab".repeat(32);
    let pinned = GithubAsset { name: "a".to_owned(), browser_download_url: "u".to_owned(), size: 7, digest: Some(format!("sha256:{hex}")) };
    assert_eq!(pinned.expected(), Expected::SizeSha256 { size: 7, sha256: &hex });
    for digest in [None, Some(format!("sha512:{hex}")), Some("sha256:xyz".to_owned())] {
        let asset = GithubAsset { digest, ..pinned.clone() };
        assert_eq!(asset.expected(), Expected::Size(7));
    }
}

#[test]
fn release_asset_json_parses_size_and_optional_digest() {
    let json = r#"[{"name":"a.zip","browser_download_url":"https://x/a.zip","size":12,"digest":"sha256:00"},{"name":"b.zip","browser_download_url":"https://x/b.zip","size":3,"digest":null},{"name":"c.zip","browser_download_url":"https://x/c.zip","size":4}]"#;
    let assets: Vec<GithubAsset> = serde_json::from_str(json).unwrap_or_else(|err| panic!("parse: {err}"));
    assert_eq!(assets[0].size, 12);
    assert_eq!(assets[0].digest.as_deref(), Some("sha256:00"));
    assert_eq!(assets[1].digest, None);
    assert_eq!(assets[2].digest, None);
}

#[test]
fn github_api_get_retries_a_transient_status() {
    let body = br#"{"assets":[]}"#;
    let fetcher = ScriptedFetcher::new(body, &[Step::Status(503, None)]);
    let result = github_api_get_with(&fetcher, &fast_policy(), &AtomicBool::new(false), "https://api.invalid/releases", "test");
    assert_eq!(result.as_deref(), Ok(r#"{"assets":[]}"#));
    assert_eq!(fetcher.offsets(), vec![0, 0]);
}

#[test]
fn github_api_get_fails_fast_on_a_permanent_status() {
    let fetcher = ScriptedFetcher::new(b"{}", &[Step::Status(404, None)]);
    let result = github_api_get_with(&fetcher, &fast_policy(), &AtomicBool::new(false), "https://api.invalid/releases", "test");
    assert!(result.is_err());
    assert_eq!(fetcher.offsets(), vec![0]);
}

#[test]
fn fraction_is_bounded_and_handles_an_empty_total() {
    assert!((fraction(0, 0) - 0.0).abs() < f32::EPSILON);
    assert!((fraction(50, 100) - 0.5).abs() < 1e-4);
    assert!((fraction(200, 100) - 1.0).abs() < f32::EPSILON);
    assert!((fraction(u64::MAX, u64::MAX) - 1.0).abs() < f32::EPSILON);
}
