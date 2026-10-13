/*
File: src/update_check.rs

Purpose:
The startup check for a newer ManhwaStudio release on GitHub, run once per process on a
worker. Its result is the launcher's update notice (`UpdateNotification`); the update itself
runs in the installer's update window after the main window has closed.

Key functions:
- `spawn(force_update_available)`: starts the named worker and returns the one-shot receiver.
- `fetch_latest_release_tag()`: the newest release tag that carries the release archive.
- `is_remote_newer(local, remote)`: the release comparison over plain version cores.

Notes:
Native only (`ureq`, OS threads). The shell (`src/shell/`) starts it on the first launcher entry
and caches the answer for every later entry. The comparison runs on `CARGO_PKG_VERSION`, never
on the git-derived display version (see the version split in `src/MODULE_README.md`).
*/

use crate::launcher::state::UpdateNotification;
use crate::runtime_log;
use ms_thread as thread;
use std::cmp::Ordering;
use std::sync::mpsc;
use web_time::Duration;

/// GitHub API endpoint listing this program's releases.
const UPDATE_API_RELEASES: &str = "https://api.github.com/repos/Vasyanator/ManhwaStudio/releases";
/// The release asset a release must carry to count as installable.
const UPDATE_ASSET_NAME: &str = "ManhwaStudio.zip";

/// Starts the update check on a named worker and returns its one-shot receiver.
///
/// The worker sends `Some(notification)` when a newer release exists (or when
/// `force_update_available`, the `--test-ver-check` override, pretends one does) and `None`
/// otherwise; a failed check is logged and reported as `None`. A failed spawn is logged and the
/// receiver then reports `Disconnected`, which the caller treats as "no update".
pub(crate) fn spawn(force_update_available: bool) -> mpsc::Receiver<Option<UpdateNotification>> {
    let (tx, rx) = mpsc::channel();
    // Two values, on purpose. The comparison against a GitHub release tag runs on the
    // plain `CARGO_PKG_VERSION`, so a development build is never mistaken for a newer
    // release than the tag it was built from; the notification is shown to a person and
    // therefore carries the extended, git-derived string.
    let local_version = env!("CARGO_PKG_VERSION").to_string();
    let display_version = env!("MS_APP_VERSION").to_string();

    let spawn_result = thread::Builder::new()
        .name("startup-version-check".to_string())
        .spawn(move || {
            let notification = if force_update_available {
                Some(UpdateNotification { local_version: display_version, remote_version: "test-update".to_string() })
            } else {
                match fetch_latest_release_tag() {
                    Ok(remote_version) if is_remote_newer(&local_version, &remote_version) => {
                        Some(UpdateNotification { local_version: display_version, remote_version })
                    }
                    Ok(remote_version) => {
                        runtime_log::log_info(format!("[startup-version-check] no update: local={display_version}, remote={remote_version}"));
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

/// The tag of the newest release that carries [`UPDATE_ASSET_NAME`]. Blocking network I/O
/// (connect timeout 10 s, read timeout 30 s); worker only. `GITHUB_TOKEN`, when set, is sent as
/// a bearer token (rate limits), never logged.
///
/// # Errors
/// A localized message for a failed request, an unreadable or malformed response, or no
/// release with the asset.
fn fetch_latest_release_tag() -> Result<String, String> {
    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout_read(Duration::from_secs(30)).build();

    let mut req = agent.get(UPDATE_API_RELEASES).set("Accept", "application/vnd.github+json").set("User-Agent", "UpdateScript/1.0");

    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
    }

    let resp = req.call().map_err(|e| tf!("startup.update_check.request_error", e = e))?;
    let body = resp.into_string().map_err(|e| tf!("startup.update_check.response_read_error", e = e))?;

    let releases: serde_json::Value = serde_json::from_str(&body).map_err(|e| tf!("startup.update_check.json_error", e = e))?;
    let releases = releases.as_array().ok_or_else(|| t!("startup.update_check.unexpected_format").to_string())?;

    for rel in releases {
        let tag = rel
            .get("tag_name")
            .and_then(|v| v.as_str())
            .or_else(|| rel.get("name").and_then(|v| v.as_str()))
            .unwrap_or("")
            .trim();
        if tag.is_empty() {
            continue;
        }

        let has_zip = rel
            .get("assets")
            .and_then(|v| v.as_array())
            .is_some_and(|assets| assets.iter().any(|asset| asset.get("name").and_then(|v| v.as_str()) == Some(UPDATE_ASSET_NAME)));

        if has_zip {
            return Ok(tag.to_string());
        }
    }

    Err(tf!("startup.update_check.asset_not_found", UPDATE_ASSET_NAME = UPDATE_ASSET_NAME))
}

/// Whether release `remote` is newer than `local` (both plain version strings, an optional
/// leading `v` ignored).
fn is_remote_newer(local: &str, remote: &str) -> bool {
    compare_versions(remote, local).is_gt()
}

/// Orders two version strings part by part (split on `.`, `-`, `+`, `_`); a numeric part
/// outranks a text part, and with equal common parts the longer version is greater.
fn compare_versions(a: &str, b: &str) -> Ordering {
    let pa = parse_version_to_parts(a);
    let pb = parse_version_to_parts(b);

    for (left, right) in pa.iter().zip(pb.iter()) {
        let ord = compare_version_part(left, right);
        if !ord.is_eq() {
            return ord;
        }
    }

    pa.len().cmp(&pb.len())
}

/// Splits a version string into comparable parts (see [`compare_versions`]).
fn parse_version_to_parts(version: &str) -> Vec<VersionPart> {
    let normalized = normalize_version(version);
    normalized
        .split(['.', '-', '+', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| match part.parse::<u64>() {
            Ok(num) => VersionPart::Num(num),
            Err(_) => VersionPart::Text(part.to_ascii_lowercase()),
        })
        .collect()
}

/// Trims whitespace and one leading `v` / `V`.
fn normalize_version(version: &str) -> &str {
    let trimmed = version.trim();
    trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed)
}

/// Orders two parts: numbers numerically, text lexically, a number above text.
fn compare_version_part(left: &VersionPart, right: &VersionPart) -> Ordering {
    match (left, right) {
        (VersionPart::Num(a), VersionPart::Num(b)) => a.cmp(b),
        (VersionPart::Text(a), VersionPart::Text(b)) => a.cmp(b),
        (VersionPart::Num(_), VersionPart::Text(_)) => Ordering::Greater,
        (VersionPart::Text(_), VersionPart::Num(_)) => Ordering::Less,
    }
}

/// One component of a version string.
enum VersionPart {
    Num(u64),
    Text(String),
}

#[cfg(test)]
mod tests {
    use super::{compare_versions, is_remote_newer};
    use std::cmp::Ordering;

    #[test]
    fn newer_release_tags_are_detected() {
        assert!(is_remote_newer("3.6.0", "v3.7.0"));
        assert!(is_remote_newer("3.6.0", "3.6.1"));
        assert!(!is_remote_newer("3.6.0", "3.6.0"));
        assert!(!is_remote_newer("3.6.1", "V3.6.0"));
    }

    #[test]
    fn numeric_parts_outrank_text_and_longer_versions_win_ties() {
        assert_eq!(compare_versions("1.0.1", "1.0.beta"), Ordering::Greater);
        assert_eq!(compare_versions("1.0", "1.0.0"), Ordering::Less);
        assert_eq!(compare_versions(" v2.0 ", "2.0"), Ordering::Equal);
    }
}
