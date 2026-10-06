/*
File: crates/ms-os-integration/src/linux/xdg.rs

Purpose:
The XDG base directories the desktop entry lives under, and the startup writer that keeps the
running copy's entry and icon current without ever taking over another copy's entry.

Key items:
- `DesktopDirs` + `DesktopDirs::from_env()`: `$XDG_DATA_HOME` / `$XDG_DATA_DIRS` resolution,
  pure over an environment-accessor closure.
- `ensure_at_startup()` -> `StartupOutcome`: create if missing, refresh only if ours and
  different, leave a foreign or unreadable entry alone.
- `write_entry()`: the unconditional write of this copy's entry + icon (registration actions).
- `same_executable()`, `resolve_program_in_process_path()`: the "is this our executable" and
  the bare-name `$PATH` oracles the startup decision and the probe share.
- `refresh_linux_desktop_database()`: best-effort `update-desktop-database <dir>`.
- `DesktopEntryError`: a failed directory creation or file write.

Notes:
Blocking filesystem and process I/O: callers run `ensure_at_startup` on a worker thread (the
binary's `install_linux_desktop_integration_async`). The decision itself is
`desktop_entry::decide_startup` (pure); this file only reads, writes and refreshes. Files are
written atomically (temporary sibling + rename) so a desktop shell never reads a half-written
entry.
*/

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ms_log::runtime_log;

use super::desktop_entry::{DESKTOP_ENTRY_FILE_NAME, ICON_FILE_NAME, StartupOutcome, decide_startup, linux_desktop_entry_text, resolve_program};
use crate::copy_identity::CopyIdentity;
use crate::identity::APP_ICON_PNG;

/// The XDG data directories of the current user.
///
/// `data_home` is where this copy writes (`$XDG_DATA_HOME`, else `$HOME/.local/share`);
/// `data_dirs` are the system-wide directories, read-only for the program (`$XDG_DATA_DIRS`,
/// else `/usr/local/share` and `/usr/share`), in precedence order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopDirs {
    pub data_home: PathBuf,
    pub data_dirs: Vec<PathBuf>,
}

impl DesktopDirs {
    /// Resolves the directories from the environment read through `var`. Pure over `var`.
    ///
    /// Per the XDG Base Directory spec a relative or empty path is invalid and ignored:
    /// `$XDG_DATA_HOME` is used when absolute and non-empty, else `$HOME/.local/share` (an
    /// absolute, non-empty `$HOME`); `$XDG_DATA_DIRS` keeps its absolute entries, and falls back
    /// to `/usr/local/share:/usr/share` when unset or left with none. Returns `None` when no
    /// valid data home exists (nothing may then be written).
    #[must_use]
    pub fn from_env(var: &dyn Fn(&str) -> Option<OsString>) -> Option<Self> {
        let absolute = |name: &str| var(name).map(PathBuf::from).filter(|path| path.is_absolute());
        let data_home = absolute("XDG_DATA_HOME").or_else(|| absolute("HOME").map(|home| home.join(".local/share")))?;
        let mut data_dirs: Vec<PathBuf> = var("XDG_DATA_DIRS")
            .map(|value| std::env::split_paths(&value).filter(|path| path.is_absolute()).collect())
            .unwrap_or_default();
        if data_dirs.is_empty() {
            data_dirs = vec![PathBuf::from("/usr/local/share"), PathBuf::from("/usr/share")];
        }
        Some(Self { data_home, data_dirs })
    }

    /// [`DesktopDirs::from_env`] over the process environment.
    #[must_use]
    pub fn from_process_env() -> Option<Self> {
        Self::from_env(&|name| std::env::var_os(name))
    }

    /// `<data_home>/applications`: the directory the entry is written into.
    #[must_use]
    pub fn applications_dir(&self) -> PathBuf {
        self.data_home.join("applications")
    }

    /// `<data_home>/applications/manhwastudio_rs.desktop`.
    #[must_use]
    pub fn entry_path(&self) -> PathBuf {
        entry_path_under(&self.data_home)
    }

