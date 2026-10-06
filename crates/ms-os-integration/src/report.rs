/*
File: crates/ms-os-integration/src/report.rs

Purpose:
The registration report of a program copy: which OS records exist, whose they are, and whether
they work. Holds the shared model (`RecordReport`, `RecordStatus`, `Defect`), the ONE rule that
splits defects into broken and stale (`Defect::severity`), the ONE rule that decides which
records deserve a warning badge (`badge_worthy`), and the platform-dispatching `probe()`.

Key items:
- `Scope`, `RecordKind`, `RecordValue`, `RecordReport`, `RecordStatus`, `Defect`,
  `DefectSeverity`, `ProbeError`, `RegistrationReport`
- `Defect::severity()`, `badge_worthy()`, `classify()` (status from ownership + defects)
- `probe()`, `copy_scope()` (Windows and Linux only)

Notes:
The model is pure and compiled into native host test builds on every host, so the Windows
evaluators are tested on Linux. The readers live in `windows/probe.rs` and `linux/probe.rs`;
both feed `classify()`, so the status construction has one owner.
*/

use std::fmt;
use std::io;
use std::path::PathBuf;

#[cfg(any(target_os = "windows", target_os = "linux"))]
use crate::copy_identity::CopyIdentity;

/// Where a record lives: the current user's records or the all-users / system-wide ones.
///
/// Windows: `HKCU` / `FOLDERID_Programs` vs `HKLM` / `FOLDERID_CommonPrograms`. Linux:
/// `$XDG_DATA_HOME` vs a `$XDG_DATA_DIRS` entry (read-only for the program).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The current user's records.
    User,
    /// The all-users (Windows) or system-wide (Linux) records.
    Machine,
}

/// The kind of an OS record of a program copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordKind {
    /// Windows: the Start-menu `ManhwaStudio.lnk`. Linux: the `.desktop` entry as an
    /// application-menu item.
    StartMenu,
    /// Windows: the `Uninstall\ManhwaStudio` entry (installed-programs list). Not on Linux.
    ProgramEntry,
    /// Windows: the `App Paths\manhwastudio_rs.exe` entry. Not on Linux.
    AppPaths,
    /// Windows: the `Applications\manhwastudio_rs.exe` "Open with" key. Linux: the `.desktop`
    /// entry's `MimeType=` coverage and its file-accepting `Exec=`.
    OpenWith,
}

/// A named value of a record that a [`Defect`] refers to. Typed, so the broken/stale split in
/// [`Defect::severity`] is an exhaustive match instead of a string comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordValue {
    /// Uninstall entry `DisplayName`.
    DisplayName,
    /// Uninstall entry `Publisher`.
    Publisher,
    /// Uninstall entry `DisplayVersion`.
    DisplayVersion,
    /// Uninstall entry `InstallLocation`.
    InstallLocation,
    /// Uninstall entry `DisplayIcon`.
    DisplayIcon,
    /// Uninstall entry `UninstallString`.
    UninstallString,
    /// Uninstall entry `QuietUninstallString`.
    QuietUninstallString,
    /// App Paths default value (the launcher path).
    AppPathsExe,
    /// App Paths `Path`.
    AppPathsPath,
    /// "Open with" `FriendlyAppName`.
    FriendlyAppName,
    /// "Open with" `shell\open\command` default value.
    OpenCommand,
    /// Shortcut target path.
    ShortcutTarget,
    /// Shortcut working directory ("Start in").
    ShortcutWorkingDir,
    /// Shortcut command-line arguments.
    ShortcutArguments,
    /// Desktop entry `Exec=`.
    DesktopExec,
    /// Desktop entry `TryExec=`.
    DesktopTryExec,
    /// Desktop entry `Path=`.
    DesktopPath,
    /// Desktop entry `Icon=`.
    DesktopIcon,
    /// Desktop entry ownership key `X-ManhwaStudio-Exe=`.
    DesktopOwnerKey,
}

