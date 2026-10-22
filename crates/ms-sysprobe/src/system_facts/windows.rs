/*
File: crates/ms-sysprobe/src/system_facts/windows.rs

Purpose:
Windows half of `system_facts`: OS identity from `HKLM\SOFTWARE\Microsoft\Windows NT\
CurrentVersion`, CPU name from `HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0`, physical
cores from `GetLogicalProcessorInformationEx(RelationProcessorCore)`, GPUs from the shared PCI
walk (`pci_gpus`; names from SetupAPI `SPDRP_DEVICEDESC`).

Key functions:
- probe(): the whole Windows report.

Notes:
No PowerShell, no WMI, no COM. Every `unsafe` block carries a `// SAFETY:` comment; registry
reads go through `crate::win_registry`.
*/

use ms_log::runtime_log;
use ms_memory::MemoryAvailability;
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError};
use windows_sys::Win32::System::SystemInformation::{GetLogicalProcessorInformationEx, RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX};

use crate::win_registry::RegKey;

use super::parse::{count_processor_core_records, windows_build, windows_product_name};
use super::{CpuFacts, GpuEnumeration, GpuFacts, OsFacts, SystemFacts, pci_gpus};

/// The OS identity key.
const CURRENT_VERSION_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
/// The first processor's description key (every package reports the same brand).
const CPU0_KEY: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";
/// Upper bound of the processor-information buffer (1 MiB; a 256-core machine needs ~20 KiB).
const MAX_PROCESSOR_INFO_BYTES: usize = 1024 * 1024;

/// Collects the Windows report. Blocking; never panics.
pub(super) fn probe(memory: Option<MemoryAvailability>, logical_cores: Option<usize>) -> SystemFacts {
    // Names come from the walk's SetupAPI descriptions; an undescribed device stays unnamed.
    let (adapters, cuda_driver_version) = pci_gpus::probe();
    SystemFacts {
        os: os_facts(),
        device: None,
        cpu: CpuFacts { name: cpu_name(), physical_cores: physical_cores(), logical_cores },
        memory,
        gpus: GpuFacts::new(GpuEnumeration::Supported, adapters, cuda_driver_version),
    }
}

/// OS name, display version and build from the `CurrentVersion` key; `"Windows"` and `None`s
/// (logged) when the key cannot be opened.
fn os_facts() -> OsFacts {
    let key = match RegKey::open_local_machine(CURRENT_VERSION_KEY) {
        Ok(key) => Some(key),
        Err(status) => {
            runtime_log::log_warn(format!("[system-facts] cannot open HKLM\\{CURRENT_VERSION_KEY} (Win32 error {status})"));
            None
        }
    };
    let product_name = key.as_ref().and_then(|key| key.query_string("ProductName"));
    let build_number = key.as_ref().and_then(|key| key.query_string("CurrentBuildNumber"));
    let build_value = build_number.as_deref().and_then(|text| text.trim().parse::<u32>().ok());
    let ubr = key.as_ref().and_then(|key| key.query_u32("UBR"));
    // `DisplayVersion` ("24H2") exists from 20H2; older Windows 10 has only `ReleaseId` ("2004").
    let version = key.as_ref().and_then(|key| key.query_string("DisplayVersion").or_else(|| key.query_string("ReleaseId")));
    OsFacts {
        name: windows_product_name(product_name.as_deref(), build_value),
        version,
        build: windows_build(build_number.as_deref(), ubr),
        sdk_level: None,
        kernel: None,
        arch: std::env::consts::ARCH,
    }
}

/// `ProcessorNameString` of CPU 0 (trimmed: Intel pads it with spaces); `None` (logged) when
/// unreadable.
fn cpu_name() -> Option<String> {
    let key = match RegKey::open_local_machine(CPU0_KEY) {
        Ok(key) => key,
        Err(status) => {
            runtime_log::log_warn(format!("[system-facts] cannot open HKLM\\{CPU0_KEY} (Win32 error {status})"));
            return None;
        }
    };
    let name = key.query_string("ProcessorNameString");
    if name.is_none() {
        runtime_log::log_info("[system-facts] ProcessorNameString missing or unreadable");
    }
    name
}

/// Physical core count from `GetLogicalProcessorInformationEx(RelationProcessorCore)`: one
/// record per core. `None` (logged) when the call fails or the buffer is malformed.
fn physical_cores() -> Option<usize> {
    let mut byte_len = 0_u32;
    // SAFETY: a null buffer with length 0 is the documented size query; `byte_len` is a valid
    // out-param that receives the required size.
    let ok = unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut byte_len) };
    // SAFETY: reads the calling thread's last-error value; no preconditions.
    let error = unsafe { GetLastError() };
    if ok != 0 || error != ERROR_INSUFFICIENT_BUFFER {
        runtime_log::log_warn(format!("[system-facts] GetLogicalProcessorInformationEx size query failed (Win32 error {error})"));
        return None;
    }
    let size = usize::try_from(byte_len).ok().filter(|size| *size > 0 && *size <= MAX_PROCESSOR_INFO_BYTES)?;
    // A `u64` buffer gives the 8-byte alignment of `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX`.
    let mut storage = vec![0_u64; size.div_ceil(8)];
    // SAFETY: `storage` is writable for at least `byte_len` bytes and 8-byte aligned, as the
    // record type requires; `byte_len` is a valid in/out length.
    let ok = unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, storage.as_mut_ptr().cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(), &mut byte_len) };
    if ok == 0 {
        // SAFETY: reads the calling thread's last-error value; no preconditions.
        let error = unsafe { GetLastError() };
        runtime_log::log_warn(format!("[system-facts] GetLogicalProcessorInformationEx failed (Win32 error {error})"));
        return None;
    }
    let used = usize::try_from(byte_len).ok()?.min(size);
    // The records' in-memory bytes (Windows is little-endian, as the decoder expects).
    let bytes: Vec<u8> = storage.iter().flat_map(|word| word.to_ne_bytes()).take(used).collect();
    let cores = count_processor_core_records(&bytes);
    if cores.is_none() {
        runtime_log::log_warn(format!("[system-facts] GetLogicalProcessorInformationEx returned a malformed {used}-byte buffer"));
    }
    cores
}
