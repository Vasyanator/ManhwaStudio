/*
File: crates/ms-os-integration/src/windows/shortcut.rs

Purpose:
The `ManhwaStudio.lnk` shortcuts of a Windows install: where they live (Desktop, per-user or
all-users Start menu), what they contain, how they are written, read back and removed.

Key items:
- `ShortcutSpec` (pure): what a launcher shortcut holds — target exe, working directory = the
  copy's program root (`crate::copy_identity`: the repository root of a repository build, else
  the exe's directory), icon `<exe>,0`.
- `ShortcutTarget`: what an existing `.lnk` points at (target, arguments, working directory).
- `write_shortcut()`, `read_shortcut()`: native `IShellLinkW` + `IPersistFile` over COM.
- `create_windows_desktop_shortcut()`, `create_windows_start_menu_shortcut()`,
  `remove_windows_shortcuts_for_install()`, `resolve_windows_launcher_target()`.

Notes:
Folders come from the shell's known folders (`SHGetKnownFolderPath`: `FOLDERID_Desktop`,
`FOLDERID_Programs`, `FOLDERID_CommonPrograms`), so a Desktop redirected to OneDrive or any
other location is followed. Removal also checks the environment-variable folders the PowerShell
writer of earlier builds used, so their shortcuts are still found. Every COM call initializes
its own apartment (`ComApartment`); all functions block on file I/O and belong on a worker
thread.
*/

#[cfg(target_os = "windows")]
use std::env;
use std::ffi::OsString;
#[cfg(target_os = "windows")]
use std::fs;
#[cfg(target_os = "windows")]
use std::marker::PhantomData;
#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
use ::windows::Win32::Foundation::RPC_E_CHANGED_MODE;
#[cfg(target_os = "windows")]
use ::windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, IPersistFile, STGM_READ,
};
#[cfg(target_os = "windows")]
use ::windows::Win32::UI::Shell::{
    FOLDERID_CommonPrograms, FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, KF_FLAG_DEFAULT,
    KF_FLAG_DONT_VERIFY, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath, ShellLink,
};
#[cfg(target_os = "windows")]
use ::windows::core::{GUID, HSTRING, Interface};

#[cfg(target_os = "windows")]
use super::{is_windows_all_users_install_dir, normalize_windows_path};
#[cfg(target_os = "windows")]
use crate::IntegrationError;
#[cfg(target_os = "windows")]
use crate::identity::{SHORTCUT_FILE_NAME, WINDOWS_EXE_NAME};

/// Icon index of the launcher's own icon: PE icon resource 0 of the exe, which Windows can
/// always resolve as long as the exe exists.
const LAUNCHER_ICON_INDEX: i32 = 0;

/// Content of a launcher shortcut. Pure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutSpec {
    /// Executable the shortcut starts.
    pub target: PathBuf,
    /// Working directory of the started process: the program root of the target's copy.
    pub working_dir: PathBuf,
    /// File the icon is taken from (the target exe itself).
    pub icon_path: PathBuf,
    /// Icon resource index inside `icon_path`.
    pub icon_index: i32,
}

impl ShortcutSpec {
    /// The shortcut that starts `launcher` with its own icon (`<exe>,0`) in the program root
    /// derived from the executable path alone (`crate::copy_identity::exe_program_root`: the
    /// repository root of a repository build, else the exe's directory). For a copy known only
    /// by its exe: the installer's own shortcuts, another copy's link judged by the probe.
    /// `None` when `launcher` has no parent directory (a bare file name), because the working
    /// directory would then be undefined.
    #[must_use]
    pub fn for_launcher(launcher: &Path) -> Option<Self> {
        let working_dir = crate::copy_identity::exe_program_root(launcher)?;
        Some(Self::with_working_dir(launcher, working_dir))
    }

    /// The shortcut of `identity`: its exe, started in `identity.program_root` (the single
    /// source of a launch's working directory for the running copy), with its own icon.
    #[must_use]
    pub fn for_copy(identity: &crate::CopyIdentity) -> Self {
        Self::with_working_dir(&identity.exe, identity.program_root.clone())
    }