impl RecordValue {
    /// The value's name as the OS shows it (registry value name, shortcut field, desktop key).
    /// Technical identifier for logs and defect details, never translated.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::DisplayName => "DisplayName",
            Self::Publisher => "Publisher",
            Self::DisplayVersion => "DisplayVersion",
            Self::InstallLocation => "InstallLocation",
            Self::DisplayIcon => "DisplayIcon",
            Self::UninstallString => "UninstallString",
            Self::QuietUninstallString => "QuietUninstallString",
            Self::AppPathsExe => "(Default)",
            Self::AppPathsPath | Self::DesktopPath => "Path",
            Self::FriendlyAppName => "FriendlyAppName",
            Self::OpenCommand => r"shell\open\command",
            Self::ShortcutTarget => "Target",
            Self::ShortcutWorkingDir => "Start in",
            Self::ShortcutArguments => "Arguments",
            Self::DesktopExec => "Exec",
            Self::DesktopTryExec => "TryExec",
            Self::DesktopIcon => "Icon",
            Self::DesktopOwnerKey => crate::identity::LINUX_OWNER_EXE_KEY,
        }
    }
}

/// One problem found in an existing record, judged against the copy that owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Defect {
    /// The executable the record launches does not exist.
    TargetMissing { path: String },
    /// The working directory the record starts in does not exist.
    WorkingDirMissing { path: String },
    /// The working directory is the launched executable's own directory, the value records were
    /// written with before the copy's program root moved off it (a repository build now starts
    /// in its repository root), and differs from the program root the writer uses now. The
    /// directory exists and the right executable starts; only the runtime root of that launch is
    /// the old one, so Repair rewrites it. Any other working-directory mismatch is `WrongValue`.
    WorkingDirOutdated { expected: String, found: String },
    /// A value the record should carry is absent (or blank).
    ValueMissing { value: RecordValue },
    /// A value differs from what the owning copy's writer puts there.
    WrongValue { value: RecordValue, expected: String, found: String },
    /// A command line that does not have the written shape (`"<exe>" <tail>`, or a desktop
    /// `Exec=` that cannot be parsed or takes no file).
    MalformedCommand { command: String },
    /// Readable image types the "Open with" record does not offer (extensions on Windows, MIME
    /// types on Linux), in table order.
    MissingImageTypes { types: Vec<String> },
    /// The icon the record names does not exist.
    IconMissing,
    /// `DisplayVersion` names another version than this copy's `version_core`.
    VersionOutdated { found: String },
    /// The record at ManhwaStudio's name launches a program that is not a ManhwaStudio
    /// executable (its file name is not the product's executable name): `value` names it as
    /// `found`. Reported inside `OursBroken` (the record sits at this copy's name and does not
    /// start ManhwaStudio) with `expected` = this copy's executable; Repair and Remove of such a
    /// record need confirmation (`actions::needs_confirmation`), since whose it is is unknown.
    ForeignProgram { value: RecordValue, expected: String, found: String },
}

/// Whether a defect stops the record from working (`Broken`, badged) or only leaves outdated or
/// cosmetic values behind (`Stale`, never badged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefectSeverity {
    /// The record does not work for its copy.
    Broken,
    /// The record works; some values are missing, outdated or cosmetic.
    Stale,
}

