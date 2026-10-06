/*
File: crates/ms-os-integration/src/windows/elevation.rs

Purpose:
Windows elevation: whether this process runs elevated, the fire-and-forget UAC relaunch of the
running executable, and the elevated system-registration helper round trip — the unelevated
side (`apply_elevated`) and the elevated side (`run_elevated_helper`).

Key functions:
- `is_running_elevated()`
- `relaunch_self_elevated_with_args()`
- `apply_elevated()`: `ShellExecuteExW("runas")` of this executable with
  `--system-registration-apply <tokens> --system-registration-result <file>`, wait on its
  process handle, read and delete its result file.
- `run_elevated_helper()`: the helper's entry point (called by `src/main.rs`); runs the decoded
  `Machine`-scope actions in-process and writes the result file. NEVER elevates.

Notes:
`relaunch_self_elevated_with_args` returns once the elevated process has been started and
carries no result channel back; its loop guards (`--continue-*` flags) are the caller's
responsibility. The helper's loop guard is structural: it has no elevation code, and when it
is not elevated it fails every action with `NotElevated` (exit 1). The protocol (tokens,
result JSON, exit codes, result-path rule) is owned by `crate::actions`.
*/

#[cfg(target_os = "windows")]
use std::env;
#[cfg(target_os = "windows")]
use std::path::Path;
#[cfg(target_os = "windows")]
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use super::to_wide;
#[cfg(target_os = "windows")]
use crate::IntegrationError;
#[cfg(target_os = "windows")]
use crate::actions::{
    ActionError, ActionRequest, ElevationError, HelperOutcomes, SYSTEM_REGISTRATION_APPLY_FLAG, SYSTEM_REGISTRATION_RESULT_FLAG, discard_result_file,
    encode_actions, new_result_path, read_helper_result, run_helper_protocol,
};

/// True when the process token is elevated (`TokenElevation`). Any failure to query the token
/// reads as "not elevated".
#[cfg(target_os = "windows")]
pub fn is_running_elevated() -> bool {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
        let mut out_size: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut out_size,
        ) != 0;
        // A failed close of a query-only token handle leaks nothing the process relies on and
        // leaves no decision to change: the elevation answer above is already complete.
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Starts the running executable elevated (`ShellExecuteW` verb `runas`) with the raw
/// command-line `args`, in working directory `root_dir`. Returns once the launch was accepted;
/// the elevated process runs on its own.
///
/// # Errors
/// `DetermineExe` when the running executable is unknown; `UacDenied` when `ShellExecuteW`
/// reports failure (the user declined the UAC prompt, or the launch failed).
#[cfg(target_os = "windows")]
pub fn relaunch_self_elevated_with_args(root_dir: &Path, args: &str) -> Result<(), IntegrationError> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe = env::current_exe().map_err(|e| IntegrationError::DetermineExe { source: e })?;
    let verb = to_wide("runas");
    let exe_w = to_wide(exe.to_string_lossy().as_ref());
    let args_w = to_wide(args);
    let root_dir_w = to_wide(root_dir.to_string_lossy().as_ref());
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe_w.as_ptr(),
            args_w.as_ptr(),
            root_dir_w.as_ptr(),
            SW_SHOWNORMAL,
        )
    };
    if (result as isize) <= 32 {
        return Err(IntegrationError::UacDenied);
    }
    Ok(())
}

/// Longest single wait on the helper process; the total wait is bounded by the caller's timeout.
#[cfg(target_os = "windows")]
const HELPER_WAIT_SLICE: Duration = Duration::from_millis(500);

/// Closes a process handle on drop.
#[cfg(target_os = "windows")]
struct ProcessHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
        // SAFETY: the handle came from ShellExecuteExW (SEE_MASK_NOCLOSEPROCESS) and is closed
        // exactly once, here.
        if unsafe { CloseHandle(self.0) } == 0 {
            // SAFETY: plain thread-local error read right after the failed call.
            let code = unsafe { GetLastError() };
            ms_log::runtime_log::log_warn(format!("[os-registration] could not close the helper process handle (Windows error {code})"));
        }
    }
}

