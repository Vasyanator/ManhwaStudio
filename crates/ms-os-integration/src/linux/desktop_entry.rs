/*
File: crates/ms-os-integration/src/linux/desktop_entry.rs

Purpose:
The exact text of the Linux desktop entry a program copy writes for itself, the reading of an
existing entry's owner, and the startup decision over both. Pure: no filesystem, environment or
process access (ownership is judged through a caller-supplied oracle), so the text is pinned by
a byte-exact golden and every decision is table-tested.

Key items:
- `linux_desktop_entry_text()`: the whole `.desktop` file for one `CopyIdentity`.
- `entry_owner()`: whose entry an existing file is (`X-ManhwaStudio-Exe=`, else legacy `Exec`).
- `resolve_program()`: the `$PATH` lookup of a bare program name (Desktop Entry spec), and
  `resolved_entry_owner()`: the owner resolved through it — the one ownership rule of the
  startup writer and the probe.
- `decide_startup()` -> `StartupOutcome`: create / refresh / keep / leave foreign / unreadable.
- `main_group_entries()`: the `[Desktop Entry]` group parser shared with the probe.
- `desktop_mime_types()`, `desktop_mime_type_list()`, `escape_desktop_exec_arg()`,
  `escape_desktop_string_value()`.

Notes:
The readable input types come from `ms_config::single_image::INPUT_FILE_TYPES`, the one table
the single-image open path also reads, so "Open with" never offers a type the program cannot
load. The ownership key is the executable (`X-ManhwaStudio-Exe`), the same identity the Windows
records are judged by; `Path=` carries the program root.
*/

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::copy_identity::CopyIdentity;

/// File name of the desktop entry inside an `applications/` directory; also the desktop id
/// that `StartupWMClass` and the icon name match.
pub const DESKTOP_ENTRY_FILE_NAME: &str = "manhwastudio_rs.desktop";

/// File name of the icon inside `icons/hicolor/512x512/apps/` (the entry's `Icon=` name + `.png`).
pub const ICON_FILE_NAME: &str = "manhwastudio_rs.png";

/// Vendor key naming the executable the entry was written for: the entry's ownership key.
pub const OWNER_EXE_KEY: &str = crate::identity::LINUX_OWNER_EXE_KEY;

/// What startup does (or did) with the desktop entry of the running copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupOutcome {
    /// No entry existed; one was written for this copy.
    Created,
    /// The entry was this copy's (by key, or a legacy entry whose `Exec` runs this executable)
    /// but its text differed; it was rewritten.
    Refreshed,
    /// The entry is this copy's and already byte-identical; nothing was written.
    AlreadyCurrent,
    /// The entry belongs to another copy (`exe`); it is left untouched, even when `exe` no
    /// longer exists.
    LeftForeign { exe: PathBuf },
    /// The entry exists but could not be read, or names no owner; it is left untouched.
    Unreadable,
}

/// Whose entry an existing desktop file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryOwner {
    /// The `X-ManhwaStudio-Exe=` value.
    Key(PathBuf),
    /// No ownership key: the first `Exec=` argument (an entry written before the key existed).
    LegacyExec(PathBuf),
    /// Neither key nor a parsable `Exec=` in the `[Desktop Entry]` group.
    Unknown,
}

/// Text of the Linux desktop entry for `identity`. Pure.
///
/// `Exec=` ends in `%f` (one local file, or nothing when launched from a menu), which arrives
/// as the positional image argument. `TryExec=` hides the entry once the executable is gone;
/// `Path=` starts a menu launch in the program root so the runtime-root resolution lands on this
/// copy; `X-ManhwaStudio-Exe=` is the ownership key. `MimeType=` lists every readable input type
/// (`ms_config::single_image::INPUT_FILE_TYPES`), so the program is OFFERED in "Open with";
/// it never becomes the default handler (that would need a `mimeapps.list` entry).
#[must_use]
pub fn linux_desktop_entry_text(identity: &CopyIdentity) -> String {
    let exe = escape_desktop_string_value(&identity.exe.to_string_lossy());
    format!(
        "[Desktop Entry]\n\
Type=Application\n\
Name=ManhwaStudio\n\
Comment=Comic translation editor\n\
TryExec={exe}\n\
Exec={} %f\n\
Path={}\n\
Icon=manhwastudio_rs\n\
Terminal=false\n\
Categories=Graphics;\n\
MimeType={}\n\
StartupNotify=true\n\
StartupWMClass=manhwastudio_rs\n\
X-KDE-DBUS-Restricted-Interfaces=org.kde.kwin.Screenshot,org.kde.KWin.ScreenShot2\n\
{OWNER_EXE_KEY}={exe}\n",
        escape_desktop_exec_arg(&identity.exe),
        escape_desktop_string_value(&identity.program_root.to_string_lossy()),
        desktop_mime_type_list()
    )
}