impl Defect {
    /// The broken/stale split. The ONLY owner of that decision.
    ///
    /// Broken: a missing target or working directory, a malformed command, a record that
    /// launches a program other than ManhwaStudio (`ForeignProgram`), a missing value
    /// without which the record does nothing (`UninstallString`, `DisplayName` — the entry is
    /// hidden without it — `InstallLocation` — the installer finds installs by it — the App
    /// Paths executable, the open command, `Exec=`), and a wrong value that decides what is
    /// launched, from where, or which install an entry describes. Stale: everything else (an
    /// additive value missing on an older record, display texts, icons, image types, version,
    /// and a working directory still at the executable's own directory where the program root
    /// moved off it — `WorkingDirOutdated`, so existing repository-build entries do not badge).
    #[must_use]
    pub fn severity(&self) -> DefectSeverity {
        match self {
            Self::TargetMissing { .. } | Self::WorkingDirMissing { .. } | Self::MalformedCommand { .. } | Self::ForeignProgram { .. } => {
                DefectSeverity::Broken
            }
            Self::MissingImageTypes { .. } | Self::IconMissing | Self::VersionOutdated { .. } | Self::WorkingDirOutdated { .. } => DefectSeverity::Stale,
            Self::ValueMissing { value } => match value {
                RecordValue::DisplayName
                | RecordValue::InstallLocation
                | RecordValue::UninstallString
                | RecordValue::AppPathsExe
                | RecordValue::OpenCommand
                | RecordValue::ShortcutTarget
                | RecordValue::DesktopExec => DefectSeverity::Broken,
                RecordValue::Publisher
                | RecordValue::DisplayVersion
                | RecordValue::DisplayIcon
                | RecordValue::QuietUninstallString
                | RecordValue::AppPathsPath
                | RecordValue::FriendlyAppName
                | RecordValue::ShortcutWorkingDir
                | RecordValue::ShortcutArguments
                | RecordValue::DesktopTryExec
                | RecordValue::DesktopPath
                | RecordValue::DesktopIcon
                | RecordValue::DesktopOwnerKey => DefectSeverity::Stale,
            },
            Self::WrongValue { value, .. } => match value {
                RecordValue::UninstallString
                | RecordValue::InstallLocation
                | RecordValue::DisplayIcon
                | RecordValue::AppPathsExe
                | RecordValue::AppPathsPath
                | RecordValue::OpenCommand
                | RecordValue::ShortcutTarget
                | RecordValue::ShortcutWorkingDir
                | RecordValue::DesktopExec
                | RecordValue::DesktopTryExec
                | RecordValue::DesktopPath => DefectSeverity::Broken,
                RecordValue::DisplayName
                | RecordValue::Publisher
                | RecordValue::DisplayVersion
                | RecordValue::QuietUninstallString
                | RecordValue::FriendlyAppName
                | RecordValue::ShortcutArguments
                | RecordValue::DesktopIcon
                | RecordValue::DesktopOwnerKey => DefectSeverity::Stale,
            },
        }
    }
}

/// Why a record could not be judged. `Display` is English, for logs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProbeError {
    /// A registry key's existence or one of its values could not be read (Win32 error `code`,
    /// e.g. 5 = access denied, 1630 = a value of an unexpected type).
    #[error("could not read registry {} of '{key}' (Windows error {code})", value_label(.value_name.as_deref()))]
    Registry { key: String, value_name: Option<String>, code: u32 },
    /// A file or directory could not be read.
    #[error("could not read '{}': {message}", path.display())]
    Io { path: PathBuf, kind: io::ErrorKind, message: String },
    /// A shortcut could not be read (the `IntegrationError` log text).
    #[error("could not read the shortcut '{}': {message}", path.display())]
    Shortcut { path: PathBuf, message: String },
    /// The shell could not resolve the folder the record lives in.
    #[error("the folder {folder} could not be resolved")]
    FolderUnresolved { folder: &'static str },
    /// The record exists but names no program (no target, command or ownership key).
    #[error("the record '{location}' names no program")]
    NoOwner { location: String },
    /// Neither `$XDG_DATA_HOME` nor `$HOME` is an absolute path: there is no per-user data
    /// directory to look in.
    #[error("no per-user data directory ($XDG_DATA_HOME / $HOME unset or relative)")]
    NoDataHome,
}

impl ProbeError {
    /// [`ProbeError::Io`] for `path` from an I/O error.
    #[must_use]
    pub fn io(path: PathBuf, error: &io::Error) -> Self {
        Self::Io { path, kind: error.kind(), message: error.to_string() }
    }
}

/// Log label of a registry value name: `value 'Name'`, or `default value` for `None`.
fn value_label(value_name: Option<&str>) -> String {
    value_name.map_or_else(|| "default value".to_owned(), |name| format!("value '{name}'"))
}

/// The `exists` oracle the readers hand to the pure evaluators: whether `path` exists. An
/// existence check that itself fails (permissions on a parent) is logged and answered `true`,
/// so an unreadable location is never reported as a missing target (which would badge).
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) fn path_exists(path: &std::path::Path) -> bool {
    match path.try_exists() {
        Ok(exists) => exists,
        Err(error) => {
            ms_log::runtime_log::log_warn(format!(
                "[os-registration] could not check whether '{}' exists ({error}); treated as existing",
                path.display()
            ));
            true
        }
    }
}