/// Runs the `Machine`-scope `requests` in an elevated copy of this executable and returns each
/// action's outcome, in request order. Shows the UAC prompt (`ShellExecuteExW` verb `runas`,
/// `SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC`), waits on the helper process in slices of at most
/// 500 ms up to `timeout` — counted from the helper's start, so the time the user spends on the
/// UAC prompt is not part of it — then reads its exit code and its result file (unique name under
/// `std::env::temp_dir()`, deleted after reading). The helper starts in this copy's program root
/// (`copy_identity::current_program_root`). Blocking: run it on a worker thread, never the GUI
/// thread.
///
/// # Errors
/// `InvalidBatch` (empty batch or a `User`-scope request — those run in-process), `Declined`
/// (UAC prompt declined: nothing changed), `LaunchFailed` / `WaitFailed` (Win32 code), `TimedOut`
/// (outcome unknown; the helper may still finish), `HelperFailed` (an exit code other than 0/1:
/// no result; exit 2 means no action ran), `ResultUnreadable` (missing or invalid result file).
/// Every error after the launch deletes the result file if it exists.
#[cfg(target_os = "windows")]
pub fn apply_elevated(requests: &[ActionRequest], timeout: Duration) -> Result<HelperOutcomes, ElevationError> {
    use windows_sys::Win32::Foundation::{ERROR_CANCELLED, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    use super::values::quote_windows_arg;

    let tokens = encode_actions(requests).map_err(ElevationError::InvalidBatch)?;
    let exe = env::current_exe().map_err(|error| ElevationError::LaunchFailed { code: io_error_code(&error) })?;
    // The helper starts in this copy's program root (the repository root of a repository
    // build), so its runtime-root resolution — the shared user config it renders messages
    // from — lands on this copy, as for a menu launch.
    let program_root = crate::copy_identity::current_program_root(&exe);
    let result_path = new_result_path(&env::temp_dir());
    let params = format!(
        "{SYSTEM_REGISTRATION_APPLY_FLAG} {tokens} {SYSTEM_REGISTRATION_RESULT_FLAG} {}",
        quote_windows_arg(&result_path.to_string_lossy())
    );
    let verb = to_wide("runas");
    let exe_w = to_wide(&exe.to_string_lossy());
    let params_w = to_wide(&params);
    let dir_w = to_wide(&program_root.to_string_lossy());
    let struct_size = u32::try_from(std::mem::size_of::<SHELLEXECUTEINFOW>()).map_err(|_| ElevationError::LaunchFailed { code: ERROR_INVALID_PARAMETER })?;
    // SAFETY: SHELLEXECUTEINFOW is a plain C struct; all-zero is its documented "unset" state.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = struct_size;
    // NOCLOSEPROCESS: hand back the process handle to wait on; NOASYNC: the launch is complete
    // when the call returns (this worker thread has no message loop).
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = exe_w.as_ptr();
    info.lpParameters = params_w.as_ptr();
    info.lpDirectory = dir_w.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    ms_log::runtime_log::log_info(format!("[os-registration] starting the elevated helper for {tokens}"));
    // SAFETY: `info` is fully initialized; every string pointer is NUL-terminated and outlives
    // the call.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        // SAFETY: plain thread-local error read right after the failed call.
        let code = unsafe { GetLastError() };
        if code == ERROR_CANCELLED {
            ms_log::runtime_log::log_info("[os-registration] the UAC prompt was declined; nothing changed");
            return Err(ElevationError::Declined);
        }
        ms_log::runtime_log::log_error(format!("[os-registration] the elevated helper could not be started (Windows error {code})"));
        return Err(ElevationError::LaunchFailed { code });
    }
    if info.hProcess.is_null() {
        // The shell reported success without a process (e.g. the launch was routed through
        // DDE): there is nothing to wait on and no result will be known.
        return Err(ElevationError::LaunchFailed { code: ERROR_INVALID_HANDLE });
    }
    let process = ProcessHandle(info.hProcess);
    // NOASYNC makes ShellExecuteExW return only after the UAC prompt was answered, so the
    // deadline counts the helper's run, not the user's reading time.
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            ms_log::runtime_log::log_error(format!("[os-registration] the elevated helper did not finish within {timeout:?}"));
            // The helper may still finish and write the file; a later stray file is harmless.
            discard_result_file(&result_path);
            return Err(ElevationError::TimedOut);
        }
        // The slice is at most 500 ms, so the millisecond count always fits a u32.
        let slice_ms = u32::try_from(remaining.min(HELPER_WAIT_SLICE).as_millis()).unwrap_or(u32::MAX);
        // SAFETY: `process.0` is a live process handle with SYNCHRONIZE access.
        match unsafe { WaitForSingleObject(process.0, slice_ms) } {
            WAIT_OBJECT_0 => break,
            WAIT_TIMEOUT => {}
            _ => {
                // SAFETY: plain thread-local error read right after the failed wait.
                let code = unsafe { GetLastError() };
                ms_log::runtime_log::log_error(format!("[os-registration] waiting for the elevated helper failed (Windows error {code})"));
                // The helper may still finish and write the file; delete whatever is there now.
                discard_result_file(&result_path);
                return Err(ElevationError::WaitFailed { code });
            }
        }
    }
    let mut exit_code: u32 = 0;
    // SAFETY: `process.0` is a live process handle; `exit_code` is a valid out-pointer.
    if unsafe { GetExitCodeProcess(process.0, &mut exit_code) } == 0 {
        // SAFETY: plain thread-local error read right after the failed call.
        let code = unsafe { GetLastError() };
        discard_result_file(&result_path);
        return Err(ElevationError::WaitFailed { code });
    }
    drop(process);
    ms_log::runtime_log::log_info(format!("[os-registration] the elevated helper exited with code {exit_code}"));
    match i32::try_from(exit_code) {
        Ok(crate::actions::HELPER_EXIT_ALL_OK | crate::actions::HELPER_EXIT_SOME_FAILED) => read_helper_result(&result_path, requests),
        _ => {
            discard_result_file(&result_path);
            Err(ElevationError::HelperFailed { exit_code })
        }
    }
}