    fn with_working_dir(launcher: &Path, working_dir: PathBuf) -> Self {
        Self { target: launcher.to_path_buf(), working_dir, icon_path: launcher.to_path_buf(), icon_index: LAUNCHER_ICON_INDEX }
    }

    /// The icon in the shell's `<file>,<index>` notation (as Explorer's shortcut properties show
    /// it).
    #[must_use]
    pub fn icon_location(&self) -> String {
        format!("{},{}", self.icon_path.display(), self.icon_index)
    }
}

/// What an existing `.lnk` points at, as `IShellLinkW` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutTarget {
    /// Target path (environment variables expanded); empty for a link without a file-system
    /// target.
    pub target: PathBuf,
    /// Command-line arguments, verbatim; empty when none.
    pub arguments: OsString,
    /// Working directory; empty when the link sets none.
    pub working_dir: PathBuf,
}

/// The part of a Win32 output buffer before its first NUL (the whole buffer when it has none).
/// Pure.
fn until_nul(buffer: &[u16]) -> &[u16] {
    buffer.iter().position(|&unit| unit == 0).map_or(buffer, |end| &buffer[..end])
}

/// Length (UTF-16 units) of the buffers `read_shortcut` passes to `IShellLinkW` getters: the
/// longest path Win32 can express, so no field is truncated (`.lnk` fields are far shorter).
#[cfg(target_os = "windows")]
const LINK_TEXT_BUFFER_LEN: usize = 32_768;

/// A COM apartment entered for the duration of one shortcut operation on the calling thread.
///
/// `CoInitializeEx(COINIT_APARTMENTTHREADED)` on entry; `CoUninitialize` on drop only when the
/// entry call succeeded (`S_OK` or `S_FALSE`, each of which must be balanced). When the thread
/// already runs a multithreaded apartment the call returns `RPC_E_CHANGED_MODE`: COM is usable
/// there, but this guard did not initialize anything and must not uninitialize it.
#[cfg(target_os = "windows")]
struct ComApartment {
    balance_on_drop: bool,
    /// Makes the guard `!Send`: COM initialization is per thread, so the balancing
    /// `CoUninitialize` must run on the thread that entered.
    _thread_bound: PhantomData<*const ()>,
}

#[cfg(target_os = "windows")]
impl ComApartment {
    /// Enters the apartment.
    ///
    /// # Errors
    /// The `CoInitializeEx` failure other than `RPC_E_CHANGED_MODE`.
    fn enter() -> ::windows::core::Result<Self> {
        // SAFETY: plain FFI call with the documented reserved `None` argument; the matching
        // `CoUninitialize` runs in `Drop` on this same thread (the guard is not `Send`).
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Self { balance_on_drop: false, _thread_bound: PhantomData });
        }
        hr.ok()?;
        Ok(Self { balance_on_drop: true, _thread_bound: PhantomData })
    }
}

#[cfg(target_os = "windows")]
impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.balance_on_drop {
            // SAFETY: balances the successful `CoInitializeEx` of `enter` on the same thread;
            // every COM interface of the operation was declared after the guard and is
            // therefore already released.
            unsafe { CoUninitialize() };
        }
    }
}

/// Maps a COM failure of writing `lnk` at `step` to `IntegrationError::ShortcutWrite`.
#[cfg(target_os = "windows")]
fn write_error(lnk: &Path, step: &'static str, error: &::windows::core::Error) -> IntegrationError {
    IntegrationError::ShortcutWrite { path: lnk.to_path_buf(), step, code: error.code().0, message: error.message() }
}

/// Maps a COM failure of reading `lnk` at `step` to `IntegrationError::ShortcutRead`.
#[cfg(target_os = "windows")]
fn read_error(lnk: &Path, step: &'static str, error: &::windows::core::Error) -> IntegrationError {
    IntegrationError::ShortcutRead { path: lnk.to_path_buf(), step, code: error.code().0, message: error.message() }
}

