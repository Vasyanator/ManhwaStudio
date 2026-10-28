/*
File: crates/ms-sdcpp/src/platform_check.rs

Purpose:
Whether this host can load a build of the pinned sd.cpp release: the glibc floor and the
Vulkan loader on Linux, the VC++ runtime DLLs (with a version floor) and the Vulkan loader on
Windows. Answers with a typed, localized `RequirementProblem`.

Key items:
- RequirementProblem    : what is missing or too old
- check_requirements()  : the probe for one catalog row (worker thread only)
- VC_REDIST_URL         : Microsoft's permanent link to the current x64 VC++ redistributable
- glibc_satisfies() / version_satisfies() : the pure version comparisons

Notes:
- Linux: `gnu_get_libc_version()` (a non-glibc libc is `NotGlibc`); the Vulkan loader is
  probed with `dlopen("libvulkan.so.1", RTLD_LAZY | RTLD_LOCAL)` + `dlclose` — the same lookup
  the backend library's DT_NEEDED will do inside the worker.
- Windows: the five runtime DLLs the pinned DLLs import must exist in the system directory,
  `msvcp140.dll`'s file version must reach the row's floor, and `vulkan-1.dll` must exist there
  for the Vulkan build. Nothing is loaded into the app process.
- Probes touch the filesystem / the dynamic loader: never on the GUI thread.
*/

use crate::catalog::PackageRow;

/// Microsoft's permanent download link of the current x64 Visual C++ redistributable
/// (2015-2022 line), offered to the user when the runtime is missing or too old.
pub const VC_REDIST_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";

/// Why the host cannot run a build. `Display` is the localized message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequirementProblem {
    /// The Linux C library is not glibc (the release is built against glibc).
    #[error("{}", t!("sdcpp.requirement.not_glibc_error"))]
    NotGlibc,
    /// glibc `found` is older than `required` (major, minor).
    #[error("{}", tf!("sdcpp.requirement.glibc_too_old_error", found = found, required = format!("{}.{}", required.0, required.1)))]
    GlibcTooOld { found: String, required: (u32, u32) },
    /// These VC++ runtime DLLs are absent from the system directory (or their version is
    /// unreadable).
    #[error("{}", tf!("sdcpp.requirement.vc_runtime_missing_error", missing = missing.join(", "), url = VC_REDIST_URL))]
    VcRuntimeMissing { missing: Vec<String> },
    /// The VC++ runtime (`msvcp140.dll` file version `found`) is older than `required`.
    #[error("{}", tf!("sdcpp.requirement.vc_runtime_too_old_error", found = found, required = format!("{}.{}", required.0, required.1), url = VC_REDIST_URL))]
    VcRuntimeTooOld { found: String, required: (u16, u16) },
    /// The system Vulkan loader `library` cannot be found.
    #[error("{}", tf!("sdcpp.requirement.vulkan_loader_missing_error", library = library))]
    VulkanLoaderMissing { library: &'static str },
}

/// Whether a glibc version string (`"2.39"`, `"2.38.1"`) is at least `required` (major,
/// minor). An unparseable string is `false`.
#[must_use]
pub fn glibc_satisfies(version: &str, required: (u32, u32)) -> bool {
    let mut parts = version.trim().split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u32>().ok());
    match (major, minor) {
        (Some(major), Some(minor)) => (major, minor) >= required,
        _ => false,
    }
}

/// Whether a four-part file version (`major.minor.build.revision`) is at least `required`
/// (major, minor).
#[must_use]
pub fn version_satisfies(found: [u16; 4], required: (u16, u16)) -> bool {
    (found[0], found[1]) >= required
}

/// Checks the host requirements of `row` on this machine: the glibc floor (`min_glibc`), the
/// VC++ runtime (`min_vc_runtime`) and the Vulkan loader (`needs_vulkan_loader`), in that
/// order; the first problem found is returned. On an OS the catalog has no row for, nothing is
/// checked.
///
/// # Errors
/// The first [`RequirementProblem`] found.
///
/// # Threading
/// Filesystem / dynamic-loader probes: worker thread only.
pub fn check_requirements(row: &PackageRow) -> Result<(), RequirementProblem> {
    imp::check(row)
}

#[cfg(target_os = "linux")]
mod imp {
    use ms_log::runtime_log;

    use super::{RequirementProblem, glibc_satisfies};
    use crate::catalog::PackageRow;

    /// The soname of the system Vulkan loader the Vulkan backend links against.
    const VULKAN_LOADER: &str = "libvulkan.so.1";

    pub(super) fn check(row: &PackageRow) -> Result<(), RequirementProblem> {
        if let Some(required) = row.min_glibc {
            let found = glibc_version().ok_or(RequirementProblem::NotGlibc)?;
            if !glibc_satisfies(&found, required) {
                return Err(RequirementProblem::GlibcTooOld { found, required });
            }
        }
        if row.needs_vulkan_loader && !vulkan_loader_present() {
            return Err(RequirementProblem::VulkanLoaderMissing { library: VULKAN_LOADER });
        }
        Ok(())
    }

