/*
File: crates/ms-sysprobe/src/system_facts/macos.rs

Purpose:
macOS half of `system_facts`: OS version / build / Darwin release, model, CPU brand and core
counts through `sysctlbyname` (no process), GPUs from ONE
`system_profiler SPDisplaysDataType -json` run.

Key functions:
- probe(): the whole macOS report.

Notes:
`system_profiler` is slow (0.5-3 s) and runs under `SYSTEM_PROFILER_TIMEOUT`. Every `unsafe`
block carries a `// SAFETY:` comment.
*/

use std::ffi::CString;
use std::time::Duration;

use ms_log::runtime_log;
use ms_memory::MemoryAvailability;

use crate::gpu_utils::run_command_bounded;

use super::parse::parse_system_profiler_displays;
use super::{CpuFacts, DeviceFacts, GpuAdapterFacts, GpuEnumeration, GpuFacts, OsFacts, SystemFacts};

/// Upper bound of the `system_profiler` run.
const SYSTEM_PROFILER_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound of one sysctl string value (brand strings and versions are under 100 bytes).
const MAX_SYSCTL_STRING_BYTES: usize = 4096;

/// Collects the macOS report. Blocking; never panics.
pub(super) fn probe(memory: Option<MemoryAvailability>, logical_cores: Option<usize>) -> SystemFacts {
    let (enumeration, adapters) = match gpu_adapters() {
        Some(adapters) => (GpuEnumeration::Supported, adapters),
        None => (GpuEnumeration::Unsupported, Vec::new()),
    };
    SystemFacts {
        os: OsFacts {
            name: "macOS".to_string(),
            version: sysctl_string("kern.osproductversion"),
            build: sysctl_string("kern.osversion"),
            sdk_level: None,
            kernel: sysctl_string("kern.osrelease"),
            arch: std::env::consts::ARCH,
        },
        device: Some(DeviceFacts { manufacturer: Some("Apple".to_string()), model: sysctl_string("hw.model"), soc_manufacturer: None, soc_model: None }),
        cpu: CpuFacts {
            name: sysctl_string("machdep.cpu.brand_string"),
            physical_cores: sysctl_count("hw.physicalcpu"),
            // `hw.logicalcpu` is the machine's count; the process-visible count wins when known.
            logical_cores: logical_cores.or_else(|| sysctl_count("hw.logicalcpu")),
        },
        memory,
        gpus: GpuFacts::new(enumeration, adapters, None),
    }
}

/// The adapters `system_profiler` lists; `None` (logged) when it could not be run or parsed,
/// which the report states as an unsupported enumeration rather than "no GPU".
fn gpu_adapters() -> Option<Vec<GpuAdapterFacts>> {
    let output = match run_command_bounded("system_profiler", &["SPDisplaysDataType", "-json"], SYSTEM_PROFILER_TIMEOUT) {
        Ok(output) if output.status.success() => output.stdout,
        Ok(output) => {
            runtime_log::log_warn(format!("[system-facts] `system_profiler SPDisplaysDataType -json` exited with {}", output.status));
            return None;
        }
        Err(err) => {
            runtime_log::log_warn(format!("[system-facts] `system_profiler SPDisplaysDataType -json` failed: {err}"));
            return None;
        }
    };
    match parse_system_profiler_displays(&output) {
        Ok(adapters) => Some(adapters),
        Err(err) => {
            runtime_log::log_warn(format!("[system-facts] system_profiler output unusable: {err}"));
            None
        }
    }
}

/// Reads a string sysctl (`kern.osproductversion`); `None` (logged) when it is missing or
/// unreadable.
fn sysctl_string(name: &str) -> Option<String> {
    let key = CString::new(name).ok()?;
    let mut len: libc::size_t = 0;
    // SAFETY: `key` is NUL-terminated and outlives the call; a null `oldp` with a valid `oldlenp`
    // is the documented size query; no new value is written (null `newp`, length 0).
    let rc = unsafe { libc::sysctlbyname(key.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 || len == 0 || len > MAX_SYSCTL_STRING_BYTES {
        runtime_log::log_info(format!("[system-facts] sysctl {name} size query failed ({}; len {len})", std::io::Error::last_os_error()));
        return None;
    }
    let mut buffer = vec![0_u8; len];
    // SAFETY: `buffer` is writable for `len` bytes and `len` holds that size; same key and no
    // new value as above.
    let rc = unsafe { libc::sysctlbyname(key.as_ptr(), buffer.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 {
        runtime_log::log_info(format!("[system-facts] sysctl {name} failed: {}", std::io::Error::last_os_error()));
        return None;
    }
    buffer.truncate(len.min(buffer.len()));
    let text: Vec<u8> = buffer.into_iter().take_while(|byte| *byte != 0).collect();
    let value = String::from_utf8_lossy(&text).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Reads a 32-bit integer sysctl (`hw.physicalcpu`) as a positive count; `None` (logged)
/// otherwise.
fn sysctl_count(name: &str) -> Option<usize> {
    let key = CString::new(name).ok()?;
    let mut value: libc::c_int = 0;
    let mut len: libc::size_t = std::mem::size_of::<libc::c_int>();
    // SAFETY: `value` is a writable `c_int` and `len` holds its size; no new value is written.
    let rc = unsafe { libc::sysctlbyname(key.as_ptr(), (&raw mut value).cast(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 || len != std::mem::size_of::<libc::c_int>() {
        runtime_log::log_info(format!("[system-facts] sysctl {name} failed ({}; len {len})", std::io::Error::last_os_error()));
        return None;
    }
    usize::try_from(value).ok().filter(|count| *count > 0)
}
