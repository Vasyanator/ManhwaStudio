/*
File: crates/ms-installer/src/download.rs

Purpose:
The installer's network layer: GitHub release metadata requests and release-asset downloads,
both on the shared `ms-download` engine (retry with capped exponential backoff, `Range`
resume, connect/read timeouts, cancel), plus the progress / retry text the installer and
update windows show for them.

Key items:
- `GithubAsset`: one release asset as the GitHub API lists it (name, URL, size, digest).
- `github_api_get()`: one GitHub API GET (`fetch_small`), `GITHUB_TOKEN` as bearer.
- `AssetDownload` + `download_asset()`: one asset into its destination through
  `fetch_verified`, pinned to the asset's size and to a sha256: the caller's required pin
  (the release's `SHA256SUMS` entry, `release.rs`) when given, else GitHub's digest if any.
- `fetch_small_asset()`: a small release asset (the `SHA256SUMS` list) into memory
  (`fetch_small`), its length checked against the listed size and its bytes against GitHub's
  sha256 digest when one is listed.
- `AssetIdentity`: the sidecar `<file>.identity.json` that decides whether the staged files
  left by an earlier run may be reused.

Notes:
- Cross-run resume: the staged files (`<file>.part`, the verified `<file>.download`) are reused
  only when the sidecar names the same `{url, size, digest}`; otherwise they are deleted before
  the transfer starts, so bytes of another release can never be resumed. A matching sidecar is
  necessary, not sufficient: the engine resumes a `.part` of an earlier run only when the sha256
  is pinned (GitHub `digest`) or the server names a strong identity; a size-only asset restarts
  (a release asset can be replaced with one of the same size). The published file is
  never deleted up front: it is replaced only by the final rename of a verified download.
- The token is only a request header (`FetcherConfig::bearer`): never logged; ureq drops it on
  the redirect to GitHub's object storage.
- Logs go to `ms_log::runtime_log` with the `[installer-download]` tag (start, finish,
  failure; the engine logs every retry). User-facing text is localized here.
*/

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

use ms_download::resumable::{FetcherConfig, RangeFetcher, ResumableFetch, RetryPolicy, StreamEvent, UreqFetcher};
use ms_download::{DownloadError, Expected, FileEvent, FileRequest, SmallRequest, fetch_small, fetch_verified};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use web_time::{Duration, Instant};

use super::install::InstallEvent;
use super::utils::{format_bytes, send_progress};

/// User-Agent header sent with installer HTTP requests.
pub(crate) const INSTALLER_USER_AGENT: &str = "ManhwaStudioMiniLauncher/installer";

/// Log prefix of this module's lines (and of the engine's retry warnings it drives).
const LOG_TAG: &str = "[installer-download]";

/// Largest accepted GitHub API answer. The release list of this repository is a few hundred
/// KiB; the cap only bounds memory against a broken or hostile answer.
const GITHUB_API_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Minimum interval between two transfer / hash progress events sent to the window.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(120);

/// One release asset of the GitHub releases API.
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct GithubAsset {
    pub(crate) name: String,
    pub(crate) browser_download_url: String,
    /// Size in bytes; a required field of the API's release-asset object.
    pub(crate) size: u64,
    /// `"sha256:<64 hex>"` for assets GitHub has hashed; `null` (or absent) for older ones.
    #[serde(default)]
    pub(crate) digest: Option<String>,
}

impl GithubAsset {
    /// The sha256 hex digits of `digest` when it is a well-formed `sha256:` digest. Any other
    /// algorithm or a malformed value is logged and ignored (the size still pins the file).
    fn sha256_pin(&self) -> Option<&str> {
        let digest = self.digest.as_deref()?;
        match digest.strip_prefix("sha256:") {
            Some(hex) if hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) => Some(hex),
            _ => {
                ms_log::runtime_log::log_warn(format!("{LOG_TAG} unusable asset digest, verifying the size only: asset={} digest={digest}", self.name));
                None
            }
        }
    }

    /// What the downloaded file must match: size + `required_sha256` when the caller pins one,
    /// else size + GitHub's digest when published, otherwise the size alone. A required pin
    /// replaces the digest without comparing the two: `release::ReleaseChecksums::pin_for`
    /// refuses an asset whose digest disagrees with its `SHA256SUMS` entry before any download.
    fn expected<'a>(&'a self, required_sha256: Option<&'a str>) -> Expected<'a> {
        match required_sha256.or_else(|| self.sha256_pin()) {
            Some(sha256) => Expected::SizeSha256 { size: self.size, sha256 },
            None => Expected::Size(self.size),
        }
    }
}

