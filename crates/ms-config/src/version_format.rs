/*
FILE OVERVIEW: crates/ms-config/src/version_format.rs
Pure, dependency-free parsing, stripping and ordering of the application version string.

The program version (`MS_APP_VERSION`, emitted by `build.rs`) is the commit count of the commit
a build was made from (`git rev-list --count HEAD`, e.g. `335`), or the full `CARGO_PKG_VERSION`
(e.g. `3.6.0`, `330.0.0`) when no repository is available. It is the ONE version of a build:
displayed, printed by `--version`, compared against release tags (plain numbers, `330`) and
handed to other processes. `CARGO_PKG_VERSION` is packaging metadata only (Windows FILEVERSION).

This file is compiled TWICE, deliberately:
- as a normal module of `ms-config` (re-exported by the binary as `crate::version_format`), and
- as part of the build script, which pulls it in with `include!("crates/ms-config/src/version_format.rs")`.

That is the only way the exact code `build.rs` runs is also covered by `cargo test`: a
`#[cfg(test)] mod tests` inside `build.rs` would never be executed by the test harness,
while `#[cfg(test)]` is simply false when the build script is compiled, so the test module
below is skipped there.

Because of that dual compilation the file must stay std-only: no `t!`/`tf!`, no logging, no
reference to any other item of the crate. Keep it that way.

Key functions:
- `parse_commit_count()`: strict parse of `git rev-list --count HEAD` output (used by `build.rs`).
- `version_core()`: strips semver build metadata. A current build's version carries none; it
  is still needed for ANOTHER copy's `--version` output, which older builds print as
  `3.6.0+<hash>-<n>[-dirty]`.
- `compare_release_versions()` / `is_remote_newer()`: THE release-version ordering, used by
  the startup update check, the installer's update window and its updater / PyTorch-minimum
  checks. There is no other comparator; do not add one.

Notes:
Items compiled into the build script but not called there carry `#[allow(dead_code)]`
(not `#[expect]`: on the crate side they ARE used, so an expectation would be unfulfilled).
Names are fully qualified (`std::cmp::Ordering`) because `include!` pastes them into the
build script's root, where an extra `use` could collide.
*/

/// Parses the stdout of `git rev-list --count HEAD` into the commit count that IS this
/// build's program version (`MS_APP_VERSION`, see `build.rs`).
///
/// Strict on purpose, because a wrong number is a wrong release identity: after trimming
/// surrounding whitespace the text must be one or more ASCII digits (no sign, no inner
/// whitespace, no second line) and the value must be a positive `u64` — a repository whose
/// `HEAD` resolves has at least one commit. Anything else is `None`, and the caller falls back
/// to `CARGO_PKG_VERSION`. Never panics.
#[must_use]
pub fn parse_commit_count(output: &str) -> Option<u64> {
    let trimmed = output.trim();
    // `u64::from_str` alone would also accept a leading `+`.
    if trimmed.is_empty() || !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<u64>().ok().filter(|count| *count > 0)
}

/// Returns the semver core of `version`: the input trimmed, with build metadata
/// (everything from the first `+`) removed.
///
/// A version this build produces (`MS_APP_VERSION`) has no build metadata, so for it this is
/// the identity. It exists for the OTHER side of a cross-process comparison: an installed copy
/// built before commit-count versioning prints `3.6.0+<hash>-<n>[-dirty]` from `--version`, and
/// that must compare as the `3.6.0` release it was built from. Callers comparing a probed
/// `--version` output against a release tag reduce both sides with this function first.
///
/// Pre-release identifiers (a leading `-` segment) are intentionally preserved: they are
/// part of the semver core and carry ordering meaning. Malformed and empty input is
/// returned as-is (possibly empty); the function never panics.
// Compiled into the build script too, which never calls it (see the file header Notes).
#[allow(dead_code)]
pub fn version_core(version: &str) -> &str {
    let trimmed = version.trim();
    trimmed.find('+').map_or(trimmed, |index| &trimmed[..index])
}