/// The startup decision for an entry file whose current bytes are `existing` (`None` = no file)
/// when this copy's entry text is `expected`. `resolve` resolves a program named in the entry
/// (a bare name through `$PATH`, [`resolve_program`]); `is_ours` tells whether a resolved path
/// names this copy's executable. Pure over both oracles.
///
/// Ownership is [`resolved_entry_owner`]; an entry of another copy is never taken over, alive or
/// not, and an entry with no owner, or whose bare owner name is not on `$PATH`, is left alone
/// ([`StartupOutcome::Unreadable`]).
#[must_use]
pub fn decide_startup(existing: Option<&[u8]>, expected: &str, resolve: &dyn Fn(&Path) -> Option<PathBuf>, is_ours: &dyn Fn(&Path) -> bool) -> StartupOutcome {
    let Some(bytes) = existing else {
        return StartupOutcome::Created;
    };
    let owner_exe = match resolved_entry_owner(&String::from_utf8_lossy(bytes), resolve) {
        EntryOwner::Key(exe) | EntryOwner::LegacyExec(exe) => exe,
        EntryOwner::Unknown => return StartupOutcome::Unreadable,
    };
    if !is_ours(&owner_exe) {
        StartupOutcome::LeftForeign { exe: owner_exe }
    } else if bytes == expected.as_bytes() {
        StartupOutcome::AlreadyCurrent
    } else {
        StartupOutcome::Refreshed
    }
}

/// Reads the owner of an existing entry from its `[Desktop Entry]` group
/// ([`main_group_entries`]): the `X-ManhwaStudio-Exe=` value when present (key wins over
/// `Exec`), else the first `Exec=` argument.
#[must_use]
pub fn entry_owner(text: &str) -> EntryOwner {
    let entries = main_group_entries(text);
    let value_of = |wanted: &str| entries.iter().find(|(key, _)| *key == wanted).map(|(_, value)| *value);
    if let Some(value) = value_of(OWNER_EXE_KEY).map(unescape_desktop_string_value).filter(|value| !value.is_empty()) {
        return EntryOwner::Key(PathBuf::from(value));
    }
    match value_of("Exec").and_then(exec_first_argument) {
        Some(argv0) => EntryOwner::LegacyExec(PathBuf::from(argv0)),
        None => EntryOwner::Unknown,
    }
}

/// [`entry_owner`] with the owner resolved to the file it runs through `resolve` (production:
/// [`resolve_program`] over the process `$PATH`). A bare name that does not resolve reads as
/// [`EntryOwner::Unknown`]: whose entry it is cannot be told, so it is neither judged nor
/// touched. The one ownership rule of the startup writer and the registration probe.
#[must_use]
pub fn resolved_entry_owner(text: &str, resolve: &dyn Fn(&Path) -> Option<PathBuf>) -> EntryOwner {
    match entry_owner(text) {
        EntryOwner::Key(exe) => resolve(&exe).map_or(EntryOwner::Unknown, EntryOwner::Key),
        EntryOwner::LegacyExec(exe) => resolve(&exe).map_or(EntryOwner::Unknown, EntryOwner::LegacyExec),
        EntryOwner::Unknown => EntryOwner::Unknown,
    }
}

