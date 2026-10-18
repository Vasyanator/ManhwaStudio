/*
File: crates/ms-installer/src/release.rs

Purpose:
The one owner of "which ManhwaStudio release is the update" and of its integrity manifest:
the release feed (URL and asset names), the selection of the newest updatable release, and
the release's `SHA256SUMS` list, from which every app download (platform binary, app archive)
takes the sha256 it must match before it may replace the program or be extracted.

Key items:
- `APP_RELEASES_API`, `APP_ZIP_ASSET_NAME`, `APP_CHECKSUMS_ASSET_NAME`: the release feed.
- `select_app_release()`: pure selection of the NEWEST updatable release over the
  releases-list JSON (used by the binary's startup update check, the update window, the binary
  stage and the install worker's archive download); `fetch_latest_app_release()` adds the GET.
- `select_app_release_by_tag()`: pure selection of the release whose tag equals a given program
  version (the `--continue-update` archive stage: the new binary takes the archive of ITS OWN
  release, never a newer one published meanwhile); `fetch_app_release_by_tag()` adds the GET.
- `ChecksumsAsset::fetch()`: downloads (verified against GitHub's digest of the manifest when
  listed) and strictly parses the release's `SHA256SUMS`.
- `ReleaseChecksums::pin_for()`: the required sha256 of one asset (missing entry or a
  disagreeing GitHub digest is a hard error).
- `parse_sha256sums()`: the strict `sha256sum`-format parser.

Notes:
- Selection rule, frozen by the installed 3.6.0 updater: the FIRST release of the list (GitHub's
  order, newest first) whose `tag_name` (else `name`) is non-empty and that carries the archive
  and this platform's binary — plus, for this and later builds, `SHA256SUMS`. A release without
  the manifest is skipped, never taken unverified. The `--continue-update` stage instead takes
  the release whose tag equals its own version (`select_app_release_by_tag`), so the archive
  always belongs to the binary the first stage installed; if that release is not listed the
  stage fails with a user-visible error instead of mixing releases.
- `SHA256SUMS` format (the release script writes `sha256sum` output, one line per other asset,
  sorted by name). What the parser GUARANTEES: every line has the exact syntax
  `<64 lowercase hex><two spaces><bare asset name>\n` (a missing final newline, an uppercase
  digit, a `*` binary marker, a path, whitespace in a name or an empty list is an error) and no
  name appears twice. It does NOT check line order or that every release asset is listed:
  pins are taken only for the assets this program downloads, and a missing entry for one of
  those is a hard error at `ReleaseChecksums::pin_for`.
- Verification itself is `ms_download::fetch_verified` (through `download.rs`): the manifest's
  hex is the file's pin, checked before the file is published, so a mismatch deletes the staged
  bytes and is never retried (`PROJECT_RULES.md` "Network downloads").
- Logs: `[installer-release]` lines in the session log; user-visible errors are localized.
*/

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::AtomicBool;

use serde::Deserialize;

use ms_config::version_format::{compare_release_versions, version_core};

use super::download::{GithubAsset, fetch_small_asset, github_api_get};
use super::utils::platform_binary_asset_name;

/// GitHub API endpoint listing this program's releases. The one copy: the startup update
/// check (`src/update_check.rs`), the update window and the installer's download paths use it.
pub const APP_RELEASES_API: &str = "https://api.github.com/repos/Vasyanator/ManhwaStudio/releases";
/// The application archive asset; a release without it is not installable or updatable.
pub const APP_ZIP_ASSET_NAME: &str = "ManhwaStudio.zip";
/// The release's checksum manifest (`sha256sum` output over every other asset).
pub const APP_CHECKSUMS_ASSET_NAME: &str = "SHA256SUMS";

/// Upper bound on the manifest's size: a handful of lines is a few hundred bytes.
const CHECKSUMS_MAX_BYTES: u64 = 64 * 1024;

/// Log prefix of this module's lines.
const LOG_TAG: &str = "[installer-release]";

