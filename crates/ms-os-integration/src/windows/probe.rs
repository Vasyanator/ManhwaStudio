/*
File: crates/ms-os-integration/src/windows/probe.rs

Purpose:
The Windows half of the registration probe: reads the Start-menu shortcut, the Uninstall entry,
the App Paths entry and the "Open with" key under BOTH roots (`HKCU` / `FOLDERID_Programs` and
`HKLM` / `FOLDERID_CommonPrograms`) and judges each against the copy that owns it.

Key items:
- `ObservedUninstall`, `ObservedAppPaths`, `ObservedOpenWith`, `ObservedShortcut`: what a
  reader found (`Observed<T>` = `Ok(None)` absent, `Err` unreadable).
- `evaluate_program_entry()`, `evaluate_app_paths()`, `evaluate_open_with()`,
  `evaluate_start_menu()`: pure judges over observed values, the identity and an `exists`
  oracle.
- `probe_records()` (Windows only): the readers + evaluators for every record.

Notes:
Expected values come from the writers' own tables (`values::uninstall_and_app_paths_values`,
`values::windows_open_with_registry_values`, `shortcut::ShortcutSpec::for_copy` /
`for_launcher`), so the
probe can never expect something the installer does not write. Paths compare by the uninstall
ownership rule (`values::normalize_windows_path_text`: case-insensitive, `/` = `\`, trailing
separators ignored); display texts compare exactly. The record's owner is the executable it
launches (`resolve_owner`): ours when ANY identifying value names this copy, else the first
ManhwaStudio-named executable (another copy, judged against THAT copy), else a foreign program
(`OursBroken` with `Defect::ForeignProgram`). `NoModify` / `NoRepair`
(`REG_DWORD`) are not judged: the registry reader is `REG_SZ`-only and both are cosmetic. The
pure part is compiled into host test builds; the readers are `target_os = "windows"` only.
*/

use std::path::{Path, PathBuf};

use super::shortcut::ShortcutSpec;
use super::values::{
    RegistryValue, app_paths_key, command_executable, normalize_windows_path_text, present_path_value, uninstall_and_app_paths_values, uninstall_key,
    windows_open_with_app_key, windows_open_with_registry_values,
};
use crate::copy_identity::CopyIdentity;
use crate::identity::WINDOWS_EXE_NAME;
use crate::report::{Defect, ProbeError, RecordKind, RecordReport, RecordStatus, RecordValue, Scope, classify};

/// What a reader found for one record: `Ok(None)` = the record does not exist, `Err` = its
/// existence or a value could not be read.
pub type Observed<T> = Result<Option<T>, ProbeError>;

/// The `REG_SZ` values of an existing Uninstall entry (`None` = value absent).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedUninstall {
    pub display_name: Option<String>,
    pub publisher: Option<String>,
    pub display_version: Option<String>,
    pub install_location: Option<String>,
    pub display_icon: Option<String>,
    pub uninstall_string: Option<String>,
    pub quiet_uninstall_string: Option<String>,
}

/// The values of an existing App Paths entry: the default value (launcher) and `Path`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedAppPaths {
    pub exe: Option<String>,
    pub path: Option<String>,
}

/// An existing "Open with" key: `FriendlyAppName`, the `shell\open\command` default value, and
/// which of the expected `SupportedTypes` value names (`.png`, …) exist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedOpenWith {
    pub friendly_app_name: Option<String>,
    pub command: Option<String>,
    pub supported_types: Vec<String>,
}

/// An existing `.lnk` as `read_shortcut` reports it (lossy UTF-8 of the arguments).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedShortcut {
    pub target: PathBuf,
    pub arguments: String,
    pub working_dir: PathBuf,
}

/// `HKCU` for `User`, `HKLM` for `Machine`: the registry root the writers use for that scope.
#[must_use]
pub fn registry_root(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "HKCU",
        Scope::Machine => "HKLM",
    }
}

/// The directory part of a Windows path text (before its last `\` or `/`), `None` without one.
fn windows_parent(path: &str) -> Option<&str> {
    path.rfind(['\\', '/']).map(|index| &path[..index])
}

/// Whether two Windows path texts name the same path by the uninstall ownership rule.
fn same_windows_path(left: &str, right: &str) -> bool {
    normalize_windows_path_text(left) == normalize_windows_path_text(right)
}

/// `DisplayIcon` without a trailing `,<index>` icon index.
fn strip_icon_index(value: &str) -> &str {
    match value.rsplit_once(',') {
        Some((path, index)) if index.trim().parse::<i32>().is_ok() => path,
        _ => value,
    }
}

/// Splits a command line of the written shape `"<exe>"<tail>` into the exe and the trimmed tail.
/// `None` when it does not start with a quote or the quote is not closed.
fn split_quoted_command(command: &str) -> Option<(&str, &str)> {
    let rest = command.trim().strip_prefix('"')?;
    let (exe, tail) = rest.split_once('"')?;
    Some((exe, tail.trim()))
}

/// The executable a found command line launches: the quoted exe of `"<exe>"<tail>`, else the
/// first token (`values::command_executable`, the uninstall ownership rule). `None` when blank.
fn command_owner(command: &str) -> Option<&str> {
    split_quoted_command(command).map(|(exe, _)| exe).or_else(|| Some(command_executable(command))).filter(|exe| !exe.trim().is_empty())
}

/// How a found command compares with the expected one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandCheck {
    Matches,
    /// Not `"<exe>"<tail>`, or another tail (other arguments, unquoted `%1`, …).
    Malformed,
    /// The right shape and tail, but another executable.
    OtherExe,
}

/// Compares `found` with `expected` (both `"<exe>" <tail>`): exe by the path rule, tail exactly.
fn check_command(found: &str, expected: &str) -> CommandCheck {
    let (Some((found_exe, found_tail)), Some((expected_exe, expected_tail))) = (split_quoted_command(found), split_quoted_command(expected)) else {
        return CommandCheck::Malformed;
    };
    if found_tail != expected_tail {
        CommandCheck::Malformed
    } else if same_windows_path(found_exe, expected_exe) {
        CommandCheck::Matches
    } else {
        CommandCheck::OtherExe
    }
}

/// The `REG_SZ` data the writer's table holds for `value_name` under `key`.
fn table_string(table: &[RegistryValue], key: &str, value_name: Option<&str>) -> String {
    table
        .iter()
        .find_map(|value| match value {
            RegistryValue::String(value) if value.key == key && value.value_name.as_deref() == value_name => Some(value.data.clone()),
            RegistryValue::String(_) | RegistryValue::Dword(_) => None,
        })
        .unwrap_or_default()
}

/// Appends `ValueMissing` when `found` is absent or blank and returns the trimmed value.
fn require<'a>(found: Option<&'a str>, value: RecordValue, defects: &mut Vec<Defect>) -> Option<&'a str> {
    let present = found.map(str::trim).filter(|text| !text.is_empty());
    if present.is_none() {
        defects.push(Defect::ValueMissing { value });
    }
    present
}

/// Appends `WrongValue` when `found` differs from `expected` exactly (display texts).
fn check_text(found: &str, expected: &str, value: RecordValue, defects: &mut Vec<Defect>) {
    if found != expected {
        defects.push(Defect::WrongValue { value, expected: expected.to_owned(), found: found.to_owned() });
    }
}

/// Appends `WrongValue` when the path `found` (one pair of quotes allowed) differs from
/// `expected` by the path rule.
fn check_path(found: &str, expected: &str, value: RecordValue, defects: &mut Vec<Defect>) {
    let found_path = present_path_value(Some(found)).unwrap_or(found);
    if !same_windows_path(found_path, expected) {
        defects.push(Defect::WrongValue { value, expected: expected.to_owned(), found: found.to_owned() });
    }
}