/// The file a program named in `Exec=` / `TryExec=` runs. Per the Desktop Entry spec a value
/// with a `/` is a path (returned unchanged); a bare name is looked up in the absolute
/// directories of `path_var` (`$PATH`) in order, the first one where `is_executable` holds for
/// `<dir>/<name>` wins. `None` for an empty value or a bare name found nowhere (no `$PATH`
/// included). Relative `$PATH` entries are skipped: a menu launch has no meaningful working
/// directory to search. Pure over `path_var` and `is_executable`.
#[must_use]
pub fn resolve_program(program: &Path, path_var: Option<&OsStr>, is_executable: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let text = program.to_string_lossy();
    if text.is_empty() {
        return None;
    }
    if text.contains('/') {
        return Some(program.to_path_buf());
    }
    std::env::split_paths(path_var?).filter(|dir| dir.is_absolute()).map(|dir| dir.join(program)).find(|candidate| is_executable(candidate))
}

/// The raw (still escaped) `key=value` pairs of the `[Desktop Entry]` group, in file order,
/// first occurrence of each key only. Keys of other groups, comments, blank lines and lines
/// without `=` are skipped; spaces around `=` are ignored (spec). Localized keys keep their
/// `[locale]` suffix, so `Name[de]` never answers for `Name`. The one parser of the group,
/// shared by the ownership rule and the registration probe.
#[must_use]
pub(crate) fn main_group_entries(text: &str) -> Vec<(&str, &str)> {
    let mut in_main_group = false;
    let mut entries: Vec<(&str, &str)> = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_main_group = line.trim_end() == "[Desktop Entry]";
            continue;
        }
        if !in_main_group {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim_end(), value.trim_start());
        if !entries.iter().any(|(seen, _)| *seen == key) {
            entries.push((key, value));
        }
    }
    entries
}

/// The first argument of an `Exec=` value: string escapes removed first, then the Exec quoting
/// rules (double quotes with `\`-escapes, `%%` = `%`). `None` for an empty or unterminated value.
pub(crate) fn exec_first_argument(value: &str) -> Option<String> {
    let unescaped = unescape_desktop_string_value(value);
    let mut chars = unescaped.chars();
    let mut argument = String::new();
    if unescaped.starts_with('"') {
        chars.next();
        loop {
            match chars.next()? {
                '\\' => argument.push(chars.next()?),
                '"' => break,
                ch => argument.push(ch),
            }
        }
    } else {
        argument.extend(chars.take_while(|ch| !ch.is_whitespace()));
    }
    let argument = argument.replace("%%", "%");
    (!argument.is_empty()).then_some(argument)
}

/// The MIME types the entry offers "Open with" for: every `INPUT_FILE_TYPES` MIME type, in
/// table order. The one list both the writer ([`desktop_mime_type_list`]) and the registration
/// probe use.
pub fn desktop_mime_types() -> impl Iterator<Item = &'static str> {
    ms_config::single_image::INPUT_FILE_TYPES.iter().map(|file_type| file_type.mime)
}

/// The `MimeType=` value: [`desktop_mime_types`], `;`-separated with the trailing `;` the
/// Desktop Entry spec requires for string lists.
#[must_use]
pub fn desktop_mime_type_list() -> String {
    desktop_mime_types().map(|mime| format!("{mime};")).collect()
}

/// Quotes one `Exec=` argument per the Desktop Entry spec. Inside double quotes `"`, `` ` ``,
/// `$` and `\` need a backslash; the key file's own string escaping is applied first, so that
/// backslash is itself written doubled (`\\"`, and `\\\\` for a literal backslash). A
/// literal `%` is written `%%` so it is not read as a field code.
#[must_use]
pub fn escape_desktop_exec_arg(path: &Path) -> String {
    let mut out = String::with_capacity(path.as_os_str().len() + 2);
    out.push('"');
    for ch in path.to_string_lossy().chars() {
        match ch {
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(ch);
            }
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Escapes a plain `string` value (`TryExec=`, `Path=`, `X-ManhwaStudio-Exe=`) per the Desktop
/// Entry spec: `\\`, `\n`, `\t`, `\r`, and `\s` for a leading space that would otherwise be
/// trimmed.
#[must_use]
pub fn escape_desktop_string_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for (index, ch) in value.chars().enumerate() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            ' ' if index == 0 => out.push_str("\\s"),
            _ => out.push(ch),
        }
    }
    out
}