    /// `<data_home>/icons/hicolor/512x512/apps/manhwastudio_rs.png`.
    #[must_use]
    pub fn icon_path(&self) -> PathBuf {
        icon_path_under(&self.data_home)
    }
}

/// `<data_dir>/applications/manhwastudio_rs.desktop` for any XDG data directory (the per-user
/// one or a `$XDG_DATA_DIRS` entry).
#[must_use]
pub(crate) fn entry_path_under(data_dir: &Path) -> PathBuf {
    data_dir.join("applications").join(DESKTOP_ENTRY_FILE_NAME)
}

/// `<data_dir>/icons/hicolor/512x512/apps/manhwastudio_rs.png` for any XDG data directory.
#[must_use]
pub(crate) fn icon_path_under(data_dir: &Path) -> PathBuf {
    data_dir.join("icons/hicolor/512x512/apps").join(ICON_FILE_NAME)
}

/// A failed write of the desktop entry or its icon. `Display` is English, for logs; the startup
/// writer is log-only, so there is no user-facing text.
#[derive(Debug, thiserror::Error)]
pub enum DesktopEntryError {
    /// A parent directory could not be created.
    #[error("could not create the directory '{}': {source}", path.display())]
    CreateDir { path: PathBuf, source: io::Error },
    /// The file (or its temporary sibling) could not be written or renamed into place.
    #[error("could not write '{}': {source}", path.display())]
    Write { path: PathBuf, source: io::Error },
}

/// Keeps the running copy's desktop entry under `dirs.data_home` current, then refreshes the
/// desktop database when the entry was written. Blocking; never touches `dirs.data_dirs`.
///
/// Missing entry -> written ([`StartupOutcome::Created`]). Entry of this copy (ownership key, or
/// a legacy entry whose `Exec` runs `identity.exe`, a bare name resolved through `$PATH`) ->
/// rewritten only when its bytes differ
/// ([`StartupOutcome::Refreshed`] / [`StartupOutcome::AlreadyCurrent`]). Entry of another copy
/// -> left untouched even when that copy is gone ([`StartupOutcome::LeftForeign`]). Unreadable
/// or ownerless entry (also: a bare `Exec` name `$PATH` does not find) -> left untouched, cause
/// logged ([`StartupOutcome::Unreadable`]). The icon
/// is (re)written only for this copy's entry and only when its bytes differ.
///
/// # Errors
/// [`DesktopEntryError`] when a directory or file could not be written; nothing is refreshed then.
pub fn ensure_at_startup(identity: &CopyIdentity, dirs: &DesktopDirs) -> Result<StartupOutcome, DesktopEntryError> {
    ensure_at_startup_with(identity, dirs, &resolve_program_in_process_path, &mut refresh_linux_desktop_database)
}

/// [`ensure_at_startup`] with the program resolver and the desktop-database refresh injected
/// (tests pass a scratch `$PATH` and a recorder).
fn ensure_at_startup_with(
    identity: &CopyIdentity,
    dirs: &DesktopDirs,
    resolve: &dyn Fn(&Path) -> Option<PathBuf>,
    refresh: &mut dyn FnMut(&Path),
) -> Result<StartupOutcome, DesktopEntryError> {
    let entry_path = dirs.entry_path();
    let existing = match fs::read(&entry_path) {
        Ok(bytes) => Some(bytes),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[desktop-integration] could not read the desktop entry '{}' ({err}); it is left untouched",
                entry_path.display()
            ));
            return Ok(StartupOutcome::Unreadable);
        }
    };
    let expected = linux_desktop_entry_text(identity);
    let outcome = decide_startup(existing.as_deref(), &expected, resolve, &|found| same_executable(found, &identity.exe));
    match &outcome {
        StartupOutcome::Created | StartupOutcome::Refreshed => {
            write_entry_text(dirs, &expected)?;
            refresh(&dirs.applications_dir());
        }
        // A current entry with a deleted icon would show a generic icon; repair only the icon.
        StartupOutcome::AlreadyCurrent => ensure_icon(&dirs.icon_path())?,
        StartupOutcome::LeftForeign { .. } => {}
        StartupOutcome::Unreadable => runtime_log::log_warn(format!(
            "[desktop-integration] the desktop entry '{}' names no owner (no X-ManhwaStudio-Exe, no Exec, or a bare program name not on $PATH); it is left untouched",
            entry_path.display()
        )),
    }
    Ok(outcome)
}