/// Pushes the working-directory defect of an EXISTING directory `found` against `expected`:
/// none when they name the same path; `WorkingDirOutdated` when `found` is the directory of the
/// record's executable `owner` (what records held before the program root moved off it, e.g. a
/// repository build that now starts in its repository root); `WrongValue` otherwise.
fn check_working_dir(found: &str, expected: &str, owner: &str, value: RecordValue, defects: &mut Vec<Defect>) {
    let found_path = present_path_value(Some(found)).unwrap_or(found);
    if same_windows_path(found_path, expected) {
        return;
    }
    if windows_parent(owner).is_some_and(|exe_dir| same_windows_path(found_path, exe_dir)) {
        defects.push(Defect::WorkingDirOutdated { expected: expected.to_owned(), found: found.to_owned() });
    } else {
        defects.push(Defect::WrongValue { value, expected: expected.to_owned(), found: found.to_owned() });
    }
}

/// Builds a report row.
fn row(kind: RecordKind, scope: Scope, location: String, status: RecordStatus) -> RecordReport {
    RecordReport { kind, scope, location, status, shadowed: false }
}

/// The status of an existing record owned by the executable text `owner`, after `defects` were
/// collected; adds `TargetMissing` when the owner does not exist.
fn owned_status(owner: &str, identity: &CopyIdentity, exists: &dyn Fn(&Path) -> bool, mut defects: Vec<Defect>) -> RecordStatus {
    let owner_path = PathBuf::from(owner);
    let alive = exists(&owner_path);
    if !alive {
        defects.insert(0, Defect::TargetMissing { path: owner.to_owned() });
    }
    let ours = same_windows_path(owner, &identity.exe.to_string_lossy());
    classify(owner_path, ours, alive, defects)
}

/// Whose a record is, decided from its identifying executables.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Owner {
    /// Some identifying value names this copy's executable.
    Ours,
    /// None names this copy; the first ManhwaStudio-named one is another copy's executable.
    OtherCopy(String),
    /// None names a ManhwaStudio executable: the record at our name launches `found`, read
    /// from `value`.
    Foreign { value: RecordValue, found: String },
}

/// Whether the Windows path text `exe` names a file called [`WINDOWS_EXE_NAME`] (ASCII
/// case-insensitive, as Windows file names compare).
fn is_manhwastudio_exe(exe: &str) -> bool {
    exe.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().is_some_and(|name| name.eq_ignore_ascii_case(WINDOWS_EXE_NAME))
}

/// The owner of a record from its identifying executables `candidates` (the value each was
/// read from, the executable text), in priority order; `None` when there is none.
///
/// ANY candidate naming this copy makes the record ours (a hand-edited or half-rewritten entry
/// whose other values still name this copy is this copy's broken record, which Repair fixes).
/// Else the first candidate whose file name is the ManhwaStudio executable is another copy's;
/// else the record launches a foreign program ([`Owner::Foreign`], the first candidate).
fn resolve_owner(candidates: &[(RecordValue, String)], identity: &CopyIdentity) -> Option<Owner> {
    let ours = identity.exe.to_string_lossy();
    if candidates.iter().any(|(_, exe)| same_windows_path(exe, &ours)) {
        return Some(Owner::Ours);
    }
    if let Some((_, exe)) = candidates.iter().find(|(_, exe)| is_manhwastudio_exe(exe)) {
        return Some(Owner::OtherCopy(exe.clone()));
    }
    candidates.first().map(|(value, found)| Owner::Foreign { value: *value, found: found.clone() })
}

/// The status of a record at ManhwaStudio's name that launches the foreign program `found`
/// (read from `value`): this copy's name, broken, judged against nothing else (see
/// [`Defect::ForeignProgram`]).
fn foreign_status(value: RecordValue, found: String, identity: &CopyIdentity) -> RecordStatus {
    let expected = identity.exe.to_string_lossy().into_owned();
    classify(identity.exe.clone(), true, true, vec![Defect::ForeignProgram { value, expected, found }])
}

/// Judges the Uninstall entry under `scope`'s root.
///
/// Owner ([`resolve_owner`] over, in this order, the quoted `UninstallString` executable,
/// `DisplayIcon`, `InstallLocation\manhwastudio_rs.exe` and the unquoted command's first token):
/// ours when ANY of them names this copy, else the first ManhwaStudio executable among them,
/// else a foreign program (`OursBroken` with `ForeignProgram`); none -> `Unreadable(NoOwner)`.
/// Against the owner:
/// `UninstallString` must be `"<exe>" --uninstall` (else `MalformedCommand`), `InstallLocation`
/// the exe's directory, `DisplayIcon` the exe, `DisplayName` / `Publisher` the product texts,
/// `QuietUninstallString` the uninstall command. `DisplayVersion` is compared with
/// `identity.version_core` only for this copy's own entry and only when the identity carries a
/// version (`VersionOutdated`); the `--uninstall` support of another copy is not judged.
#[must_use]
pub fn evaluate_program_entry(scope: Scope, observed: &Observed<ObservedUninstall>, identity: &CopyIdentity, exists: &dyn Fn(&Path) -> bool) -> RecordReport {
    let root = registry_root(scope);
    let location = uninstall_key(root);
    let values = match observed {
        Ok(None) => return row(RecordKind::ProgramEntry, scope, location, RecordStatus::Missing),
        Err(error) => return row(RecordKind::ProgramEntry, scope, location, RecordStatus::Unreadable(error.clone())),
        Ok(Some(values)) => values,
    };
    // A quoted command names its exe exactly; an unquoted one is cut at the first space, so it
    // is trusted only after every other value (it is reported as malformed either way).
    let uninstall_string = values.uninstall_string.as_deref();
    let from_command = uninstall_string.and_then(split_quoted_command).map(|(exe, _)| exe).filter(|exe| !exe.trim().is_empty()).map(str::to_owned);
    let from_icon = present_path_value(values.display_icon.as_deref().map(strip_icon_index)).map(str::to_owned);
    let from_location = present_path_value(values.install_location.as_deref()).map(|dir| format!(r"{}\{WINDOWS_EXE_NAME}", dir.trim_end_matches(['\\', '/'])));
    let from_unquoted = uninstall_string.and_then(command_owner).map(str::to_owned);
    let candidates: Vec<(RecordValue, String)> = [
        (RecordValue::UninstallString, from_command),
        (RecordValue::DisplayIcon, from_icon),
        (RecordValue::InstallLocation, from_location),
        (RecordValue::UninstallString, from_unquoted),
    ]
    .into_iter()
    .filter_map(|(value, exe)| exe.map(|exe| (value, exe)))
    .collect();
    let owner = match resolve_owner(&candidates, identity) {
        None => return row(RecordKind::ProgramEntry, scope, location.clone(), RecordStatus::Unreadable(ProbeError::NoOwner { location })),
        Some(Owner::Foreign { value, found }) => return row(RecordKind::ProgramEntry, scope, location, foreign_status(value, found, identity)),
        Some(Owner::Ours) => identity.exe.to_string_lossy().into_owned(),
        Some(Owner::OtherCopy(exe)) => exe,
    };
    let install_dir = windows_parent(&owner).unwrap_or_default();
    let table = uninstall_and_app_paths_values(root, install_dir, &owner, identity.version_core.as_deref().unwrap_or_default());
    let expected = |value_name: &str| table_string(&table, &location, Some(value_name));
    let ours = same_windows_path(&owner, &identity.exe.to_string_lossy());

    let mut defects = Vec::new();
    if let Some(command) = require(values.uninstall_string.as_deref(), RecordValue::UninstallString, &mut defects) {
        match check_command(command, &expected("UninstallString")) {
            CommandCheck::Matches => {}
            CommandCheck::Malformed => defects.push(Defect::MalformedCommand { command: command.to_owned() }),
            CommandCheck::OtherExe => defects.push(Defect::WrongValue { value: RecordValue::UninstallString, expected: expected("UninstallString"), found: command.to_owned() }),
        }
    }
    if let Some(found) = require(values.install_location.as_deref(), RecordValue::InstallLocation, &mut defects) {
        check_path(found, install_dir, RecordValue::InstallLocation, &mut defects);
    }
    if let Some(found) = require(values.display_icon.as_deref(), RecordValue::DisplayIcon, &mut defects) {
        check_path(strip_icon_index(found), &owner, RecordValue::DisplayIcon, &mut defects);
    }
    if let Some(found) = require(values.display_name.as_deref(), RecordValue::DisplayName, &mut defects) {
        check_text(found, &expected("DisplayName"), RecordValue::DisplayName, &mut defects);
    }
    if let Some(found) = require(values.publisher.as_deref(), RecordValue::Publisher, &mut defects) {
        check_text(found, &expected("Publisher"), RecordValue::Publisher, &mut defects);
    }
    if let Some(found) = require(values.quiet_uninstall_string.as_deref(), RecordValue::QuietUninstallString, &mut defects)
        && check_command(found, &expected("QuietUninstallString")) != CommandCheck::Matches
    {
        defects.push(Defect::WrongValue { value: RecordValue::QuietUninstallString, expected: expected("QuietUninstallString"), found: found.to_owned() });
    }
    if let Some(found) = require(values.display_version.as_deref(), RecordValue::DisplayVersion, &mut defects)
        && ours
        && let Some(version) = identity.version_core.as_deref()
        && found != version
    {
        defects.push(Defect::VersionOutdated { found: found.to_owned() });
    }
    row(RecordKind::ProgramEntry, scope, location, owned_status(&owner, identity, exists, defects))
}