/// The newest release this build can update to or install from (see [`select_app_release`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRelease {
    /// The trimmed `tag_name` (else `name`) — the release's version, e.g. `330`.
    pub tag: String,
    /// `ManhwaStudio.zip`.
    pub(crate) archive: GithubAsset,
    /// This platform's executable (`utils::platform_binary_asset_name`).
    pub(crate) binary: GithubAsset,
    /// `SHA256SUMS`.
    pub(crate) checksums: ChecksumsAsset,
}

/// The release a `--continue-update` run takes its archive from: the one whose tag equals the
/// running program's version (see [`select_app_release_by_tag`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchiveRelease {
    /// The trimmed `tag_name` (else `name`).
    pub(crate) tag: String,
    /// `ManhwaStudio.zip`.
    pub(crate) archive: GithubAsset,
    /// `SHA256SUMS`.
    pub(crate) checksums: ChecksumsAsset,
}

/// The `SHA256SUMS` asset of one release, tagged with that release for messages and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChecksumsAsset {
    tag: String,
    asset: GithubAsset,
}

/// Why no release could be selected from a releases-list answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseSelectError {
    /// The answer is not a JSON array of release objects; the parser's text (English).
    Malformed(String),
    /// No listed release carries the archive, this platform's binary AND `SHA256SUMS`.
    /// `binary` names the platform asset that was required.
    NoUpdatableRelease { binary: String },
    /// No listed release has the tag `version` together with the archive and `SHA256SUMS`.
    OwnReleaseMissing { version: String },
}

impl ReleaseSelectError {
    /// The localized message for the user.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::Malformed(error) => tf!("installer.release.malformed_list_error", e = error),
            Self::NoUpdatableRelease { binary } => tf!(
                "installer.release.no_updatable_release_error",
                archive = APP_ZIP_ASSET_NAME,
                binary = binary,
                checksums = APP_CHECKSUMS_ASSET_NAME
            ),
            Self::OwnReleaseMissing { version } => tf!(
                "installer.release.own_release_missing_error",
                version = version,
                archive = APP_ZIP_ASSET_NAME,
                checksums = APP_CHECKSUMS_ASSET_NAME
            ),
        }
    }
}

impl fmt::Display for ReleaseSelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(error) => write!(f, "malformed release list: {error}"),
            Self::NoUpdatableRelease { binary } => {
                write!(f, "no release carries {APP_ZIP_ASSET_NAME}, {binary} and {APP_CHECKSUMS_ASSET_NAME}")
            }
            Self::OwnReleaseMissing { version } => {
                write!(f, "no release {version} with {APP_ZIP_ASSET_NAME} and {APP_CHECKSUMS_ASSET_NAME}")
            }
        }
    }
}

impl std::error::Error for ReleaseSelectError {}