/// What a probe found for one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordStatus {
    /// The record does not exist.
    Missing,
    /// The record belongs to this copy and holds exactly what its writer puts there.
    OursOk,
    /// The record belongs to this copy and works; values are outdated (never badged). Every
    /// defect is [`DefectSeverity::Stale`].
    OursStale(Vec<Defect>),
    /// The record belongs to this copy and does not work (badged). At least one defect is
    /// [`DefectSeverity::Broken`]; stale ones are listed too.
    OursBroken(Vec<Defect>),
    /// The record belongs to another copy whose executable is `exe`; `alive` = that file
    /// exists. `defects` are judged against THAT copy.
    OtherCopy { exe: PathBuf, alive: bool, defects: Vec<Defect> },
    /// The record could not be read or names no program (never badged, logged by `probe`).
    Unreadable(ProbeError),
}

/// One record of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordReport {
    pub kind: RecordKind,
    pub scope: Scope,
    /// Where the record lives: a registry key (`HKLM\…`), a `.lnk` or a `.desktop` path. For
    /// display and logs.
    pub location: String,
    pub status: RecordStatus,
    /// True on a `Machine` row that the system does not use because a higher-precedence record
    /// of the same name exists: on Windows the `HKCU` "Open with" key over the `HKLM` one, on
    /// Linux a `.desktop` file of the same id in `$XDG_DATA_HOME` or an earlier
    /// `$XDG_DATA_DIRS` entry. Always false on `User` rows and on a row whose own record does
    /// not exist (`Missing`): only an existing record can be hidden.
    pub shadowed: bool,
}

/// Everything a probe found for one program copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationReport {
    /// Every judged record, grouped by kind, `User` before `Machine`.
    pub records: Vec<RecordReport>,
    /// The scope this copy's own records belong in ([`copy_scope`]).
    pub copy_scope: Scope,
    /// Windows: the process runs elevated. Linux: always false (no record needs it).
    pub running_elevated: bool,
    /// [`CopyIdentity::is_dev_copy`] of the probed copy (`crate::CopyIdentity`).
    pub dev_copy: bool,
}

/// Whether a record's status deserves a warning badge (decision 3 of the plan: badges only for
/// broken records). `OursBroken` and an `OtherCopy` whose executable is gone are badged; an
/// alive `OtherCopy` only when its own record is broken; `Missing`, `OursOk`, `OursStale` and
/// `Unreadable` never.
#[must_use]
pub fn badge_worthy(status: &RecordStatus) -> bool {
    match status {
        RecordStatus::OursBroken(_) | RecordStatus::OtherCopy { alive: false, .. } => true,
        RecordStatus::OtherCopy { alive: true, defects, .. } => defects.iter().any(|defect| defect.severity() == DefectSeverity::Broken),
        RecordStatus::Missing | RecordStatus::OursOk | RecordStatus::OursStale(_) | RecordStatus::Unreadable(_) => false,
    }
}

/// The status of an existing record whose owner executable is `owner_exe`, given whether that
/// is this copy (`ours`), whether the file exists (`alive`) and the defects found against the
/// owner. Duplicate defects are dropped (first occurrence kept). The one constructor of the
/// owned/other statuses, shared by both platform evaluators.
#[must_use]
pub fn classify(owner_exe: PathBuf, ours: bool, alive: bool, defects: Vec<Defect>) -> RecordStatus {
    let mut unique: Vec<Defect> = Vec::with_capacity(defects.len());
    for defect in defects {
        if !unique.contains(&defect) {
            unique.push(defect);
        }
    }
    if !ours {
        return RecordStatus::OtherCopy { exe: owner_exe, alive, defects: unique };
    }
    if unique.is_empty() {
        RecordStatus::OursOk
    } else if unique.iter().any(|defect| defect.severity() == DefectSeverity::Broken) {
        RecordStatus::OursBroken(unique)
    } else {
        RecordStatus::OursStale(unique)
    }
}