/// Judges the App Paths entry under `scope`'s root. Owner ([`resolve_owner`] over the default
/// value and `Path\manhwastudio_rs.exe`): ours when either names this copy, else the first
/// ManhwaStudio executable, else a foreign program. The default value must be the exe and
/// `Path` its directory.
#[must_use]
pub fn evaluate_app_paths(scope: Scope, observed: &Observed<ObservedAppPaths>, identity: &CopyIdentity, exists: &dyn Fn(&Path) -> bool) -> RecordReport {
    let location = app_paths_key(registry_root(scope));
    let values = match observed {
        Ok(None) => return row(RecordKind::AppPaths, scope, location, RecordStatus::Missing),
        Err(error) => return row(RecordKind::AppPaths, scope, location, RecordStatus::Unreadable(error.clone())),
        Ok(Some(values)) => values,
    };
    let from_exe = present_path_value(values.exe.as_deref()).map(str::to_owned);
    let from_path = present_path_value(values.path.as_deref()).map(|dir| format!(r"{}\{WINDOWS_EXE_NAME}", dir.trim_end_matches(['\\', '/'])));
    let candidates: Vec<(RecordValue, String)> =
        [(RecordValue::AppPathsExe, from_exe), (RecordValue::AppPathsPath, from_path)].into_iter().filter_map(|(value, exe)| exe.map(|exe| (value, exe))).collect();
    let owner = match resolve_owner(&candidates, identity) {
        None => return row(RecordKind::AppPaths, scope, location.clone(), RecordStatus::Unreadable(ProbeError::NoOwner { location })),
        Some(Owner::Foreign { value, found }) => return row(RecordKind::AppPaths, scope, location, foreign_status(value, found, identity)),
        Some(Owner::Ours) => identity.exe.to_string_lossy().into_owned(),
        Some(Owner::OtherCopy(exe)) => exe,
    };
    let install_dir = windows_parent(&owner).unwrap_or_default();
    let mut defects = Vec::new();
    if let Some(found) = require(values.exe.as_deref(), RecordValue::AppPathsExe, &mut defects) {
        check_path(found, &owner, RecordValue::AppPathsExe, &mut defects);
    }
    if let Some(found) = require(values.path.as_deref(), RecordValue::AppPathsPath, &mut defects) {
        check_path(found, install_dir, RecordValue::AppPathsPath, &mut defects);
    }
    row(RecordKind::AppPaths, scope, location, owned_status(&owner, identity, exists, defects))
}

/// The `SupportedTypes` value names (`.png`, …) the "Open with" writer registers, in table order.
#[must_use]
pub fn expected_supported_types(root: &str) -> Vec<String> {
    let supported_key = format!(r"{}\SupportedTypes", windows_open_with_app_key(root));
    windows_open_with_registry_values(root, "")
        .into_iter()
        .filter(|value| value.key == supported_key)
        .filter_map(|value| value.value_name)
        .collect()
}

/// Judges the "Open with" key under `scope`'s root. Owner = the open command's executable (a
/// foreign program when its file name is not the ManhwaStudio executable); a key without a
/// command names no program (`Unreadable(NoOwner)`). The command must be `"<exe>" "%1"`,
/// `FriendlyAppName` the product name, every readable type listed in `SupportedTypes`.
/// `shadowed` marks an existing `Machine` key hidden by an `HKCU` key of the same name; a
/// missing one is never shadowed.
#[must_use]
pub fn evaluate_open_with(
    scope: Scope,
    observed: &Observed<ObservedOpenWith>,
    shadowed: bool,
    identity: &CopyIdentity,
    exists: &dyn Fn(&Path) -> bool,
) -> RecordReport {
    let root = registry_root(scope);
    let location = windows_open_with_app_key(root);
    let shadowed = shadowed && scope == Scope::Machine && !matches!(observed, Ok(None));
    let status = match observed {
        Ok(None) => RecordStatus::Missing,
        Err(error) => RecordStatus::Unreadable(error.clone()),
        Ok(Some(values)) => match values.command.as_deref().and_then(command_owner).map(|exe| (exe, resolve_owner(&[(RecordValue::OpenCommand, exe.to_owned())], identity))) {
            None | Some((_, None)) => RecordStatus::Unreadable(ProbeError::NoOwner { location: location.clone() }),
            Some((_, Some(Owner::Foreign { value, found }))) => foreign_status(value, found, identity),
            // The command names one executable: ours or another copy's, judged as itself.
            Some((owner, Some(Owner::Ours | Owner::OtherCopy(_)))) => {
                let table = windows_open_with_registry_values(root, owner);
                let command_key = format!(r"{location}\shell\open\command");
                let expected_command = table.iter().find(|value| value.key == command_key).map(|value| value.data.clone()).unwrap_or_default();
                let expected_name = table.iter().find(|value| value.value_name.as_deref() == Some("FriendlyAppName")).map(|value| value.data.clone()).unwrap_or_default();
                let mut defects = Vec::new();
                if let Some(command) = values.command.as_deref() {
                    match check_command(command, &expected_command) {
                        CommandCheck::Matches => {}
                        CommandCheck::Malformed => defects.push(Defect::MalformedCommand { command: command.to_owned() }),
                        CommandCheck::OtherExe => defects.push(Defect::WrongValue { value: RecordValue::OpenCommand, expected: expected_command, found: command.to_owned() }),
                    }
                }
                if let Some(found) = require(values.friendly_app_name.as_deref(), RecordValue::FriendlyAppName, &mut defects) {
                    check_text(found, &expected_name, RecordValue::FriendlyAppName, &mut defects);
                }
                let missing: Vec<String> = expected_supported_types(root).into_iter().filter(|name| !values.supported_types.contains(name)).collect();
                if !missing.is_empty() {
                    defects.push(Defect::MissingImageTypes { types: missing });
                }
                owned_status(owner, identity, exists, defects)
            }
        },
    };
    RecordReport { kind: RecordKind::OpenWith, scope, location, status, shadowed }
}