/// Inverse of [`escape_desktop_string_value`]; an unknown escape keeps its backslash, a trailing
/// lone backslash is kept as is.
pub(crate) fn unescape_desktop_string_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(exe: &str, root: &str) -> CopyIdentity {
        CopyIdentity { exe: PathBuf::from(exe), program_root: PathBuf::from(root), version_core: None }
    }

    fn opt_identity() -> CopyIdentity {
        identity("/opt/ManhwaStudio/manhwastudio_rs", "/opt/ManhwaStudio")
    }

    #[test]
    fn desktop_entry_passes_one_file_and_lists_every_input_mime_type() {
        let entry = linux_desktop_entry_text(&opt_identity());
        assert!(entry.contains("\nExec=\"/opt/ManhwaStudio/manhwastudio_rs\" %f\n"), "{entry}");
        assert!(!entry.contains("%u"), "a URL field code would hand over non-file URIs");
        let mime_line = entry
            .lines()
            .find_map(|line| line.strip_prefix("MimeType="))
            .expect("the entry must carry a MimeType line");
        assert!(mime_line.ends_with(';'), "string lists end with ';': {mime_line}");
        let listed: Vec<&str> = mime_line.trim_end_matches(';').split(';').collect();
        let expected: Vec<&str> = ms_config::single_image::INPUT_FILE_TYPES.iter().map(|file_type| file_type.mime).collect();
        assert_eq!(listed, expected);
        assert!(listed.contains(&"image/png") && listed.contains(&"image/jpeg"));
        assert!(entry.starts_with("[Desktop Entry]\n") && entry.contains("\nType=Application\n"));
    }

    /// Byte-exact golden of the whole entry. Pinned before the OS-integration extraction (R0)
    /// and changed DELIBERATELY by the Linux startup-policy package (B1): it added `TryExec=`,
    /// `Path=` and the `X-ManhwaStudio-Exe=` ownership key, and replaced the
    /// "ManhwaStudio Rust Prototype" comment.
    #[test]
    fn desktop_entry_text_is_byte_exact() {
        let entry = linux_desktop_entry_text(&opt_identity());
        let expected = "[Desktop Entry]\n\
Type=Application\n\
Name=ManhwaStudio\n\
Comment=Comic translation editor\n\
TryExec=/opt/ManhwaStudio/manhwastudio_rs\n\
Exec=\"/opt/ManhwaStudio/manhwastudio_rs\" %f\n\
Path=/opt/ManhwaStudio\n\
Icon=manhwastudio_rs\n\
Terminal=false\n\
Categories=Graphics;\n\
MimeType=image/png;image/jpeg;image/webp;image/bmp;image/tiff;image/gif;image/x-tga;image/qoi;\n\
StartupNotify=true\n\
StartupWMClass=manhwastudio_rs\n\
X-KDE-DBUS-Restricted-Interfaces=org.kde.kwin.Screenshot,org.kde.KWin.ScreenShot2\n\
X-ManhwaStudio-Exe=/opt/ManhwaStudio/manhwastudio_rs\n";
        assert_eq!(entry, expected);
    }

    #[test]
    fn desktop_exec_argument_is_quoted_and_escaped() {
        assert_eq!(escape_desktop_exec_arg(Path::new("/home/u/My Apps/ms")), "\"/home/u/My Apps/ms\"");
        assert_eq!(escape_desktop_exec_arg(Path::new("/home/u/Программы/ms")), "\"/home/u/Программы/ms\"");
        assert_eq!(escape_desktop_exec_arg(Path::new("/a\"b")), "\"/a\\\\\"b\"");
        assert_eq!(escape_desktop_exec_arg(Path::new("/a$b`c")), "\"/a\\\\$b\\\\`c\"");
        assert_eq!(escape_desktop_exec_arg(Path::new("/a\\b")), "\"/a\\\\\\\\b\"");
        assert_eq!(escape_desktop_exec_arg(Path::new("/100%/ms")), "\"/100%%/ms\"");
    }

    /// Every path a writer can emit reads back as the same owner, through the key and through
    /// the legacy `Exec` argument alike.
    #[test]
    fn owner_round_trips_through_key_and_exec() {
        for exe in ["/opt/ms/manhwastudio_rs", "/home/u/My Apps/ms", "/a\"b$c`d", "/a\\b", "/100%/ms", "/home/u/Программы/ms", " /lead"] {
            let entry = linux_desktop_entry_text(&identity(exe, "/opt/ms"));
            assert_eq!(entry_owner(&entry), EntryOwner::Key(PathBuf::from(exe)), "{entry}");
            let legacy: String = entry.lines().filter(|line| !line.starts_with(OWNER_EXE_KEY)).map(|line| format!("{line}\n")).collect();
            assert_eq!(entry_owner(&legacy), EntryOwner::LegacyExec(PathBuf::from(exe)), "{legacy}");
            assert_eq!(unescape_desktop_string_value(&escape_desktop_string_value(exe)), exe);
        }
    }

    #[test]
    fn owner_reads_only_the_main_group_and_prefers_the_key() {
        let text = "# c\n[Desktop Action x]\nX-ManhwaStudio-Exe=/wrong\nExec=/wrong2\n[Desktop Entry]\nExec = /legacy/ms %f\nX-ManhwaStudio-Exe = /keyed/ms\n";
        assert_eq!(entry_owner(text), EntryOwner::Key(PathBuf::from("/keyed/ms")));
        assert_eq!(entry_owner("[Desktop Entry]\nExec=/plain/ms --flag %f\n"), EntryOwner::LegacyExec(PathBuf::from("/plain/ms")));
        assert_eq!(entry_owner("[Desktop Entry]\nName=x\n"), EntryOwner::Unknown);
        assert_eq!(entry_owner("[Desktop Entry]\nExec=\"/unterminated\n"), EntryOwner::Unknown);
        assert_eq!(entry_owner("[Desktop Entry]\nX-ManhwaStudio-Exe=\nExec=/fallback/ms\n"), EntryOwner::LegacyExec(PathBuf::from("/fallback/ms")));
        assert_eq!(entry_owner(""), EntryOwner::Unknown);
    }

    /// The startup decision table: missing, ours by key, ours by legacy `Exec`, current,
    /// foreign (by key even when `Exec` is ours), and an entry with no owner.
    #[test]
    fn startup_decision_table() {
        let ours = opt_identity();
        let expected = linux_desktop_entry_text(&ours);
        let is_ours = |path: &Path| path == ours.exe;
        let resolve = |path: &Path| Some(path.to_path_buf());
        assert_eq!(decide_startup(None, &expected, &resolve, &is_ours), StartupOutcome::Created);
        assert_eq!(decide_startup(Some(expected.as_bytes()), &expected, &resolve, &is_ours), StartupOutcome::AlreadyCurrent);
        let stale_keyed = expected.replace("Comment=Comic translation editor", "Comment=old");
        assert_eq!(decide_startup(Some(stale_keyed.as_bytes()), &expected, &resolve, &is_ours), StartupOutcome::Refreshed);
        let legacy = "[Desktop Entry]\nExec=\"/opt/ManhwaStudio/manhwastudio_rs\" %f\n";
        assert_eq!(decide_startup(Some(legacy.as_bytes()), &expected, &resolve, &is_ours), StartupOutcome::Refreshed);
        let foreign = linux_desktop_entry_text(&identity("/home/u/other/manhwastudio_rs", "/home/u/other"));
        assert_eq!(
            decide_startup(Some(foreign.as_bytes()), &expected, &resolve, &is_ours),
            StartupOutcome::LeftForeign { exe: PathBuf::from("/home/u/other/manhwastudio_rs") }
        );
        let keyed_elsewhere = expected.replace("X-ManhwaStudio-Exe=/opt/ManhwaStudio/manhwastudio_rs", "X-ManhwaStudio-Exe=/elsewhere/ms");
        assert_eq!(
            decide_startup(Some(keyed_elsewhere.as_bytes()), &expected, &resolve, &is_ours),
            StartupOutcome::LeftForeign { exe: PathBuf::from("/elsewhere/ms") }
        );
        assert_eq!(decide_startup(Some(b"[Desktop Entry]\nName=x\n"), &expected, &resolve, &is_ours), StartupOutcome::Unreadable);
    }

    /// A legacy entry with a bare `Exec=` name (PATH lookup): judged by the program `$PATH`
    /// finds — ours (refreshed), another copy (left alone, reported resolved) — and left alone
    /// as ownerless when `$PATH` finds nothing.
    // Unix `$PATH` syntax (`:`) and absolute paths: a Windows host test build parses neither.
    #[cfg(unix)]
    #[test]
    fn startup_decision_resolves_a_bare_exec_through_path() {
        let ours = opt_identity();
        let expected = linux_desktop_entry_text(&ours);
        let is_ours = |path: &Path| path == ours.exe;
        let bare = b"[Desktop Entry]\nExec=manhwastudio_rs %f\n";
        let path_var = OsStr::new("/usr/bin:/opt/ManhwaStudio");
        let on_path = |found: &'static str| move |path: &Path| resolve_program(path, Some(path_var), &|candidate| candidate == Path::new(found));
        assert_eq!(decide_startup(Some(bare), &expected, &on_path("/opt/ManhwaStudio/manhwastudio_rs"), &is_ours), StartupOutcome::Refreshed);
        assert_eq!(
            decide_startup(Some(bare), &expected, &on_path("/usr/bin/manhwastudio_rs"), &is_ours),
            StartupOutcome::LeftForeign { exe: PathBuf::from("/usr/bin/manhwastudio_rs") }
        );
        assert_eq!(decide_startup(Some(bare), &expected, &on_path("/nowhere/manhwastudio_rs"), &is_ours), StartupOutcome::Unreadable);
    }

    /// The `$PATH` resolver table: paths pass through, bare names take the first executable hit
    /// in an absolute `$PATH` directory, and an empty name, no `$PATH` or no hit resolve to `None`.
    // Unix `$PATH` syntax (`:`) and absolute paths: a Windows host test build parses neither.
    #[cfg(unix)]
    #[test]
    fn resolve_program_table() {
        let executables = [Path::new("/usr/local/bin/ms"), Path::new("/usr/bin/ms"), Path::new("/usr/bin/other")];
        let is_executable = |path: &Path| executables.contains(&path);
        let resolve = |program: &str, path_var: Option<&str>| resolve_program(Path::new(program), path_var.map(OsStr::new), &is_executable);
        let some = |path: &str| Some(PathBuf::from(path));

        assert_eq!(resolve("/opt/ms/ms", None), some("/opt/ms/ms"), "an absolute path is never looked up");
        assert_eq!(resolve("/gone/ms", Some("/usr/bin")), some("/gone/ms"), "a missing absolute path stays itself");
        assert_eq!(resolve("bin/ms", Some("/usr")), some("bin/ms"), "a value with a slash is a path");
        assert_eq!(resolve("ms", Some("/usr/local/bin:/usr/bin")), some("/usr/local/bin/ms"), "first hit wins");
        assert_eq!(resolve("ms", Some("/usr/bin:/usr/local/bin")), some("/usr/bin/ms"));
        assert_eq!(resolve("other", Some("/usr/local/bin:/usr/bin")), some("/usr/bin/other"));
        assert_eq!(resolve("ms", Some("usr/bin:/sbin")), None, "relative entries are skipped");
        assert_eq!(resolve("ms", Some("")), None);
        assert_eq!(resolve("ms", None), None, "no $PATH, no lookup");
        assert_eq!(resolve("missing", Some("/usr/local/bin:/usr/bin")), None);
        assert_eq!(resolve("", Some("/usr/bin")), None);

        let unresolved = |_: &Path| None;
        assert_eq!(resolved_entry_owner("[Desktop Entry]\nExec=ms %f\n", &unresolved), EntryOwner::Unknown);
        let on_path = |path: &Path| resolve_program(path, Some(OsStr::new("/usr/bin")), &is_executable);
        assert_eq!(resolved_entry_owner("[Desktop Entry]\nExec=ms %f\n", &on_path), EntryOwner::LegacyExec(PathBuf::from("/usr/bin/ms")));
        assert_eq!(resolved_entry_owner("[Desktop Entry]\nX-ManhwaStudio-Exe=/k/ms\nExec=ms\n", &on_path), EntryOwner::Key(PathBuf::from("/k/ms")));
    }
}