/// Writes (creates or overwrites) the `.lnk` file at `lnk` with `spec` through the shell's
/// `IShellLinkW` + `IPersistFile`. The parent folder must exist. Blocking.
///
/// # Errors
/// `ShortcutWrite` naming the COM step that failed (apartment entry, link creation, a setter,
/// `IPersistFile::Save`).
#[cfg(target_os = "windows")]
pub fn write_shortcut(lnk: &Path, spec: &ShortcutSpec) -> Result<(), IntegrationError> {
    // Declared first, so it is dropped last: every interface below is released before the
    // apartment is left.
    let _apartment = ComApartment::enter().map_err(|e| write_error(lnk, "CoInitializeEx", &e))?;
    // SAFETY (whole block): COM calls on interfaces created in this apartment on this thread;
    // every string argument is an owned `HSTRING` that outlives the call.
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| write_error(lnk, "CoCreateInstance(ShellLink)", &e))?;
    unsafe { link.SetPath(&HSTRING::from(spec.target.as_path())) }
        .map_err(|e| write_error(lnk, "IShellLinkW::SetPath", &e))?;
    unsafe { link.SetWorkingDirectory(&HSTRING::from(spec.working_dir.as_path())) }
        .map_err(|e| write_error(lnk, "IShellLinkW::SetWorkingDirectory", &e))?;
    unsafe { link.SetIconLocation(&HSTRING::from(spec.icon_path.as_path()), spec.icon_index) }
        .map_err(|e| write_error(lnk, "IShellLinkW::SetIconLocation", &e))?;
    let file: IPersistFile = link.cast().map_err(|e| write_error(lnk, "QueryInterface(IPersistFile)", &e))?;
    // `fRemember = true`: the saved file becomes the link's current file, the documented
    // choice for "Save As" of a new link.
    unsafe { file.Save(&HSTRING::from(lnk), true) }.map_err(|e| write_error(lnk, "IPersistFile::Save", &e))?;
    Ok(())
}

/// Reads the target, arguments and working directory of the `.lnk` file at `lnk` through
/// `IPersistFile::Load` + `IShellLinkW`. Read-only (`STGM_READ`), no link resolution (a dead
/// target is reported as stored, never searched for). Blocking.
///
/// # Errors
/// `ShortcutRead` naming the COM step that failed (apartment entry, link creation, `Load`, a
/// getter).
#[cfg(target_os = "windows")]
pub fn read_shortcut(lnk: &Path) -> Result<ShortcutTarget, IntegrationError> {
    let _apartment = ComApartment::enter().map_err(|e| read_error(lnk, "CoInitializeEx", &e))?;
    // SAFETY (whole block): as in `write_shortcut`; each getter writes at most `len` units into
    // its own live buffer and `GetPath` accepts a null find-data pointer.
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| read_error(lnk, "CoCreateInstance(ShellLink)", &e))?;
    let file: IPersistFile = link.cast().map_err(|e| read_error(lnk, "QueryInterface(IPersistFile)", &e))?;
    unsafe { file.Load(&HSTRING::from(lnk), STGM_READ) }.map_err(|e| read_error(lnk, "IPersistFile::Load", &e))?;

    let mut buffer = vec![0u16; LINK_TEXT_BUFFER_LEN];
    // Flags 0 (no `SLGP_*`): the stored long path with environment variables expanded.
    unsafe { link.GetPath(&mut buffer, std::ptr::null_mut(), 0) }
        .map_err(|e| read_error(lnk, "IShellLinkW::GetPath", &e))?;
    let target = PathBuf::from(OsString::from_wide(until_nul(&buffer)));

    buffer.fill(0);
    unsafe { link.GetArguments(&mut buffer) }.map_err(|e| read_error(lnk, "IShellLinkW::GetArguments", &e))?;
    let arguments = OsString::from_wide(until_nul(&buffer));

    buffer.fill(0);
    unsafe { link.GetWorkingDirectory(&mut buffer) }
        .map_err(|e| read_error(lnk, "IShellLinkW::GetWorkingDirectory", &e))?;
    let working_dir = PathBuf::from(OsString::from_wide(until_nul(&buffer)));

    Ok(ShortcutTarget { target, arguments, working_dir })
}

