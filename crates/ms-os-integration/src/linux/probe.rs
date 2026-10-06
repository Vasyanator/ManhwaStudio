/*
File: crates/ms-os-integration/src/linux/probe.rs

Purpose:
The Linux half of the registration probe: reads `manhwastudio_rs.desktop` in `$XDG_DATA_HOME`
(the `User` rows) and in every `$XDG_DATA_DIRS` entry (read-only `Machine` rows) and judges each
copy twice — as the application-menu item (`RecordKind::StartMenu`) and as the "Open with"
registration (`RecordKind::OpenWith`: its `MimeType=` coverage and a file-accepting `Exec=`).

Key items:
- `ObservedDesktopEntry`: what the reader found at one path.
- `evaluate_desktop_entry()`: the pure judge over `EntryOracles` (existence, "is this
  executable ours", the `$PATH` lookup of a bare program name).
- `probe_records()`, `probe_records_in()` (Linux only): reader + judge over `DesktopDirs`.

Notes:
Ownership is `desktop_entry::resolved_entry_owner` (the `X-ManhwaStudio-Exe=` key, else the
legacy `Exec=` argument, a bare name resolved through `$PATH`) and this copy is recognised by
`xdg::same_executable`, the rules the startup writer uses, so the probe and the writer always
agree on whose entry it is. The expected MIME
types are `desktop_entry::desktop_mime_types`, the list the writer emits. A system-wide copy that
does not exist yields no row (it is not actionable); one hidden by a higher-precedence copy of
the same desktop id is marked `shadowed`.
*/

use std::path::{Path, PathBuf};

use super::desktop_entry::{EntryOwner, ICON_FILE_NAME, desktop_mime_types, exec_first_argument, main_group_entries, resolved_entry_owner, unescape_desktop_string_value};
use crate::copy_identity::{CopyIdentity, repo_build_root};
use crate::report::{Defect, ProbeError, RecordKind, RecordReport, RecordStatus, RecordValue, Scope, classify};

/// One desktop-entry path as the reader found it: `Ok(None)` = no file, `Ok(Some(text))` = its
/// content (lossy UTF-8), `Err` = it could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedDesktopEntry {
    pub path: PathBuf,
    pub content: Result<Option<String>, ProbeError>,
}

/// The `Icon=` name the writer puts in the entry (`manhwastudio_rs`, the icon file's stem).
fn icon_name() -> &'static str {
    ICON_FILE_NAME.strip_suffix(".png").unwrap_or(ICON_FILE_NAME)
}

/// Whether an `Exec=` value passes a file or URL to the program (a `%f` / `%F` / `%u` / `%U`
/// field code), which "Open with" needs.
fn exec_takes_a_file(exec: &str) -> bool {
    ["%f", "%F", "%u", "%U"].iter().any(|code| exec.contains(code))
}

/// The filesystem and environment oracles of [`evaluate_desktop_entry`] (production: the
/// process filesystem, `xdg::same_executable` and `xdg::resolve_program_in_process_path`).
pub struct EntryOracles<'a> {
    /// Whether a path exists.
    pub exists: &'a dyn Fn(&Path) -> bool,
    /// Whether a (resolved) path names this copy's executable.
    pub is_ours: &'a dyn Fn(&Path) -> bool,
    /// The file a program named in the entry runs: a path unchanged, a bare name through
    /// `$PATH` (`desktop_entry::resolve_program`); `None` when a bare name is found nowhere.
    pub resolve: &'a dyn Fn(&Path) -> Option<PathBuf>,
}