/// One release object of the releases list; only the fields the selection reads.
#[derive(Deserialize)]
struct ReleaseListItem {
    tag_name: Option<String>,
    name: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

/// Selects the newest updatable release from the releases-list JSON `body` for THIS platform's
/// binary asset. Pure; the one selection every update and install path uses (rule in the file
/// header).
///
/// # Errors
/// [`ReleaseSelectError::Malformed`] when `body` is not a release array,
/// [`ReleaseSelectError::NoUpdatableRelease`] when no release carries all three assets.
pub fn select_app_release(body: &str) -> Result<AppRelease, ReleaseSelectError> {
    select_app_release_for(body, platform_binary_asset_name())
}

/// [`select_app_release`] for an explicit platform binary asset name (tests pin every name).
fn select_app_release_for(body: &str, binary_name: &str) -> Result<AppRelease, ReleaseSelectError> {
    for release in parse_release_list(body)? {
        let Some(tag) = release_tag(&release) else {
            continue;
        };
        let (Some(archive), Some(binary), Some(checksums)) = (find_asset(&release, APP_ZIP_ASSET_NAME), find_asset(&release, binary_name), find_asset(&release, APP_CHECKSUMS_ASSET_NAME)) else {
            continue;
        };
        return Ok(AppRelease { checksums: ChecksumsAsset { tag: tag.clone(), asset: checksums }, tag, archive, binary });
    }
    Err(ReleaseSelectError::NoUpdatableRelease { binary: binary_name.to_owned() })
}

/// Selects the release whose tag is the SAME version as `own_version` (this build's
/// `MS_APP_VERSION`), carrying the archive and `SHA256SUMS`; the platform binary is not
/// required (this program already is it). Equality is `compare_release_versions` over the
/// `version_core` of both sides, so tag `330` matches `330` (and `v330`). Newer releases are
/// ignored on purpose: the archive must belong to the binary that is running, or a release
/// published between the two update stages would leave a mixed installation. Pure.
///
/// # Errors
/// [`ReleaseSelectError::Malformed`] when `body` is not a release array,
/// [`ReleaseSelectError::OwnReleaseMissing`] when no such release is listed.
pub(crate) fn select_app_release_by_tag(body: &str, own_version: &str) -> Result<ArchiveRelease, ReleaseSelectError> {
    for release in parse_release_list(body)? {
        let Some(tag) = release_tag(&release) else {
            continue;
        };
        if !compare_release_versions(version_core(&tag), version_core(own_version)).is_eq() {
            continue;
        }
        let (Some(archive), Some(checksums)) = (find_asset(&release, APP_ZIP_ASSET_NAME), find_asset(&release, APP_CHECKSUMS_ASSET_NAME)) else {
            continue;
        };
        return Ok(ArchiveRelease { checksums: ChecksumsAsset { tag: tag.clone(), asset: checksums }, tag, archive });
    }
    Err(ReleaseSelectError::OwnReleaseMissing { version: own_version.trim().to_owned() })
}

/// Parses the releases-list JSON.
fn parse_release_list(body: &str) -> Result<Vec<ReleaseListItem>, ReleaseSelectError> {
    serde_json::from_str(body).map_err(|err| ReleaseSelectError::Malformed(err.to_string()))
}

/// The trimmed `tag_name` (else `name`) of `release`; `None` when empty (the release is skipped).
fn release_tag(release: &ReleaseListItem) -> Option<String> {
    let tag = release.tag_name.as_deref().or(release.name.as_deref()).unwrap_or_default().trim();
    (!tag.is_empty()).then(|| tag.to_owned())
}

/// The asset of `release` named exactly `name`.
fn find_asset(release: &ReleaseListItem, name: &str) -> Option<GithubAsset> {
    release.assets.iter().find(|asset| asset.name == name).cloned()
}

/// Fetches the releases list and selects the newest updatable release. `context` names the
/// caller in log lines; `cancel` stops the request and its retry waits. Blocking network
/// I/O: worker threads only.
///
/// # Errors
/// A localized message: the request failed, or [`ReleaseSelectError::user_message`].
pub(crate) fn fetch_latest_app_release(context: &str, cancel: &AtomicBool) -> Result<AppRelease, String> {
    let body = github_api_get(APP_RELEASES_API, context, cancel).map_err(|e| tf!("installer.utils.get_releases_error", e = e))?;
    let release = select_app_release(&body).map_err(|err| {
        ms_log::runtime_log::log_warn(format!("{LOG_TAG} release selection failed: caller={context} error={err}"));
        err.user_message()
    })?;
    ms_log::runtime_log::log_info(format!("{LOG_TAG} selected release: caller={context} tag={}", release.tag));
    Ok(release)
}

/// Fetches the releases list and selects the release of `own_version`
/// ([`select_app_release_by_tag`]). `context` names the caller in log lines; `cancel` stops the
/// request and its retry waits. Blocking network I/O: worker threads only.
///
/// # Errors
/// A localized message: the request failed, or [`ReleaseSelectError::user_message`].
pub(crate) fn fetch_app_release_by_tag(own_version: &str, context: &str, cancel: &AtomicBool) -> Result<ArchiveRelease, String> {
    let body = github_api_get(APP_RELEASES_API, context, cancel).map_err(|e| tf!("installer.utils.get_releases_error", e = e))?;
    let release = select_app_release_by_tag(&body, own_version).map_err(|err| {
        ms_log::runtime_log::log_error(format!("{LOG_TAG} release selection failed: caller={context} version={own_version} error={err}"));
        err.user_message()
    })?;
    ms_log::runtime_log::log_info(format!("{LOG_TAG} selected own release: caller={context} tag={}", release.tag));
    Ok(release)
}

impl ChecksumsAsset {
    /// Downloads this `SHA256SUMS` and parses it strictly. The body must have exactly the
    /// listed size and, when GitHub lists a sha256 digest for the manifest itself, match it
    /// (`download::fetch_small_asset`). Blocking network I/O (retried per the installer's
    /// policy, cancellable): worker threads only.
    ///
    /// # Errors
    /// A localized message when the download or its digest check fails or the manifest is
    /// malformed.
    pub(crate) fn fetch(&self, cancel: &AtomicBool) -> Result<ReleaseChecksums, String> {
        let body = fetch_small_asset(&self.asset, CHECKSUMS_MAX_BYTES, cancel)
            .map_err(|e| tf!("installer.release.checksums_download_error", checksums = APP_CHECKSUMS_ASSET_NAME, tag = self.tag, e = e))?;
        self.parse(&body)
    }

