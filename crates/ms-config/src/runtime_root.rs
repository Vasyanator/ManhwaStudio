/*
File: crates/ms-config/src/runtime_root.rs

Purpose:
The pure rules of runtime-root resolution: which directory `program_dir()` / `data_dir()`
(`lib.rs`) return, decided over an injected marker oracle so every precedence row is
unit-testable without a file system.

Key items:
- `repo_build_root()`: the repository-build rule (`<repo>/target/<any profile>/<exe>` ->
  `<repo>`). The ONE owner of that rule: `ms-os-integration`'s `copy_identity` calls it.
- `resolve_runtime_root_with()`: the precedence (working directory, executable directory,
  repository root, fallback) over a `has_markers` oracle.

Notes:
No I/O here. `lib.rs::resolve_runtime_root` gathers the working directory and executable path,
handles the macOS `.app` bundle first and passes the real `dir_has_program_markers` probe in.
*/

use std::path::{Path, PathBuf};

/// The repository root when `exe` is a repository build: its directory (any name — `release`,
/// `debug`, a custom profile) lies directly in a directory named `target`, and the root is the
/// directory that contains `target`. The name compares ASCII case-insensitively on Windows
/// (case-insensitive file system) and exactly elsewhere. `None` for any other layout, and when
/// the root would be empty (a relative `target/<profile>/<exe>`). Pure (path components only).
#[must_use]
pub fn repo_build_root(exe: &Path) -> Option<PathBuf> {
    repo_build_root_with(exe, cfg!(target_os = "windows"))
}

/// [`repo_build_root`] with the case rule explicit, so both platforms' rules are testable on
/// every host.
fn repo_build_root_with(exe: &Path, case_insensitive: bool) -> Option<PathBuf> {
    let profile_dir = exe.parent()?;
    // A profile directory needs a real name: `<x>/target/..` or `target/` alone is no layout.
    profile_dir.file_name()?;
    let target_dir = profile_dir.parent()?;
    let target_name = target_dir.file_name()?.to_str()?;
    let is_target = if case_insensitive { target_name.eq_ignore_ascii_case("target") } else { target_name == "target" };
    if !is_target {
        return None;
    }
    target_dir.parent().filter(|root| !root.as_os_str().is_empty()).map(Path::to_path_buf)
}

