/*
File: crates/ms-sdcpp/src/client/spawn.rs

Purpose:
How the app starts an sd.cpp worker process: the `WorkerLauncher` seam (real: run the app's
own executable with `--sdcpp-worker`; tests: an in-process fake over OS pipes), the pipes and
child handle a launch yields, and the readable description of how a worker ended.

Key items:
- WORKER_FLAG               : the hidden command-line flag of the worker mode
- WorkerLauncher / WorkerPipes / WorkerChild : the seam
- ProcessLauncher           : `<exe> --sdcpp-worker`, piped stdio, no console window
- describe_exit()           : "signal 6 (SIGABRT)", "code 134", "code 0xC0000409"

Notes:
The worker inherits no `GGML_BACKEND_PATH` (removed here and again in the worker), so the
backends always come from the package directory. `GGML_NO_BACKTRACE` is set so a ggml abort
does not attach gdb for a backtrace: that took ~35 s and pushed the `GGML_ASSERT` line out of
the crash report's stderr tail.
*/

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};

use crate::error::SdCppError;

/// The hidden flag that starts the worker mode of the app's executable.
pub const WORKER_FLAG: &str = "--sdcpp-worker";

/// Environment variable carrying the app's process id to the worker, which exits when its
/// parent is already someone else (the app died before the worker armed its parent-death
/// signal).
pub const PARENT_PID_ENV: &str = "MS_SDCPP_PARENT_PID";

/// A running worker's process handle.
pub trait WorkerChild: Send {
    /// `Some(description)` once the process has exited (see [`describe_exit`]).
    ///
    /// # Errors
    /// The OS error of the status query.
    fn try_wait(&mut self) -> io::Result<Option<String>>;
    /// Kills the process (no-op when it already exited) and reaps it.
    ///
    /// # Errors
    /// The OS error of the kill.
    fn kill(&mut self) -> io::Result<()>;
}

/// The pipes and handle of one launched worker.
pub struct WorkerPipes {
    /// Requests to the worker.
    pub stdin: Box<dyn Write + Send>,
    /// Events from the worker.
    pub stdout: Box<dyn Read + Send>,
    /// The worker's diagnostics (drained and logged), if piped.
    pub stderr: Option<Box<dyn Read + Send>>,
    /// The process handle.
    pub child: Box<dyn WorkerChild>,
}

/// Starts worker processes.
pub trait WorkerLauncher: Send + Sync {
    /// Starts one worker.
    ///
    /// # Errors
    /// `SdCppError::WorkerSpawn` when the process cannot be started.
    fn launch(&self) -> Result<WorkerPipes, SdCppError>;
}

/// Launches `<exe> --sdcpp-worker` with piped stdin / stdout / stderr.
#[derive(Debug, Clone)]
pub struct ProcessLauncher {
    exe: PathBuf,
}

impl ProcessLauncher {
    /// A launcher of `exe` (a binary whose `main` routes [`WORKER_FLAG`] to
    /// `ms_sdcpp::worker::run_worker_main`).
    #[must_use]
    pub fn new(exe: PathBuf) -> Self {
        Self { exe }
    }

    /// A launcher of the running executable.
    ///
    /// # Errors
    /// `WorkerSpawn` when the executable path cannot be determined.
    pub fn current_exe() -> Result<Self, SdCppError> {
        std::env::current_exe().map(Self::new).map_err(|err| SdCppError::WorkerSpawn { detail: format!("current executable: {err}") })
    }
}

/// `std::process::Child` as a [`WorkerChild`].
struct ProcessChild(Child);

impl WorkerChild for ProcessChild {
    fn try_wait(&mut self) -> io::Result<Option<String>> {
        Ok(self.0.try_wait()?.map(describe_exit))
    }

    fn kill(&mut self) -> io::Result<()> {
        if self.0.try_wait()?.is_some() {
            return Ok(());
        }
        self.0.kill()?;
        self.0.wait().map(|_| ())
    }
}

impl ProcessLauncher {
    /// The worker command: `<exe> --sdcpp-worker`, piped stdio, the app's pid in
    /// [`PARENT_PID_ENV`], no `GGML_BACKEND_PATH`, `GGML_NO_BACKTRACE` set (see the header),
    /// no console window (Windows).
    fn command(&self) -> Command {
        let mut command = Command::new(&self.exe);
        command
            .arg(WORKER_FLAG)
            .env_remove("GGML_BACKEND_PATH")
            .env("GGML_NO_BACKTRACE", "1")
            .env(PARENT_PID_ENV, std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        command
    }
}

impl WorkerLauncher for ProcessLauncher {
    fn launch(&self) -> Result<WorkerPipes, SdCppError> {
        let mut child = self.command().spawn().map_err(|err| SdCppError::WorkerSpawn { detail: format!("{}: {err}", self.exe.display()) })?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            // Never leave a started process behind: kill and reap it before reporting.
            let mut child = ProcessChild(child);
            if let Err(err) = child.kill() {
                ms_log::runtime_log::log_warn(format!("[sdcpp] cannot kill a worker without pipes: {err}"));
            }
            return Err(SdCppError::WorkerSpawn { detail: "the worker's pipes were not created".to_owned() });
        };
        let stderr = child.stderr.take().map(|stderr| Box::new(stderr) as Box<dyn Read + Send>);
        Ok(WorkerPipes { stdin: Box::new(stdin), stdout: Box::new(stdout), stderr, child: Box::new(ProcessChild(child)) })
    }
}

/// A short description of how a process ended: `code N` (Windows NTSTATUS-like codes in hex),
/// or on Unix `signal N (NAME)` for a signal.
#[must_use]
pub fn describe_exit(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            let name = match signal {
                libc::SIGABRT => " (SIGABRT)",
                libc::SIGSEGV => " (SIGSEGV)",
                libc::SIGKILL => " (SIGKILL)",
                libc::SIGBUS => " (SIGBUS)",
                libc::SIGILL => " (SIGILL)",
                libc::SIGFPE => " (SIGFPE)",
                _ => "",
            };
            return format!("signal {signal}{name}");
        }
    }
    match status.code() {
        // Windows crash codes (0xC0000005, 0xC0000409, …) are negative as `i32`.
        Some(code) if cfg!(windows) && code < 0 => format!("code {:#010X}", code.cast_unsigned()),
        Some(code) => format!("code {code}"),
        None => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worker_command_carries_the_flag_and_the_parent_pid() {
        let command = ProcessLauncher::new(PathBuf::from("/opt/app/manhwastudio_rs")).command();
        assert_eq!(command.get_args().collect::<Vec<_>>(), [WORKER_FLAG]);
        let envs: Vec<_> = command.get_envs().collect();
        let pid = std::process::id().to_string();
        assert!(envs.contains(&(std::ffi::OsStr::new(PARENT_PID_ENV), Some(std::ffi::OsStr::new(&pid)))), "{envs:?}");
        assert!(envs.contains(&(std::ffi::OsStr::new("GGML_BACKEND_PATH"), None)), "{envs:?}");
    }
}