    /// Parses an already-downloaded manifest `body` of this release.
    fn parse(&self, body: &[u8]) -> Result<ReleaseChecksums, String> {
        let entries = std::str::from_utf8(body)
            .map_err(|_| ChecksumsParseError { line: 1, kind: ChecksumsParseErrorKind::NotUtf8 })
            .and_then(parse_sha256sums)
            .map_err(|err| {
                ms_log::runtime_log::log_error(format!("{LOG_TAG} malformed {APP_CHECKSUMS_ASSET_NAME}: tag={} error={err}", self.tag));
                tf!("installer.release.checksums_malformed_error", checksums = APP_CHECKSUMS_ASSET_NAME, tag = self.tag, line = err.line)
            })?;
        Ok(ReleaseChecksums { tag: self.tag.clone(), entries })
    }
}

/// The parsed `SHA256SUMS` of one release: asset name -> 64 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReleaseChecksums {
    tag: String,
    entries: BTreeMap<String, String>,
}

impl ReleaseChecksums {
    /// The sha256 `asset` must match, to be passed as `AssetDownload::required_sha256`.
    ///
    /// # Errors
    /// A localized, logged message when the manifest has no entry for `asset`, or when GitHub
    /// publishes a well-formed sha256 digest for it that differs from the entry (the release
    /// is inconsistent; nothing is downloaded).
    pub(crate) fn pin_for(&self, asset: &GithubAsset) -> Result<&str, String> {
        let Some(expected) = self.entries.get(&asset.name) else {
            ms_log::runtime_log::log_error(format!("{LOG_TAG} {APP_CHECKSUMS_ASSET_NAME} has no entry: tag={} asset={}", self.tag, asset.name));
            return Err(tf!("installer.release.checksum_missing_error", checksums = APP_CHECKSUMS_ASSET_NAME, tag = self.tag, asset = asset.name));
        };
        let github_sha256 = asset.digest.as_deref().and_then(|digest| digest.strip_prefix("sha256:"));
        if let Some(github) = github_sha256
            && is_lower_hex_sha256(github)
            && github != expected
        {
            ms_log::runtime_log::log_error(format!(
                "{LOG_TAG} {APP_CHECKSUMS_ASSET_NAME} disagrees with the GitHub digest: tag={} asset={} manifest={expected} github={github}",
                self.tag, asset.name
            ));
            return Err(tf!("installer.release.checksum_conflict_error", checksums = APP_CHECKSUMS_ASSET_NAME, tag = self.tag, asset = asset.name));
        }
        Ok(expected)
    }
}