/// The source the staged `.part` / `.download` were downloaded from, persisted next to them so a later
/// run resumes or keeps them only for the very same asset.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct AssetIdentity {
    url: String,
    size: u64,
    digest: Option<String>,
}

impl AssetIdentity {
    fn of(asset: &GithubAsset) -> Self {
        Self { url: asset.browser_download_url.clone(), size: asset.size, digest: asset.digest.clone() }
    }
}

/// The `FetcherConfig` of every installer request: installer user agent, `extra_headers`,
/// and `GITHUB_TOKEN` (trimmed, when non-empty) as bearer.
fn installer_fetcher_config(extra_headers: Vec<(String, String)>) -> FetcherConfig {
    let bearer = std::env::var("GITHUB_TOKEN").ok().map(|token| token.trim().to_owned()).filter(|token| !token.is_empty());
    FetcherConfig { user_agent: Some(INSTALLER_USER_AGENT.to_owned()), bearer, headers: extra_headers, ..FetcherConfig::default() }
}

/// Performs a GitHub API GET and returns the body as text.
///
/// `context` names the caller in log lines (e.g. `update-window`). Transient failures are
/// retried per `RetryPolicy::standard()`; `cancel` stops the request and any backoff wait.
///
/// # Errors
/// The (English, technical) failure text, or the localized cancel text; callers wrap it into
/// their own localized message.
pub(crate) fn github_api_get(url: &str, context: &str, cancel: &AtomicBool) -> Result<String, String> {
    let fetcher = UreqFetcher::with_config(installer_fetcher_config(vec![("Accept".to_owned(), "application/vnd.github+json".to_owned())]));
    github_api_get_with(&fetcher, &RetryPolicy::standard(), cancel, url, context)
}

/// [`github_api_get`] over an injected fetcher and retry schedule (tests use a scripted one).
fn github_api_get_with(fetcher: &dyn RangeFetcher, retry: &RetryPolicy<'_>, cancel: &AtomicBool, url: &str, context: &str) -> Result<String, String> {
    let request = SmallRequest { url, max_bytes: GITHUB_API_MAX_BYTES };
    let log_context = format!("github-api caller={context}");
    match fetch_small(fetcher, retry, cancel, &request, LOG_TAG, &log_context) {
        Ok(body) => String::from_utf8(body).map_err(|err| format!("the GitHub API answer is not UTF-8: {err}")),
        Err(DownloadError::Cancelled) => Err(t!("installer.utils.download_cancelled_error").to_string()),
        Err(err) => {
            ms_log::runtime_log::log_warn(format!("{LOG_TAG} GitHub API request failed: {log_context} error={err}"));
            Err(err.to_string())
        }
    }
}

/// Fetches the small release asset `asset` into memory, at most `max_bytes`, and checks that
/// exactly the listed `size` arrived and, when GitHub lists a well-formed sha256 `digest` for
/// the asset, that the body matches it (a short, long or corrupted body is refused, never
/// parsed).
///
/// Transient failures are retried per `RetryPolicy::standard()`; `cancel` stops the request
/// and any backoff wait. No JSON `Accept` header: this is a file, not an API answer.
///
/// # Errors
/// The (English, technical) failure text, or the localized cancel text; callers wrap it into
/// their own localized message.
pub(crate) fn fetch_small_asset(asset: &GithubAsset, max_bytes: u64, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
    let fetcher = UreqFetcher::with_config(installer_fetcher_config(Vec::new()));
    fetch_small_asset_with(&fetcher, &RetryPolicy::standard(), cancel, asset, max_bytes)
}

