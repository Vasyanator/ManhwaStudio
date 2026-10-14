/*
FILE OVERVIEW: crates/ms-config/src/version_format.rs
Pure, dependency-free composition, stripping and ordering of the application version string.

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
- `compose_app_version()`: builds the human-facing version out of the Cargo version, the
  optional git describe result and the dirty flag.
- `version_core()`: strips semver build metadata, for the machine-facing comparisons.
- `compare_release_versions()` / `is_remote_newer()`: THE release-version ordering, used by
  the startup update check, the installer's update window and its updater / PyTorch-minimum
  checks. There is no other comparator; do not add one.

Notes:
Items compiled into the build script but not called there carry `#[allow(dead_code)]`
(not `#[expect]`: on the crate side they ARE used, so an expectation would be unfulfilled).
Names are fully qualified (`std::cmp::Ordering`) because `include!` pastes them into the
build script's root, where an extra `use` could collide.
*/

/// Composes the human-facing application version string.
///
/// `base_version` is the version declared in `Cargo.toml` (`CARGO_PKG_VERSION`); it is
/// trimmed but otherwise reproduced verbatim. `git_info` is `Some((short_hash, distance))`
/// where `distance` is the number of commits between the last reachable tag and `HEAD`
/// (`git describe --long`), or `None` when git is unavailable, the source tree is not a
/// repository, or any git command failed. `dirty` reports uncommitted changes in the
/// source directories only (`src/`, `crates/`).
///
/// Shape of the result:
/// - `git_info == None` -> `base_version` verbatim, `dirty` ignored. This is the
///   GitHub-source-ZIP case: without any git identification a lone dirty marker would
///   claim knowledge the build does not have.
/// - distance `0` (HEAD exactly on a tag) -> `base_version`, plus `+dirty` when dirty.
/// - distance `n > 0` -> `base_version+<hash>-<n>`, plus `-dirty` when dirty.
///
/// The marker separator differs on purpose: the first `+` opens semver build metadata, so
/// the marker joins with `+` when no `+` exists yet and with `-` when one already does.
///
/// Never fails and never panics; the result is always non-empty when `base_version` is.
// Compiled into both the crate and the build script; the crate itself only reads the
// composed value out of the environment, so this is dead code on the crate side.
#[allow(dead_code)]
pub fn compose_app_version(base_version: &str, git_info: Option<(&str, u32)>, dirty: bool) -> String {
    let base = base_version.trim();
    let Some((short_hash, distance)) = git_info else {
        return base.to_string();
    };

    if distance == 0 {
        if dirty { format!("{base}+dirty") } else { base.to_string() }
    } else if dirty {
        format!("{base}+{short_hash}-{distance}-dirty")
    } else {
        format!("{base}+{short_hash}-{distance}")
    }
}

/// Returns the semver core of `version`: the input trimmed, with build metadata
/// (everything from the first `+`) removed.
///
/// This is the machine-facing half of the version contract. Anything compared against a
/// release tag, a Python-side constant or another process's `--version` output must be
/// reduced with this function first, so that a git-suffixed development build still
/// compares as the release it was built from.
///
/// Pre-release identifiers (a leading `-` segment) are intentionally preserved: they are
/// part of the semver core and carry ordering meaning. Malformed and empty input is
/// returned as-is (possibly empty); the function never panics.
// See the note on `compose_app_version`: unused on the build-script side of the include.
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
// See the note on `compose_app_version`: unused on the build-script side of the include.
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
// See the note on `compose_app_version`: unused on the build-script side of the include.
#[allow(dead_code)]
pub fn is_remote_newer(local: &str, remote: &str) -> bool {
    compare_release_versions(remote, local).is_gt()
}

/// Splits a version string into the parts [`compare_release_versions`] orders.
// See the note on `compose_app_version`: unused on the build-script side of the include.
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
// See the note on `compose_app_version`: unused on the build-script side of the include.
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
enum ReleaseVersionPart {
    Number(u64),
    Text(String),
}

#[cfg(test)]
mod tests {
    use super::{compare_release_versions, compose_app_version, is_remote_newer, version_core};
    use std::cmp::Ordering;

    #[test]
    fn no_git_info_returns_the_base_version_verbatim() {
        assert_eq!(compose_app_version("3.6.0", None, false), "3.6.0");
        // A source ZIP has no git data at all, so even a "dirty" probe result must not
        // decorate the version.
        assert_eq!(compose_app_version("3.6.0", None, true), "3.6.0");
        assert_eq!(compose_app_version("  3.6.0\n", None, false), "3.6.0");
    }

    #[test]
    fn on_tag_clean_returns_the_base_version() {
        assert_eq!(compose_app_version("3.6.0", Some(("1cd9638", 0)), false), "3.6.0");
    }

    #[test]
    fn on_tag_dirty_appends_the_marker_with_a_plus() {
        assert_eq!(compose_app_version("3.6.0", Some(("1cd9638", 0)), true), "3.6.0+dirty");
    }

    #[test]
    fn ahead_of_tag_clean_appends_hash_and_distance() {
        assert_eq!(compose_app_version("3.6.0", Some(("1cd9638", 83)), false), "3.6.0+1cd9638-83");
    }

    #[test]
    fn ahead_of_tag_dirty_appends_the_marker_with_a_dash() {
        assert_eq!(compose_app_version("3.6.0", Some(("1cd9638", 83)), true), "3.6.0+1cd9638-83-dirty");
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
}