/// Judges the Start-menu shortcut at `lnk` (`None` = its folder could not be resolved;
/// `folder` names it). Owner = the link target (a foreign program when its file name is not the
/// ManhwaStudio executable: `OursBroken` with `ForeignProgram`). The working directory must be
/// the program root the writer uses (`ShortcutSpec::for_copy` for this copy,
/// `ShortcutSpec::for_launcher` of the target for another copy); one still at the target's own
/// directory where the program root moved off it is `WorkingDirOutdated`. The link carries no
/// arguments.
#[must_use]
pub fn evaluate_start_menu(
    scope: Scope,
    lnk: Option<&Path>,
    folder: &'static str,
    observed: &Observed<ObservedShortcut>,
    identity: &CopyIdentity,
    exists: &dyn Fn(&Path) -> bool,
) -> RecordReport {
    let Some(lnk) = lnk else {
        let location = format!(r"{folder}\{}", crate::identity::SHORTCUT_FILE_NAME);
        return row(RecordKind::StartMenu, scope, location, RecordStatus::Unreadable(ProbeError::FolderUnresolved { folder }));
    };
    let location = lnk.display().to_string();
    let link = match observed {
        Ok(None) => return row(RecordKind::StartMenu, scope, location, RecordStatus::Missing),
        Err(error) => return row(RecordKind::StartMenu, scope, location, RecordStatus::Unreadable(error.clone())),
        Ok(Some(link)) => link,
    };
    let owner = link.target.to_string_lossy().into_owned();
    if owner.trim().is_empty() {
        return row(RecordKind::StartMenu, scope, location.clone(), RecordStatus::Unreadable(ProbeError::NoOwner { location }));
    }
    // The target is the one identifying value: ours or another copy's is judged as itself.
    if let Some(Owner::Foreign { value, found }) = resolve_owner(&[(RecordValue::ShortcutTarget, owner.clone())], identity) {
        return row(RecordKind::StartMenu, scope, location, foreign_status(value, found, identity));
    }
    let mut defects = Vec::new();
    let working_dir = link.working_dir.to_string_lossy();
    if let Some(found) = require(Some(&working_dir), RecordValue::ShortcutWorkingDir, &mut defects) {
        // This copy's link is judged against the shortcut the action writer makes for it (the
        // identity's program root); another copy's against the spec of its exe alone.
        let expected = if same_windows_path(&owner, &identity.exe.to_string_lossy()) {
            Some(ShortcutSpec::for_copy(identity))
        } else {
            ShortcutSpec::for_launcher(&link.target)
        };
        if !exists(Path::new(found)) {
            defects.push(Defect::WorkingDirMissing { path: found.to_owned() });
        } else if let Some(spec) = expected {
            check_working_dir(found, &spec.working_dir.to_string_lossy(), &owner, RecordValue::ShortcutWorkingDir, &mut defects);
        }
    }
    if !link.arguments.trim().is_empty() {
        defects.push(Defect::WrongValue { value: RecordValue::ShortcutArguments, expected: String::new(), found: link.arguments.clone() });
    }
    row(RecordKind::StartMenu, scope, location, owned_status(&owner, identity, exists, defects))
}

/// Reads and judges every Windows record of `identity` under both roots, in the order
/// StartMenu, ProgramEntry, AppPaths, OpenWith (each `User` then `Machine`). Blocking.
#[cfg(target_os = "windows")]
#[must_use]
pub fn probe_records(identity: &CopyIdentity) -> Vec<RecordReport> {
    let exists = |path: &Path| crate::report::path_exists(path);
    let mut records = Vec::with_capacity(8);
    for (scope, all_users, folder) in [(Scope::User, false, "Programs"), (Scope::Machine, true, "CommonPrograms")] {
        let lnk = super::shortcut::windows_start_menu_programs_dir(all_users).map(|dir| dir.join(crate::identity::SHORTCUT_FILE_NAME));
        let observed = lnk.as_deref().map_or(Ok(None), read_start_menu);
        records.push(evaluate_start_menu(scope, lnk.as_deref(), folder, &observed, identity, &exists));
    }
    for scope in [Scope::User, Scope::Machine] {
        records.push(evaluate_program_entry(scope, &read_uninstall(registry_root(scope)), identity, &exists));
    }
    for scope in [Scope::User, Scope::Machine] {
        records.push(evaluate_app_paths(scope, &read_app_paths(registry_root(scope)), identity, &exists));
    }
    let user_open_with = read_open_with(registry_root(Scope::User));
    // Explorer's merged HKCR view takes the HKCU key over the HKLM one of the same name.
    let machine_shadowed = matches!(user_open_with, Ok(Some(_)));
    records.push(evaluate_open_with(Scope::User, &user_open_with, false, identity, &exists));
    records.push(evaluate_open_with(Scope::Machine, &read_open_with(registry_root(Scope::Machine)), machine_shadowed, identity, &exists));
    records
}

/// Reads the `.lnk` at `lnk`: absent -> `Ok(None)`.
#[cfg(target_os = "windows")]
fn read_start_menu(lnk: &Path) -> Observed<ObservedShortcut> {
    match lnk.try_exists() {
        Ok(false) => return Ok(None),
        Ok(true) => {}
        Err(error) => return Err(ProbeError::io(lnk.to_path_buf(), &error)),
    }
    let link = super::shortcut::read_shortcut(lnk).map_err(|error| ProbeError::Shortcut { path: lnk.to_path_buf(), message: error.to_string() })?;
    Ok(Some(ObservedShortcut { target: link.target, arguments: link.arguments.to_string_lossy().into_owned(), working_dir: link.working_dir }))
}

/// Reads the named `REG_SZ` values of `key` when the key exists (`Ok(None)` when it does not).
/// Any read failure makes the whole record unreadable.
#[cfg(target_os = "windows")]
fn read_key_values<const N: usize>(key: &str, value_names: [Option<&str>; N]) -> Observed<[Option<String>; N]> {
    use super::registry::{RegistryKeyPresence, reg_read_string, registry_key_presence};
    match registry_key_presence(key) {
        RegistryKeyPresence::Absent => return Ok(None),
        RegistryKeyPresence::Present => {}
        RegistryKeyPresence::Unknown(code) => return Err(ProbeError::Registry { key: key.to_owned(), value_name: None, code }),
    }
    let mut values: [Option<String>; N] = std::array::from_fn(|_| None);
    for (slot, value_name) in values.iter_mut().zip(value_names) {
        *slot = reg_read_string(key, value_name)
            .map_err(|code| ProbeError::Registry { key: key.to_owned(), value_name: value_name.map(str::to_owned), code })?;
    }
    Ok(Some(values))
}

/// Reads the Uninstall entry under `root`.
#[cfg(target_os = "windows")]
fn read_uninstall(root: &str) -> Observed<ObservedUninstall> {
    let names = [
        Some("DisplayName"),
        Some("Publisher"),
        Some("DisplayVersion"),
        Some("InstallLocation"),
        Some("DisplayIcon"),
        Some("UninstallString"),
        Some("QuietUninstallString"),
    ];
    Ok(read_key_values(&uninstall_key(root), names)?.map(
        |[display_name, publisher, display_version, install_location, display_icon, uninstall_string, quiet_uninstall_string]| ObservedUninstall {
            display_name,
            publisher,
            display_version,
            install_location,
            display_icon,
            uninstall_string,
            quiet_uninstall_string,
        },
    ))
}