/// The portable runtime root (everything after the macOS `.app` bundle rule) for the working
/// directory `cwd` and executable path `exe` (either `None` when the OS could not report it),
/// decided by the marker oracle `has_markers` (`dir_has_program_markers` in production):
///
/// 1. `cwd` when it has markers — a launch from a copy's root (records, run-dev scripts, a
///    checkout) always uses that root, even for another copy's executable;
/// 2. else the executable directory when it has markers — an installed or unpacked copy is
///    self-contained wherever it was started from;
/// 3. else the repository root of a repository build ([`repo_build_root`]) when it has markers —
///    a bare build output only works with its checkout, and a launch with an unrelated working
///    directory (Windows "Open with" or a double-click start in the file's or the shell's
///    directory) must still find it;
/// 4. else `cwd`, else the executable directory, else `.`.
///
/// Pure over `has_markers`; it probes at most the three candidate directories.
pub(crate) fn resolve_runtime_root_with(cwd: Option<&Path>, exe: Option<&Path>, has_markers: &dyn Fn(&Path) -> bool) -> PathBuf {
    if let Some(cwd) = cwd
        && has_markers(cwd)
    {
        return cwd.to_path_buf();
    }
    let exe_dir = exe.and_then(Path::parent).filter(|dir| !dir.as_os_str().is_empty());
    if let Some(exe_dir) = exe_dir
        && has_markers(exe_dir)
    {
        return exe_dir.to_path_buf();
    }
    if let Some(repo_root) = exe.and_then(repo_build_root)
        && has_markers(&repo_root)
    {
        return repo_root;
    }
    cwd.or(exe_dir).map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The repository-build rule: any profile directory directly under `target`, Linux and
    /// Windows path shapes, the case rule per platform, and the layouts that do not match.
    #[test]
    fn repo_build_root_matches_target_profile_layouts_only() {
        let root = |exe: &str, ci: bool| repo_build_root_with(Path::new(exe), ci);
        for ci in [false, true] {
            assert_eq!(root("/home/u/src/ms/target/release/manhwastudio_rs", ci), Some(PathBuf::from("/home/u/src/ms")));
            assert_eq!(root("/home/u/src/ms/target/debug/manhwastudio_rs", ci), Some(PathBuf::from("/home/u/src/ms")));
            assert_eq!(root("/home/u/src/ms/target/release-lto/manhwastudio_rs", ci), Some(PathBuf::from("/home/u/src/ms")));
            // Trailing separators of the components do not matter.
            assert_eq!(root("/home/u/src/ms//target/release//manhwastudio_rs", ci), Some(PathBuf::from("/home/u/src/ms")));
            // A directory named `release` that is not under `target`.
            assert_eq!(root("/home/u/release/manhwastudio_rs", ci), None);
            assert_eq!(root("/home/u/build/release/manhwastudio_rs", ci), None);
            // `target` as the executable's own directory, or two levels too high.
            assert_eq!(root("/home/u/src/ms/target/manhwastudio_rs", ci), None);
            assert_eq!(root("/home/u/src/ms/target/x86_64-pc-windows-gnu/release/manhwastudio_rs", ci), None);
            // Root edge cases: `target` directly under the file-system root, no room for a
            // root, a relative layout, a bare file name.
            assert_eq!(root("/target/release/manhwastudio_rs", ci), Some(PathBuf::from("/")));
            assert_eq!(root("/release/manhwastudio_rs", ci), None);
            assert_eq!(root("target/release/manhwastudio_rs", ci), None);
            assert_eq!(root("manhwastudio_rs", ci), None);
            assert_eq!(root("", ci), None);
            // Windows path shape (forward slashes: a Linux `Path` does not split on `\`).
            assert_eq!(root("C:/src/ms/target/release/manhwastudio_rs.exe", ci), Some(PathBuf::from("C:/src/ms")));
            assert_eq!(root("C:/Program Files/ManhwaStudio/manhwastudio_rs.exe", ci), None);
        }
        // The name rule: case-insensitive on Windows only.
        assert_eq!(root("C:/src/ms/Target/Release/manhwastudio_rs.exe", true), Some(PathBuf::from("C:/src/ms")));
        assert_eq!(root("/home/u/src/ms/Target/release/manhwastudio_rs", false), None);
        assert_eq!(root("/home/u/src/ms/targets/release/manhwastudio_rs", true), None);
    }

    /// The precedence table of the runtime root: every documented row, over a marker oracle.
    #[test]
    fn runtime_root_prefers_cwd_then_exe_dir_then_repo_root_then_fallback() {
        let resolve = |cwd: Option<&str>, exe: Option<&str>, marked: &[&str]| {
            let marked: Vec<PathBuf> = marked.iter().map(PathBuf::from).collect();
            resolve_runtime_root_with(cwd.map(Path::new), exe.map(Path::new), &|dir: &Path| marked.iter().any(|m| m == dir))
        };
        let repo_exe = Some("/home/u/src/ms/target/release/manhwastudio_rs");
        let installed_exe = Some("/opt/ms/manhwastudio_rs");

        // 1. A marked working directory wins over everything, even another copy's root.
        assert_eq!(resolve(Some("/opt/ms"), repo_exe, &["/opt/ms", "/home/u/src/ms"]), PathBuf::from("/opt/ms"));
        assert_eq!(resolve(Some("/home/u/src/ms"), installed_exe, &["/opt/ms", "/home/u/src/ms"]), PathBuf::from("/home/u/src/ms"));
        // 2. Installed copy started from an unrelated directory: its own directory.
        assert_eq!(resolve(Some("/home/u/Pictures"), installed_exe, &["/opt/ms"]), PathBuf::from("/opt/ms"));
        // A marked executable directory beats the repository root, even under `target/`.
        assert_eq!(resolve(Some("/home/u"), repo_exe, &["/home/u/src/ms/target/release", "/home/u/src/ms"]), PathBuf::from("/home/u/src/ms/target/release"));
        // 3. Repository build started elsewhere ("Open with", double-click): the checkout, for
        //    any profile name, with or without a working directory.
        assert_eq!(resolve(Some("C:/Windows/System32"), repo_exe, &["/home/u/src/ms"]), PathBuf::from("/home/u/src/ms"));
        assert_eq!(resolve(Some("/home/u/Pictures"), Some("/home/u/src/ms/target/debug/manhwastudio_rs"), &["/home/u/src/ms"]), PathBuf::from("/home/u/src/ms"));
        assert_eq!(resolve(Some("/home/u/Pictures"), Some("/home/u/src/ms/target/my-profile/manhwastudio_rs"), &["/home/u/src/ms"]), PathBuf::from("/home/u/src/ms"));
        assert_eq!(resolve(None, repo_exe, &["/home/u/src/ms"]), PathBuf::from("/home/u/src/ms"));
        // A build output outside `target/<profile>` gets no repository rescue.
        assert_eq!(resolve(Some("/home/u/Pictures"), Some("/home/u/src/ms/build/manhwastudio_rs"), &["/home/u/src/ms"]), PathBuf::from("/home/u/Pictures"));
        // 4. An unmarked repository root is no runtime root: the old fallback stays.
        assert_eq!(resolve(Some("/home/u/Pictures"), repo_exe, &[]), PathBuf::from("/home/u/Pictures"));
        assert_eq!(resolve(None, repo_exe, &[]), PathBuf::from("/home/u/src/ms/target/release"));
        assert_eq!(resolve(None, None, &[]), PathBuf::from("."));
        assert_eq!(resolve(None, Some("manhwastudio_rs"), &[]), PathBuf::from("."));
    }
}
