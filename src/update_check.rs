/*
File: src/update_check.rs

Purpose:
The startup check for a newer ManhwaStudio release on GitHub, run once per process on a
worker. Its result is the launcher's update notice (`UpdateNotification`); the update itself
runs in the installer's update window after the main window has closed.

Key functions:
- `spawn(force_update_available)`: starts the named worker and returns the one-shot receiver.
- `fetch_latest_release_tag()`: the tag of the newest updatable release.

Notes:
The release comparison is `ms_config::version_format::is_remote_newer`; the release feed URL
and the choice of the release (`ms_installer::release::select_app_release`: archive, this
platform's binary and `SHA256SUMS`, the same rule the update window applies) are
`ms_installer::release`'s; this file owns neither, only its own lighter request policy.
Native only (`ms_download::fetch_small`, OS threads). The shell (`src/shell/`) starts it on the first launcher entry
and caches the answer for every later entry. The local side is `MS_APP_VERSION`, the one
program version (see the version contract in `crates/ms-config/src/version_format.rs`).
*/

use crate::launcher::state::UpdateNotification;
use crate::runtime_log;
use crate::version_format::is_remote_newer;
use ms_installer::release::{APP_RELEASES_API, select_app_release};
use ms_download::resumable::{FetcherConfig, RetryPolicy, UreqFetcher};
use ms_download::{SmallRequest, fetch_small};
use ms_thread as thread;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use web_time::Duration;

/// Largest accepted release-list answer (one page of releases is well under 1 MiB).
const RELEASES_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Starts the update check on a named worker and returns its one-shot receiver.
///
/// The worker sends `Some(notification)` when a newer release exists (or when
/// `force_update_available`, the `--test-ver-check` override, pretends one does) and `None`
/// otherwise; a failed check is logged and reported as `None`. A failed spawn is logged and the
/// receiver then reports `Disconnected`, which the caller treats as "no update".
pub(crate) fn spawn(force_update_available: bool) -> mpsc::Receiver<Option<UpdateNotification>> {
    let (tx, rx) = mpsc::channel();
    // The one program version: compared with the release tag and shown in the notice.
    let local_version = env!("MS_APP_VERSION").to_string();

    let spawn_result = thread::Builder::new()
        .name("startup-version-check".to_string())
        .spawn(move || {
            let notification = if force_update_available {
                Some(UpdateNotification { local_version, remote_version: "test-update".to_string() })
            } else {
                match fetch_latest_release_tag() {
                    Ok(remote_version) if is_remote_newer(&local_version, &remote_version) => {
                        Some(UpdateNotification { local_version, remote_version })
                    }
                    Ok(remote_version) => {
                        runtime_log::log_info(format!("[startup-version-check] no update: local={local_version}, remote={remote_version}"));
                        None
                    }
                    Err(err) => {
                        runtime_log::log_warn(format!("[startup-version-check] {err}"));
                        None
                    }
                }
            };
            if tx.send(notification).is_err() {
                runtime_log::log_info("[startup-version-check] the main window closed before the check finished; result dropped");
            }
        });
    if let Err(err) = spawn_result {
        runtime_log::log_warn(format!("[startup-version-check] failed to spawn version check worker: {err}"));
    }
    rx
}

/// The tag of the newest updatable release (`select_app_release`). Blocking network I/O
/// through `ms_download::fetch_small` (connect timeout 10 s, read timeout 30 s, at most 2
/// retries of a transient failure with backoff, `Range` resume); worker only, never
/// cancelled (the result is dropped when the window is gone). `GITHUB_TOKEN`, when set, is
/// sent as a bearer token (rate limits), never logged.
///
/// # Errors
/// A localized message for a failed request, an unreadable or malformed response, or no
/// updatable release.
fn fetch_latest_release_tag() -> Result<String, String> {
    let bearer = std::env::var("GITHUB_TOKEN").ok().map(|token| token.trim().to_string()).filter(|token| !token.is_empty());
    let fetcher = UreqFetcher::with_config(FetcherConfig {
        user_agent: Some("UpdateScript/1.0".to_string()),
        bearer,
        headers: vec![("Accept".to_string(), "application/vnd.github+json".to_string())],
        connect_timeout: Duration::from_secs(10),
        read_timeout: Duration::from_secs(30),
    });
    // A startup notice is not worth minutes of retries: two, then give up until next start.
    let retry = RetryPolicy { max_retries: 2, ..RetryPolicy::standard() };
    let never_cancelled = AtomicBool::new(false);
    let request = SmallRequest { url: APP_RELEASES_API, max_bytes: RELEASES_MAX_BYTES };
    let body = fetch_small(&fetcher, &retry, &never_cancelled, &request, "[startup-version-check]", "release list")
        .map_err(|e| tf!("startup.update_check.request_error", e = e))?;
    let body = String::from_utf8(body).map_err(|e| tf!("startup.update_check.response_read_error", e = e))?;

    select_app_release(&body).map(|release| release.tag).map_err(|err| err.user_message())
}
