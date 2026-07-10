/*
File: src/python_manager.rs

Purpose:
Central Python runtime manager for the Rust application.

Main responsibilities:
- discover the app-local Python runtime (`installer_files/venv`, `venv`,
  `installer_files/env`, or `installer_files/python`);
- build configured Python `Command` values for scripts, inline code, and long-lived daemons;
- spawn long-lived Python children inside a Windows Job Object that kills them when the parent
  process dies;
- provide shell activation snippets for UI consoles and environment probes;
- apply shared process settings such as UTF-8 environment variables and hidden Windows windows.

Notes:
This module does not install Python or packages. The installer owns downloads and dependency
installation, but it uses this module for the same executable-discovery contract as runtime code.

Web (wasm) build:
Python process management has no meaning in the browser (no OS processes, no local
Python runtime), so the entire module compiles out on `wasm32`. Every caller of this
module is itself a native-only subsystem (installer, launcher settings, AI backend
supervisor, AI install probe) that gates its use behind the same `not(wasm32)` cfg, so
no shared/web code references these items.
*/

// Whole native subsystem: compiled out on the web target (convention 3).
#![cfg(not(target_arch = "wasm32"))]

use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

#[derive(Debug, Clone)]
pub enum PythonEnvironment {
    VirtualEnv { root: PathBuf },
    CondaEnv { root: PathBuf },
    StandalonePython { executable: PathBuf },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonShellKind {
    #[cfg(target_os = "windows")]
    PowerShell,
    #[cfg(not(target_os = "windows"))]
    PosixSh,
}

#[derive(Debug)]
pub struct ManagedPythonChild {
    child: Child,
    #[cfg(target_os = "windows")]
    _kill_job: WindowsKillOnDropJob,
}

impl ManagedPythonChild {
    #[cfg(target_os = "windows")]
    fn new(mut child: Child) -> Result<Self, String> {
        let kill_job = match WindowsKillOnDropJob::assign_child(&child) {
            Ok(job) => job,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err);
            }
        };
        Ok(Self {
            child,
            _kill_job: kill_job,
        })
    }

    #[cfg(not(target_os = "windows"))]
    fn new(child: Child) -> Result<Self, String> {
        Ok(Self { child })
    }

    #[must_use]
    pub fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Deref for ManagedPythonChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl DerefMut for ManagedPythonChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

#[must_use]
pub fn has_supported_python_env(root_dir: &Path) -> bool {
    detect_python_environment(root_dir).is_ok()
}

pub fn detect_python_environment(root_dir: &Path) -> Result<PythonEnvironment, String> {
    let managed_venv = root_dir.join("installer_files").join("venv");
    if activation_root_exists(&managed_venv) {
        return Ok(PythonEnvironment::VirtualEnv { root: managed_venv });
    }

    let regular_venv = root_dir.join("venv");
    if activation_root_exists(&regular_venv) {
        return Ok(PythonEnvironment::VirtualEnv { root: regular_venv });
    }

    let conda_env = root_dir.join("installer_files").join("env");
    if conda_python_exists(root_dir, &conda_env) {
        return Ok(PythonEnvironment::CondaEnv { root: conda_env });
    }

    if let Some(executable) = resolve_standalone_python_executable(root_dir) {
        return Ok(PythonEnvironment::StandalonePython { executable });
    }

    Err(t!("python_manager.env_not_found").to_string())
}

pub fn resolve_python_executable(root_dir: &Path) -> Result<PathBuf, String> {
    match detect_python_environment(root_dir)? {
        PythonEnvironment::VirtualEnv { root } | PythonEnvironment::CondaEnv { root } => {
            resolve_python_executable_in_dir(&root)
        }
        PythonEnvironment::StandalonePython { executable } => Ok(executable),
    }
}

