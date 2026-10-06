/*
File: crates/ms-os-integration/src/copy_identity.rs

Purpose:
`CopyIdentity`: which program copy an OS record is written for, or judged against. A record is
"ours" when it names this copy's executable; the program root is the directory a launch from
the record must start in so the runtime-root resolution of `ms_config` lands on this copy.

Key items:
- `CopyIdentity` (`exe`, `program_root`, `version_core`)
- `CopyIdentity::current()`: the running copy.
- `program_root_for()`: the pure program-root rule.
- `repo_build_root()`: re-export of `ms_config::repo_build_root`, the repository-build rule
  (`<repo>/target/<profile>/<exe>`).
- `exe_program_root()`: the program root of a copy known only by its executable path.

Notes:
Native-only (`ms_config` is). `version_core` is what the Windows Uninstall entry's
`DisplayVersion` is written with and judged against (actions and probe); the Linux records do
not use it.
*/

use std::path::{Path, PathBuf};

// The repository-build rule has ONE owner, `ms_config::runtime_root` (the runtime-root
// resolution applies it too); re-exported so this crate names it where it names the copy.
pub use ms_config::repo_build_root;

use crate::IntegrationError;

/// The program copy an OS record belongs to.
///
/// `exe` is the absolute path of the launcher executable (the ownership key of every record);
/// `program_root` is the directory a launch from a record must use as its working directory
/// (see [`program_root_for`]); `version_core` is the copy's `version_core` when the caller judges
/// versions, `None` when it does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyIdentity {
    pub exe: PathBuf,
    pub program_root: PathBuf,
    pub version_core: Option<String>,
}

impl CopyIdentity {
    /// The running copy: `std::env::current_exe()` and the program root derived from it and
    /// from `ms_config::program_dir()` by [`program_root_for`]. Blocking (filesystem probes).
    ///
    /// # Errors
    /// [`IntegrationError::DetermineExe`] when the executable path cannot be resolved.
    pub fn current(version_core: Option<&str>) -> Result<Self, IntegrationError> {
        let exe = std::env::current_exe().map_err(|source| IntegrationError::DetermineExe { source })?;
        let program_root = current_program_root(&exe);
        Ok(Self { exe, program_root, version_core: version_core.map(str::to_owned) })
    }

    /// The repository root when this copy is a repository build (see [`repo_build_root`]),
    /// `None` otherwise. Pure (path components only).
    #[must_use]
    pub fn repo_build_root(&self) -> Option<PathBuf> {
        repo_build_root(&self.exe)
    }

    /// True when no program files sit next to the executable (a build output such as
    /// `target/release`), so its runtime root comes from the working directory or its checkout
    /// (`ms_config::runtime_root`), not from the executable's location. Blocking (filesystem
    /// probe).
    #[must_use]
    pub fn is_dev_copy(&self) -> bool {
        self.exe.parent().is_none_or(|dir| !ms_config::dir_has_program_markers(dir))
    }
}

/// The program root of the RUNNING copy whose executable is `exe`: [`program_root_for`] over
/// the runtime root this run resolved (`ms_config::program_dir()`) and the real marker probe.
/// Blocking (filesystem probes).
pub(crate) fn current_program_root(exe: &Path) -> PathBuf {
    program_root_for(exe, &ms_config::program_dir(), &ms_config::dir_has_program_markers)
}