/// Writes `identity`'s desktop entry and the program icon under `dirs.data_home`, whatever entry
/// was there before (another copy's, an unreadable one): the explicit "register this copy" action
/// of the registration tab. Does NOT refresh the desktop database; the caller refreshes once after
/// all its writes. Blocking.
///
/// # Errors
/// [`DesktopEntryError`] when a directory or file could not be written.
pub(crate) fn write_entry(identity: &CopyIdentity, dirs: &DesktopDirs) -> Result<(), DesktopEntryError> {
    write_entry_text(dirs, &linux_desktop_entry_text(identity))
}

/// The icon (when its bytes differ), then the entry text `expected`, both atomically: the one
/// writer of a full entry, shared by the startup policy and [`write_entry`]. The icon goes
/// first so a menu never shows the new entry with a missing icon.
fn write_entry_text(dirs: &DesktopDirs, expected: &str) -> Result<(), DesktopEntryError> {
    ensure_icon(&dirs.icon_path())?;
    write_atomically(&dirs.entry_path(), expected.as_bytes())
}

/// Whether `found` (a path read from an entry) is the executable `exe`: the canonical paths
/// when both resolve, else the lossy text of both (a missing file can only match lexically).
pub(crate) fn same_executable(found: &Path, exe: &Path) -> bool {
    match (fs::canonicalize(found), fs::canonicalize(exe)) {
        (Ok(found), Ok(exe)) => found == exe,
        _ => found.to_string_lossy() == exe.to_string_lossy(),
    }
}

/// [`resolve_program`] over the process `$PATH`, with "an executable regular file" (after
/// symlinks) as the hit test: the production resolver of a bare program name in an entry.
/// Blocking (one metadata read per `$PATH` directory).
pub(crate) fn resolve_program_in_process_path(program: &Path) -> Option<PathBuf> {
    resolve_program(program, std::env::var_os("PATH").as_deref(), &is_executable_file)
}

/// Whether `path` is a regular file (after symlinks) with an execute bit; any metadata failure
/// reads as "no" (the lookup moves on to the next `$PATH` directory, as a shell does).
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Writes the program icon to `icon_path` unless it already holds exactly those bytes.
fn ensure_icon(icon_path: &Path) -> Result<(), DesktopEntryError> {
    // A read failure (missing file, permissions) means the icon must be (re)written; a write
    // failure after that is the error the caller sees.
    if fs::read(icon_path).is_ok_and(|bytes| bytes == APP_ICON_PNG) {
        return Ok(());
    }
    write_atomically(icon_path, APP_ICON_PNG)
}

/// Writes `bytes` to a hidden temporary sibling of `path` and renames it over `path`, creating
/// the parent directory first. The temporary name never ends in `.desktop`, so a directory scan
/// cannot pick it up; it is removed when the rename fails.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), DesktopEntryError> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(DesktopEntryError::Write { path: path.to_path_buf(), source: io::Error::new(io::ErrorKind::InvalidInput, "path has no parent or file name") });
    };
    fs::create_dir_all(parent).map_err(|source| DesktopEntryError::CreateDir { path: parent.to_path_buf(), source })?;
    let mut temp_name = OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".tmp-{}", std::process::id()));
    let temp_path = parent.join(temp_name);
    let written = fs::write(&temp_path, bytes).and_then(|()| fs::rename(&temp_path, path));
    if let Err(source) = written {
        if let Err(remove_err) = fs::remove_file(&temp_path)
            && remove_err.kind() != io::ErrorKind::NotFound
        {
            runtime_log::log_warn(format!(
                "[desktop-integration] could not remove the temporary file '{}': {remove_err}",
                temp_path.display()
            ));
        }
        return Err(DesktopEntryError::Write { path: path.to_path_buf(), source });
    }
    Ok(())
}