/// [`fetch_small_asset`] over an injected fetcher and retry schedule (tests use a scripted one).
fn fetch_small_asset_with(fetcher: &dyn RangeFetcher, retry: &RetryPolicy<'_>, cancel: &AtomicBool, asset: &GithubAsset, max_bytes: u64) -> Result<Vec<u8>, String> {
    if asset.size > max_bytes {
        return Err(format!("the asset is {} bytes, more than the {max_bytes}-byte limit", asset.size));
    }
    let request = SmallRequest { url: &asset.browser_download_url, max_bytes };
    let log_context = format!("small-asset asset={}", asset.name);
    let body = match fetch_small(fetcher, retry, cancel, &request, LOG_TAG, &log_context) {
        Ok(body) => body,
        Err(DownloadError::Cancelled) => return Err(t!("installer.utils.download_cancelled_error").to_string()),
        Err(err) => {
            ms_log::runtime_log::log_warn(format!("{LOG_TAG} small asset request failed: {log_context} error={err}"));
            return Err(err.to_string());
        }
    };
    let received = u64::try_from(body.len()).unwrap_or(u64::MAX);
    if received != asset.size {
        ms_log::runtime_log::log_warn(format!("{LOG_TAG} small asset size mismatch: {log_context} listed={} received={received}", asset.size));
        return Err(format!("received {received} bytes, the release lists {}", asset.size));
    }
    if let Some(expected) = asset.sha256_pin() {
        let actual: String = sha2::Sha256::digest(&body).iter().map(|byte| format!("{byte:02x}")).collect();
        // GitHub's digest is lowercase today; compare case-insensitively so a casing change
        // cannot turn into a false "corrupted" verdict.
        if !actual.eq_ignore_ascii_case(expected) {
            ms_log::runtime_log::log_warn(format!("{LOG_TAG} small asset sha256 mismatch: {log_context} expected={expected} actual={actual}"));
            return Err(format!("the body does not match the published sha256 {expected}"));
        }
    }
    Ok(body)
}

/// One release asset to download into `dst_path`.
pub(crate) struct AssetDownload<'a> {
    pub(crate) asset: &'a GithubAsset,
    /// The published file; its `.part` and `.identity.json` siblings are derived from it.
    pub(crate) dst_path: &'a Path,
    /// Names the asset in progress labels and messages.
    pub(crate) label_prefix: &'a str,
    /// The slice of the overall progress bar the transfer reports into.
    pub(crate) overall_range: (f32, f32),
    /// A sha256 (64 lowercase hex) the file MUST match, from the release's `SHA256SUMS`
    /// (`release::ReleaseChecksums::pin_for`); `None` pins GitHub's digest when published.
    pub(crate) required_sha256: Option<&'a str>,
}

/// Downloads `req.asset` into `req.dst_path`, verified against its size and its sha256
/// (`req.required_sha256`, else GitHub's digest when published), tolerating an unreliable
/// connection. The verification runs before the file is published, so nothing downstream
/// (executable replacement, archive extraction) ever sees unverified bytes.
///
/// The body streams into `<file>.part`; transient failures are retried with capped
/// exponential backoff and resumed with `Range` (shown as progress, not as an error); the
/// verified file is published as `<file>.download` and then renamed onto `dst_path`, so
/// `dst_path` never holds a partial or unverified download and an existing `dst_path` is only
/// ever replaced by that final rename (a failed or offline attempt leaves it untouched). The
/// staged files (`.part`, `.download`) left by an earlier run are reused only when their
/// identity sidecar names the same asset. `cancel` stops the transfer, a hash pass or a
/// backoff wait, keeping the `.part`.
///
/// # Errors
/// A localized message: the cancel text, the checksum-mismatch text (the `.part` is deleted as
/// proven wrong; not retried), the download error naming the asset, or a failed final rename
/// (the verified `.download` is kept).
pub(crate) fn download_asset(req: &AssetDownload<'_>, tx: &mpsc::Sender<InstallEvent>, cancel: &AtomicBool) -> Result<(), String> {
    let fetcher = UreqFetcher::with_config(installer_fetcher_config(Vec::new()));
    download_asset_with(&fetcher, &RetryPolicy::standard(), req, tx, cancel)
}