/// The program root of the copy whose executable is `exe`, given the runtime root this run
/// resolved (`resolved_root`) and the marker probe `has_markers`. Pure over `has_markers`.
///
/// `ms_config` resolves the runtime root as "working directory if it has program markers, else
/// the executable directory if it has them, else a repository build's checkout if it has them,
/// else the working directory" (`ms_config::runtime_root`). A record's `Path=` /
/// working directory becomes the working directory of a launch from it, so the returned
/// directory must make that resolution land on THIS copy:
/// - the repository root when `exe` is a repository build (`<repo>/target/<profile>/<exe>`, see
///   [`repo_build_root`]): a build output has no program files of its own and only works with
///   its checkout, wherever the current run was started from;
/// - else the executable directory when it has markers (an installed copy): it resolves there no
///   matter where the current run was started from, whereas `resolved_root` may be another
///   copy's root that merely was the working directory of this run;
/// - otherwise `resolved_root` when it has markers (a build output elsewhere, started from its
///   checkout: the checkout is the only root that copy works with);
/// - otherwise the executable directory (no root anywhere; a launch then falls back to its
///   working directory, which this makes the executable directory).
#[must_use]
pub fn program_root_for(exe: &Path, resolved_root: &Path, has_markers: &dyn Fn(&Path) -> bool) -> PathBuf {
    if let Some(repo_root) = repo_build_root(exe) {
        return repo_root;
    }
    let exe_dir = exe.parent().unwrap_or(exe);
    if has_markers(exe_dir) {
        exe_dir.to_path_buf()
    } else if has_markers(resolved_root) {
        resolved_root.to_path_buf()
    } else {
        exe_dir.to_path_buf()
    }
}

/// The program root of a copy known only by its executable path (the installer's own records,
/// another copy's records judged by the probe): the repository root of a repository build,
/// else the executable's directory. `None` when `exe` has no non-empty parent. Pure.
#[must_use]
pub fn exe_program_root(exe: &Path) -> Option<PathBuf> {
    repo_build_root(exe).or_else(|| exe.parent().filter(|dir| !dir.as_os_str().is_empty()).map(Path::to_path_buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program-root rule over a marker oracle: every row of the documented table.
    #[test]
    fn program_root_prefers_repo_root_then_exe_dir_then_resolved_root_then_exe_dir() {
        let installed = |dir: &Path| dir == Path::new("/opt/ms") || dir == Path::new("/home/u/other");
        // Installed copy started from inside another copy's root: still its own directory.
        assert_eq!(program_root_for(Path::new("/opt/ms/manhwastudio_rs"), Path::new("/home/u/other"), &installed), PathBuf::from("/opt/ms"));
        // Repository build: the repository root, whatever the run's root and the markers say.
        let none = |_: &Path| false;
        for resolved in ["/home/u/src/ms", "/home/u/other", "/home/u/src/ms/target/release"] {
            assert_eq!(
                program_root_for(Path::new("/home/u/src/ms/target/release/manhwastudio_rs"), Path::new(resolved), &none),
                PathBuf::from("/home/u/src/ms"),
                "resolved {resolved}"
            );
        }
        let everywhere = |_: &Path| true;
        assert_eq!(program_root_for(Path::new("/home/u/src/ms/target/debug/manhwastudio_rs"), Path::new("/home/u/other"), &everywhere), PathBuf::from("/home/u/src/ms"));
        // Build output outside `target/<profile>` started from its checkout: the checkout.
        let checkout = |dir: &Path| dir == Path::new("/home/u/src/ms");
        assert_eq!(program_root_for(Path::new("/home/u/build/out/manhwastudio_rs"), Path::new("/home/u/src/ms"), &checkout), PathBuf::from("/home/u/src/ms"));
        // No markers anywhere: the executable directory.
        assert_eq!(program_root_for(Path::new("/tmp/x/manhwastudio_rs"), Path::new("/home/u"), &none), PathBuf::from("/tmp/x"));
    }

    /// The exe-only program root: repository root for a repository build, else the exe's
    /// directory, `None` without one.
    #[test]
    fn exe_program_root_uses_repo_root_or_exe_dir() {
        assert_eq!(exe_program_root(Path::new("/home/u/src/ms/target/debug/manhwastudio_rs")), Some(PathBuf::from("/home/u/src/ms")));
        assert_eq!(exe_program_root(Path::new("/opt/ms/manhwastudio_rs")), Some(PathBuf::from("/opt/ms")));
        assert_eq!(exe_program_root(Path::new("manhwastudio_rs")), None);
    }
}