/// Runs `update-desktop-database <apps_dir>` so the entry's `MimeType=` list shows up in "Open
/// with" menus without a re-login. Optional tool: absence or failure is logged only. It only
/// rebuilds the MIME -> applications cache; no default handler is ever set.
pub fn refresh_linux_desktop_database(apps_dir: &Path) {
    match std::process::Command::new("update-desktop-database").arg(apps_dir).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => runtime_log::log_warn(format!(
            "[desktop-integration] update-desktop-database '{}' failed ({}): {}",
            apps_dir.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => runtime_log::log_info(
            "[desktop-integration] update-desktop-database is not installed; the desktop picks up the entry on its own rescan",
        ),
        Err(err) => runtime_log::log_warn(format!(
            "[desktop-integration] could not run update-desktop-database: {err}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A uniquely named scratch directory under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("ms-os-integration-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()));
            if path.exists() {
                fs::remove_dir_all(&path).expect("stale scratch dir must be removable");
            }
            fs::create_dir_all(&path).expect("scratch dir must be creatable");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            // Restore write permission first, a test may have made a directory read-only.
            make_writable_recursively(&self.0);
            if let Err(err) = fs::remove_dir_all(&self.0) {
                eprintln!("could not remove scratch dir {}: {err}", self.0.display());
            }
        }
    }

    fn make_writable_recursively(dir: &Path) {
        if let Err(err) = fs::set_permissions(dir, fs::Permissions::from_mode(0o755)) {
            eprintln!("could not reset permissions of {}: {err}", dir.display());
        }
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    make_writable_recursively(&entry.path());
                }
            }
        }
    }

    /// An identity whose executable is a real file under `scratch` (so canonicalization works).
    fn scratch_identity(scratch: &Scratch, name: &str) -> CopyIdentity {
        let root = scratch.0.join(name);
        fs::create_dir_all(&root).expect("copy dir");
        let exe = root.join("manhwastudio_rs");
        fs::write(&exe, b"").expect("fake exe");
        CopyIdentity { exe, program_root: root, version_core: None }
    }

    fn dirs_in(scratch: &Scratch) -> DesktopDirs {
        DesktopDirs { data_home: scratch.0.join("data"), data_dirs: Vec::new() }
    }

    fn run(identity: &CopyIdentity, dirs: &DesktopDirs) -> (StartupOutcome, Vec<PathBuf>) {
        let mut refreshed = Vec::new();
        let outcome = ensure_at_startup_with(identity, dirs, &|path| Some(path.to_path_buf()), &mut |dir| refreshed.push(dir.to_path_buf())).expect("startup write");
        (outcome, refreshed)
    }

    #[test]
    fn creates_then_stays_current_without_writing() {
        let scratch = Scratch::new("create");
        let ours = scratch_identity(&scratch, "ours");
        let dirs = dirs_in(&scratch);

        let (outcome, refreshed) = run(&ours, &dirs);
        assert_eq!(outcome, StartupOutcome::Created);
        assert_eq!(refreshed, vec![dirs.applications_dir()]);
        assert_eq!(fs::read_to_string(dirs.entry_path()).expect("entry"), linux_desktop_entry_text(&ours));
        assert_eq!(fs::read(dirs.icon_path()).expect("icon"), APP_ICON_PNG);

        // Idempotence: with both directories read-only, any write would fail the call.
        let apps = dirs.applications_dir();
        let icons = dirs.icon_path().parent().expect("icon dir").to_path_buf();
        for dir in [&apps, &icons] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).expect("read-only dir");
        }
        let (outcome, refreshed) = run(&ours, &dirs);
        assert_eq!(outcome, StartupOutcome::AlreadyCurrent);
        assert!(refreshed.is_empty(), "nothing written, nothing refreshed");
        let leftovers: Vec<_> = fs::read_dir(&apps).expect("apps dir").flatten().map(|entry| entry.file_name()).collect();
        assert_eq!(leftovers, vec![OsString::from(DESKTOP_ENTRY_FILE_NAME)], "no temporary file left behind");
    }

    #[test]
    fn refreshes_an_own_stale_entry_found_by_key() {
        let scratch = Scratch::new("refresh-key");
        let ours = scratch_identity(&scratch, "ours");
        let dirs = dirs_in(&scratch);
        let stale = linux_desktop_entry_text(&ours).replace("Comment=Comic translation editor", "Comment=old");
        write_atomically(&dirs.entry_path(), stale.as_bytes()).expect("seed");

        let (outcome, refreshed) = run(&ours, &dirs);
        assert_eq!(outcome, StartupOutcome::Refreshed);
        assert_eq!(refreshed.len(), 1);
        assert_eq!(fs::read_to_string(dirs.entry_path()).expect("entry"), linux_desktop_entry_text(&ours));
    }

    /// A repository build's own entry written with the executable's directory as `Path=` (the
    /// value before the repository-build rule) is rewritten at startup with the repository root.
    #[test]
    fn refreshes_a_repo_build_entry_to_the_repository_root() {
        let scratch = Scratch::new("refresh-repo");
        let repo = scratch.0.join("repo");
        let profile_dir = repo.join("target").join("release");
        fs::create_dir_all(&profile_dir).expect("profile dir");
        let exe = profile_dir.join("manhwastudio_rs");
        fs::write(&exe, b"").expect("fake exe");
        let no_markers = |_: &Path| false;
        let program_root = crate::copy_identity::program_root_for(&exe, &profile_dir, &no_markers);
        assert_eq!(program_root, repo);
        let ours = CopyIdentity { exe: exe.clone(), program_root, version_core: None };
        let dirs = dirs_in(&scratch);
        let old = CopyIdentity { exe, program_root: profile_dir.clone(), version_core: None };
        write_atomically(&dirs.entry_path(), linux_desktop_entry_text(&old).as_bytes()).expect("seed");

        let (outcome, refreshed) = run(&ours, &dirs);
        assert_eq!(outcome, StartupOutcome::Refreshed);
        assert_eq!(refreshed.len(), 1);
        let entry = fs::read_to_string(dirs.entry_path()).expect("entry");
        assert_eq!(entry, linux_desktop_entry_text(&ours));
        assert!(entry.contains(&format!("\nPath={}\n", repo.display())), "{entry}");
    }

    /// A pre-key entry whose `Exec` runs this executable is adopted and gains the key.
    #[test]
    fn refreshes_a_legacy_entry_whose_exec_is_ours() {
        let scratch = Scratch::new("refresh-legacy");
        let ours = scratch_identity(&scratch, "ours");
        let dirs = dirs_in(&scratch);
        let legacy = format!("[Desktop Entry]\nType=Application\nName=ManhwaStudio\nExec={} %f\n", super::super::desktop_entry::escape_desktop_exec_arg(&ours.exe));
        write_atomically(&dirs.entry_path(), legacy.as_bytes()).expect("seed");

        let (outcome, refreshed) = run(&ours, &dirs);
        assert_eq!(outcome, StartupOutcome::Refreshed);
        assert_eq!(refreshed.len(), 1);
        assert!(fs::read_to_string(dirs.entry_path()).expect("entry").contains("\nX-ManhwaStudio-Exe="));
    }

    /// Another copy's entry — alive, gone, or legacy — is never touched, and neither is the icon.
    #[test]
    fn leaves_a_foreign_entry_untouched() {
        let scratch = Scratch::new("foreign");
        let ours = scratch_identity(&scratch, "ours");
        let other = scratch_identity(&scratch, "other");
        let dirs = dirs_in(&scratch);
        let gone = CopyIdentity { exe: scratch.0.join("gone/manhwastudio_rs"), program_root: scratch.0.join("gone"), version_core: None };
        let legacy_other = format!("[Desktop Entry]\nExec={} %f\n", super::super::desktop_entry::escape_desktop_exec_arg(&other.exe));
        for (text, owner) in [
            (linux_desktop_entry_text(&other), &other.exe),
            (linux_desktop_entry_text(&gone), &gone.exe),
            (legacy_other, &other.exe),
        ] {
            write_atomically(&dirs.entry_path(), text.as_bytes()).expect("seed");
            let (outcome, refreshed) = run(&ours, &dirs);
            assert_eq!(outcome, StartupOutcome::LeftForeign { exe: owner.clone() });
            assert!(refreshed.is_empty());
            assert_eq!(fs::read_to_string(dirs.entry_path()).expect("entry"), text);
            assert!(!dirs.icon_path().exists(), "a foreign entry's icon is not ours to write");
        }
    }

    /// A hand-written legacy entry with a bare `Exec=` name: resolved through a scratch `$PATH`
    /// with the production hit test. Another copy found there -> left alone; this copy found
    /// there (a symlink to it) -> adopted; found nowhere -> left alone as ownerless. A foreign
    /// entry is never overwritten in any of these cases.
    #[test]
    fn resolves_a_bare_exec_through_path_and_never_takes_a_foreign_entry() {
        let scratch = Scratch::new("bare-exec");
        let ours = scratch_identity(&scratch, "ours");
        let other = scratch_identity(&scratch, "other");
        for exe in [&ours.exe, &other.exe] {
            fs::set_permissions(exe, fs::Permissions::from_mode(0o755)).expect("executable fake exe");
        }
        let dirs = dirs_in(&scratch);
        let bin = scratch.0.join("bin");
        fs::create_dir_all(&bin).expect("bin dir");
        let bare = "[Desktop Entry]\nType=Application\nName=Hand made\nExec=manhwastudio_rs %f\n";
        let run_with = |path_var: &OsString| {
            write_atomically(&dirs.entry_path(), bare.as_bytes()).expect("seed");
            let resolve = |path: &Path| resolve_program(path, Some(path_var.as_os_str()), &is_executable_file);
            let mut refreshed = 0;
            let outcome = ensure_at_startup_with(&ours, &dirs, &resolve, &mut |_| refreshed += 1).expect("startup");
            (outcome, refreshed, fs::read_to_string(dirs.entry_path()).expect("entry"))
        };

        // Another copy's directory first on $PATH (ours later): the entry is the other copy's.
        let path_var = std::env::join_paths([other.program_root.as_path(), ours.program_root.as_path()]).expect("PATH");
        let (outcome, refreshed, text) = run_with(&path_var);
        assert_eq!(outcome, StartupOutcome::LeftForeign { exe: other.exe.clone() });
        assert_eq!((refreshed, text.as_str()), (0, bare), "a foreign entry is never overwritten");

        // Found nowhere (an empty directory, a non-executable file): ownerless, left alone.
        fs::write(bin.join("manhwastudio_rs"), b"").expect("non-executable file");
        let path_var = std::env::join_paths([bin.as_path()]).expect("PATH");
        let (outcome, refreshed, text) = run_with(&path_var);
        assert_eq!(outcome, StartupOutcome::Unreadable);
        assert_eq!((refreshed, text.as_str()), (0, bare));

        // A symlink to this copy on $PATH: the entry is ours and is adopted.
        fs::remove_file(bin.join("manhwastudio_rs")).expect("remove the plain file");
        std::os::unix::fs::symlink(&ours.exe, bin.join("manhwastudio_rs")).expect("symlink to ours");
        let (outcome, refreshed, text) = run_with(&path_var);
        assert_eq!(outcome, StartupOutcome::Refreshed);
        assert_eq!(refreshed, 1);
        assert_eq!(text, linux_desktop_entry_text(&ours));
    }

    #[test]
    fn leaves_an_unreadable_or_ownerless_entry_untouched() {
        let scratch = Scratch::new("unreadable");
        let ours = scratch_identity(&scratch, "ours");
        let dirs = dirs_in(&scratch);
        write_atomically(&dirs.entry_path(), b"[Desktop Entry]\nName=Hand made\n").expect("seed");
        let (outcome, refreshed) = run(&ours, &dirs);
        assert_eq!(outcome, StartupOutcome::Unreadable);
        assert!(refreshed.is_empty());
        assert_eq!(fs::read(dirs.entry_path()).expect("entry"), b"[Desktop Entry]\nName=Hand made\n");

        // A directory in place of the file cannot be read: left alone, not an error.
        fs::remove_file(dirs.entry_path()).expect("remove seed");
        fs::create_dir(dirs.entry_path()).expect("dir in place of the entry");
        let (outcome, refreshed) = run(&ours, &dirs);
        assert_eq!(outcome, StartupOutcome::Unreadable);
        assert!(refreshed.is_empty());
    }

    #[test]
    fn reports_a_write_failure_without_refreshing() {
        let scratch = Scratch::new("write-fail");
        let ours = scratch_identity(&scratch, "ours");
        // A read-only, empty `data_home`: the entry reads as missing, but no directory can be
        // created below it.
        let dirs = dirs_in(&scratch);
        fs::create_dir_all(&dirs.data_home).expect("data home");
        fs::set_permissions(&dirs.data_home, fs::Permissions::from_mode(0o555)).expect("read-only data home");
        let mut refreshed = 0;
        let result = ensure_at_startup_with(&ours, &dirs, &|path| Some(path.to_path_buf()), &mut |_| refreshed += 1);
        assert!(matches!(result, Err(DesktopEntryError::CreateDir { .. })), "{result:?}");
        assert_eq!(refreshed, 0);
    }

    /// The XDG resolver table.
    #[test]
    fn desktop_dirs_follow_the_xdg_spec() {
        let default_dirs = vec![PathBuf::from("/usr/local/share"), PathBuf::from("/usr/share")];
        let resolve = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = vars.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
            DesktopDirs::from_env(&|name| vars.iter().find(|(key, _)| key == name).map(|(_, value)| OsString::from(value)))
        };
        let dirs = |home: &str, data_dirs: Vec<PathBuf>| Some(DesktopDirs { data_home: PathBuf::from(home), data_dirs });

        assert_eq!(resolve(&[("HOME", "/home/u")]), dirs("/home/u/.local/share", default_dirs.clone()));
        assert_eq!(resolve(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "/data/u")]), dirs("/data/u", default_dirs.clone()));
        assert_eq!(resolve(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "")]), dirs("/home/u/.local/share", default_dirs.clone()));
        assert_eq!(resolve(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "rel/data")]), dirs("/home/u/.local/share", default_dirs.clone()));
        assert_eq!(resolve(&[("XDG_DATA_HOME", "/data/u")]), dirs("/data/u", default_dirs.clone()));
        assert_eq!(resolve(&[]), None);
        assert_eq!(resolve(&[("HOME", "")]), None);
        assert_eq!(resolve(&[("HOME", "relative")]), None);
        assert_eq!(
            resolve(&[("HOME", "/home/u"), ("XDG_DATA_DIRS", "/a:rel::/b")]),
            dirs("/home/u/.local/share", vec![PathBuf::from("/a"), PathBuf::from("/b")])
        );
        assert_eq!(resolve(&[("HOME", "/home/u"), ("XDG_DATA_DIRS", "")]), dirs("/home/u/.local/share", default_dirs.clone()));
        assert_eq!(resolve(&[("HOME", "/home/u"), ("XDG_DATA_DIRS", "rel")]), dirs("/home/u/.local/share", default_dirs));

        let resolved = DesktopDirs { data_home: PathBuf::from("/d"), data_dirs: Vec::new() };
        assert_eq!(resolved.entry_path(), PathBuf::from("/d/applications/manhwastudio_rs.desktop"));
        assert_eq!(resolved.icon_path(), PathBuf::from("/d/icons/hicolor/512x512/apps/manhwastudio_rs.png"));
    }
}