pub fn resolve_python_executable_in_dir(python_dir: &Path) -> Result<PathBuf, String> {
    for rel in python_executable_candidates() {
        let candidate = python_dir.join(rel);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    let nested = std::fs::read_dir(python_dir)
        .map_err(|err| tf!("python_manager.read_dir_error", python_dir = python_dir.display(), err = err))?;
    for entry_result in nested {
        let entry = entry_result
            .map_err(|err| tf!("python_manager.read_entry_error", python_dir = python_dir.display(), err = err))?;
        let candidate_root = entry.path();
        if !candidate_root.is_dir() {
            continue;
        }
        for rel in python_executable_candidates() {
            let candidate = candidate_root.join(rel);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(tf!("python_manager.executable_not_found", python_dir = python_dir.display()))
}

pub fn build_python_command(root_dir: &Path) -> Result<Command, String> {
    let python = resolve_python_executable(root_dir)?;
    let mut command = Command::new(python);
    configure_python_command(&mut command);
    Ok(command)
}

pub fn build_python_script_command(root_dir: &Path, script_name: &str) -> Result<Command, String> {
    let mut command = build_python_command(root_dir)?;
    command
        .current_dir(root_dir)
        .arg("-X")
        .arg("utf8")
        .arg(script_name);
    Ok(command)
}

// Kept as a general helper alongside `build_python_script_command`; the advanced
// browser downloaders that used it now run inside the unified AI backend over IPC.
#[allow(dead_code)]
pub fn build_python_script_path_command(
    root_dir: &Path,
    script_path: &Path,
) -> Result<Command, String> {
    let mut command = build_python_command(root_dir)?;
    command.current_dir(root_dir).arg("-u").arg(script_path);
    Ok(command)
}

pub fn spawn_kill_with_parent(mut command: Command) -> Result<ManagedPythonChild, String> {
    let child = command
        .spawn()
        .map_err(|err| format!("failed to spawn Python child process: {err}"))?;
    ManagedPythonChild::new(child)
}

pub fn configure_python_command(command: &mut Command) {
    apply_windows_no_window(command);
    command.env("PYTHONIOENCODING", "utf-8");
    command.env("PYTHONUTF8", "1");
}

pub fn apply_windows_no_window(command: &mut Command) {
    apply_windows_no_window_inner(command);
}

#[cfg(target_os = "windows")]
#[derive(Debug)]
struct WindowsKillOnDropJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// The wrapper has unique ownership of a kernel job handle. Moving that ownership to the worker
// thread that owns the child process is safe; Drop still closes the handle exactly once.
#[cfg(target_os = "windows")]
unsafe impl Send for WindowsKillOnDropJob {}

#[cfg(target_os = "windows")]
impl WindowsKillOnDropJob {
    fn assign_child(child: &Child) -> Result<Self, String> {
        use std::mem::{size_of, zeroed};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let info_len = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .map_err(|_| "Windows job information size does not fit u32".to_string())?;

        // SAFETY: Passing null security attributes/name creates an unnamed job owned by this
        // process. The returned handle is checked before use and closed in Drop.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(format!(
                "CreateJobObjectW failed with Windows error {}",
                last_windows_error()
            ));
        }

        // SAFETY: The all-zero structure is a valid baseline for this POD WinAPI struct; only
        // LimitFlags is set before SetInformationJobObject reads it.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        // SAFETY: `handle` is a valid job handle, `limits` points to a properly initialized
        // JOBOBJECT_EXTENDED_LIMIT_INFORMATION, and `info_len` matches the structure size.
        let set_result = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                info_len,
            )
        };
        if set_result == 0 {
            let error = last_windows_error();
            // SAFETY: `handle` was created successfully above and is not used after this close.
            unsafe {
                CloseHandle(handle);
            }
            return Err(format!(
                "SetInformationJobObject(KILL_ON_JOB_CLOSE) failed with Windows error {error}"
            ));
        }

        let process_handle = child.as_raw_handle();
        // SAFETY: The job handle is valid and the child process handle is owned by std::process.
        // Assigning the child to the job does not transfer ownership of the process handle.
        let assign_result = unsafe { AssignProcessToJobObject(handle, process_handle) };
        if assign_result == 0 {
            let error = last_windows_error();
            // SAFETY: `handle` was created successfully above and is not used after this close.
            unsafe {
                CloseHandle(handle);
            }
            return Err(format!(
                "AssignProcessToJobObject failed with Windows error {error}"
            ));
        }

        Ok(Self { handle })
    }
}