/// Path of the shell known folder `folder_id` (`label` names it in the log), or `None` with a
/// logged warning when the shell cannot resolve it. `flags` decide whether a missing folder is
/// an error (`KF_FLAG_DEFAULT`) or still reported (`KF_FLAG_DONT_VERIFY`).
#[cfg(target_os = "windows")]
fn known_folder_path(folder_id: &GUID, flags: KNOWN_FOLDER_FLAG, label: &str) -> Option<PathBuf> {
    // SAFETY: `folder_id` is a valid GUID reference and `None` selects the current user's
    // token. On success the returned buffer is a NUL-terminated string allocated by the shell,
    // read once and freed with `CoTaskMemFree` exactly once below.
    match unsafe { SHGetKnownFolderPath(folder_id, flags, None) } {
        Ok(raw) => {
            let path = PathBuf::from(OsString::from_wide(unsafe { raw.as_wide() }));
            unsafe { CoTaskMemFree(Some(raw.0.cast_const().cast())) };
            Some(path)
        }
        Err(error) => {
            ms_log::runtime_log::log_warn(format!(
                "[windows-shortcut] known folder {label} unresolved: {} (HRESULT 0x{:08X})",
                error.message(),
                error.code().0
            ));
            None
        }
    }
}

/// The current user's Desktop (`FOLDERID_Desktop`, following any redirection such as
/// OneDrive), when it exists.
#[cfg(target_os = "windows")]
fn windows_desktop_dir() -> Option<PathBuf> {
    known_folder_path(&FOLDERID_Desktop, KF_FLAG_DEFAULT, "Desktop").filter(|p| p.is_dir())
}

/// The Desktop path the PowerShell shortcut writer of earlier builds used:
/// `%USERPROFILE%\Desktop`, when the variable is set and the directory exists. Used only to
/// find shortcuts to remove.
#[cfg(target_os = "windows")]
fn legacy_desktop_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|p| p.join("Desktop"))
        .filter(|p| p.is_dir())
}

/// Start-menu Programs folder: `FOLDERID_CommonPrograms` for `all_users`, else
/// `FOLDERID_Programs`. Not required to exist yet (the writer creates it).
#[cfg(target_os = "windows")]
pub(crate) fn windows_start_menu_programs_dir(all_users: bool) -> Option<PathBuf> {
    if all_users {
        known_folder_path(&FOLDERID_CommonPrograms, KF_FLAG_DONT_VERIFY, "CommonPrograms")
    } else {
        known_folder_path(&FOLDERID_Programs, KF_FLAG_DONT_VERIFY, "Programs")
    }
}

/// The Start-menu Programs path the PowerShell shortcut writer of earlier builds used:
/// `%ProgramData%` (default `C:\ProgramData`) for `all_users`, else `%APPDATA%` (`None` when
/// unset), each joined with `Microsoft\Windows\Start Menu\Programs`. Used only to find
/// shortcuts to remove.
#[cfg(target_os = "windows")]
fn legacy_start_menu_programs_dir(all_users: bool) -> Option<PathBuf> {
    let base = if all_users {
        Some(env::var_os("ProgramData").map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from))
    } else {
        env::var_os("APPDATA").map(PathBuf::from)
    };
    base.map(|base| base.join("Microsoft").join("Windows").join("Start Menu").join("Programs"))
}

/// Creates `ManhwaStudio.lnk` on the current user's Desktop (known folder, redirection
/// followed) targeting the install's launcher, and returns its path.
///
/// # Errors
/// `DesktopNotFound` when the Desktop cannot be resolved or does not exist, otherwise the
/// errors of the shortcut write (launcher not found, folder creation, COM failure).
#[cfg(target_os = "windows")]
pub fn create_windows_desktop_shortcut(install_dir: &Path) -> Result<PathBuf, IntegrationError> {
    let desktop_dir = windows_desktop_dir().ok_or(IntegrationError::DesktopNotFound)?;
    let shortcut_path = desktop_dir.join(SHORTCUT_FILE_NAME);
    create_windows_shortcut_at(install_dir, &shortcut_path)?;
    Ok(shortcut_path)
}