/// Reads the App Paths entry under `root`.
#[cfg(target_os = "windows")]
fn read_app_paths(root: &str) -> Observed<ObservedAppPaths> {
    Ok(read_key_values(&app_paths_key(root), [None, Some("Path")])?.map(|[exe, path]| ObservedAppPaths { exe, path }))
}

/// Reads the "Open with" key under `root`: its name, its command and which expected
/// `SupportedTypes` values exist.
#[cfg(target_os = "windows")]
fn read_open_with(root: &str) -> Observed<ObservedOpenWith> {
    let app_key = windows_open_with_app_key(root);
    let Some([friendly_app_name]) = read_key_values(&app_key, [Some("FriendlyAppName")])? else {
        return Ok(None);
    };
    // Subkeys read as absent values when they do not exist (`reg_read_string` -> `Ok(None)`).
    let [command] = read_key_values(&format!(r"{app_key}\shell\open\command"), [None])?.unwrap_or_default();
    let supported_key = format!(r"{app_key}\SupportedTypes");
    let mut supported_types = Vec::new();
    for name in expected_supported_types(root) {
        let found = super::registry::reg_read_string(&supported_key, Some(&name))
            .map_err(|code| ProbeError::Registry { key: supported_key.clone(), value_name: Some(name.clone()), code })?;
        if found.is_some() {
            supported_types.push(name);
        }
    }
    Ok(Some(ObservedOpenWith { friendly_app_name, command, supported_types }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTALL: &str = r"C:\Program Files\ManhwaStudio";
    const EXE: &str = r"C:\Program Files\ManhwaStudio\manhwastudio_rs.exe";
    const OTHER: &str = r"D:\Portable\ManhwaStudio\manhwastudio_rs.exe";

    fn identity() -> CopyIdentity {
        CopyIdentity { exe: PathBuf::from(EXE), program_root: PathBuf::from(INSTALL), version_core: Some("3.2.1".to_owned()) }
    }

    /// Oracle: every path exists except those in `missing`.
    fn all_exist_but(missing: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |path: &Path| !missing.iter().any(|gone| same_windows_path(&path.to_string_lossy(), gone))
    }

    /// The Uninstall values the installer writes for `exe` (from the writer's table).
    fn written_uninstall(exe: &str, version: &str) -> ObservedUninstall {
        let dir = windows_parent(exe).expect("exe has a dir");
        let table = uninstall_and_app_paths_values("HKLM", dir, exe, version);
        let key = uninstall_key("HKLM");
        let get = |name: &str| Some(table_string(&table, &key, Some(name)));
        ObservedUninstall {
            display_name: get("DisplayName"),
            publisher: get("Publisher"),
            display_version: get("DisplayVersion"),
            install_location: get("InstallLocation"),
            display_icon: get("DisplayIcon"),
            uninstall_string: get("UninstallString"),
            quiet_uninstall_string: get("QuietUninstallString"),
        }
    }

    fn written_open_with(exe: &str) -> ObservedOpenWith {
        ObservedOpenWith {
            friendly_app_name: Some("ManhwaStudio".to_owned()),
            command: Some(format!("\"{exe}\" \"%1\"")),
            supported_types: expected_supported_types("HKLM"),
        }
    }

    fn status_of(report: RecordReport) -> RecordStatus {
        report.status
    }

    /// Missing and unreadable records, every kind.
    #[test]
    fn absent_and_unreadable_records() {
        let id = identity();
        let exists = all_exist_but(&[]);
        let denied = ProbeError::Registry { key: "k".to_owned(), value_name: None, code: 5 };
        assert_eq!(status_of(evaluate_program_entry(Scope::User, &Ok(None), &id, &exists)), RecordStatus::Missing);
        assert_eq!(status_of(evaluate_app_paths(Scope::User, &Ok(None), &id, &exists)), RecordStatus::Missing);
        assert_eq!(status_of(evaluate_open_with(Scope::User, &Ok(None), false, &id, &exists)), RecordStatus::Missing);
        let lnk = PathBuf::from("C:/Users/u/Start Menu/Programs/ManhwaStudio.lnk");
        assert_eq!(status_of(evaluate_start_menu(Scope::User, Some(&lnk), "Programs", &Ok(None), &id, &exists)), RecordStatus::Missing);
        assert_eq!(status_of(evaluate_program_entry(Scope::Machine, &Err(denied.clone()), &id, &exists)), RecordStatus::Unreadable(denied.clone()));
        assert_eq!(status_of(evaluate_app_paths(Scope::Machine, &Err(denied.clone()), &id, &exists)), RecordStatus::Unreadable(denied.clone()));
        assert_eq!(status_of(evaluate_open_with(Scope::Machine, &Err(denied.clone()), false, &id, &exists)), RecordStatus::Unreadable(denied.clone()));
        assert_eq!(status_of(evaluate_start_menu(Scope::Machine, Some(&lnk), "CommonPrograms", &Err(denied.clone()), &id, &exists)), RecordStatus::Unreadable(denied));
        let unresolved = evaluate_start_menu(Scope::Machine, None, "CommonPrograms", &Ok(None), &id, &exists);
        assert_eq!(unresolved.status, RecordStatus::Unreadable(ProbeError::FolderUnresolved { folder: "CommonPrograms" }));
        assert_eq!(unresolved.location, r"CommonPrograms\ManhwaStudio.lnk");
    }

    /// The entry the installer writes is `OursOk`; locations name the scope's root.
    #[test]
    fn written_program_entry_is_ours_ok() {
        let id = identity();
        let report = evaluate_program_entry(Scope::Machine, &Ok(Some(written_uninstall(EXE, "3.2.1"))), &id, &all_exist_but(&[]));
        assert_eq!(report.status, RecordStatus::OursOk);
        assert_eq!(report.location, r"HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\ManhwaStudio");
        let report = evaluate_program_entry(Scope::User, &Ok(Some(written_uninstall(EXE, "3.2.1"))), &id, &all_exist_but(&[]));
        assert!(report.location.starts_with(r"HKCU\"));
        // Case and separators of the stored paths do not matter.
        let mut values = written_uninstall(EXE, "3.2.1");
        values.install_location = Some(r"c:/program files/manhwastudio/".to_owned());
        values.display_icon = Some(r"C:\PROGRAM FILES\ManhwaStudio\manhwastudio_rs.exe,0".to_owned());
        assert_eq!(status_of(evaluate_program_entry(Scope::Machine, &Ok(Some(values)), &id, &all_exist_but(&[]))), RecordStatus::OursOk);
        // A copy that does not judge versions accepts any DisplayVersion.
        let unversioned = CopyIdentity { version_core: None, ..identity() };
        let report = evaluate_program_entry(Scope::Machine, &Ok(Some(written_uninstall(EXE, "1.0.0"))), &unversioned, &all_exist_but(&[]));
        assert_eq!(report.status, RecordStatus::OursOk);
    }

    /// Stale defects of an own entry: old version, a pre-version entry, display texts.
    #[test]
    fn program_entry_stale_defects() {
        let id = identity();
        let exists = all_exist_but(&[]);
        let outdated = evaluate_program_entry(Scope::Machine, &Ok(Some(written_uninstall(EXE, "3.0.0"))), &id, &exists);
        assert_eq!(outdated.status, RecordStatus::OursStale(vec![Defect::VersionOutdated { found: "3.0.0".to_owned() }]));
        let mut legacy = written_uninstall(EXE, "3.2.1");
        legacy.display_version = None;
        legacy.quiet_uninstall_string = None;
        legacy.publisher = Some("Someone".to_owned());
        assert_eq!(
            status_of(evaluate_program_entry(Scope::Machine, &Ok(Some(legacy)), &id, &exists)),
            RecordStatus::OursStale(vec![
                Defect::WrongValue { value: RecordValue::Publisher, expected: "Vasyanator".to_owned(), found: "Someone".to_owned() },
                Defect::ValueMissing { value: RecordValue::QuietUninstallString },
                Defect::ValueMissing { value: RecordValue::DisplayVersion },
            ])
        );
    }

    /// Broken defects of an own entry: malformed / unquoted uninstall command, wrong location,
    /// wrong icon, missing name, gone executable.
    #[test]
    fn program_entry_broken_defects() {
        let id = identity();
        let exists = all_exist_but(&[]);
        let judge = |values: ObservedUninstall| status_of(evaluate_program_entry(Scope::Machine, &Ok(Some(values)), &id, &exists));

        let mut unquoted = written_uninstall(EXE, "3.2.1");
        unquoted.uninstall_string = Some(format!("{EXE} --uninstall"));
        // Unquoted: the owner comes from DisplayIcon, the command itself is malformed.
        assert_eq!(judge(unquoted), RecordStatus::OursBroken(vec![Defect::MalformedCommand { command: format!("{EXE} --uninstall") }]));
        let only_unquoted = ObservedUninstall { uninstall_string: Some(r"C:\Apps\manhwastudio_rs.exe --uninstall".to_owned()), ..ObservedUninstall::default() };
        let RecordStatus::OtherCopy { exe, .. } = judge(only_unquoted) else {
            panic!("an unquoted command alone still names its owner");
        };
        assert_eq!(exe, PathBuf::from(r"C:\Apps\manhwastudio_rs.exe"));

        let mut wrong_tail = written_uninstall(EXE, "3.2.1");
        wrong_tail.uninstall_string = Some(format!("\"{EXE}\" --remove"));
        assert_eq!(judge(wrong_tail), RecordStatus::OursBroken(vec![Defect::MalformedCommand { command: format!("\"{EXE}\" --remove") }]));

        let mut wrong_location = written_uninstall(EXE, "3.2.1");
        wrong_location.install_location = Some(r"C:\Elsewhere".to_owned());
        assert_eq!(
            judge(wrong_location),
            RecordStatus::OursBroken(vec![Defect::WrongValue { value: RecordValue::InstallLocation, expected: INSTALL.to_owned(), found: r"C:\Elsewhere".to_owned() }])
        );

        let mut wrong_icon = written_uninstall(EXE, "3.2.1");
        wrong_icon.display_icon = Some(OTHER.to_owned());
        assert_eq!(
            judge(wrong_icon),
            RecordStatus::OursBroken(vec![Defect::WrongValue { value: RecordValue::DisplayIcon, expected: EXE.to_owned(), found: OTHER.to_owned() }])
        );

        let mut nameless = written_uninstall(EXE, "3.2.1");
        nameless.display_name = None;
        assert_eq!(judge(nameless), RecordStatus::OursBroken(vec![Defect::ValueMissing { value: RecordValue::DisplayName }]));

        let mut no_command = written_uninstall(EXE, "3.2.1");
        no_command.uninstall_string = None;
        // The owner then comes from DisplayIcon.
        assert_eq!(judge(no_command), RecordStatus::OursBroken(vec![Defect::ValueMissing { value: RecordValue::UninstallString }]));

        let gone = all_exist_but(&[EXE]);
        let report = evaluate_program_entry(Scope::Machine, &Ok(Some(written_uninstall(EXE, "3.2.1"))), &id, &gone);
        assert_eq!(report.status, RecordStatus::OursBroken(vec![Defect::TargetMissing { path: EXE.to_owned() }]));
    }

    /// Another copy's entry: alive and valid, alive and broken, gone; its version is not judged.
    #[test]
    fn program_entry_of_another_copy() {
        let id = identity();
        let report = evaluate_program_entry(Scope::Machine, &Ok(Some(written_uninstall(OTHER, "1.0.0"))), &id, &all_exist_but(&[]));
        assert_eq!(report.status, RecordStatus::OtherCopy { exe: PathBuf::from(OTHER), alive: true, defects: Vec::new() });
        let mut broken = written_uninstall(OTHER, "1.0.0");
        broken.install_location = Some(r"E:\Elsewhere".to_owned());
        let report = evaluate_program_entry(Scope::Machine, &Ok(Some(broken)), &id, &all_exist_but(&[]));
        assert_eq!(
            report.status,
            RecordStatus::OtherCopy {
                exe: PathBuf::from(OTHER),
                alive: true,
                defects: vec![Defect::WrongValue { value: RecordValue::InstallLocation, expected: r"D:\Portable\ManhwaStudio".to_owned(), found: r"E:\Elsewhere".to_owned() }],
            }
        );
        let report = evaluate_program_entry(Scope::Machine, &Ok(Some(written_uninstall(OTHER, "1.0.0"))), &id, &all_exist_but(&[OTHER]));
        assert_eq!(report.status, RecordStatus::OtherCopy { exe: PathBuf::from(OTHER), alive: false, defects: vec![Defect::TargetMissing { path: OTHER.to_owned() }] });
    }

    /// Owner fallbacks: DisplayIcon, then InstallLocation; nothing -> NoOwner.
    #[test]
    fn program_entry_owner_fallbacks() {
        let id = identity();
        let exists = all_exist_but(&[]);
        let only_location = ObservedUninstall { install_location: Some(format!("{INSTALL}\\")), ..ObservedUninstall::default() };
        let RecordStatus::OursBroken(defects) = status_of(evaluate_program_entry(Scope::Machine, &Ok(Some(only_location)), &id, &exists)) else {
            panic!("an entry with only InstallLocation is ours and broken");
        };
        assert!(defects.contains(&Defect::ValueMissing { value: RecordValue::UninstallString }));
        let empty = ObservedUninstall { display_name: Some("ManhwaStudio".to_owned()), ..ObservedUninstall::default() };
        let report = evaluate_program_entry(Scope::User, &Ok(Some(empty)), &id, &exists);
        assert_eq!(report.status, RecordStatus::Unreadable(ProbeError::NoOwner { location: uninstall_key("HKCU") }));
    }

    /// App Paths: written = ok; wrong `Path` = broken; missing `Path` = stale; owner from
    /// `Path` alone; another copy; no evidence.
    #[test]
    fn app_paths_table() {
        let id = identity();
        let exists = all_exist_but(&[]);
        let judge = |exe: Option<&str>, path: Option<&str>| {
            status_of(evaluate_app_paths(Scope::Machine, &Ok(Some(ObservedAppPaths { exe: exe.map(str::to_owned), path: path.map(str::to_owned) })), &id, &exists))
        };
        assert_eq!(judge(Some(EXE), Some(INSTALL)), RecordStatus::OursOk);
        assert_eq!(judge(Some(&format!("\"{EXE}\"")), Some(INSTALL)), RecordStatus::OursOk);
        assert_eq!(judge(Some(EXE), None), RecordStatus::OursStale(vec![Defect::ValueMissing { value: RecordValue::AppPathsPath }]));
        assert_eq!(
            judge(Some(EXE), Some(r"D:\x")),
            RecordStatus::OursBroken(vec![Defect::WrongValue { value: RecordValue::AppPathsPath, expected: INSTALL.to_owned(), found: r"D:\x".to_owned() }])
        );
        assert_eq!(judge(None, Some(INSTALL)), RecordStatus::OursBroken(vec![Defect::ValueMissing { value: RecordValue::AppPathsExe }]));
        assert_eq!(judge(Some(OTHER), Some(r"D:\Portable\ManhwaStudio")), RecordStatus::OtherCopy { exe: PathBuf::from(OTHER), alive: true, defects: Vec::new() });
        assert_eq!(judge(None, None), RecordStatus::Unreadable(ProbeError::NoOwner { location: app_paths_key("HKLM") }));
    }

    /// "Open with": written = ok; missing image types = stale; unquoted `%1` = broken; no
    /// command = no owner; another copy alive/dead; shadowed only on the Machine row.
    #[test]
    fn open_with_table() {
        let id = identity();
        let exists = all_exist_but(&[]);
        let judge = |values: ObservedOpenWith| status_of(evaluate_open_with(Scope::User, &Ok(Some(values)), false, &id, &exists));
        assert_eq!(judge(written_open_with(EXE)), RecordStatus::OursOk);

        let mut partial = written_open_with(EXE);
        partial.supported_types.retain(|name| name != ".webp" && name != ".qoi");
        assert_eq!(judge(partial), RecordStatus::OursStale(vec![Defect::MissingImageTypes { types: vec![".webp".to_owned(), ".qoi".to_owned()] }]));

        let mut unquoted_arg = written_open_with(EXE);
        unquoted_arg.command = Some(format!("\"{EXE}\" %1"));
        assert_eq!(judge(unquoted_arg), RecordStatus::OursBroken(vec![Defect::MalformedCommand { command: format!("\"{EXE}\" %1") }]));

        let mut renamed = written_open_with(EXE);
        renamed.friendly_app_name = None;
        assert_eq!(judge(renamed), RecordStatus::OursStale(vec![Defect::ValueMissing { value: RecordValue::FriendlyAppName }]));

        let mut no_command = written_open_with(EXE);
        no_command.command = None;
        assert_eq!(judge(no_command), RecordStatus::Unreadable(ProbeError::NoOwner { location: windows_open_with_app_key("HKCU") }));

        assert_eq!(judge(written_open_with(OTHER)), RecordStatus::OtherCopy { exe: PathBuf::from(OTHER), alive: true, defects: Vec::new() });
        let dead = evaluate_open_with(Scope::User, &Ok(Some(written_open_with(OTHER))), false, &id, &all_exist_but(&[OTHER]));
        assert_eq!(dead.status, RecordStatus::OtherCopy { exe: PathBuf::from(OTHER), alive: false, defects: vec![Defect::TargetMissing { path: OTHER.to_owned() }] });

        assert!(evaluate_open_with(Scope::Machine, &Ok(Some(written_open_with(EXE))), true, &id, &exists).shadowed);
        assert!(!evaluate_open_with(Scope::User, &Ok(Some(written_open_with(EXE))), true, &id, &exists).shadowed);
        assert!(!evaluate_open_with(Scope::Machine, &Ok(None), true, &id, &exists).shadowed, "a missing key is never shadowed");
        let denied = ProbeError::Registry { key: "k".to_owned(), value_name: None, code: 5 };
        assert!(evaluate_open_with(Scope::Machine, &Err(denied), true, &id, &exists).shadowed, "an unreadable key exists");
    }

    /// The expected SupportedTypes names are the writer's (every readable extension with a dot).
    #[test]
    fn expected_supported_types_follow_the_writer() {
        let names = expected_supported_types("HKCU");
        let expected: Vec<String> = ms_config::single_image::input_extensions().map(|extension| format!(".{extension}")).collect();
        assert_eq!(names, expected);
    }

    /// Start-menu shortcut: written = ok; another working dir = broken; blank = stale;
    /// arguments = stale; gone working dir / target = broken; other copy; no target.
    #[test]
    fn start_menu_table() {
        let exe = "C:/Apps/MS/manhwastudio_rs.exe";
        let id = CopyIdentity { exe: PathBuf::from(exe), program_root: PathBuf::from("C:/Apps/MS"), version_core: None };
        let lnk = PathBuf::from("C:/Users/u/Programs/ManhwaStudio.lnk");
        let link = |target: &str, args: &str, dir: &str| ObservedShortcut { target: PathBuf::from(target), arguments: args.to_owned(), working_dir: PathBuf::from(dir) };
        let judge = |observed: ObservedShortcut, exists: &dyn Fn(&Path) -> bool| status_of(evaluate_start_menu(Scope::User, Some(&lnk), "Programs", &Ok(Some(observed)), &id, exists));
        let all = all_exist_but(&[]);
        assert_eq!(judge(link(exe, "", "C:/Apps/MS"), &all), RecordStatus::OursOk);
        assert_eq!(judge(link(r"c:\apps\ms\MANHWASTUDIO_RS.EXE", "", r"C:\Apps\MS\"), &all), RecordStatus::OursOk);
        assert_eq!(
            judge(link(exe, "", "C:/Other"), &all),
            RecordStatus::OursBroken(vec![Defect::WrongValue { value: RecordValue::ShortcutWorkingDir, expected: "C:/Apps/MS".to_owned(), found: "C:/Other".to_owned() }])
        );
        assert_eq!(judge(link(exe, "", ""), &all), RecordStatus::OursStale(vec![Defect::ValueMissing { value: RecordValue::ShortcutWorkingDir }]));
        assert_eq!(
            judge(link(exe, "--ignore-installed", "C:/Apps/MS"), &all),
            RecordStatus::OursStale(vec![Defect::WrongValue { value: RecordValue::ShortcutArguments, expected: String::new(), found: "--ignore-installed".to_owned() }])
        );
        assert_eq!(judge(link(exe, "", "C:/Gone"), &all_exist_but(&["C:/Gone"])), RecordStatus::OursBroken(vec![Defect::WorkingDirMissing { path: "C:/Gone".to_owned() }]));
        assert_eq!(
            judge(link(exe, "", "C:/Apps/MS"), &all_exist_but(&["C:/Apps/MS/manhwastudio_rs.exe"])),
            RecordStatus::OursBroken(vec![Defect::TargetMissing { path: exe.to_owned() }])
        );
        assert_eq!(
            judge(link("D:/P/manhwastudio_rs.exe", "", "D:/P"), &all),
            RecordStatus::OtherCopy { exe: PathBuf::from("D:/P/manhwastudio_rs.exe"), alive: true, defects: Vec::new() }
        );
        assert_eq!(judge(link("", "", ""), &all), RecordStatus::Unreadable(ProbeError::NoOwner { location: lnk.display().to_string() }));
    }

    /// A repository build's link starts in the repository root; one still in the exe's own
    /// directory (written before the rule) is stale, another existing directory is broken. The
    /// same holds for another copy's repository-build link.
    #[test]
    fn start_menu_repo_build_working_dir() {
        let exe = "C:/src/ms/target/release/manhwastudio_rs.exe";
        let id = CopyIdentity { exe: PathBuf::from(exe), program_root: PathBuf::from("C:/src/ms"), version_core: None };
        let lnk = PathBuf::from("C:/Users/u/Programs/ManhwaStudio.lnk");
        let link = |target: &str, dir: &str| ObservedShortcut { target: PathBuf::from(target), arguments: String::new(), working_dir: PathBuf::from(dir) };
        let judge = |observed: ObservedShortcut| status_of(evaluate_start_menu(Scope::User, Some(&lnk), "Programs", &Ok(Some(observed)), &id, &all_exist_but(&[])));
        assert_eq!(judge(link(exe, "C:/src/ms")), RecordStatus::OursOk);
        assert_eq!(
            judge(link(exe, "C:/src/ms/target/release")),
            RecordStatus::OursStale(vec![Defect::WorkingDirOutdated { expected: "C:/src/ms".to_owned(), found: "C:/src/ms/target/release".to_owned() }])
        );
        assert_eq!(
            judge(link(exe, "C:/Other")),
            RecordStatus::OursBroken(vec![Defect::WrongValue { value: RecordValue::ShortcutWorkingDir, expected: "C:/src/ms".to_owned(), found: "C:/Other".to_owned() }])
        );
        let other = "D:/dev/ms/target/debug/manhwastudio_rs.exe";
        assert_eq!(
            judge(link(other, "D:/dev/ms/target/debug")),
            RecordStatus::OtherCopy {
                exe: PathBuf::from(other),
                alive: true,
                defects: vec![Defect::WorkingDirOutdated { expected: "D:/dev/ms".to_owned(), found: "D:/dev/ms/target/debug".to_owned() }]
            }
        );
        assert_eq!(judge(link(other, "D:/dev/ms")), RecordStatus::OtherCopy { exe: PathBuf::from(other), alive: true, defects: Vec::new() });
    }

    /// Ownership by ANY identifying value: an entry whose `UninstallString` names a gone copy
    /// but whose `InstallLocation` / `DisplayIcon` still name this copy is this copy's broken
    /// entry (Repair), not another copy's; the same for App Paths `Path`.
    #[test]
    fn any_identifying_value_naming_this_copy_makes_it_ours() {
        let id = identity();
        let gone = r"D:\Gone\manhwastudio_rs.exe";
        let exists = all_exist_but(&[r"D:\Gone\manhwastudio_rs.exe"]);
        let mut entry = written_uninstall(EXE, "3.2.1");
        entry.uninstall_string = Some(format!("\"{gone}\" --uninstall"));
        let status = status_of(evaluate_program_entry(Scope::User, &Ok(Some(entry)), &id, &exists));
        assert_eq!(
            status,
            RecordStatus::OursBroken(vec![Defect::WrongValue {
                value: RecordValue::UninstallString,
                expected: format!("\"{EXE}\" --uninstall"),
                found: format!("\"{gone}\" --uninstall"),
            }])
        );
        // Only InstallLocation names this copy.
        let mut location_only = written_uninstall(OTHER, "3.2.1");
        location_only.install_location = Some(INSTALL.to_owned());
        assert!(matches!(status_of(evaluate_program_entry(Scope::User, &Ok(Some(location_only)), &id, &exists)), RecordStatus::OursBroken(_)));

        let app_paths = ObservedAppPaths { exe: Some(gone.to_owned()), path: Some(INSTALL.to_owned()) };
        assert_eq!(
            status_of(evaluate_app_paths(Scope::User, &Ok(Some(app_paths)), &id, &exists)),
            RecordStatus::OursBroken(vec![Defect::WrongValue { value: RecordValue::AppPathsExe, expected: EXE.to_owned(), found: gone.to_owned() }])
        );
    }

    /// A record at our name that launches a program which is not a ManhwaStudio executable is a
    /// broken record at our name (`ForeignProgram`, badged), never "another copy".
    #[test]
    fn foreign_programs_are_broken_records_at_our_name() {
        let id = identity();
        let exists = all_exist_but(&[]);
        let notepad = r"C:\Windows\notepad.exe";
        let foreign = |value: RecordValue| RecordStatus::OursBroken(vec![Defect::ForeignProgram { value, expected: EXE.to_owned(), found: notepad.to_owned() }]);

        let lnk = PathBuf::from("C:/Users/u/Programs/ManhwaStudio.lnk");
        let link = ObservedShortcut { target: PathBuf::from(notepad), arguments: String::new(), working_dir: PathBuf::from(r"C:\Windows") };
        let status = status_of(evaluate_start_menu(Scope::User, Some(&lnk), "Programs", &Ok(Some(link)), &id, &exists));
        assert_eq!(status, foreign(RecordValue::ShortcutTarget));
        assert!(crate::report::badge_worthy(&status));

        let open_with = ObservedOpenWith { command: Some(format!("\"{notepad}\" \"%1\"")), ..written_open_with(EXE) };
        assert_eq!(status_of(evaluate_open_with(Scope::User, &Ok(Some(open_with)), false, &id, &exists)), foreign(RecordValue::OpenCommand));

        let entry = ObservedUninstall {
            uninstall_string: Some(format!("\"{notepad}\" --uninstall")),
            display_icon: Some(notepad.to_owned()),
            ..ObservedUninstall::default()
        };
        assert_eq!(status_of(evaluate_program_entry(Scope::User, &Ok(Some(entry)), &id, &exists)), foreign(RecordValue::UninstallString));

        let app_paths = ObservedAppPaths { exe: Some(notepad.to_owned()), path: None };
        assert_eq!(status_of(evaluate_app_paths(Scope::User, &Ok(Some(app_paths)), &id, &exists)), foreign(RecordValue::AppPathsExe));

        // A ManhwaStudio executable among the values decides "another copy" over a foreign one.
        let mixed = ObservedUninstall {
            uninstall_string: Some(format!("\"{notepad}\" --uninstall")),
            display_icon: Some(OTHER.to_owned()),
            ..ObservedUninstall::default()
        };
        let RecordStatus::OtherCopy { exe, .. } = status_of(evaluate_program_entry(Scope::User, &Ok(Some(mixed)), &id, &exists)) else {
            panic!("a ManhwaStudio executable names the owner");
        };
        assert_eq!(exe, PathBuf::from(OTHER));
    }

    /// The ManhwaStudio executable name test: file name only, ASCII case-insensitive.
    #[test]
    fn manhwastudio_exe_names() {
        assert!(is_manhwastudio_exe(r"C:\A\manhwastudio_rs.exe"));
        assert!(is_manhwastudio_exe("D:/b/MANHWASTUDIO_RS.EXE"));
        assert!(is_manhwastudio_exe("manhwastudio_rs.exe"));
        assert!(!is_manhwastudio_exe(r"C:\Windows\notepad.exe"));
        assert!(!is_manhwastudio_exe(r"C:\manhwastudio_rs.exe\other.exe"));
        assert!(!is_manhwastudio_exe(r"C:\A\manhwastudio_rs"));
    }

    /// Command and icon parsing helpers.
    #[test]
    fn command_helpers() {
        assert_eq!(split_quoted_command(r#" "C:\a b\x.exe" --uninstall "#), Some((r"C:\a b\x.exe", "--uninstall")));
        assert_eq!(split_quoted_command(r"C:\x.exe --uninstall"), None);
        assert_eq!(split_quoted_command(r#""C:\x.exe --uninstall"#), None);
        assert_eq!(check_command(r#""c:\X.EXE"   --uninstall"#, r#""C:\x.exe" --uninstall"#), CommandCheck::Matches);
        assert_eq!(check_command(r#""C:\y.exe" --uninstall"#, r#""C:\x.exe" --uninstall"#), CommandCheck::OtherExe);
        assert_eq!(check_command(r#""C:\x.exe""#, r#""C:\x.exe" --uninstall"#), CommandCheck::Malformed);
        assert_eq!(strip_icon_index(r"C:\x.exe,0"), r"C:\x.exe");
        assert_eq!(strip_icon_index(r"C:\x.exe, -3"), r"C:\x.exe");
        assert_eq!(strip_icon_index(r"C:\a,b\x.exe"), r"C:\a,b\x.exe");
        assert_eq!(windows_parent(r"C:\a\x.exe"), Some(r"C:\a"));
        assert_eq!(windows_parent("x.exe"), None);
        assert_eq!(registry_root(Scope::User), "HKCU");
        assert_eq!(registry_root(Scope::Machine), "HKLM");
    }
}
