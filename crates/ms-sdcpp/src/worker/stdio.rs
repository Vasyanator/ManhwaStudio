/*
File: crates/ms-sdcpp/src/worker/stdio.rs

Purpose:
The worker process's OS setup before any sd.cpp library is loaded: take the ORIGINAL stdout
as the private frame channel and point the process's stdout at stderr, so a stray `printf`
of the library (or of ggml) can never corrupt a frame; make the worker die with the app
(Linux); point the DLL search at the package directory (Windows).

Key functions:
- take_frame_stdout()    : the frame writer; fd 1 / STD_OUTPUT_HANDLE now lead to stderr
- die_with_parent()      : Linux `prctl(PR_SET_PDEATHSIG, SIGKILL)`, a no-op elsewhere
- prepare_library_dir()  : chdir to the package; Windows also `SetDllDirectoryW`

Notes:
- Windows: the MSVC-built DLLs bring the UCRT, which initializes its stdio from the CURRENT
  standard handles when it is first loaded, i.e. after the redirection here.
- All of it runs on the worker's main thread before the reader thread starts.
*/

use std::fs::File;
use std::io;
use std::path::Path;

/// Returns a writer on the process's original stdout and redirects stdout to stderr.
///
/// # Errors
/// The OS error of `dup` / `dup2` (Unix) or `GetStdHandle` / `DuplicateHandle` /
/// `SetStdHandle` (Windows).
pub(crate) fn take_frame_stdout() -> io::Result<File> {
    imp::take_frame_stdout()
}

/// Asks the kernel to kill this process when its parent (the app) dies (Linux), then closes
/// the race of an app that died BEFORE that request: `false` when the current parent is not
/// the app whose pid `expected_parent` names (it was reparented), so the caller must exit.
/// Without a pid (an older launcher) nothing is checked.
pub(crate) fn die_with_parent(expected_parent: Option<u32>) -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `prctl(PR_SET_PDEATHSIG, sig)` takes plain integers and has no memory effects.
        let result = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
        if result != 0 {
            eprintln!("[sdcpp-worker] prctl(PR_SET_PDEATHSIG) failed: {}", io::Error::last_os_error());
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: `getppid` has no preconditions.
        let actual = unsafe { libc::getppid() };
        let actual = u32::try_from(actual).unwrap_or(0);
        if !parent_matches(expected_parent, actual) {
            eprintln!("[sdcpp-worker] the app (pid {expected_parent:?}) is gone (parent is now {actual}); exiting");
            return false;
        }
    }
    #[cfg(not(unix))]
    let _ = expected_parent;
    true
}

/// Whether the current parent pid `actual` is the expected one (`None` = not checked).
#[cfg(any(unix, test))]
pub(crate) fn parent_matches(expected: Option<u32>, actual: u32) -> bool {
    expected.is_none_or(|expected| expected == actual)
}

/// Makes `dir` the working directory and (Windows) the DLL directory, so the libraries'
/// dependencies resolve inside the package.
///
/// # Errors
/// The OS error of `chdir` or `SetDllDirectoryW`.
pub(crate) fn prepare_library_dir(dir: &Path) -> io::Result<()> {
    std::env::set_current_dir(dir)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call.
        let ok = unsafe { windows_sys::Win32::System::LibraryLoader::SetDllDirectoryW(wide.as_ptr()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::fd::FromRawFd;

    pub(super) fn take_frame_stdout() -> io::Result<File> {
        // SAFETY: `dup(1)` duplicates the process's stdout descriptor; no memory is touched.
        let frames = unsafe { libc::dup(libc::STDOUT_FILENO) };
        if frames < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both descriptors are open (stderr is inherited from the app); `dup2`
        // atomically replaces fd 1 with a copy of fd 2.
        if unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) } < 0 {
            let err = io::Error::last_os_error();
            // SAFETY: `frames` is the descriptor `dup` returned above and is closed once.
            unsafe { libc::close(frames) };
            return Err(err);
        }
        // SAFETY: `frames` is a freshly duplicated descriptor owned by nothing else.
        Ok(unsafe { File::from_raw_fd(frames) })
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};

    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub(super) fn take_frame_stdout() -> io::Result<File> {
        // SAFETY: plain queries of the process's standard handles.
        let (out, err) = unsafe { (GetStdHandle(STD_OUTPUT_HANDLE), GetStdHandle(STD_ERROR_HANDLE)) };
        if out.is_null() || out == INVALID_HANDLE_VALUE || err.is_null() || err == INVALID_HANDLE_VALUE {
            return Err(io::Error::other("the worker has no stdout / stderr handle"));
        }
        let mut frames: HANDLE = std::ptr::null_mut();
        // SAFETY: `out` is a valid handle of this process; `frames` is a valid out pointer.
        let ok = unsafe { DuplicateHandle(GetCurrentProcess(), out, GetCurrentProcess(), &raw mut frames, 0, 0, DUPLICATE_SAME_ACCESS) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // Owned at once, so every error path below closes the duplicate.
        // SAFETY: `frames` is a freshly duplicated handle owned by nothing else.
        let frames = unsafe { OwnedHandle::from_raw_handle(frames) };
        // SAFETY: `err` is this process's valid stderr handle.
        if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, err) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(File::from(frames))
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::fs::File;
    use std::io;

    pub(super) fn take_frame_stdout() -> io::Result<File> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "the sd.cpp worker is not supported on this platform"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parent_check_compares_pids() {
        assert!(parent_matches(None, 1));
        assert!(parent_matches(Some(4242), 4242));
        assert!(!parent_matches(Some(4242), 1), "reparented to init");
    }

    #[cfg(unix)]
    #[test]
    fn this_process_is_its_parents_child() {
        // SAFETY: `getppid` has no preconditions.
        let parent = u32::try_from(unsafe { libc::getppid() }).expect("pid fits");
        assert!(parent_matches(Some(parent), parent));
    }
}