#[cfg(target_os = "windows")]
fn last_windows_error() -> u32 {
    // SAFETY: GetLastError reads the calling thread's last-error code and has no preconditions.
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

#[cfg(target_os = "windows")]
impl Drop for WindowsKillOnDropJob {
    fn drop(&mut self) {
        // SAFETY: `handle` is owned by this wrapper and closed exactly once here.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[must_use]
pub fn activation_commands(environment: &PythonEnvironment, shell: PythonShellKind) -> Vec<String> {
    match environment {
        PythonEnvironment::VirtualEnv { root } => {
            let scripts_dir = virtual_env_scripts_dir(root);
            vec![
                shell_echo_command(
                    shell,
                    &tf!("python_manager.activate_venv", root = root.display()),
                ),
                shell_set_path_command(shell, &scripts_dir),
                shell_set_env_command(shell, "VIRTUAL_ENV", root),
            ]
        }
        PythonEnvironment::CondaEnv { root } => {
            let mut commands = vec![
                shell_echo_command(shell, &tf!("python_manager.activate_conda", root = root.display())),
                shell_set_path_command(shell, &conda_primary_bin(root)),
            ];
            if let Some(extra_path) = conda_secondary_bin(root) {
                commands.push(shell_prepend_path_command(shell, &extra_path));
            }
            commands.push(shell_set_env_command(shell, "CONDA_PREFIX", root));
            commands
        }
        PythonEnvironment::StandalonePython { executable } => {
            let executable_dir = executable
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| executable.clone());
            vec![
                shell_echo_command(
                    shell,
                    &tf!("python_manager.use_standalone", executable = executable.display()),
                ),
                shell_set_path_command(shell, &executable_dir),
            ]
        }
    }
}

#[must_use]
pub fn configure_pip_fallback_command(shell: PythonShellKind) -> String {
    match shell {
        #[cfg(target_os = "windows")]
        PythonShellKind::PowerShell => concat!(
            "if (-not (Get-Command pip -ErrorAction SilentlyContinue)) { ",
            "python -m pip --version > $null 2> $null; ",
            "if ($LASTEXITCODE -eq 0) { ",
            "function global:pip { python -m pip @args }; ",
            "Write-Output 'pip-команда не найдена; команды pip будут выполняться через python -m pip.' ",
            "} else { ",
            "if (Get-Command uv -ErrorAction SilentlyContinue) { ",
            "function global:pip { uv pip @args }; ",
            "Write-Output 'pip не найден в Python env; команды pip будут выполняться через uv pip.' ",
            "} else { ",
            "Write-Output 'pip не найден в Python env; uv не найден в PATH.' ",
            "} ",
            "} ",
            "}"
        )
        .to_string(),
        #[cfg(not(target_os = "windows"))]
        PythonShellKind::PosixSh => concat!(
            "if ! command -v pip >/dev/null 2>&1; then ",
            "if python -m pip --version >/dev/null 2>&1; then ",
            "pip() { python -m pip \"$@\"; }; ",
            "printf '%s\n' 'pip-команда не найдена; команды pip будут выполняться через python -m pip.'; ",
            "else ",
            "if command -v uv >/dev/null 2>&1; then ",
            "pip() { uv pip \"$@\"; }; ",
            "printf '%s\n' 'pip не найден в Python env; команды pip будут выполняться через uv pip.'; ",
            "else ",
            "printf '%s\n' 'pip не найден в Python env; uv не найден в PATH.'; ",
            "fi; ",
            "fi; ",
            "fi"
        )
        .to_string(),
    }
}

#[must_use]
pub fn python_ready_probe_command(shell: PythonShellKind) -> String {
    match shell {
        #[cfg(target_os = "windows")]
        PythonShellKind::PowerShell => "Write-Output 'Python console is ready.'; python --version",
        #[cfg(not(target_os = "windows"))]
        PythonShellKind::PosixSh => "printf '%s\n' 'Python console is ready.'; python --version",
    }
    .to_string()
}

fn activation_root_exists(root: &Path) -> bool {
    if cfg!(target_os = "windows") {
        root.join("Scripts").join("python.exe").is_file()
    } else {
        root.join("bin").join("python").is_file() || root.join("bin").join("python3").is_file()
    }
}

fn conda_python_exists(app_root: &Path, env_root: &Path) -> bool {
    if cfg!(target_os = "windows") {
        let conda_root = app_root.join("installer_files").join("conda");
        let has_conda_runner = conda_root.join("condabin").join("conda.bat").is_file()
            || conda_root.join("_conda.exe").is_file();
        has_conda_runner && env_root.join("python.exe").is_file()
    } else {
        let conda_root = app_root.join("installer_files").join("conda");
        conda_root.join("bin").join("conda").is_file()
            && (env_root.join("bin").join("python").is_file()
                || env_root.join("bin").join("python3").is_file())
    }
}

fn resolve_standalone_python_executable(root_dir: &Path) -> Option<PathBuf> {
    let python_dir = root_dir.join("installer_files").join("python");
    if !python_dir.is_dir() {
        return None;
    }

    resolve_python_executable_in_dir(&python_dir).ok()
}

fn python_executable_candidates() -> &'static [&'static str] {
    if cfg!(target_os = "windows") {
        &[
            "python.exe",
            "bin/python.exe",
            "Scripts/python.exe",
            "python/python.exe",
        ]
    } else {
        &[
            "bin/python3",
            "bin/python",
            "python/bin/python3",
            "python/bin/python",
        ]
    }
}

fn virtual_env_scripts_dir(root: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        root.join("Scripts")
    } else {
        root.join("bin")
    }
}

