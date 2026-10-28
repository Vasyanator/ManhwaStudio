/*
File: crates/ms-sdcpp/src/ffi/api.rs

Purpose:
Loads an installed sd.cpp package into the CURRENT process (the worker process, never the
app) and resolves every C entry point the worker calls: first ggml's dispatcher, whose
`ggml_backend_load_all_from_path(<package dir>)` registers the backends shipped next to it,
then the sd.cpp library; then checks that `sd_commit()` is the pinned release.

Key items:
- SdApi            : the loaded libraries and the resolved function pointers
- SdApi::load()    : the load sequence and ABI check
- SdApi::commit / version / physical_cores / list_devices : safe wrappers of the info calls
- classify_load_error() : maps a loader message onto `LoadProblem`
- parse_device_list()   : the `name<TAB>description` lines of `sd_list_devices`

Notes:
- Result memory is freed only by the library's own free functions
  (`free_sd_image_generation_result`, `free_sd_ctx`): it may use another CRT on Windows.
- `sd_*_to_str` / `*_params_to_str` are never called (their strings would need our free).
- Windows: each library is loaded with `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR |
  LOAD_LIBRARY_SEARCH_DEFAULT_DIRS`, so its own directory (ggml-base.dll) and System32 (the
  VC++ runtime, vulkan-1.dll) are searched and nothing else. Linux: the libraries carry
  `RUNPATH=$ORIGIN`, so a full-path load finds their siblings.
*/

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;

use crate::catalog::{PINNED, PackageRow};
use crate::error::{LoadProblem, SdCppError};
use crate::request::DeviceInfo;

use super::types::{SampleMethod, Scheduler, SdCancelMode, SdCtx, SdCtxParams, SdImageGenerationResult, SdImgGenParams, SdLogCb, SdProgressCb};

/// The loaded libraries and every resolved entry point. The function pointers stay valid as
/// long as the libraries are loaded, i.e. as long as this value lives (the library handles are
/// dropped last, see the field order).
pub(crate) struct SdApi {
    pub(crate) set_log_callback: unsafe extern "C" fn(SdLogCb, *mut c_void),
    pub(crate) set_progress_callback: unsafe extern "C" fn(SdProgressCb, *mut c_void),
    pub(crate) ctx_params_init: unsafe extern "C" fn(*mut SdCtxParams),
    pub(crate) new_ctx: unsafe extern "C" fn(*const SdCtxParams) -> *mut SdCtx,
    pub(crate) free_ctx: unsafe extern "C" fn(*mut SdCtx),
    pub(crate) img_gen_params_init: unsafe extern "C" fn(*mut SdImgGenParams),
    pub(crate) generate_image: unsafe extern "C" fn(*mut SdCtx, *const SdImgGenParams, *mut *mut SdImageGenerationResult) -> bool,
    pub(crate) free_image_generation_result: unsafe extern "C" fn(*mut SdImageGenerationResult),
    pub(crate) cancel_generation: unsafe extern "C" fn(*mut SdCtx, SdCancelMode),
    pub(crate) default_sample_method: unsafe extern "C" fn(*const SdCtx) -> SampleMethod,
    pub(crate) default_scheduler: unsafe extern "C" fn(*const SdCtx, SampleMethod) -> Scheduler,
    pub(crate) str_to_sample_method: unsafe extern "C" fn(*const c_char) -> SampleMethod,
    pub(crate) str_to_scheduler: unsafe extern "C" fn(*const c_char) -> Scheduler,
    list_devices: unsafe extern "C" fn(*mut c_char, usize) -> usize,
    sd_commit: unsafe extern "C" fn() -> *const c_char,
    sd_version: unsafe extern "C" fn() -> *const c_char,
    num_physical_cores: unsafe extern "C" fn() -> i32,
    // Declared after the function pointers so they are dropped (unloaded) last; sd.cpp before
    // ggml because it depends on it.
    _lib_sd: libloading::Library,
    _lib_ggml: libloading::Library,
}