/// Orders two release version strings: `Greater` when `left` is the newer release.
///
/// Both sides are trimmed and lose ONE leading `v` / `V`; the rest is split on `.`, `-`, `+`
/// and `_` (empty parts dropped). Parts are compared pairwise: two numeric parts (`u64`)
/// numerically, two text parts by their ASCII-lowercased bytes, and a numeric part outranks a
/// text part (so `1.0.1 > 1.0.beta`). When the common parts are equal the version with MORE
/// parts is greater (`1.0 < 1.0.0`, `3.6.0 < 3.6.0-beta`).
///
/// This is deliberately not full semver: a pre-release suffix and build metadata are just
/// further parts, so callers that may see build metadata reduce both sides with
/// [`version_core`] first. A part that overflows `u64` counts as text. Never panics.
// Compiled into the build script too, which never calls it (see the file header Notes).
#[allow(dead_code)]
pub fn compare_release_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let left_parts = release_version_parts(left);
    let right_parts = release_version_parts(right);
    for (left_part, right_part) in left_parts.iter().zip(right_parts.iter()) {
        let ordering = match (left_part, right_part) {
            (ReleaseVersionPart::Number(left), ReleaseVersionPart::Number(right)) => left.cmp(right),
            (ReleaseVersionPart::Text(left), ReleaseVersionPart::Text(right)) => left.cmp(right),
            (ReleaseVersionPart::Number(_), ReleaseVersionPart::Text(_)) => std::cmp::Ordering::Greater,
            (ReleaseVersionPart::Text(_), ReleaseVersionPart::Number(_)) => std::cmp::Ordering::Less,
        };
        if !ordering.is_eq() {
            return ordering;
        }
    }
    left_parts.len().cmp(&right_parts.len())
}

/// Whether release `remote` is strictly newer than `local`, per [`compare_release_versions`].
// Compiled into the build script too, which never calls it (see the file header Notes).
#[allow(dead_code)]
pub fn is_remote_newer(local: &str, remote: &str) -> bool {
    compare_release_versions(remote, local).is_gt()
}