/// Judges one desktop entry as `[StartMenu row, OpenWith row]` for `scope`.
///
/// Owner = `resolved_entry_owner` (key, else legacy `Exec`, a bare name resolved through
/// `oracles.resolve`); none, or a bare name not on `$PATH` -> both rows `Unreadable(NoOwner)`
/// (never badged). `Exec=` / `TryExec=` programs are resolved the same way before they are
/// compared with the owner. `icon_candidates` are the places the `manhwastudio_rs` icon may live. Both rows carry
/// the launch defects (gone executable, `Exec=` / `TryExec=` / `Path=` pointing elsewhere, a
/// missing ownership key on a legacy entry); the menu row adds the icon, the "Open with" row
/// adds the MIME coverage and a file-accepting `Exec=`. `Path=` is compared with
/// `identity.program_root` for this copy's entry and with the repository root for another
/// copy that is a repository build (`copy_identity::repo_build_root`); any other copy's root is
/// unknown, only its existence is checked. A `Path=` still at the owner's own directory where
/// the root moved off it is `WorkingDirOutdated` (stale), any other mismatch `WrongValue`.
/// `shadowed` is kept on `Machine` rows only.
#[must_use]
pub fn evaluate_desktop_entry(
    scope: Scope,
    observed: &ObservedDesktopEntry,
    shadowed: bool,
    identity: &CopyIdentity,
    icon_candidates: &[PathBuf],
    oracles: &EntryOracles<'_>,
) -> [RecordReport; 2] {
    let EntryOracles { exists, is_ours, resolve } = *oracles;
    let location = observed.path.display().to_string();
    let shadowed = shadowed && scope == Scope::Machine;
    let rows = |menu: RecordStatus, open_with: RecordStatus| {
        [
            RecordReport { kind: RecordKind::StartMenu, scope, location: location.clone(), status: menu, shadowed },
            RecordReport { kind: RecordKind::OpenWith, scope, location: location.clone(), status: open_with, shadowed },
        ]
    };
    let text = match &observed.content {
        Ok(None) => return rows(RecordStatus::Missing, RecordStatus::Missing),
        Err(error) => return rows(RecordStatus::Unreadable(error.clone()), RecordStatus::Unreadable(error.clone())),
        Ok(Some(text)) => text,
    };
    let (owner, keyed) = match resolved_entry_owner(text, resolve) {
        EntryOwner::Key(exe) => (exe, true),
        EntryOwner::LegacyExec(exe) => (exe, false),
        EntryOwner::Unknown => {
            let error = ProbeError::NoOwner { location: location.clone() };
            return rows(RecordStatus::Unreadable(error.clone()), RecordStatus::Unreadable(error));
        }
    };
    let ours = is_ours(&owner);
    let alive = exists(&owner);
    // The owner's own executable, after the same `$PATH` resolution as the owner: for this copy
    // by the writer's rule, for another copy lexically. A bare name found nowhere names nobody.
    let names_owner = |path: &Path| resolve(path).is_some_and(|resolved| if ours { is_ours(&resolved) } else { resolved == owner });
    let entries = main_group_entries(text);
    let value_of = |key: &str| entries.iter().find(|(found, _)| *found == key).map(|(_, value)| *value);

    let mut launch = Vec::new();
    if !alive {
        launch.push(Defect::TargetMissing { path: owner.display().to_string() });
    }
    if !keyed {
        launch.push(Defect::ValueMissing { value: RecordValue::DesktopOwnerKey });
    }
    let exec = value_of("Exec");
    let mut exec_parsed = false;
    match exec {
        None => launch.push(Defect::ValueMissing { value: RecordValue::DesktopExec }),
        Some(exec) => match exec_first_argument(exec) {
            None => launch.push(Defect::MalformedCommand { command: exec.to_owned() }),
            Some(argv0) => {
                exec_parsed = true;
                if !names_owner(Path::new(&argv0)) {
                    launch.push(Defect::WrongValue { value: RecordValue::DesktopExec, expected: owner.display().to_string(), found: argv0 });
                }
            }
        },
    }
    match value_of("TryExec").map(unescape_desktop_string_value) {
        None => launch.push(Defect::ValueMissing { value: RecordValue::DesktopTryExec }),
        Some(try_exec) if !names_owner(Path::new(&try_exec)) => {
            launch.push(Defect::WrongValue { value: RecordValue::DesktopTryExec, expected: owner.display().to_string(), found: try_exec });
        }
        Some(_) => {}
    }
    match value_of("Path").map(unescape_desktop_string_value) {
        None => launch.push(Defect::ValueMissing { value: RecordValue::DesktopPath }),
        Some(dir) if !exists(Path::new(&dir)) => launch.push(Defect::WorkingDirMissing { path: dir }),
        Some(dir) => {
            // This copy's root is the identity's; another copy's is known only when it is a
            // repository build (its repository root), otherwise only its existence is judged.
            let expected = if ours { Some(identity.program_root.clone()) } else { repo_build_root(&owner) };
            if let Some(expected) = expected.filter(|expected| Path::new(&dir) != expected) {
                let expected = expected.display().to_string();
                if owner.parent().is_some_and(|exe_dir| Path::new(&dir) == exe_dir) {
                    launch.push(Defect::WorkingDirOutdated { expected, found: dir });
                } else {
                    launch.push(Defect::WrongValue { value: RecordValue::DesktopPath, expected, found: dir });
                }
            }
        }
    }

    let mut menu = launch.clone();
    match value_of("Icon").map(unescape_desktop_string_value) {
        None => menu.push(Defect::ValueMissing { value: RecordValue::DesktopIcon }),
        Some(icon) if Path::new(&icon).is_absolute() => {
            if !exists(Path::new(&icon)) {
                menu.push(Defect::IconMissing);
            }
        }
        Some(icon) if icon == icon_name() => {
            if !icon_candidates.iter().any(|candidate| exists(candidate)) {
                menu.push(Defect::IconMissing);
            }
        }
        Some(icon) => menu.push(Defect::WrongValue { value: RecordValue::DesktopIcon, expected: icon_name().to_owned(), found: icon }),
    }

    let mut open_with = launch;
    if let Some(exec) = exec
        && exec_parsed
        && !exec_takes_a_file(exec)
    {
        open_with.push(Defect::MalformedCommand { command: exec.to_owned() });
    }
    let listed: Vec<String> = value_of("MimeType")
        .map(|list| list.split(';').map(str::trim).filter(|mime| !mime.is_empty()).map(str::to_owned).collect())
        .unwrap_or_default();
    let missing: Vec<String> = desktop_mime_types().filter(|mime| !listed.iter().any(|found| found == mime)).map(str::to_owned).collect();
    if !missing.is_empty() {
        open_with.push(Defect::MissingImageTypes { types: missing });
    }

    rows(classify(owner.clone(), ours, alive, menu), classify(owner, ours, alive, open_with))
}