/// Opens the library at `path` with this platform's search rules (see the file header).
///
/// # Safety
/// Loading a library runs its initializers; `path` must be a library of the pinned package.
unsafe fn open_library(path: &Path) -> Result<libloading::Library, libloading::Error> {
    #[cfg(windows)]
    {
        use libloading::os::windows::{LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, Library};
        // SAFETY: forwarded from the caller's contract.
        unsafe { Library::load_with_flags(path, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) }.map(libloading::Library::from)
    }
    #[cfg(not(windows))]
    {
        // SAFETY: forwarded from the caller's contract.
        unsafe { libloading::Library::new(path) }
    }
}

/// Maps a failed library load onto `LibraryLoad`.
fn load_error(path: &Path, err: &libloading::Error) -> SdCppError {
    let detail = format!("{}: {err}", path.display());
    SdCppError::LibraryLoad { problem: classify_load_error(&detail), detail }
}

/// Resolves the symbol `name` (NUL-terminated) of type `T` from `lib` and copies it out.
///
/// # Safety
/// `T` must be the exact function-pointer type of the C declaration of `name` in the vendored
/// header, and the copied pointer must not outlive `lib`.
unsafe fn symbol<T: Copy>(lib: &libloading::Library, name: &[u8]) -> Result<T, SdCppError> {
    // SAFETY: the caller guarantees `T` matches the C declaration.
    let symbol = unsafe { lib.get::<T>(name) }.map_err(|err| SdCppError::LibraryLoad {
        problem: LoadProblem::Other,
        detail: format!("missing symbol {}: {err}", String::from_utf8_lossy(name.strip_suffix(b"\0").unwrap_or(name))),
    })?;
    Ok(*symbol)
}

/// A NUL-terminated copy of `path` in the encoding ggml expects (bytes on Unix, UTF-8 on
/// Windows, where ggml converts with `u8path`).
fn path_c_string(path: &Path) -> Result<CString, SdCppError> {
    let invalid = || SdCppError::LibraryLoad { problem: LoadProblem::Other, detail: format!("package path is not representable for ggml: {}", path.display()) };
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = path.to_str().ok_or_else(invalid)?.as_bytes().to_vec();
    CString::new(bytes).map_err(|_| invalid())
}

/// Copies a C string the library owns (static storage) into a Rust string.
///
/// # Safety
/// `raw` must be null or point to a NUL-terminated string valid for the duration of the call.
unsafe fn owned_c_string(raw: *const c_char) -> String {
    if raw.is_null() {
        return String::new();
    }
    // SAFETY: non-null and NUL-terminated per the caller's contract.
    unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned()
}