/// Creates `ManhwaStudio.lnk` in the Start-menu Programs folder matching the install kind
/// (`FOLDERID_CommonPrograms` for all-users, `FOLDERID_Programs` for per-user) and returns its
/// path.
///
/// # Errors
/// `StartMenuFolderNotFound` when the folder cannot be resolved, otherwise the errors of the
/// shortcut write.
#[cfg(target_os = "windows")]
pub fn create_windows_start_menu_shortcut(install_dir: &Path) -> Result<PathBuf, IntegrationError> {
    let programs_dir = windows_start_menu_programs_dir(is_windows_all_users_install_dir(install_dir))
        .ok_or(IntegrationError::StartMenuFolderNotFound)?;
    let shortcut_path = programs_dir.join(SHORTCUT_FILE_NAME);
    create_windows_shortcut_at(install_dir, &shortcut_path)?;
    Ok(shortcut_path)
}

/// Writes the shortcut at `shortcut_path` (creating its folder) that starts the install's
/// launcher found in `install_dir` ([`ShortcutSpec::for_launcher`]).
#[cfg(target_os = "windows")]
fn create_windows_shortcut_at(install_dir: &Path, shortcut_path: &Path) -> Result<(), IntegrationError> {
    let launcher_path = resolve_windows_launcher_target(install_dir)?;
    // `resolve_windows_launcher_target` returns `install_dir.join(<file name>)`, which always
    // has a non-empty parent unless `install_dir` itself is empty; that case has no launcher.
    let spec = ShortcutSpec::for_launcher(&launcher_path)
        .ok_or_else(|| IntegrationError::LauncherExeNotFound { install_dir: install_dir.to_path_buf() })?;
    if let Some(parent) = shortcut_path.parent() {
        fs::create_dir_all(parent).map_err(|e| IntegrationError::CreateShortcutFolder { parent: parent.to_path_buf(), source: e })?;
    }
    write_shortcut(shortcut_path, &spec)?;
    ms_log::runtime_log::log_info(format!(
        "[windows-shortcut] wrote '{}' -> '{}' (icon {})",
        shortcut_path.display(),
        spec.target.display(),
        spec.icon_location()
    ));
    Ok(())
}

/// The executable a shortcut or registry entry of `install_dir` launches:
/// `manhwastudio_rs.exe` when present, else a file in `install_dir` named like the running
/// executable.
///
/// # Errors
/// `LauncherExeNotFound` when neither file exists.
#[cfg(target_os = "windows")]
pub fn resolve_windows_launcher_target(install_dir: &Path) -> Result<PathBuf, IntegrationError> {
    let preferred_main = install_dir.join(WINDOWS_EXE_NAME);
    if preferred_main.is_file() {
        return Ok(preferred_main);
    }

    let fallback_name = env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_os_string()))
        .unwrap_or_else(|| WINDOWS_EXE_NAME.into());
    let fallback = install_dir.join(fallback_name);
    if fallback.is_file() {
        return Ok(fallback);
    }

    Err(IntegrationError::LauncherExeNotFound { install_dir: install_dir.to_path_buf() })
}

/// Removes `ManhwaStudio.lnk` from the Desktop and from the Start-menu Programs folder of the
/// install's kind, by file name only (whatever they target). Each location is looked up both
/// as the shell's known folder and as the environment-variable path earlier builds wrote to;
/// the two usually coincide and are then visited once. Stops at the first failed removal.
///
/// # Errors
/// `RemoveFile` / `RemoveFolder` of the first path that could not be removed.
#[cfg(target_os = "windows")]
pub fn remove_windows_shortcuts_for_install(install_dir: &Path) -> Result<(), IntegrationError> {
    let all_users = is_windows_all_users_install_dir(install_dir);
    let candidate_dirs = [
        windows_desktop_dir(),
        legacy_desktop_dir(),
        windows_start_menu_programs_dir(all_users),
        legacy_start_menu_programs_dir(all_users),
    ];
    let mut visited: Vec<String> = Vec::new();
    for dir in candidate_dirs.into_iter().flatten() {
        let key = normalize_windows_path(&dir);
        if visited.contains(&key) {
            continue;
        }
        visited.push(key);
        remove_path_if_exists(&dir.join(SHORTCUT_FILE_NAME))?;
    }
    Ok(())
}

