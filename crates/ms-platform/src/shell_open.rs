/*
File: shell_open.rs

Purpose:
Hands a URL or a folder to the operating system's default handler: the default browser for a
URL, the file manager for a folder. The one owner of "how does this platform open X".

Key structures:
- ShellTarget: what to open (URL or directory).
- ShellCommand: the program, its arguments and the console-window flag one target maps to.
- ShellOpenError: the typed failure, with a localized `Display`.

Key functions:
- open_url(): opens a URL in the default browser.
- reveal_dir(): opens a directory in the file manager.
- command_for(): the pure per-platform command table (host-tested per OS cfg).

Notes:
Spawn only: neither entry point waits for the handler, so both are safe on the GUI thread (a
process spawn, no I/O wait). On unix a detached `shell-open-reap` thread waits for the handler
so it never lingers as a zombie, and logs a failed handler at warn level. Available where `PlatformFeature::ShellOpen` says so (desktop);
elsewhere both return `ShellOpenError::Unavailable`. Command per OS:
Windows `cmd /C start "" <url>` (no console window) / `explorer.exe <dir>`, macOS `open`,
other unix `xdg-open`.
*/

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::Path;
use std::process::Command;

use crate::{PlatformFeature, Unavailable};

/// What the OS shell is asked to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellTarget<'a> {
    /// A URL for the default browser (passed through unchanged).
    Url(&'a str),
    /// A directory for the system file manager.
    Dir(&'a Path),
}

/// The process one [`ShellTarget`] maps to on this platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommand {
    /// Program name, resolved through `PATH`.
    pub program: &'static str,
    /// Arguments in order.
    pub args: Vec<OsString>,
    /// Windows only: spawn with `CREATE_NO_WINDOW`, so `cmd` flashes no console window.
    pub no_console_window: bool,
}

/// Failure to hand a target to the OS shell. `Display` is the localized [`Self::user_message`];
/// log it with `{:?}` for the typed details.
#[derive(Debug)]
pub enum ShellOpenError {
    /// This platform has no shell handler (`PlatformFeature::ShellOpen`).
    Unavailable(Unavailable),
    /// The handler program could not be started (missing from `PATH`, permission denied, ...).
    Spawn {
        /// The URL, or the directory as displayed by `Path::display`.
        target: String,
        /// Whether `target` is a directory (selects the message).
        is_dir: bool,
        /// The OS error of the spawn.
        source: io::Error,
    },
}

impl ShellOpenError {
    /// Localized message for a status line or toast.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            ShellOpenError::Unavailable(unavailable) => unavailable.user_message(),
            ShellOpenError::Spawn { target, is_dir: false, source } => ms_i18n::interpolate(
                ms_i18n::resolve_key("platform.shell_open.open_url_error"),
                &[("url", target as &dyn fmt::Display), ("e", source as &dyn fmt::Display)],
            ),
            ShellOpenError::Spawn { target, is_dir: true, source } => ms_i18n::interpolate(
                ms_i18n::resolve_key("platform.shell_open.reveal_dir_error"),
                &[("path", target as &dyn fmt::Display), ("e", source as &dyn fmt::Display)],
            ),
        }
    }
}

impl fmt::Display for ShellOpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for ShellOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ShellOpenError::Unavailable(unavailable) => Some(unavailable),
            ShellOpenError::Spawn { source, .. } => Some(source),
        }
    }
}

impl From<Unavailable> for ShellOpenError {
    fn from(unavailable: Unavailable) -> Self {
        ShellOpenError::Unavailable(unavailable)
    }
}

/// Opens `url` in the default browser. Returns once the handler process is spawned; on unix a
/// detached thread reaps it and logs a non-zero exit at warn level.
///
/// # Errors
/// [`ShellOpenError::Unavailable`] off desktop, [`ShellOpenError::Spawn`] when the handler
/// program cannot be started.
pub fn open_url(url: &str) -> Result<(), ShellOpenError> {
    spawn(ShellTarget::Url(url))
}