/// A malformed `SHA256SUMS`: the 1-based `line` and what is wrong with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChecksumsParseError {
    pub(crate) line: usize,
    pub(crate) kind: ChecksumsParseErrorKind,
}

/// What is wrong with a `SHA256SUMS` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChecksumsParseErrorKind {
    /// The bytes are not UTF-8.
    NotUtf8,
    /// The list holds no line at all.
    Empty,
    /// The last line lacks its `\n` (a truncated or hand-edited file).
    MissingFinalNewline,
    /// The first 64 characters are not lowercase hex digits.
    BadHash,
    /// The hash is not followed by exactly two spaces.
    BadSeparator,
    /// The name is empty, carries a path separator, a `*` binary marker, whitespace or a
    /// control character.
    BadName,
    /// The same asset name appears twice.
    DuplicateName,
}

impl fmt::Display for ChecksumsParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            ChecksumsParseErrorKind::NotUtf8 => "not UTF-8",
            ChecksumsParseErrorKind::Empty => "no entries",
            ChecksumsParseErrorKind::MissingFinalNewline => "missing final newline",
            ChecksumsParseErrorKind::BadHash => "not 64 lowercase hex digits",
            ChecksumsParseErrorKind::BadSeparator => "hash not followed by exactly two spaces",
            ChecksumsParseErrorKind::BadName => "invalid asset name",
            ChecksumsParseErrorKind::DuplicateName => "duplicate asset name",
        };
        write!(f, "line {}: {what}", self.line)
    }
}

impl std::error::Error for ChecksumsParseError {}

/// Parses `text` in the exact `sha256sum` format of the file header into asset name -> hex.
///
/// Line order and completeness are not checked (see the file header for what is guaranteed).
///
/// # Errors
/// [`ChecksumsParseError`] naming the first offending line (the line after the last one for
/// a missing final newline is never reported: that error names the unterminated line).
pub(crate) fn parse_sha256sums(text: &str) -> Result<BTreeMap<String, String>, ChecksumsParseError> {
    if text.is_empty() {
        return Err(ChecksumsParseError { line: 1, kind: ChecksumsParseErrorKind::Empty });
    }
    let Some(body) = text.strip_suffix('\n') else {
        let line = text.split('\n').count();
        return Err(ChecksumsParseError { line, kind: ChecksumsParseErrorKind::MissingFinalNewline });
    };
    let mut entries = BTreeMap::new();
    for (index, line_text) in body.split('\n').enumerate() {
        let line = index + 1;
        let error = |kind| ChecksumsParseError { line, kind };
        // `get` on a char boundary: a multi-byte character inside the first 64 bytes is not hex.
        let hash = line_text.get(..64).filter(|hash| is_lower_hex_sha256(hash)).ok_or_else(|| error(ChecksumsParseErrorKind::BadHash))?;
        let name = line_text[64..].strip_prefix("  ").ok_or_else(|| error(ChecksumsParseErrorKind::BadSeparator))?;
        if !is_plain_asset_name(name) {
            return Err(error(ChecksumsParseErrorKind::BadName));
        }
        if entries.insert(name.to_owned(), hash.to_owned()).is_some() {
            return Err(error(ChecksumsParseErrorKind::DuplicateName));
        }
    }
    Ok(entries)
}

/// Exactly 64 lowercase hex digits.
fn is_lower_hex_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A bare release asset name: non-empty, no path separator, no leading `*` (sha256sum's binary
/// marker, which would follow ONE space and so already fails the separator check), no
/// whitespace and no control character.
fn is_plain_asset_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('*') && !name.chars().any(|ch| ch == '/' || ch == '\\' || ch.is_whitespace() || ch.is_control())
}

#[cfg(test)]
mod tests;