impl fmt::Display for Scope {
    /// `user` / `machine`, for logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::User => "user",
            Self::Machine => "machine",
        })
    }
}

/// The scope this copy's own records are written in. Windows: `Machine` for an executable under
/// `%ProgramFiles%` / `%ProgramFiles(x86)%` (the installer's all-users rule), else `User`.
/// Linux: always `User` (the program never writes system-wide). Reads the environment on
/// Windows.
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[must_use]
pub fn copy_scope(identity: &CopyIdentity) -> Scope {
    #[cfg(target_os = "windows")]
    {
        let exe_dir = identity.exe.parent().unwrap_or(&identity.exe);
        if crate::windows::is_windows_all_users_install_dir(exe_dir) { Scope::Machine } else { Scope::User }
    }
    #[cfg(target_os = "linux")]
    {
        // The identity does not decide the Linux scope; the binding keeps one signature.
        let _linux_scope_ignores_identity = identity;
        Scope::User
    }
}

/// Probes every OS record of the copy `identity`: blocking, read-only, never panics. A record
/// that cannot be read becomes `Unreadable` (logged) without affecting the others. Run it on a
/// worker thread (registry, COM and filesystem I/O).
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[must_use]
pub fn probe(identity: &CopyIdentity) -> RegistrationReport {
    #[cfg(target_os = "windows")]
    let (records, running_elevated) = (crate::windows::probe::probe_records(identity), crate::windows::elevation::is_running_elevated());
    #[cfg(target_os = "linux")]
    let (records, running_elevated) = (crate::linux::probe::probe_records(identity), false);
    for record in &records {
        if let RecordStatus::Unreadable(error) = &record.status {
            ms_log::runtime_log::log_warn(format!(
                "[os-registration] {:?} ({}) at {} could not be judged: {error}",
                record.kind, record.scope, record.location
            ));
        }
    }
    let badged = records.iter().filter(|record| badge_worthy(&record.status)).count();
    ms_log::runtime_log::log_info(format!(
        "[os-registration] probed {} records for '{}' ({badged} broken)",
        records.len(),
        identity.exe.display()
    ));
    RegistrationReport { records, copy_scope: copy_scope(identity), running_elevated, dev_copy: identity.is_dev_copy() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value_missing(value: RecordValue) -> Defect {
        Defect::ValueMissing { value }
    }

    fn wrong(value: RecordValue) -> Defect {
        Defect::WrongValue { value, expected: "e".to_owned(), found: "f".to_owned() }
    }

    /// The severity table, row by row: every defect kind and every value in both the
    /// missing and the wrong position.
    #[test]
    fn defect_severity_table() {
        use DefectSeverity::{Broken, Stale};
        assert_eq!(Defect::TargetMissing { path: "p".to_owned() }.severity(), Broken);
        assert_eq!(Defect::WorkingDirMissing { path: "p".to_owned() }.severity(), Broken);
        assert_eq!(Defect::WorkingDirOutdated { expected: "e".to_owned(), found: "f".to_owned() }.severity(), Stale);
        assert_eq!(Defect::MalformedCommand { command: "c".to_owned() }.severity(), Broken);
        assert_eq!(Defect::ForeignProgram { value: RecordValue::ShortcutTarget, expected: "e".to_owned(), found: "f".to_owned() }.severity(), Broken);
        assert_eq!(Defect::MissingImageTypes { types: vec![".png".to_owned()] }.severity(), Stale);
        assert_eq!(Defect::IconMissing.severity(), Stale);
        assert_eq!(Defect::VersionOutdated { found: "1.0.0".to_owned() }.severity(), Stale);

        // (value, severity when missing, severity when wrong)
        let table = [
            (RecordValue::DisplayName, Broken, Stale),
            (RecordValue::Publisher, Stale, Stale),
            (RecordValue::DisplayVersion, Stale, Stale),
            (RecordValue::InstallLocation, Broken, Broken),
            (RecordValue::DisplayIcon, Stale, Broken),
            (RecordValue::UninstallString, Broken, Broken),
            (RecordValue::QuietUninstallString, Stale, Stale),
            (RecordValue::AppPathsExe, Broken, Broken),
            (RecordValue::AppPathsPath, Stale, Broken),
            (RecordValue::FriendlyAppName, Stale, Stale),
            (RecordValue::OpenCommand, Broken, Broken),
            (RecordValue::ShortcutTarget, Broken, Broken),
            (RecordValue::ShortcutWorkingDir, Stale, Broken),
            (RecordValue::ShortcutArguments, Stale, Stale),
            (RecordValue::DesktopExec, Broken, Broken),
            (RecordValue::DesktopTryExec, Stale, Broken),
            (RecordValue::DesktopPath, Stale, Broken),
            (RecordValue::DesktopIcon, Stale, Stale),
            (RecordValue::DesktopOwnerKey, Stale, Stale),
        ];
        for (value, missing, wrong_severity) in table {
            assert_eq!(value_missing(value).severity(), missing, "missing {value:?}");
            assert_eq!(wrong(value).severity(), wrong_severity, "wrong {value:?}");
        }
    }

    /// Badges only for broken records: ours broken, another copy that is gone, another alive
    /// copy whose own record is broken.
    #[test]
    fn badge_worthy_table() {
        let stale = vec![Defect::IconMissing];
        let broken = vec![Defect::IconMissing, Defect::TargetMissing { path: "p".to_owned() }];
        let other = |alive: bool, defects: Vec<Defect>| RecordStatus::OtherCopy { exe: PathBuf::from("/x"), alive, defects };
        assert!(!badge_worthy(&RecordStatus::Missing));
        assert!(!badge_worthy(&RecordStatus::OursOk));
        assert!(!badge_worthy(&RecordStatus::OursStale(stale.clone())));
        assert!(badge_worthy(&RecordStatus::OursBroken(broken.clone())));
        assert!(!badge_worthy(&other(true, Vec::new())));
        assert!(!badge_worthy(&other(true, stale.clone())));
        assert!(badge_worthy(&other(true, broken)));
        assert!(badge_worthy(&other(false, Vec::new())));
        assert!(badge_worthy(&other(false, stale)));
        assert!(!badge_worthy(&RecordStatus::Unreadable(ProbeError::NoDataHome)));
    }

    /// `classify` picks the status from ownership and the worst defect, and drops duplicates.
    #[test]
    fn classify_builds_every_owned_status() {
        let exe = PathBuf::from("/opt/ms/manhwastudio_rs");
        assert_eq!(classify(exe.clone(), true, true, Vec::new()), RecordStatus::OursOk);
        assert_eq!(classify(exe.clone(), true, true, vec![Defect::IconMissing, Defect::IconMissing]), RecordStatus::OursStale(vec![Defect::IconMissing]));
        let target = Defect::TargetMissing { path: "p".to_owned() };
        assert_eq!(
            classify(exe.clone(), true, true, vec![Defect::IconMissing, target.clone()]),
            RecordStatus::OursBroken(vec![Defect::IconMissing, target.clone()])
        );
        assert_eq!(
            classify(exe.clone(), false, false, vec![target.clone(), target.clone()]),
            RecordStatus::OtherCopy { exe, alive: false, defects: vec![target] }
        );
    }

    /// Log texts name the failing value and the Win32 code.
    #[test]
    fn probe_errors_display_for_logs() {
        let error = ProbeError::Registry { key: r"HKLM\K".to_owned(), value_name: Some("Path".to_owned()), code: 5 };
        assert_eq!(error.to_string(), r"could not read registry value 'Path' of 'HKLM\K' (Windows error 5)");
        let error = ProbeError::Registry { key: r"HKLM\K".to_owned(), value_name: None, code: 2 };
        assert_eq!(error.to_string(), r"could not read registry default value of 'HKLM\K' (Windows error 2)");
        let error = ProbeError::io(PathBuf::from("/x"), &io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
        assert_eq!(error, ProbeError::Io { path: PathBuf::from("/x"), kind: io::ErrorKind::PermissionDenied, message: "denied".to_owned() });
        assert_eq!(RecordValue::AppPathsExe.name(), "(Default)");
        assert_eq!(RecordValue::DesktopOwnerKey.name(), "X-ManhwaStudio-Exe");
    }
}