/// [`download_asset`] over an injected fetcher and retry schedule (tests use a scripted one).
fn download_asset_with(fetcher: &dyn RangeFetcher, retry: &RetryPolicy<'_>, req: &AssetDownload<'_>, tx: &mpsc::Sender<InstallEvent>, cancel: &AtomicBool) -> Result<(), String> {
    let part_path = sibling_path(req.dst_path, ".part")?;
    let staged_path = sibling_path(req.dst_path, ".download")?;
    let identity_path = sibling_path(req.dst_path, ".identity.json")?;
    reconcile_identity(&AssetIdentity::of(req.asset), &identity_path, &[&part_path, &staged_path])?;

    let log_context = format!("asset={} dest={}", req.asset.name, req.dst_path.display());
    let engine = ResumableFetch { fetcher, retry, cancel, log_tag: LOG_TAG, log_context: &log_context };
    let expected = req.asset.expected(req.required_sha256);
    // `fetch_verified` publishes into the staged sibling, never onto `dst_path`: an existing
    // destination is unproven (its size may match by chance) and must survive a failed attempt.
    let file_request = FileRequest { url: &req.asset.browser_download_url, part: &part_path, dest: &staged_path, expected };
    ms_log::runtime_log::log_info(format!("{LOG_TAG} download started: {log_context} size={} sha256_pinned={}", req.asset.size, expected.sha256().is_some()));

    let mut sink = ProgressSink { tx, req, total: req.asset.size, last_emit: None };
    match fetch_verified(&engine, &file_request, &mut |event| sink.on_event(event)) {
        Ok(size) => {
            promote_staged(&staged_path, req.dst_path, &identity_path)?;
            ms_log::runtime_log::log_info(format!("{LOG_TAG} download finished: {log_context} size={size}"));
            let done = tf!("installer.utils.download_done_progress", label_prefix = req.label_prefix);
            send_progress(tx, 1.0, done.clone(), req.overall_range.1, done);
            Ok(())
        }
        Err(err) => {
            ms_log::runtime_log::log_warn(format!("{LOG_TAG} download failed: {log_context} error={err}"));
            Err(download_error_message(&err, req.label_prefix))
        }
    }
}

/// The localized message of a failed asset download.
fn download_error_message(err: &DownloadError, label_prefix: &str) -> String {
    match err {
        DownloadError::Cancelled => t!("installer.utils.download_cancelled_error").to_string(),
        // Its own text: the file arrived but is not the published one. Retrying the same
        // release cannot fix that, so the user is told so instead of "network error".
        DownloadError::HashMismatch { .. } => tf!("installer.utils.download_checksum_mismatch_error", label_prefix = label_prefix),
        DownloadError::Busy { .. }
        | DownloadError::InvalidRequest { .. }
        | DownloadError::Io { .. }
        | DownloadError::Http { .. }
        | DownloadError::SizeMismatch { .. }
        | DownloadError::Archive { .. } => tf!("installer.utils.download_error", label_prefix = label_prefix, e = err),
    }
}

/// `dst_path` with `suffix` appended to its file name.
///
/// # Errors
/// A localized message when `dst_path` has no UTF-8 file name.
fn sibling_path(dst_path: &Path, suffix: &str) -> Result<PathBuf, String> {
    dst_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| dst_path.with_file_name(format!("{name}{suffix}")))
        .ok_or_else(|| tf!("installer.utils.create_dst_path_error", dst_path = dst_path.display(), e = "invalid file name"))
}

/// Keeps the `staged` files (`.part`, verified `.download`) only when `identity_path` names
/// `identity`; otherwise deletes them and records `identity`, so the transfer that follows
/// starts from zero for this asset. A missing or unreadable sidecar counts as a mismatch:
/// nothing proves those bytes belong to this asset. The published destination is never
/// touched here.
///
/// # Errors
/// A localized message when a stale file cannot be removed or the sidecar cannot be written.
fn reconcile_identity(identity: &AssetIdentity, identity_path: &Path, staged: &[&Path]) -> Result<(), String> {
    let recorded = std::fs::read(identity_path).ok().and_then(|bytes| serde_json::from_slice::<AssetIdentity>(&bytes).ok());
    if recorded.as_ref() == Some(identity) {
        return Ok(());
    }
    for &stale in staged {
        match std::fs::remove_file(stale) {
            Ok(()) => ms_log::runtime_log::log_info(format!("{LOG_TAG} removed a file of another download source: {}", stale.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(tf!("installer.utils.remove_partial_error", path = stale.display(), e = err)),
        }
    }
    let bytes = serde_json::to_vec(identity).map_err(|err| tf!("installer.utils.write_dst_path_error", dst_path = identity_path.display(), e = err))?;
    if let Some(parent) = identity_path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|err| tf!("installer.utils.create_dst_path_error", dst_path = parent.display(), e = err))?;
    }
    std::fs::write(identity_path, bytes).map_err(|err| tf!("installer.utils.write_dst_path_error", dst_path = identity_path.display(), e = err))
}