/// Opens directory `path` in the system file manager. Returns once the handler is spawned (on
/// unix a detached thread reaps it and logs a non-zero exit at warn level); it does not check
/// that `path` exists (the file manager reports that itself).
///
/// # Errors
/// [`ShellOpenError::Unavailable`] off desktop, [`ShellOpenError::Spawn`] when the handler
/// program cannot be started.
pub fn reveal_dir(path: &Path) -> Result<(), ShellOpenError> {
    spawn(ShellTarget::Dir(path))
}

/// The command `target` maps to on the current build target. Pure: nothing is spawned.
///
/// # Errors
/// [`Unavailable`] (`ShellOpen`) where the platform has no shell handler.
pub fn command_for(target: ShellTarget<'_>) -> Result<ShellCommand, Unavailable> {
    PlatformFeature::ShellOpen.require()?;
    #[cfg(any(target_os = "windows", target_os = "macos", all(unix, not(target_os = "android"))))]
    {
        Ok(platform_command(target))
    }
    // Android and wasm: the capability table already answered above; this keeps the answer
    // typed if the two ever disagree. `target` is a `Copy` value, nothing is discarded.
    #[cfg(not(any(target_os = "windows", target_os = "macos", all(unix, not(target_os = "android")))))]
    {
        let _ = target;
        Err(Unavailable::new(PlatformFeature::ShellOpen, crate::UnavailableReason::NotOnThisPlatform))
    }
}

/// Builds and spawns the command for `target` without waiting for it (unix: reaped by
/// [`reap_detached`]).
fn spawn(target: ShellTarget<'_>) -> Result<(), ShellOpenError> {
    let shell_command = command_for(target)?;
    let mut cmd = Command::new(shell_command.program);
    cmd.args(&shell_command.args);
    if shell_command.no_console_window {
        hide_console_window(&mut cmd);
    }
    let child = cmd.spawn().map_err(|source| {
        let (target, is_dir) = match target {
            ShellTarget::Url(url) => (url.to_owned(), false),
            ShellTarget::Dir(path) => (path.display().to_string(), true),
        };
        ShellOpenError::Spawn { target, is_dir, source }
    })?;
    #[cfg(unix)]
    reap_detached(child, shell_command.program);
    // Windows keeps no zombie: dropping the `Child` closes its handles and the process lives on.
    #[cfg(not(unix))]
    drop(child);
    Ok(())
}

/// Waits for `child` on a detached `shell-open-reap` thread, so an exited handler never stays
/// a zombie, and logs a failed wait or a non-zero exit at warn level (program name only: the
/// target may be a private URL or path). If the thread cannot start, the failure is logged and
/// the child is left unreaped (one zombie until the app exits). Desktop-only path, so plain
/// `std::thread` (never reached on wasm).
#[cfg(unix)]
fn reap_detached(child: std::process::Child, program: &'static str) {
    let spawned = std::thread::Builder::new().name("shell-open-reap".to_owned()).spawn(move || {
        if let Some(problem) = wait_and_report(child, program) {
            ms_log::runtime_log::log_warn(problem);
        }
    });
    if let Err(err) = spawned {
        ms_log::runtime_log::log_warn(format!("[shell_open] could not start the reaper thread for {program}: {err}"));
    }
}

/// Blocks until `child` exits. Returns the warn-level log line for a failed wait or a non-zero
/// exit, `None` for a clean exit.
#[cfg(unix)]
fn wait_and_report(mut child: std::process::Child, program: &str) -> Option<String> {
    match child.wait() {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!("[shell_open] {program} exited with {status}")),
        Err(err) => Some(format!("[shell_open] waiting for {program} failed: {err}")),
    }
}

/// Windows: `cmd /C start "" <url>` (the empty first argument is `start`'s window title, so a
/// quoted URL is never taken for one) and `explorer.exe <dir>`.
#[cfg(target_os = "windows")]
fn platform_command(target: ShellTarget<'_>) -> ShellCommand {
    match target {
        ShellTarget::Url(url) => ShellCommand {
            program: "cmd",
            args: vec!["/C".into(), "start".into(), "".into(), url.into()],
            no_console_window: true,
        },
        ShellTarget::Dir(path) => ShellCommand { program: "explorer.exe", args: vec![path.into()], no_console_window: false },
    }
}