/// Reads and judges every desktop entry of `identity` under the process's XDG directories.
/// Without a per-user data directory the two `User` rows are `Unreadable(NoDataHome)`. Blocking.
#[cfg(target_os = "linux")]
#[must_use]
pub fn probe_records(identity: &CopyIdentity) -> Vec<RecordReport> {
    match super::xdg::DesktopDirs::from_process_env() {
        Some(dirs) => probe_records_in(identity, &dirs),
        None => {
            let location = format!("$XDG_DATA_HOME/applications/{}", super::desktop_entry::DESKTOP_ENTRY_FILE_NAME);
            [RecordKind::StartMenu, RecordKind::OpenWith]
                .into_iter()
                .map(|kind| RecordReport { kind, scope: Scope::User, location: location.clone(), status: RecordStatus::Unreadable(ProbeError::NoDataHome), shadowed: false })
                .collect()
        }
    }
}

/// [`probe_records`] over explicit directories: the `User` pair from `dirs.data_home` (always),
/// then a `Machine` pair per distinct `dirs.data_dirs` entry whose file exists or cannot be read,
/// in precedence order, `shadowed` once a higher-precedence copy exists. Blocking.
#[cfg(target_os = "linux")]
#[must_use]
pub fn probe_records_in(identity: &CopyIdentity, dirs: &super::xdg::DesktopDirs) -> Vec<RecordReport> {
    use super::xdg::{entry_path_under, icon_path_under, resolve_program_in_process_path, same_executable};

    let exists = |path: &Path| crate::report::path_exists(path);
    let is_ours = |path: &Path| same_executable(path, &identity.exe);
    let oracles = EntryOracles { exists: &exists, is_ours: &is_ours, resolve: &resolve_program_in_process_path };
    let mut bases: Vec<&Path> = vec![dirs.data_home.as_path()];
    for dir in &dirs.data_dirs {
        if !bases.contains(&dir.as_path()) {
            bases.push(dir);
        }
    }
    let icon_candidates: Vec<PathBuf> = bases.iter().map(|base| icon_path_under(base)).collect();

    let mut records = Vec::new();
    let mut higher_copy_exists = false;
    for (index, base) in bases.iter().enumerate() {
        let observed = read_entry(entry_path_under(base));
        let scope = if index == 0 { Scope::User } else { Scope::Machine };
        let present = !matches!(observed.content, Ok(None));
        if scope == Scope::User || present {
            records.extend(evaluate_desktop_entry(scope, &observed, higher_copy_exists, identity, &icon_candidates, &oracles));
        }
        higher_copy_exists |= present;
    }
    records
}