impl SdApi {
    /// Loads the package in `dir` described by `row`: ggml's dispatcher first, registering
    /// every backend library in `dir` (`ggml_backend_load_all_from_path`), then the sd.cpp
    /// library; resolves every entry point and checks `sd_commit()` against the pinned
    /// release.
    ///
    /// # Errors
    /// `LibraryLoad` (a library or symbol could not be loaded; `MissingSystemLibrary` when the
    /// loader names a missing system library), `AbiMismatch` (another sd.cpp build).
    ///
    /// # Safety
    /// Runs the libraries' initializers in this process and trusts the vendored header's
    /// declarations; call it only in the sd.cpp worker process, on a package installed by
    /// `install` (verified pins), and at most once per process (ggml's backend registry is
    /// process-global).
    pub(crate) unsafe fn load(dir: &Path, row: &PackageRow) -> Result<Self, SdCppError> {
        let ggml_path = dir.join(row.ggml_library);
        // SAFETY: a library of the verified pinned package (caller's contract).
        let lib_ggml = unsafe { open_library(&ggml_path) }.map_err(|err| load_error(&ggml_path, &err))?;
        // SAFETY: `void ggml_backend_load_all_from_path(const char * dir_path)` in ggml-backend.h.
        let load_all: unsafe extern "C" fn(*const c_char) = unsafe { symbol(&lib_ggml, b"ggml_backend_load_all_from_path\0") }?;
        let dir_c = path_c_string(dir)?;
        // SAFETY: `dir_c` is a valid NUL-terminated path that outlives the call; the function
        // only reads it.
        unsafe { load_all(dir_c.as_ptr()) };

        let sd_path = dir.join(row.sd_library);
        // SAFETY: as above.
        let lib_sd = unsafe { open_library(&sd_path) }.map_err(|err| load_error(&sd_path, &err))?;
        // SAFETY (every `symbol` call below): each type is the exact function-pointer type of
        // the declaration of the same name in vendor/stable-diffusion.h (pinned by sha256), and
        // the pointers are stored next to `lib_sd`, which outlives them.
        let api = unsafe {
            Self {
                set_log_callback: symbol(&lib_sd, b"sd_set_log_callback\0")?,
                set_progress_callback: symbol(&lib_sd, b"sd_set_progress_callback\0")?,
                ctx_params_init: symbol(&lib_sd, b"sd_ctx_params_init\0")?,
                new_ctx: symbol(&lib_sd, b"new_sd_ctx\0")?,
                free_ctx: symbol(&lib_sd, b"free_sd_ctx\0")?,
                img_gen_params_init: symbol(&lib_sd, b"sd_img_gen_params_init\0")?,
                generate_image: symbol(&lib_sd, b"generate_image\0")?,
                free_image_generation_result: symbol(&lib_sd, b"free_sd_image_generation_result\0")?,
                cancel_generation: symbol(&lib_sd, b"sd_cancel_generation\0")?,
                default_sample_method: symbol(&lib_sd, b"sd_get_default_sample_method\0")?,
                default_scheduler: symbol(&lib_sd, b"sd_get_default_scheduler\0")?,
                str_to_sample_method: symbol(&lib_sd, b"str_to_sample_method\0")?,
                str_to_scheduler: symbol(&lib_sd, b"str_to_scheduler\0")?,
                list_devices: symbol(&lib_sd, b"sd_list_devices\0")?,
                sd_commit: symbol(&lib_sd, b"sd_commit\0")?,
                sd_version: symbol(&lib_sd, b"sd_version\0")?,
                num_physical_cores: symbol(&lib_sd, b"sd_get_num_physical_cores\0")?,
                _lib_sd: lib_sd,
                _lib_ggml: lib_ggml,
            }
        };
        let found = api.commit();
        if !found.starts_with(PINNED.short_commit) {
            return Err(SdCppError::AbiMismatch { expected: PINNED.short_commit.to_owned(), found });
        }
        Ok(api)
    }

    /// `sd_commit()`.
    pub(crate) fn commit(&self) -> String {
        // SAFETY: `const char* sd_commit(void)` returns a static string owned by the library.
        unsafe { owned_c_string((self.sd_commit)()) }
    }

    /// `sd_version()`.
    pub(crate) fn version(&self) -> String {
        // SAFETY: `const char* sd_version(void)` returns a static string owned by the library.
        unsafe { owned_c_string((self.sd_version)()) }
    }

    /// `sd_get_num_physical_cores()`, `0` when the library reports a negative count.
    pub(crate) fn physical_cores(&self) -> u32 {
        // SAFETY: a plain query without arguments.
        let cores = unsafe { (self.num_physical_cores)() };
        u32::try_from(cores).unwrap_or(0)
    }

    /// The compute devices `sd_list_devices` reports.
    pub(crate) fn list_devices(&self) -> Vec<DeviceInfo> {
        // SAFETY: a null buffer with size 0 only queries the required length.
        let needed = unsafe { (self.list_devices)(std::ptr::null_mut(), 0) };
        let Some(capacity) = needed.checked_add(1) else {
            return Vec::new();
        };
        let mut buffer = vec![0_u8; capacity];
        // SAFETY: `buffer` is writable for `capacity` bytes; the library writes at most
        // `capacity - 1` bytes plus the terminator.
        unsafe { (self.list_devices)(buffer.as_mut_ptr().cast::<c_char>(), capacity) };
        let text = CStr::from_bytes_until_nul(&buffer).map(|text| text.to_string_lossy().into_owned()).unwrap_or_default();
        parse_device_list(&text)
    }
}