/// macOS: `open <target>` for both kinds.
#[cfg(target_os = "macos")]
fn platform_command(target: ShellTarget<'_>) -> ShellCommand {
    ShellCommand { program: "open", args: vec![target_arg(target)], no_console_window: false }
}

/// Other unix desktops: `xdg-open <target>` for both kinds. Android has no `xdg-open`; it is
/// excluded here and answered by `command_for`'s fallback.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
fn platform_command(target: ShellTarget<'_>) -> ShellCommand {
    ShellCommand { program: "xdg-open", args: vec![target_arg(target)], no_console_window: false }
}

/// The single argument of the `open` / `xdg-open` form.
#[cfg(any(target_os = "macos", all(unix, not(target_os = "android"))))]
fn target_arg(target: ShellTarget<'_>) -> OsString {
    match target {
        ShellTarget::Url(url) => url.into(),
        ShellTarget::Dir(path) => path.into(),
    }
}

/// Sets `CREATE_NO_WINDOW` so a console program opens no console window.
#[cfg(target_os = "windows")]
fn hide_console_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// No console windows exist outside Windows; the flag is only ever set by the Windows table.
#[cfg(not(target_os = "windows"))]
fn hide_console_window(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UnavailableReason;
    use crate::test_catalog::{assert_key_in_shipped_catalogs, install_english};

    const URL: &str = "https://example.com/a?b=c";

    #[test]
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    fn unix_uses_xdg_open() {
        let url = command_for(ShellTarget::Url(URL)).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(url, ShellCommand { program: "xdg-open", args: vec![URL.into()], no_console_window: false });
        let dir = Path::new("/home/u/ext");
        let reveal = command_for(ShellTarget::Dir(dir)).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(reveal, ShellCommand { program: "xdg-open", args: vec![dir.into()], no_console_window: false });
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_uses_open() {
        let url = command_for(ShellTarget::Url(URL)).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(url, ShellCommand { program: "open", args: vec![URL.into()], no_console_window: false });
        let dir = Path::new("/Users/alice/ext");
        let reveal = command_for(ShellTarget::Dir(dir)).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(reveal, ShellCommand { program: "open", args: vec![dir.into()], no_console_window: false });
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn windows_uses_cmd_start_and_explorer() {
        let url = command_for(ShellTarget::Url(URL)).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(
            url,
            ShellCommand { program: "cmd", args: vec!["/C".into(), "start".into(), "".into(), URL.into()], no_console_window: true }
        );
        let dir = Path::new(r"C:\Users\alice\ext");
        let reveal = command_for(ShellTarget::Dir(dir)).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(reveal, ShellCommand { program: "explorer.exe", args: vec![dir.into()], no_console_window: false });
    }

    #[test]
    #[cfg(unix)]
    fn reaper_reports_only_a_failed_handler() {
        let spawn = |program: &str| Command::new(program).spawn().unwrap_or_else(|err| panic!("spawn {program}: {err}"));
        assert_eq!(wait_and_report(spawn("true"), "true"), None);
        let report = wait_and_report(spawn("false"), "false").unwrap_or_else(|| panic!("a non-zero exit must be reported"));
        assert!(report.contains("false"), "{report}");
    }

    #[test]
    fn off_desktop_is_unavailable() {
        if PlatformFeature::ShellOpen.is_available() {
            return;
        }
        assert_eq!(
            command_for(ShellTarget::Url(URL)),
            Err(Unavailable::new(PlatformFeature::ShellOpen, UnavailableReason::NotOnThisPlatform))
        );
    }

    #[test]
    fn spawn_messages_are_shipped_and_name_the_target() {
        assert_key_in_shipped_catalogs("platform.shell_open.open_url_error");
        assert_key_in_shipped_catalogs("platform.shell_open.reveal_dir_error");
        install_english();
        for is_dir in [false, true] {
            let err = ShellOpenError::Spawn { target: "T-1".to_owned(), is_dir, source: io::Error::from(io::ErrorKind::NotFound) };
            let message = err.to_string();
            assert!(message.contains("T-1"), "{message}");
            assert!(!message.contains("platform."), "a catalog miss would show the raw key: {message}");
            assert!(!message.contains('{'), "unfilled placeholder: {message}");
        }
    }
}
