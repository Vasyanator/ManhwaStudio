/*
FILE OVERVIEW: src/version_format.rs
Pure, dependency-free composition and stripping of the application version string.

This file is compiled TWICE, deliberately:
- as a normal crate module (`mod version_format;` in `src/main.rs`), and
- as part of the build script, which pulls it in with `include!("src/version_format.rs")`.

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
pub(crate) fn compose_app_version(base_version: &str, git_info: Option<(&str, u32)>, dirty: bool) -> String {
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
pub(crate) fn version_core(version: &str) -> &str {
    let trimmed = version.trim();
    trimmed.find('+').map_or(trimmed, |index| &trimmed[..index])
}

#[cfg(test)]
mod tests {
    use super::{compose_app_version, version_core};

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
}
