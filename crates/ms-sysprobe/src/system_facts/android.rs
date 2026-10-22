/*
File: crates/ms-sysprobe/src/system_facts/android.rs

Purpose:
Android half of `system_facts`: device, SoC and OS version from system properties
(`__system_property_get`), kernel and CPU from procfs. GPU enumeration is reported as
unsupported: the app sandbox hides `/sys/class/drm` and there is no PCI bus to walk.

Key functions:
- probe(): the whole Android report.

Notes:
No process is spawned and no JNI is used. `ro.soc.*` exists from Android 12; older devices fall
back to `ro.board.platform` for the SoC model.
*/

use std::ffi::{CString, c_char};

use ms_log::runtime_log;
use ms_memory::MemoryAvailability;

use super::parse::parse_cpuinfo;
use super::{CpuFacts, DeviceFacts, GpuEnumeration, GpuFacts, OsFacts, PROC_CPUINFO, SystemFacts, kernel_release, read_text};

/// `PROP_VALUE_MAX` of `<sys/system_properties.h>`: the value buffer `__system_property_get`
/// requires (it writes at most this many bytes including the NUL).
const PROPERTY_VALUE_MAX: usize = 92;

// The buffer size above must stay the platform's own constant.
const _: () = assert!(libc::PROP_VALUE_MAX == 92);

/// Collects the Android report. Blocking (file reads only); never panics.
pub(super) fn probe(memory: Option<MemoryAvailability>, logical_cores: Option<usize>) -> SystemFacts {
    let soc_model = system_property("ro.soc.model").or_else(|| system_property("ro.board.platform"));
    let cpuinfo = read_text(PROC_CPUINFO).map(|text| parse_cpuinfo(&text));
    let cpu_name = cpuinfo
        .as_ref()
        .and_then(|info| info.model_name.clone().or_else(|| info.hardware.clone()))
        .or_else(|| soc_model.clone());
    let sdk_level = system_property("ro.build.version.sdk").and_then(|text| match text.parse::<u32>() {
        Ok(level) => Some(level),
        Err(err) => {
            runtime_log::log_info(format!("[system-facts] ro.build.version.sdk {text:?} is not a number: {err}"));
            None
        }
    });
    SystemFacts {
        os: OsFacts {
            name: "Android".to_string(),
            version: system_property("ro.build.version.release"),
            build: None,
            sdk_level,
            kernel: kernel_release(),
            arch: std::env::consts::ARCH,
        },
        device: Some(DeviceFacts {
            manufacturer: system_property("ro.product.manufacturer"),
            model: system_property("ro.product.model"),
            soc_manufacturer: system_property("ro.soc.manufacturer"),
            soc_model,
        }),
        cpu: CpuFacts { name: cpu_name, physical_cores: cpuinfo.and_then(|info| info.physical_cores), logical_cores },
        memory,
        gpus: GpuFacts::new(GpuEnumeration::Unsupported, Vec::new(), None),
    }
}

/// Reads one system property; `None` when it is unset or blank (an unset property is normal on
/// older releases and is not logged).
fn system_property(name: &str) -> Option<String> {
    let key = match CString::new(name) {
        Ok(key) => key,
        Err(err) => {
            runtime_log::log_warn(format!("[system-facts] property name {name:?} has an interior NUL: {err}"));
            return None;
        }
    };
    let mut buffer = [c_char::default(); PROPERTY_VALUE_MAX];
    // SAFETY: `key` is a NUL-terminated C string that outlives the call; `buffer` is writable
    // for PROP_VALUE_MAX bytes, the size the API contract requires.
    let len = unsafe { libc::__system_property_get(key.as_ptr(), buffer.as_mut_ptr()) };
    if len <= 0 {
        return None;
    }
    let bytes: Vec<u8> = buffer.iter().take_while(|ch| **ch != 0).map(|ch| ch.to_ne_bytes()[0]).collect();
    let value = String::from_utf8_lossy(&bytes).trim().to_string();
    (!value.is_empty()).then_some(value)
}