/// Removes the shortcut at `path` (a directory recursively, otherwise as a file); a missing
/// path is `Ok`. Private to shortcut removal: the installer keeps its own general deleter, so
/// a change here never alters installer cleanup.
///
/// # Errors
/// `RemoveFolder` / `RemoveFile` with the path and the OS error.
#[cfg(target_os = "windows")]
fn remove_path_if_exists(path: &Path) -> Result<(), IntegrationError> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        fs::remove_dir_all(path)
            .map_err(|e| IntegrationError::RemoveFolder { path: path.to_path_buf(), source: e })?;
    } else {
        fs::remove_file(path)
            .map_err(|e| IntegrationError::RemoveFile { path: path.to_path_buf(), source: e })?;
    }
    ms_log::runtime_log::log_info(format!("[windows-shortcut] removed '{}'", path.display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ShortcutSpec, until_nul};
    use crate::CopyIdentity;
    use std::path::{Path, PathBuf};

    /// A launcher shortcut starts the exe in its own directory and takes icon 0 of the exe.
    /// Forward slashes keep the test host-independent (they separate on Windows too).
    #[test]
    fn launcher_spec_targets_exe_in_its_directory_with_its_icon() {
        let spec = ShortcutSpec::for_launcher(Path::new("C:/Apps/MS/manhwastudio_rs.exe")).expect("has a parent");
        assert_eq!(spec.target, PathBuf::from("C:/Apps/MS/manhwastudio_rs.exe"));
        assert_eq!(spec.working_dir, PathBuf::from("C:/Apps/MS"));
        assert_eq!(spec.icon_path, spec.target);
        assert_eq!(spec.icon_index, 0);
        assert_eq!(spec.icon_location(), "C:/Apps/MS/manhwastudio_rs.exe,0");
    }

    /// A repository build (`<repo>/target/<profile>/<exe>`) starts in the repository root,
    /// whether the spec comes from the exe alone or from the copy's identity.
    #[test]
    fn repo_build_spec_starts_in_the_repository_root() {
        let exe = Path::new("C:/src/ms/target/debug/manhwastudio_rs.exe");
        let spec = ShortcutSpec::for_launcher(exe).expect("has a parent");
        assert_eq!(spec.target, exe);
        assert_eq!(spec.working_dir, PathBuf::from("C:/src/ms"));
        let identity = CopyIdentity { exe: exe.to_path_buf(), program_root: PathBuf::from("C:/src/ms"), version_core: None };
        assert_eq!(ShortcutSpec::for_copy(&identity), spec);
    }

    /// A copy's shortcut takes the identity's program root verbatim.
    #[test]
    fn copy_spec_uses_the_identity_program_root() {
        let identity = CopyIdentity { exe: PathBuf::from("C:/build/out/manhwastudio_rs.exe"), program_root: PathBuf::from("C:/src/ms"), version_core: None };
        let spec = ShortcutSpec::for_copy(&identity);
        assert_eq!(spec.target, identity.exe);
        assert_eq!(spec.working_dir, PathBuf::from("C:/src/ms"));
        assert_eq!(spec.icon_location(), "C:/build/out/manhwastudio_rs.exe,0");
    }

    /// A bare file name has no working directory, so no shortcut can be specified for it.
    #[test]
    fn bare_file_name_has_no_spec() {
        assert_eq!(ShortcutSpec::for_launcher(Path::new("manhwastudio_rs.exe")), None);
        assert_eq!(ShortcutSpec::for_launcher(Path::new("")), None);
    }

    /// Win32 getters fill a fixed buffer: text ends at the first NUL, or at the buffer end.
    #[test]
    fn output_buffers_end_at_the_first_nul() {
        assert_eq!(until_nul(&[0x43, 0x3A, 0, 0x58]), &[0x43, 0x3A]);
        assert_eq!(until_nul(&[0, 0x41]), &[] as &[u16]);
        assert_eq!(until_nul(&[0x41, 0x42]), &[0x41, 0x42]);
    }
}