/// Renames the verified `staged` file onto `dst_path` (replacing an existing one; `rename`
/// replaces on every supported platform), then drops the identity sidecar, which described
/// only the staged files. A sidecar that cannot be removed is logged: the next run then finds
/// nothing staged and downloads from zero, which is correct.
///
/// # Errors
/// A localized message when the rename fails; `staged` is kept and is reused (it is still
/// bound by the sidecar) on the next run.
fn promote_staged(staged: &Path, dst_path: &Path, identity_path: &Path) -> Result<(), String> {
    std::fs::rename(staged, dst_path)
        .map_err(|err| tf!("installer.utils.download_finalize_rename_error", part_path = staged.display(), dst_path = dst_path.display(), e = err))?;
    match std::fs::remove_file(identity_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => ms_log::runtime_log::log_warn(format!("{LOG_TAG} could not remove the identity sidecar: path={} error={err}", identity_path.display())),
    }
    Ok(())
}

/// `done / total` as a progress fraction in `0.0..=1.0` (0 for an empty total), computed in
/// integers so no lossy cast is needed: 1/10000 resolution is far below one pixel of a bar.
fn fraction(done: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    let scaled = (u128::from(done) * 10_000 / u128::from(total)).min(10_000);
    // `scaled` <= 10000 fits u16; the fallback is unreachable.
    f32::from(u16::try_from(scaled).unwrap_or(10_000)) / 10_000.0
}

/// Turns `fetch_verified` events into the window's progress events.
struct ProgressSink<'a> {
    tx: &'a mpsc::Sender<InstallEvent>,
    req: &'a AssetDownload<'a>,
    /// The pinned size of the asset.
    total: u64,
    /// When the last throttled event was sent; `None` before the first one.
    last_emit: Option<Instant>,
}

impl ProgressSink<'_> {
    fn on_event(&mut self, event: FileEvent) {
        let prefix = self.req.label_prefix;
        match event {
            FileEvent::Stream(StreamEvent::Downloading { staged, .. }) => {
                if self.throttled() {
                    return;
                }
                let label = format!("{prefix}: {} / {}", format_bytes(staged), format_bytes(self.total));
                self.send(fraction(staged, self.total), label);
            }
            // Never throttled: each countdown second of a retry wait is shown.
            FileEvent::Stream(StreamEvent::Retrying { attempt, max_attempts, delay_secs, staged }) => {
                let label = tf!("installer.utils.download_retrying_progress", label_prefix = prefix, delay_secs = delay_secs, attempt = attempt, max_attempts = max_attempts);
                self.send(fraction(staged, self.total), label);
            }
            FileEvent::Verifying { done, total } => {
                if self.throttled() {
                    return;
                }
                let label = tf!("installer.utils.download_verifying_progress", label_prefix = prefix, done = format_bytes(done), total = format_bytes(total));
                self.send(fraction(done, total), label);
            }
            FileEvent::Waiting => {
                self.send(0.0, tf!("installer.utils.download_waiting_progress", label_prefix = prefix));
            }
        }
    }

    /// Whether a throttled event must be skipped now; records the send time otherwise.
    fn throttled(&mut self) -> bool {
        let now = Instant::now();
        if self.last_emit.is_some_and(|last| now.duration_since(last) < PROGRESS_INTERVAL) {
            return true;
        }
        self.last_emit = Some(now);
        false
    }

    fn send(&self, stage_value: f32, stage_label: String) {
        let (start, end) = self.req.overall_range;
        let overall_label = tf!("installer.utils.downloading_progress", label_prefix = self.req.label_prefix);
        send_progress(self.tx, stage_value, stage_label, start + stage_value * (end - start), overall_label);
    }
}

#[cfg(test)]
mod tests;