/// Reads the entry at `path`: a missing file is `Ok(None)`, any other failure `Err`.
#[cfg(target_os = "linux")]
fn read_entry(path: PathBuf) -> ObservedDesktopEntry {
    let content = match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ProbeError::io(path.clone(), &error)),
    };
    ObservedDesktopEntry { path, content }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::desktop_entry::{OWNER_EXE_KEY, linux_desktop_entry_text};

    const EXE: &str = "/opt/ms/manhwastudio_rs";
    const OTHER: &str = "/home/u/other/manhwastudio_rs";

    fn identity() -> CopyIdentity {
        CopyIdentity { exe: PathBuf::from(EXE), program_root: PathBuf::from("/opt/ms"), version_core: Some("3.2.1".to_owned()) }
    }

    fn other_identity() -> CopyIdentity {
        CopyIdentity { exe: PathBuf::from(OTHER), program_root: PathBuf::from("/home/u/other"), version_core: None }
    }

    fn icons() -> Vec<PathBuf> {
        vec![PathBuf::from("/home/u/.local/share/icons/hicolor/512x512/apps/manhwastudio_rs.png")]
    }

    fn observed(text: &str) -> ObservedDesktopEntry {
        ObservedDesktopEntry { path: PathBuf::from("/home/u/.local/share/applications/manhwastudio_rs.desktop"), content: Ok(Some(text.to_owned())) }
    }

    /// Judges `text` with every path existing except `missing`.
    fn judge(text: &str, missing: &'static [&'static str]) -> [RecordStatus; 2] {
        let exists = move |path: &Path| !missing.iter().any(|gone| path == Path::new(gone));
        let is_ours = |path: &Path| path == Path::new(EXE);
        let oracles = EntryOracles { exists: &exists, is_ours: &is_ours, resolve: &as_is };
        let [menu, open_with] = evaluate_desktop_entry(Scope::User, &observed(text), false, &identity(), &icons(), &oracles);
        assert_eq!((menu.kind, open_with.kind), (RecordKind::StartMenu, RecordKind::OpenWith));
        [menu.status, open_with.status]
    }

    fn both(status: RecordStatus) -> [RecordStatus; 2] {
        [status.clone(), status]
    }

    /// The resolver of tests whose entries name only absolute paths.
    fn as_is(path: &Path) -> Option<PathBuf> {
        Some(path.to_path_buf())
    }

    /// The entry the writer emits is `OursOk` on both rows.
    #[test]
    fn written_entry_is_ours_ok() {
        assert_eq!(judge(&linux_desktop_entry_text(&identity()), &[]), both(RecordStatus::OursOk));
    }

    #[test]
    fn missing_unreadable_and_ownerless_entries() {
        let exists = |_: &Path| true;
        let is_ours = |_: &Path| true;
        let oracles = EntryOracles { exists: &exists, is_ours: &is_ours, resolve: &as_is };
        let absent = ObservedDesktopEntry { path: PathBuf::from("/x.desktop"), content: Ok(None) };
        let [menu, open_with] = evaluate_desktop_entry(Scope::User, &absent, false, &identity(), &icons(), &oracles);
        assert_eq!((menu.status, open_with.status), (RecordStatus::Missing, RecordStatus::Missing));
        let error = ProbeError::Io { path: PathBuf::from("/x.desktop"), kind: std::io::ErrorKind::PermissionDenied, message: "denied".to_owned() };
        let denied = ObservedDesktopEntry { path: PathBuf::from("/x.desktop"), content: Err(error.clone()) };
        let [menu, open_with] = evaluate_desktop_entry(Scope::User, &denied, false, &identity(), &icons(), &oracles);
        assert_eq!([menu.status, open_with.status], both(RecordStatus::Unreadable(error)));
        let location = "/home/u/.local/share/applications/manhwastudio_rs.desktop".to_owned();
        assert_eq!(judge("[Desktop Entry]\nName=Hand made\n", &[]), both(RecordStatus::Unreadable(ProbeError::NoOwner { location })));
    }

    /// A legacy entry (no key, no `TryExec`, no `Path`) whose `Exec` is ours: stale, never broken.
    #[test]
    fn legacy_entry_without_key_is_ours_and_stale() {
        let legacy = linux_desktop_entry_text(&identity())
            .lines()
            .filter(|line| !line.starts_with(OWNER_EXE_KEY) && !line.starts_with("TryExec=") && !line.starts_with("Path="))
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        let stale = vec![
            Defect::ValueMissing { value: RecordValue::DesktopOwnerKey },
            Defect::ValueMissing { value: RecordValue::DesktopTryExec },
            Defect::ValueMissing { value: RecordValue::DesktopPath },
        ];
        assert_eq!(judge(&legacy, &[]), both(RecordStatus::OursStale(stale)));
    }

    /// A repository build's entry starts in the repository root. `Path=` still at the exe's own
    /// directory (the value before the rule) is stale on both rows, any other existing directory
    /// broken; another copy that is a repository build is judged against its repository root.
    #[test]
    fn repo_build_entry_path() {
        const REPO_EXE: &str = "/home/u/src/ms/target/release/manhwastudio_rs";
        const OTHER_REPO_EXE: &str = "/home/u/dev/ms/target/debug/manhwastudio_rs";
        let ours = CopyIdentity { exe: PathBuf::from(REPO_EXE), program_root: PathBuf::from("/home/u/src/ms"), version_core: None };
        let judge_repo = |text: &str| {
            let exists = |_: &Path| true;
            let is_ours = |path: &Path| path == Path::new(REPO_EXE);
            let oracles = EntryOracles { exists: &exists, is_ours: &is_ours, resolve: &as_is };
            let [menu, open_with] = evaluate_desktop_entry(Scope::User, &observed(text), false, &ours, &icons(), &oracles);
            [menu.status, open_with.status]
        };
        let written = linux_desktop_entry_text(&ours);
        assert!(written.contains("Path=/home/u/src/ms\n"), "{written}");
        assert_eq!(judge_repo(&written), both(RecordStatus::OursOk));
        let old_dir = written.replace("Path=/home/u/src/ms\n", "Path=/home/u/src/ms/target/release\n");
        let outdated = Defect::WorkingDirOutdated { expected: "/home/u/src/ms".to_owned(), found: "/home/u/src/ms/target/release".to_owned() };
        assert_eq!(judge_repo(&old_dir), both(RecordStatus::OursStale(vec![outdated])));
        let elsewhere = written.replace("Path=/home/u/src/ms\n", "Path=/srv/ms\n");
        let wrong = Defect::WrongValue { value: RecordValue::DesktopPath, expected: "/home/u/src/ms".to_owned(), found: "/srv/ms".to_owned() };
        assert_eq!(judge_repo(&elsewhere), both(RecordStatus::OursBroken(vec![wrong])));

        let other = CopyIdentity { exe: PathBuf::from(OTHER_REPO_EXE), program_root: PathBuf::from("/home/u/dev/ms/target/debug"), version_core: None };
        let other_old = linux_desktop_entry_text(&other);
        let outdated = Defect::WorkingDirOutdated { expected: "/home/u/dev/ms".to_owned(), found: "/home/u/dev/ms/target/debug".to_owned() };
        let [menu, _] = judge_repo(&other_old);
        assert_eq!(menu, RecordStatus::OtherCopy { exe: PathBuf::from(OTHER_REPO_EXE), alive: true, defects: vec![outdated] });
    }

    /// Broken own entries: gone executable, `Path=` elsewhere or gone, `TryExec=` elsewhere,
    /// `Exec=` elsewhere under our key.
    #[test]
    fn broken_own_entries() {
        let written = linux_desktop_entry_text(&identity());
        assert_eq!(judge(&written, &[EXE]), both(RecordStatus::OursBroken(vec![Defect::TargetMissing { path: EXE.to_owned() }])));
        assert_eq!(judge(&written, &["/opt/ms"]), both(RecordStatus::OursBroken(vec![Defect::WorkingDirMissing { path: "/opt/ms".to_owned() }])));
        let moved_root = written.replace("Path=/opt/ms\n", "Path=/srv/ms\n");
        assert_eq!(
            judge(&moved_root, &[]),
            both(RecordStatus::OursBroken(vec![Defect::WrongValue { value: RecordValue::DesktopPath, expected: "/opt/ms".to_owned(), found: "/srv/ms".to_owned() }]))
        );
        let try_elsewhere = written.replace(&format!("TryExec={EXE}\n"), "TryExec=/gone/ms\n");
        assert_eq!(
            judge(&try_elsewhere, &[]),
            both(RecordStatus::OursBroken(vec![Defect::WrongValue { value: RecordValue::DesktopTryExec, expected: EXE.to_owned(), found: "/gone/ms".to_owned() }]))
        );
        let exec_elsewhere = written.replace(&format!("Exec=\"{EXE}\" %f"), "Exec=\"/x/ms\" %f");
        assert_eq!(
            judge(&exec_elsewhere, &[]),
            both(RecordStatus::OursBroken(vec![Defect::WrongValue { value: RecordValue::DesktopExec, expected: EXE.to_owned(), found: "/x/ms".to_owned() }]))
        );
        let unterminated = written.replace(&format!("Exec=\"{EXE}\" %f"), "Exec=\"/x/ms %f");
        assert_eq!(judge(&unterminated, &[]), both(RecordStatus::OursBroken(vec![Defect::MalformedCommand { command: "\"/x/ms %f".to_owned() }])));
        let no_exec = written.replace(&format!("Exec=\"{EXE}\" %f\n"), "");
        assert_eq!(judge(&no_exec, &[]), both(RecordStatus::OursBroken(vec![Defect::ValueMissing { value: RecordValue::DesktopExec }])));
    }

    /// Row-specific defects: icon (menu only), MIME coverage and a file-less `Exec` ("Open with"
    /// only).
    #[test]
    fn row_specific_defects() {
        let written = linux_desktop_entry_text(&identity());
        let [menu, open_with] = judge(&written, &["/home/u/.local/share/icons/hicolor/512x512/apps/manhwastudio_rs.png"]);
        assert_eq!((menu, open_with), (RecordStatus::OursStale(vec![Defect::IconMissing]), RecordStatus::OursOk));
        let other_icon = written.replace("Icon=manhwastudio_rs\n", "Icon=foo\n");
        let [menu, _] = judge(&other_icon, &[]);
        assert_eq!(menu, RecordStatus::OursStale(vec![Defect::WrongValue { value: RecordValue::DesktopIcon, expected: "manhwastudio_rs".to_owned(), found: "foo".to_owned() }]));
        let absolute_icon = written.replace("Icon=manhwastudio_rs\n", "Icon=/gone/icon.png\n");
        assert_eq!(judge(&absolute_icon, &["/gone/icon.png"])[0], RecordStatus::OursStale(vec![Defect::IconMissing]));

        let fewer_types = written.replace("image/webp;", "").replace("image/qoi;", "");
        assert_eq!(
            judge(&fewer_types, &[]),
            [RecordStatus::OursOk, RecordStatus::OursStale(vec![Defect::MissingImageTypes { types: vec!["image/webp".to_owned(), "image/qoi".to_owned()] }])]
        );
        let all_types: Vec<String> = desktop_mime_types().map(str::to_owned).collect();
        let no_types = written.lines().filter(|line| !line.starts_with("MimeType=")).map(|line| format!("{line}\n")).collect::<String>();
        assert_eq!(judge(&no_types, &[])[1], RecordStatus::OursStale(vec![Defect::MissingImageTypes { types: all_types }]));
        let no_file = written.replace(&format!("Exec=\"{EXE}\" %f"), &format!("Exec=\"{EXE}\""));
        assert_eq!(judge(&no_file, &[]), [RecordStatus::OursOk, RecordStatus::OursBroken(vec![Defect::MalformedCommand { command: format!("\"{EXE}\"") }])]);
    }

    /// Another copy's entry: alive (no defects; `Path=` not compared with our root), dead,
    /// alive but broken; by key even when `Exec` would name us.
    #[test]
    fn entries_of_another_copy() {
        let other = linux_desktop_entry_text(&other_identity());
        let alive = RecordStatus::OtherCopy { exe: PathBuf::from(OTHER), alive: true, defects: Vec::new() };
        assert_eq!(judge(&other, &[]), both(alive));
        let dead = RecordStatus::OtherCopy { exe: PathBuf::from(OTHER), alive: false, defects: vec![Defect::TargetMissing { path: OTHER.to_owned() }] };
        assert_eq!(judge(&other, &[OTHER]), both(dead));
        let broken = RecordStatus::OtherCopy {
            exe: PathBuf::from(OTHER),
            alive: true,
            defects: vec![Defect::WorkingDirMissing { path: "/home/u/other".to_owned() }],
        };
        assert_eq!(judge(&other, &["/home/u/other"]), both(broken));
        let keyed_elsewhere = linux_desktop_entry_text(&identity()).replace(&format!("{OWNER_EXE_KEY}={EXE}"), &format!("{OWNER_EXE_KEY}={OTHER}"));
        let [menu, _] = judge(&keyed_elsewhere, &[]);
        let RecordStatus::OtherCopy { exe, defects, .. } = menu else {
            panic!("the key decides ownership");
        };
        assert_eq!(exe, PathBuf::from(OTHER));
        assert!(defects.contains(&Defect::WrongValue { value: RecordValue::DesktopExec, expected: OTHER.to_owned(), found: EXE.to_owned() }));
    }

    /// A hand-written legacy entry with a bare `Exec=` / `TryExec=` name (PATH lookup) is judged
    /// by the program `$PATH` finds: another alive copy (no "missing program" defect, no badge),
    /// this copy, or — found nowhere — `Unreadable(NoOwner)`, which never badges.
    // Unix `$PATH` syntax (`:`) and absolute paths: a Windows host test build parses neither.
    #[cfg(unix)]
    #[test]
    fn bare_program_names_resolve_through_path() {
        use std::ffi::OsStr;

        use crate::linux::desktop_entry::resolve_program;
        use crate::report::badge_worthy;

        let text = "[Desktop Entry]\nType=Application\nName=ManhwaStudio\nTryExec=manhwastudio_rs\nExec=manhwastudio_rs %f\nPath=/usr/share/ms\nIcon=manhwastudio_rs\nMimeType=image/png;\n";
        let exists = |_: &Path| true;
        let is_ours = |path: &Path| path == Path::new(EXE);
        let judge_on = |found: &'static str| {
            let resolve = move |path: &Path| resolve_program(path, Some(OsStr::new("/usr/local/bin:/usr/bin:/opt/ms")), &|candidate| candidate == Path::new(found));
            let oracles = EntryOracles { exists: &exists, is_ours: &is_ours, resolve: &resolve };
            let [menu, open_with] = evaluate_desktop_entry(Scope::User, &observed(text), false, &identity(), &icons(), &oracles);
            (menu.status, open_with.status)
        };

        let (menu, _) = judge_on("/usr/bin/manhwastudio_rs");
        let RecordStatus::OtherCopy { exe, alive, defects } = &menu else {
            panic!("a bare name on $PATH is another copy's entry: {menu:?}");
        };
        assert_eq!((exe.as_path(), *alive), (Path::new("/usr/bin/manhwastudio_rs"), true));
        assert!(!defects.iter().any(|defect| matches!(defect, Defect::TargetMissing { .. } | Defect::WrongValue { .. })), "{defects:?}");
        assert!(!badge_worthy(&menu));

        let (menu, _) = judge_on(EXE);
        assert!(matches!(menu, RecordStatus::OursStale(_) | RecordStatus::OursBroken(_)), "resolved to this copy: {menu:?}");

        let location = "/home/u/.local/share/applications/manhwastudio_rs.desktop".to_owned();
        let (menu, open_with) = judge_on("/nowhere/manhwastudio_rs");
        assert_eq!([menu.clone(), open_with], both(RecordStatus::Unreadable(ProbeError::NoOwner { location })));
        assert!(!badge_worthy(&menu));
    }

    /// `shadowed` survives only on Machine rows.
    #[test]
    fn shadowed_only_on_machine_rows() {
        let exists = |_: &Path| true;
        let is_ours = |path: &Path| path == Path::new(EXE);
        let oracles = EntryOracles { exists: &exists, is_ours: &is_ours, resolve: &as_is };
        let entry = observed(&linux_desktop_entry_text(&identity()));
        let machine = evaluate_desktop_entry(Scope::Machine, &entry, true, &identity(), &icons(), &oracles);
        assert!(machine.iter().all(|row| row.shadowed && row.scope == Scope::Machine));
        let user = evaluate_desktop_entry(Scope::User, &entry, true, &identity(), &icons(), &oracles);
        assert!(user.iter().all(|row| !row.shadowed));
    }

    /// Reader + judge over real temporary XDG directories (never the real HOME).
    #[cfg(target_os = "linux")]
    mod on_disk {
        use std::fs;

        use super::*;
        use crate::linux::xdg::{DesktopDirs, entry_path_under, icon_path_under};

        /// A uniquely named scratch directory under the system temp dir, removed on drop.
        struct Scratch(PathBuf);

        impl Scratch {
            fn new(tag: &str) -> Self {
                let path = std::env::temp_dir().join(format!("ms-os-integration-probe-{tag}-{}", std::process::id()));
                if path.exists() {
                    fs::remove_dir_all(&path).expect("stale scratch dir must be removable");
                }
                fs::create_dir_all(&path).expect("scratch dir must be creatable");
                Self(path)
            }

            /// A real executable file (and its directory as program root) under the scratch dir.
            fn copy(&self, name: &str) -> CopyIdentity {
                let root = self.0.join(name);
                fs::create_dir_all(&root).expect("copy dir");
                let exe = root.join("manhwastudio_rs");
                fs::write(&exe, b"").expect("fake exe");
                CopyIdentity { exe, program_root: root, version_core: None }
            }

            fn write(&self, path: &Path, bytes: &[u8]) {
                fs::create_dir_all(path.parent().expect("parent")).expect("parent dir");
                fs::write(path, bytes).expect("seed file");
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                if let Err(err) = fs::remove_dir_all(&self.0) {
                    eprintln!("could not remove scratch dir {}: {err}", self.0.display());
                }
            }
        }

        #[test]
        fn probes_user_and_system_copies() {
            let scratch = Scratch::new("dirs");
            let ours = scratch.copy("ours");
            let other = scratch.copy("other");
            let dirs = DesktopDirs {
                data_home: scratch.0.join("home"),
                data_dirs: vec![scratch.0.join("local"), scratch.0.join("empty"), scratch.0.join("usr"), scratch.0.join("local")],
            };

            // Nothing anywhere: only the two Missing User rows.
            let records = probe_records_in(&ours, &dirs);
            assert_eq!(records.len(), 2);
            assert!(records.iter().all(|row| row.scope == Scope::User && row.status == RecordStatus::Missing));

            // Ours in the user dir with its icon, another copy in `local`, ours (dead copy text)
            // in `usr`; `empty` holds nothing and `local` is listed twice.
            scratch.write(&entry_path_under(&dirs.data_home), linux_desktop_entry_text(&ours).as_bytes());
            scratch.write(&icon_path_under(&dirs.data_home), crate::identity::APP_ICON_PNG);
            scratch.write(&entry_path_under(&dirs.data_dirs[0]), linux_desktop_entry_text(&other).as_bytes());
            let gone = CopyIdentity { exe: scratch.0.join("gone/manhwastudio_rs"), program_root: scratch.0.join("gone"), version_core: None };
            scratch.write(&entry_path_under(&dirs.data_dirs[2]), linux_desktop_entry_text(&gone).as_bytes());

            let records = probe_records_in(&ours, &dirs);
            let summary: Vec<(RecordKind, Scope, bool)> = records.iter().map(|row| (row.kind, row.scope, row.shadowed)).collect();
            assert_eq!(
                summary,
                vec![
                    (RecordKind::StartMenu, Scope::User, false),
                    (RecordKind::OpenWith, Scope::User, false),
                    (RecordKind::StartMenu, Scope::Machine, true),
                    (RecordKind::OpenWith, Scope::Machine, true),
                    (RecordKind::StartMenu, Scope::Machine, true),
                    (RecordKind::OpenWith, Scope::Machine, true),
                ]
            );
            assert_eq!(records[0].status, RecordStatus::OursOk);
            assert_eq!(records[1].status, RecordStatus::OursOk);
            assert_eq!(records[0].location, entry_path_under(&dirs.data_home).display().to_string());
            assert_eq!(records[2].status, RecordStatus::OtherCopy { exe: other.exe.clone(), alive: true, defects: Vec::new() });
            let RecordStatus::OtherCopy { alive, .. } = &records[4].status else {
                panic!("the dead copy's entry is another copy's");
            };
            assert!(!alive);

            // An unreadable system copy (a directory in place of the file) is a row too.
            fs::create_dir_all(entry_path_under(&dirs.data_dirs[1])).expect("dir in place of the entry");
            let records = probe_records_in(&ours, &dirs);
            assert_eq!(records.len(), 8);
            assert!(matches!(records[4].status, RecordStatus::Unreadable(ProbeError::Io { .. })));
        }
    }
}