fn conda_primary_bin(root: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        root.to_path_buf()
    } else {
        root.join("bin")
    }
}

fn conda_secondary_bin(root: &Path) -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        Some(root.join("Scripts"))
    } else {
        None
    }
}

fn shell_echo_command(shell: PythonShellKind, message: &str) -> String {
    match shell {
        #[cfg(target_os = "windows")]
        PythonShellKind::PowerShell => format!("Write-Output '{}'", powershell_escape_str(message)),
        #[cfg(not(target_os = "windows"))]
        PythonShellKind::PosixSh => format!("printf '%s\n' '{}'", sh_escape_str(message)),
    }
}

fn shell_set_path_command(shell: PythonShellKind, path: &Path) -> String {
    match shell {
        #[cfg(target_os = "windows")]
        PythonShellKind::PowerShell => {
            format!("$env:PATH = '{};' + $env:PATH", powershell_escape(path))
        }
        #[cfg(not(target_os = "windows"))]
        PythonShellKind::PosixSh => format!("export PATH='{}':\"$PATH\"", sh_escape(path)),
    }
}

fn shell_prepend_path_command(shell: PythonShellKind, path: &Path) -> String {
    shell_set_path_command(shell, path)
}

fn shell_set_env_command(shell: PythonShellKind, key: &str, value: &Path) -> String {
    match shell {
        #[cfg(target_os = "windows")]
        PythonShellKind::PowerShell => {
            format!("$env:{key} = '{}'", powershell_escape(value))
        }
        #[cfg(not(target_os = "windows"))]
        PythonShellKind::PosixSh => format!("export {key}='{}'", sh_escape(value)),
    }
}

#[cfg(target_os = "windows")]
fn powershell_escape(path: &Path) -> String {
    powershell_escape_str(&path.to_string_lossy())
}

#[cfg(target_os = "windows")]
fn powershell_escape_str(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(not(target_os = "windows"))]
fn sh_escape(path: &Path) -> String {
    sh_escape_str(&path.to_string_lossy())
}

#[cfg(not(target_os = "windows"))]
fn sh_escape_str(value: &str) -> String {
    value.replace('\'', "'\"'\"'")
}

#[cfg(target_os = "windows")]
fn apply_windows_no_window_inner(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn apply_windows_no_window_inner(_command: &mut Command) {}