    /// The running glibc's version string; `None` on a non-glibc libc.
    #[cfg(target_env = "gnu")]
    fn glibc_version() -> Option<String> {
        // SAFETY: `gnu_get_libc_version` takes no arguments and returns a pointer to a static,
        // NUL-terminated string owned by glibc for the whole process lifetime.
        let raw = unsafe { libc::gnu_get_libc_version() };
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` is non-null and points to glibc's static NUL-terminated version string.
        let version = unsafe { std::ffi::CStr::from_ptr(raw) };
        Some(version.to_string_lossy().into_owned())
    }

    /// The running libc is not glibc: the release's libraries cannot load.
    #[cfg(not(target_env = "gnu"))]
    fn glibc_version() -> Option<String> {
        None
    }

    /// Whether the dynamic loader finds the Vulkan loader library (opened and closed again).
    fn vulkan_loader_present() -> bool {
        let name = c"libvulkan.so.1";
        // SAFETY: `name` is a valid NUL-terminated string; `dlopen` has no other preconditions.
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        if handle.is_null() {
            runtime_log::log_info(format!("[sdcpp] {VULKAN_LOADER} not found by the dynamic loader"));
            return false;
        }
        // SAFETY: `handle` was returned non-null by the `dlopen` above and is closed exactly once.
        let closed = unsafe { libc::dlclose(handle) };
        if closed != 0 {
            runtime_log::log_warn(format!("[sdcpp] dlclose({VULKAN_LOADER}) returned {closed}"));
        }
        true
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::path::{Path, PathBuf};

    use ms_log::runtime_log;

    use super::{RequirementProblem, version_satisfies};
    use crate::catalog::PackageRow;

    /// The VC++ runtime DLLs the pinned DLLs import (none is bundled in the release zips).
    const VC_RUNTIME_DLLS: [&str; 5] = ["vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll", "msvcp140_codecvt_ids.dll", "vcomp140.dll"];
    /// The DLL whose file version stands for the installed runtime's version.
    const VERSIONED_DLL: &str = "msvcp140.dll";
    /// The system Vulkan loader the Vulkan backend imports.
    const VULKAN_LOADER: &str = "vulkan-1.dll";

    pub(super) fn check(row: &PackageRow) -> Result<(), RequirementProblem> {
        let system = system_directory();
        if let Some(required) = row.min_vc_runtime {
            let Some(system) = system.as_deref() else {
                return Err(RequirementProblem::VcRuntimeMissing { missing: VC_RUNTIME_DLLS.iter().map(|dll| (*dll).to_owned()).collect() });
            };
            let missing: Vec<String> = VC_RUNTIME_DLLS.iter().filter(|dll| !system.join(dll).is_file()).map(|dll| (*dll).to_owned()).collect();
            if !missing.is_empty() {
                return Err(RequirementProblem::VcRuntimeMissing { missing });
            }
            // An unreadable version resource is reported as a missing runtime: reinstalling the
            // redistributable fixes both cases.
            let Some(found) = file_version(&system.join(VERSIONED_DLL)) else {
                return Err(RequirementProblem::VcRuntimeMissing { missing: vec![VERSIONED_DLL.to_owned()] });
            };
            if !version_satisfies(found, required) {
                return Err(RequirementProblem::VcRuntimeTooOld { found: format!("{}.{}.{}.{}", found[0], found[1], found[2], found[3]), required });
            }
        }
        if row.needs_vulkan_loader && !system.is_some_and(|system| system.join(VULKAN_LOADER).is_file()) {
            return Err(RequirementProblem::VulkanLoaderMissing { library: VULKAN_LOADER });
        }
        Ok(())
    }

    /// The Windows system directory (`C:\Windows\System32`), or `None` when it cannot be read.
    fn system_directory() -> Option<PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

        let mut buffer = vec![0_u16; 260];
        loop {
            let capacity = u32::try_from(buffer.len()).ok()?;
            // SAFETY: `buffer` is a writable UTF-16 buffer of exactly `capacity` elements.
            let written = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), capacity) };
            let written = usize::try_from(written).ok()?;
            if written == 0 {
                runtime_log::log_warn(format!("[sdcpp] GetSystemDirectoryW failed: {}", std::io::Error::last_os_error()));
                return None;
            }
            // On a too-small buffer the call returns the required size INCLUDING the NUL.
            if written >= buffer.len() {
                buffer.resize(written, 0);
                continue;
            }
            return Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..written])));
        }
    }

    /// The fixed file version (`major.minor.build.revision`) of the DLL at `path`, or `None`
    /// when it has no readable version resource (logged).
    fn file_version(path: &Path) -> Option<[u16; 4]> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW};

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut handle = 0_u32;
        // SAFETY: `wide` is a NUL-terminated UTF-16 path; `handle` is a valid out pointer.
        let size = unsafe { GetFileVersionInfoSizeW(wide.as_ptr(), &raw mut handle) };
        if size == 0 {
            runtime_log::log_warn(format!("[sdcpp] no version resource in {}: {}", path.display(), std::io::Error::last_os_error()));
            return None;
        }
        let mut data = vec![0_u8; usize::try_from(size).ok()?];
        // SAFETY: `data` is a writable buffer of exactly `size` bytes; `wide` as above.
        let ok = unsafe { GetFileVersionInfoW(wide.as_ptr(), 0, size, data.as_mut_ptr().cast()) };
        if ok == 0 {
            runtime_log::log_warn(format!("[sdcpp] cannot read the version resource of {}: {}", path.display(), std::io::Error::last_os_error()));
            return None;
        }
        let root: [u16; 2] = [u16::from(b'\\'), 0];
        let mut info: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut info_len = 0_u32;
        // SAFETY: `data` holds the version block filled above and outlives `info`; `root` is
        // the NUL-terminated sub-block "\" that selects the VS_FIXEDFILEINFO.
        let ok = unsafe { VerQueryValueW(data.as_ptr().cast(), root.as_ptr(), &raw mut info, &raw mut info_len) };
        let fits = usize::try_from(info_len).is_ok_and(|len| len >= std::mem::size_of::<VS_FIXEDFILEINFO>());
        if ok == 0 || info.is_null() || !fits {
            runtime_log::log_warn(format!("[sdcpp] {} has no fixed file version", path.display()));
            return None;
        }
        // SAFETY: `VerQueryValueW` succeeded for "\", so `info` points inside `data` at a
        // VS_FIXEDFILEINFO of at least `size_of` bytes (checked above); it may be unaligned.
        let fixed = unsafe { std::ptr::read_unaligned(info.cast::<VS_FIXEDFILEINFO>()) };
        // Each half of the two 32-bit words is one 16-bit version part; the masks make the
        // narrowing exact.
        let high = |word: u32| u16::try_from(word >> 16).unwrap_or(u16::MAX);
        let low = |word: u32| u16::try_from(word & 0xFFFF).unwrap_or(u16::MAX);
        Some([high(fixed.dwFileVersionMS), low(fixed.dwFileVersionMS), high(fixed.dwFileVersionLS), low(fixed.dwFileVersionLS)])
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod imp {
    use super::RequirementProblem;
    use crate::catalog::PackageRow;

    /// No catalog row exists here, so there is nothing to check.
    pub(super) fn check(_row: &PackageRow) -> Result<(), RequirementProblem> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glibc_versions_compare_numerically() {
        assert!(glibc_satisfies("2.38", (2, 38)));
        assert!(glibc_satisfies("2.39", (2, 38)));
        assert!(glibc_satisfies("2.40.1", (2, 38)));
        assert!(glibc_satisfies("3.0", (2, 38)));
        assert!(!glibc_satisfies("2.37", (2, 38)));
        assert!(!glibc_satisfies("2.9", (2, 38)));
        assert!(!glibc_satisfies("", (2, 38)));
        assert!(!glibc_satisfies("musl", (2, 38)));
        assert!(!glibc_satisfies("2", (2, 38)));
    }

    #[test]
    fn file_versions_compare_on_major_minor() {
        assert!(version_satisfies([14, 44, 35211, 0], (14, 44)));
        assert!(version_satisfies([14, 50, 0, 0], (14, 44)));
        assert!(version_satisfies([15, 0, 0, 0], (14, 44)));
        assert!(!version_satisfies([14, 29, 30133, 0], (14, 44)));
        assert!(!version_satisfies([13, 99, 0, 0], (14, 44)));
    }

    #[test]
    fn every_problem_displays_text() {
        let problems = [
            RequirementProblem::NotGlibc,
            RequirementProblem::GlibcTooOld { found: "2.35".into(), required: (2, 38) },
            RequirementProblem::VcRuntimeMissing { missing: vec!["vcomp140.dll".into()] },
            RequirementProblem::VcRuntimeTooOld { found: "14.29.30133.0".into(), required: (14, 44) },
            RequirementProblem::VulkanLoaderMissing { library: "libvulkan.so.1" },
        ];
        for problem in problems {
            assert!(!problem.to_string().trim().is_empty(), "{problem:?}");
        }
    }

    #[test]
    fn this_machine_probe_returns_without_panicking() {
        // The answer depends on the host; the probe must simply complete and, if it fails,
        // name a real problem.
        for row in crate::catalog::ROWS.iter().filter(|row| row.os == std::env::consts::OS) {
            if let Err(problem) = check_requirements(row) {
                assert_ne!(problem.to_string(), "");
            }
        }
    }
}