/// Win32 code of an I/O error (0 when it carries none). Win32 codes are non-negative; the
/// bit-exact reinterpretation keeps any value intact.
#[cfg(target_os = "windows")]
fn io_error_code(error: &std::io::Error) -> u32 {
    error.raw_os_error().map_or(0, |code| u32::from_ne_bytes(code.to_ne_bytes()))
}

/// The elevated helper: decodes `actions` (`--system-registration-apply`), creates
/// `result_file` (`--system-registration-result`), runs the actions in-process for the running
/// copy (`version_core` = this build's version, for `DisplayVersion`), writes their outcomes into
/// the already-open file and returns the process exit code ([`crate::actions::HELPER_EXIT_ALL_OK`],
/// `HELPER_EXIT_SOME_FAILED`, `HELPER_EXIT_USAGE`). The ordering is
/// [`crate::actions::run_helper_protocol`]'s.
///
/// Never elevates and never relaunches: when the process is not elevated every action fails with
/// `NotElevated` (exit 1), so a misrouted launch cannot loop. Refuses a result path that is not
/// an absolute `manhwastudio-sysreg-*.json` file name, one that already exists or cannot be
/// created, and any `User`-scope token (exit 2: no action ran, no result file). The caller
/// installs the UI locale first, so the failure messages are localized.
#[cfg(target_os = "windows")]
#[must_use]
pub fn run_elevated_helper(actions: &str, result_file: &Path, version_core: &str) -> i32 {
    run_helper_protocol(actions, result_file, |requests| {
        if !is_running_elevated() {
            ms_log::runtime_log::log_warn("[os-registration] helper is not elevated; no action runs");
            let failure = ActionError::NotElevated.failure();
            return requests.iter().map(|&request| (request, Err(failure.clone()))).collect();
        }
        match crate::CopyIdentity::current(Some(version_core)) {
            Ok(identity) => crate::actions::apply(&identity, requests)
                .into_iter()
                .map(|(request, outcome)| (request, outcome.map_err(|error| error.failure())))
                .collect(),
            Err(error) => {
                let failure = ActionError::from(error).failure();
                ms_log::runtime_log::log_error(format!("[os-registration] helper: {}", failure.detail));
                requests.iter().map(|&request| (request, Err(failure.clone()))).collect()
            }
        }
    })
}