/// Splits a version string into the parts [`compare_release_versions`] orders.
// Compiled into the build script too, which never calls it (see the file header Notes).
#[allow(dead_code)]
fn release_version_parts(version: &str) -> Vec<ReleaseVersionPart> {
    let trimmed = version.trim();
    // Exactly one leading `v` / `V` is a tag prefix (`v3.6.0`), not part of the version.
    let normalized = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
    normalized
        .split(['.', '-', '+', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| match part.parse::<u64>() {
            Ok(number) => ReleaseVersionPart::Number(number),
            Err(_) => ReleaseVersionPart::Text(part.to_ascii_lowercase()),
        })
        .collect()
}

/// One component of a release version string (see [`compare_release_versions`]).
// Compiled into the build script too, which never calls it (see the file header Notes).
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
enum ReleaseVersionPart {
    Number(u64),
    Text(String),
}

#[cfg(test)]
mod tests {
    use super::{compare_release_versions, is_remote_newer, parse_commit_count, version_core};
    use std::cmp::Ordering;

    #[test]
    fn commit_count_parses_plain_decimal_output() {
        assert_eq!(parse_commit_count("323\n"), Some(323));
        assert_eq!(parse_commit_count("  1 "), Some(1));
        assert_eq!(parse_commit_count("330"), Some(330));
    }

    #[test]
    fn commit_count_rejects_anything_but_a_positive_decimal() {
        assert_eq!(parse_commit_count(""), None);
        assert_eq!(parse_commit_count("  \n"), None);
        assert_eq!(parse_commit_count("0"), None);
        assert_eq!(parse_commit_count("+5"), None);
        assert_eq!(parse_commit_count("-5"), None);
        assert_eq!(parse_commit_count("3 3"), None);
        assert_eq!(parse_commit_count("3\n4"), None);
        assert_eq!(parse_commit_count("3.6.0"), None);
        assert_eq!(parse_commit_count("fatal: not a git repository"), None);
        // Overflows u64.
        assert_eq!(parse_commit_count("18446744073709551616"), None);
    }

    #[test]
    fn version_core_passes_plain_versions_through() {
        assert_eq!(version_core("3.6.0"), "3.6.0");
        assert_eq!(version_core("  3.6.0 "), "3.6.0");
        assert_eq!(version_core("3.6.0-beta.1"), "3.6.0-beta.1");
    }

    #[test]
    fn version_core_strips_build_metadata() {
        assert_eq!(version_core("3.6.0+1cd9638-83-dirty"), "3.6.0");
        assert_eq!(version_core("3.6.0+dirty"), "3.6.0");
        assert_eq!(version_core("3.6.0+a+b"), "3.6.0");
    }

    #[test]
    fn version_core_tolerates_empty_and_malformed_input() {
        assert_eq!(version_core(""), "");
        assert_eq!(version_core("   "), "");
        assert_eq!(version_core("+"), "");
        assert_eq!(version_core("+1cd9638"), "");
        assert_eq!(version_core("not-a-version"), "not-a-version");
    }

    // The cases below are the union of the three test sets the former per-call-site copies
    // carried (startup update check, installer update window, installer utils).

    #[test]
    fn newer_release_tags_are_detected() {
        assert!(is_remote_newer("3.6.0", "v3.7.0"));
        assert!(is_remote_newer("3.6.0", "3.6.1"));
        assert!(!is_remote_newer("3.6.0", "3.6.0"));
        assert!(!is_remote_newer("3.6.1", "V3.6.0"));
    }

    #[test]
    fn numeric_parts_outrank_text_and_longer_versions_win_ties() {
        assert_eq!(compare_release_versions("1.0.1", "1.0.beta"), Ordering::Greater);
        assert_eq!(compare_release_versions("1.0", "1.0.0"), Ordering::Less);
        assert_eq!(compare_release_versions(" v2.0 ", "2.0"), Ordering::Equal);
    }

    #[test]
    fn compares_numeric_release_versions() {
        assert_eq!(compare_release_versions("v3.5.0", "3.4.9"), Ordering::Greater);
        assert_eq!(compare_release_versions("3.4.0", "v3.4.0"), Ordering::Equal);
        assert_eq!(compare_release_versions("3.3.9", "3.4.0"), Ordering::Less);
    }

    #[test]
    fn detects_remote_update_against_local_version() {
        assert!(is_remote_newer("3.4.0", "v3.4.1"));
        assert!(!is_remote_newer("3.4.0", "v3.4.0"));
        assert!(!is_remote_newer("3.4.1", "v3.4.0"));
    }

    #[test]
    fn compare_release_versions_handles_prefixed_numeric_versions() {
        assert_eq!(compare_release_versions("2.9.1", "2.9.0"), Ordering::Greater);
        assert_eq!(compare_release_versions("v2.9.1", "2.9.1"), Ordering::Equal);
        assert_eq!(compare_release_versions("2.8.0", "2.9.1"), Ordering::Less);
    }

    #[test]
    fn compare_release_versions_pins_the_non_semver_edge_cases() {
        // Numbers compare numerically, not lexically.
        assert_eq!(compare_release_versions("3.10.0", "3.9.0"), Ordering::Greater);
        // A pre-release suffix is just one more part: it makes the version LONGER, hence newer.
        assert_eq!(compare_release_versions("3.6.0-beta", "3.6.0"), Ordering::Greater);
        // Text compares case-insensitively; only one leading `v` is stripped.
        assert_eq!(compare_release_versions("1.0-ALPHA", "1.0-alpha"), Ordering::Equal);
        assert_eq!(compare_release_versions("vv1", "1"), Ordering::Less);
        // `_` separates parts like `.`; empty parts are dropped.
        assert_eq!(compare_release_versions("3.6_1", "3..6.1"), Ordering::Equal);
        // A part that overflows u64 is text and loses to any number.
        assert_eq!(compare_release_versions("18446744073709551616", "0"), Ordering::Less);
        // Empty input has no parts and is older than anything with parts.
        assert_eq!(compare_release_versions("", "  "), Ordering::Equal);
        assert_eq!(compare_release_versions("", "0"), Ordering::Less);
        // Build metadata is NOT ignored here; callers reduce with `version_core` first.
        assert_eq!(compare_release_versions("3.6.0+dirty", "3.6.0"), Ordering::Greater);
        assert_eq!(compare_release_versions(version_core("3.6.0+dirty"), "3.6.0"), Ordering::Equal);
    }

    // Commit-count versioning contract (program version = commit count, releases tagged with
    // the plain number). The algorithm is unchanged; these pin the outcomes the release
    // scheme relies on.

    #[test]
    fn commit_count_versions_outrank_the_last_semver_release() {
        assert_eq!(compare_release_versions("330", "3.6.0"), Ordering::Greater);
        assert_eq!(compare_release_versions("3.6.0", "330"), Ordering::Less);
        // An installed 3.6.0 sees tag `330` as an update.
        assert!(is_remote_newer("3.6.0", "330"));
    }

    #[test]
    fn cargo_fallback_version_is_not_older_than_its_own_tag() {
        // A build without git reports the full `CARGO_PKG_VERSION` (`330.0.0`); tag `330` must
        // not look newer, or that build would offer the same release forever.
        assert!(!is_remote_newer("330.0.0", "330"));
        assert_ne!(compare_release_versions("330.0.0", "330"), Ordering::Less);
    }

    #[test]
    fn commit_count_updates_only_to_a_higher_count() {
        assert!(!is_remote_newer("335", "330"), "a build ahead of the release must not update back");
        assert!(!is_remote_newer("330", "330"));
        assert!(is_remote_newer("327", "330"));
    }

    #[test]
    fn old_extended_version_output_reduces_below_a_commit_count_tag() {
        // `--version` of a build from before commit-count versioning.
        let old_output = "3.6.0+abc1234-199";
        assert_eq!(compare_release_versions(version_core(old_output), "330"), Ordering::Less);
        assert!(is_remote_newer(version_core(old_output), version_core("330")));
    }
}