/// Parses `sd_list_devices` output: one `name<TAB>description` per line (blank lines skipped,
/// a line without a tab is a name with an empty description).
pub(crate) fn parse_device_list(text: &str) -> Vec<DeviceInfo> {
    text.lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, description) = line.split_once('\t').unwrap_or((line, ""));
            DeviceInfo { name: name.trim().to_owned(), description: description.trim().to_owned() }
        })
        .collect()
}

/// Recognizes a missing system library in a loader message: an unresolved shared object
/// (`libgomp.so.1: cannot open shared object file`), a missing symbol version
/// (`` version `GLIBCXX_3.4.32' not found ``) or a named Windows DLL. Anything else is
/// `LoadProblem::Other`.
pub(crate) fn classify_load_error(message: &str) -> LoadProblem {
    const SYSTEM_LIBRARIES: [&str; 11] = [
        "libgomp.so.1",
        "libstdc++.so.6",
        "libvulkan.so.1",
        "libgcc_s.so.1",
        "libm.so.6",
        "vcruntime140.dll",
        "vcruntime140_1.dll",
        "msvcp140.dll",
        "msvcp140_codecvt_ids.dll",
        "vcomp140.dll",
        "vulkan-1.dll",
    ];
    let lower = message.to_ascii_lowercase();
    if let Some(library) = SYSTEM_LIBRARIES.iter().find(|library| lower.contains(*library)) {
        return LoadProblem::MissingSystemLibrary { library: (*library).to_owned() };
    }
    // glibc's loader: version `GLIBCXX_3.4.32' not found (required by ...).
    if let Some(start) = message.find("version `") {
        let rest = &message[start + "version `".len()..];
        if let Some(end) = rest.find('\'') {
            return LoadProblem::MissingSystemLibrary { library: rest[..end].to_owned() };
        }
    }
    LoadProblem::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_list_lines_are_parsed() {
        let devices = parse_device_list("Vulkan0\tAMD Radeon Graphics (RADV RAPHAEL_MENDOCINO)\nVulkan1\tAMD Radeon AI PRO R9700 (RADV GFX1201)\nCPU\n\n");
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[1], DeviceInfo { name: "Vulkan1".into(), description: "AMD Radeon AI PRO R9700 (RADV GFX1201)".into() });
        assert_eq!(devices[2], DeviceInfo { name: "CPU".into(), description: String::new() });
        assert_eq!(parse_device_list(""), Vec::<DeviceInfo>::new());
    }

    #[test]
    fn load_errors_name_the_missing_system_library() {
        let cases = [
            ("libggml-base.so.0: libgomp.so.1: cannot open shared object file: No such file or directory", "libgomp.so.1"),
            ("/p/libstable-diffusion.so: /lib/libstdc++.so.6: version `GLIBCXX_3.4.32' not found (required by /p/libstable-diffusion.so)", "libstdc++.so.6"),
            ("libggml-vulkan.so: libvulkan.so.1: cannot open shared object file", "libvulkan.so.1"),
            ("LoadLibraryExW failed: VCRUNTIME140_1.dll was not found", "vcruntime140_1.dll"),
            ("/p/libggml.so.0: version `GLIBC_2.38' not found", "GLIBC_2.38"),
        ];
        for (message, library) in cases {
            assert_eq!(classify_load_error(message), LoadProblem::MissingSystemLibrary { library: library.to_owned() }, "{message}");
        }
        assert_eq!(classify_load_error("LoadLibraryExW failed"), LoadProblem::Other);
    }
}
