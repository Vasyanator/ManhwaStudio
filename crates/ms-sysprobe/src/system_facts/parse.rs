/*
File: crates/ms-sysprobe/src/system_facts/parse.rs

Purpose:
The pure, I/O-free parsers of `system_facts`: every text or binary format the platform probes
read is decoded here so it is unit-tested on any host without the tool or the hardware.

Key structures:
- CpuInfoSummary: what `/proc/cpuinfo` says (model name, `Hardware`, physical cores).
- NvidiaSmiGpu: one row of the `nvidia-smi --query-gpu` CSV.

Key functions:
- parse_os_release(): `/etc/os-release` -> product name (Linux).
- parse_cpuinfo(): `/proc/cpuinfo` -> CpuInfoSummary (Linux, Android).
- parse_nvidia_smi_query() / split_csv_line(): `nvidia-smi --query-gpu ... --format=csv,
  noheader,nounits` rows (RFC 4180 quoting).
- parse_lspci_display_names(): `lspci -mm -nn` -> (vendor, device) -> name for display devices.
- parse_system_profiler_displays(): `system_profiler SPDisplaysDataType -json` -> adapters.
- windows_product_name() / windows_build(): registry `CurrentVersion` values -> OS facts.
- count_processor_core_records(): `GetLogicalProcessorInformationEx` buffer -> core count.

Notes:
Each parser is compiled for the platforms that call it plus `test`, so no target carries dead
code and every parser is tested on every host.
*/

#[cfg(any(target_os = "linux", windows, test))]
use crate::gpu_utils::RuntimeVersion;

#[cfg(any(target_os = "macos", test))]
use super::{GpuAdapterFacts, GpuKind, GpuVendor};

/// Product name from an `/etc/os-release` text: `PRETTY_NAME`, else `NAME` followed by
/// `VERSION` when present, else `None`. Values may be double- or single-quoted; shell escapes
/// (`\"`, `\\`, `\$`, `` \` ``) are undone; comments and malformed lines are skipped.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_os_release(text: &str) -> Option<String> {
    let mut pretty = None;
    let mut name = None;
    let mut version = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let value = unquote_os_release_value(raw);
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "PRETTY_NAME" => pretty = Some(value),
            "NAME" => name = Some(value),
            "VERSION" => version = Some(value),
            _ => {}
        }
    }
    pretty.or_else(|| name.map(|name| version.map_or_else(|| name.clone(), |version| format!("{name} {version}"))))
}

/// Strips one level of matching quotes from an os-release value and undoes its shell escapes.
#[cfg(any(target_os = "linux", test))]
fn unquote_os_release_value(raw: &str) -> String {
    let trimmed = raw.trim();
    let inner = ['"', '\'']
        .iter()
        .find_map(|quote| trimmed.strip_prefix(*quote).and_then(|rest| rest.strip_suffix(*quote)))
        .unwrap_or(trimmed);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(next) = chars.peek().copied()
            && matches!(next, '"' | '\\' | '$' | '`')
        {
            out.push(next);
            chars.next();
            continue;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

/// What `/proc/cpuinfo` says about the processor.
#[cfg(any(target_os = "linux", target_os = "android", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CpuInfoSummary {
    /// First non-empty `model name` (x86, most ARM servers); `None` on most Android kernels.
    pub(crate) model_name: Option<String>,
    /// The `Hardware` line (older ARM / Android kernels: the SoC, e.g.
    /// `"Qualcomm Technologies, Inc SM8150"`).
    pub(crate) hardware: Option<String>,
    /// Distinct (`physical id`, `core id`) pairs over all `processor` blocks; `None` when any
    /// block lacks either field (ARM, many VMs) or no block exists.
    pub(crate) physical_cores: Option<usize>,
}

/// Parses `/proc/cpuinfo` (blocks separated by blank lines, `key : value` lines). Pure.
#[cfg(any(target_os = "linux", target_os = "android", test))]
pub(crate) fn parse_cpuinfo(text: &str) -> CpuInfoSummary {
    let mut model_name = None;
    let mut hardware = None;
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut processor_blocks = 0_usize;
    let mut block_without_topology = false;
    for block in text.split("\n\n") {
        let mut is_processor = false;
        let mut physical_id = None;
        let mut core_id = None;
        for line in block.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "processor" => is_processor = true,
                "physical id" => physical_id = Some(value.to_string()),
                "core id" => core_id = Some(value.to_string()),
                "model name" if model_name.is_none() && !value.is_empty() => model_name = Some(value.to_string()),
                "Hardware" if hardware.is_none() && !value.is_empty() => hardware = Some(value.to_string()),
                _ => {}
            }
        }
        if !is_processor {
            continue;
        }
        processor_blocks += 1;
        match (physical_id, core_id) {
            (Some(physical), Some(core)) => {
                if !pairs.contains(&(physical.clone(), core.clone())) {
                    pairs.push((physical, core));
                }
            }
            _ => block_without_topology = true,
        }
    }
    let physical_cores = (processor_blocks > 0 && !block_without_topology && !pairs.is_empty()).then_some(pairs.len());
    CpuInfoSummary { model_name, hardware, physical_cores }
}

/// The `nvidia-smi` query with compute capability (drivers from ~510 on). `compute_cap` is the
/// LAST column so the fallback query without it parses with one column fewer.
#[cfg(any(target_os = "linux", windows))]
pub(crate) const NVIDIA_SMI_QUERY_WITH_COMPUTE_CAP: &str = "--query-gpu=name,memory.total,driver_version,pci.device_id,compute_cap";

/// The same query for drivers that reject the `compute_cap` field.
#[cfg(any(target_os = "linux", windows))]
pub(crate) const NVIDIA_SMI_QUERY_WITHOUT_COMPUTE_CAP: &str = "--query-gpu=name,memory.total,driver_version,pci.device_id";

/// The format flags of both queries: CSV without header and without units (MiB as a number).
#[cfg(any(target_os = "linux", windows))]
pub(crate) const NVIDIA_SMI_FORMAT: &str = "--format=csv,noheader,nounits";

/// One GPU row of the `nvidia-smi` query.
#[cfg(any(target_os = "linux", windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NvidiaSmiGpu {
    /// Product name (`"NVIDIA GeForce RTX 4090"`).
    pub(crate) name: Option<String>,
    /// `memory.total` in bytes (reported in MiB).
    pub(crate) memory_bytes: Option<u64>,
    /// Driver version (`"560.94"`).
    pub(crate) driver_version: Option<String>,
    /// PCI device id from `pci.device_id` (`0x268410DE` = device `0x2684`, vendor `0x10DE`).
    pub(crate) device_id: Option<u16>,
    /// Compute capability (`8.9`); `None` when not queried or `[N/A]`.
    pub(crate) compute_capability: Option<RuntimeVersion>,
}

/// Parses the rows of `nvidia-smi --query-gpu=name,memory.total,driver_version,pci.device_id
/// [,compute_cap] --format=csv,noheader,nounits`. `with_compute_cap` says which query ran.
/// Cells are decoded as CSV ([`split_csv_line`]: a name containing a comma arrives quoted);
/// `[N/A]` / `[Not Supported]` cells are `None`; a line with another cell count or a broken
/// quote is skipped. Pure.
#[cfg(any(target_os = "linux", windows, test))]
pub(crate) fn parse_nvidia_smi_query(text: &str, with_compute_cap: bool) -> Vec<NvidiaSmiGpu> {
    let columns = if with_compute_cap { 5 } else { 4 };
    text.lines()
        .filter_map(|line| {
            let cells = split_csv_line(line)?;
            if cells.len() != columns {
                return None;
            }
            Some(NvidiaSmiGpu {
                name: nvidia_cell(&cells[0]).map(str::to_string),
                memory_bytes: nvidia_cell(&cells[1]).and_then(|mib| mib.parse::<u64>().ok()).and_then(|mib| mib.checked_mul(1024 * 1024)),
                driver_version: nvidia_cell(&cells[2]).map(str::to_string),
                device_id: nvidia_cell(&cells[3]).and_then(parse_nvidia_pci_device_id),
                compute_capability: if with_compute_cap { nvidia_cell(&cells[4]).and_then(parse_major_minor) } else { None },
            })
        })
        .collect()
}

/// Splits one CSV line into its cells, RFC 4180 style: cells are separated by `,`; a cell
/// whose first non-blank character is `"` is quoted up to the closing `"`, with `""` standing
/// for one literal quote, and may contain commas. Unquoted cells are trimmed (`nvidia-smi`
/// separates cells with `", "`). `None` for an unterminated quote or text between a closing
/// quote and the next separator. An empty line is one empty cell. Pure.
#[cfg(any(target_os = "linux", windows, test))]
pub(crate) fn split_csv_line(line: &str) -> Option<Vec<String>> {
    let mut cells = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|ch| *ch == ' ' || *ch == '\t').is_some() {}
        let mut cell = String::new();
        if chars.next_if_eq(&'"').is_some() {
            loop {
                match chars.next()? {
                    '"' if chars.next_if_eq(&'"').is_some() => cell.push('"'),
                    '"' => break,
                    ch => cell.push(ch),
                }
            }
            while chars.next_if(|ch| *ch == ' ' || *ch == '\t').is_some() {}
            match chars.next() {
                None => {
                    cells.push(cell);
                    return Some(cells);
                }
                Some(',') => cells.push(cell),
                Some(_) => return None,
            }
        } else {
            let mut ended = true;
            for ch in chars.by_ref() {
                if ch == ',' {
                    ended = false;
                    break;
                }
                cell.push(ch);
            }
            cells.push(cell.trim().to_string());
            if ended {
                return Some(cells);
            }
        }
    }
}

/// A query cell, or `None` for an empty / `[N/A]` / `[Not Supported]` / `N/A` cell.
#[cfg(any(target_os = "linux", windows, test))]
fn nvidia_cell(cell: &str) -> Option<&str> {
    let cell = cell.trim();
    (!cell.is_empty() && !cell.starts_with('[') && !cell.eq_ignore_ascii_case("N/A")).then_some(cell)
}

/// `0x268410DE` -> `0x2684` (device id in the high 16 bits, vendor id in the low 16).
#[cfg(any(target_os = "linux", windows, test))]
fn parse_nvidia_pci_device_id(cell: &str) -> Option<u16> {
    let hex = cell.strip_prefix("0x").or_else(|| cell.strip_prefix("0X")).unwrap_or(cell);
    let combined = u32::from_str_radix(hex, 16).ok()?;
    u16::try_from(combined >> 16).ok()
}

/// `"8.9"` -> 8.9; anything else `None`.
#[cfg(any(target_os = "linux", windows, test))]
fn parse_major_minor(cell: &str) -> Option<RuntimeVersion> {
    let (major, minor) = cell.split_once('.')?;
    Some(RuntimeVersion::new(major.trim().parse().ok()?, minor.trim().parse().ok()?))
}

/// Parses `lspci -mm -nn` output into `((vendor id, device id), name)` for display-class
/// functions (class `03xx`). The name is the marketing name in the device field's last
/// bracket when present (`"AD102 [GeForce RTX 4090] [2684]"` -> `"GeForce RTX 4090"`), else
/// the device field without its id; pci.ids' placeholder `"Device"` yields no entry. Pure.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_lspci_display_names(text: &str) -> Vec<((u16, u16), String)> {
    text.lines()
        .filter_map(|line| {
            let quoted = lspci_quoted_fields(line);
            let (class, vendor, device) = (quoted.first()?, quoted.get(1)?, quoted.get(2)?);
            let (_, class_id) = split_trailing_hex_id(class)?;
            if class_id >> 8 != 0x03 {
                return None;
            }
            let (_, vendor_id) = split_trailing_hex_id(vendor)?;
            let (device_text, device_id) = split_trailing_hex_id(device)?;
            let name = marketing_name(device_text)?;
            Some(((vendor_id, device_id), name))
        })
        .collect()
}

/// The double-quoted fields of one `lspci -mm` line, in order (unquoted slot and `-r` / `-p`
/// option tokens are skipped).
#[cfg(any(target_os = "linux", test))]
fn lspci_quoted_fields(line: &str) -> Vec<&str> {
    // Quotes never nest in `-mm` output, so every odd segment between quotes is a field.
    line.split('"').skip(1).step_by(2).collect()
}

/// Splits `"Name [abcd]"` into `("Name", 0xabcd)`; `None` without a trailing 4-hex-digit id.
#[cfg(any(target_os = "linux", test))]
fn split_trailing_hex_id(field: &str) -> Option<(&str, u16)> {
    let trimmed = field.trim();
    let open = trimmed.strip_suffix(']')?.rfind('[')?;
    let id_text = &trimmed[open + 1..trimmed.len() - 1];
    if id_text.len() != 4 {
        return None;
    }
    let id = u16::from_str_radix(id_text, 16).ok()?;
    Some((trimmed[..open].trim_end(), id))
}

/// The marketing name inside a device field's last bracket, else the whole field; `None` for
/// an empty field or pci.ids' unknown-device placeholder `"Device"`.
#[cfg(any(target_os = "linux", test))]
fn marketing_name(device_text: &str) -> Option<String> {
    let text = device_text.trim();
    let name = text
        .strip_suffix(']')
        .and_then(|rest| rest.rfind('[').map(|open| rest[open + 1..].trim()))
        .filter(|inner| !inner.is_empty())
        .unwrap_or(text);
    (!name.is_empty() && name != "Device").then(|| name.to_string())
}

/// Parses `system_profiler SPDisplaysDataType -json` into adapters (unordered).
///
/// Per item: name `sppci_model` (else `_name`); vendor from `spdisplays_vendor` /
/// `sppci_vendor` (`"sppci_vendor_Apple"`, `"sppci_vendor_amd"`, ...), else the name's leading
/// vendor word, else [`GpuVendor::Other`] with `spdisplays_vendor-id`; PCI id from
/// `spdisplays_vendor-id` + `spdisplays_device-id`; kind Apple -> integrated, else `sppci_bus`
/// builtin -> integrated / pcie -> discrete; VRAM (`spdisplays_vram`, `"8 GB"`) for discrete
/// only; `sppci_cores`; AMD gfx from the pinned table by device id. Pure.
///
/// # Errors
/// A text that is not JSON, or has no `SPDisplaysDataType` array.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn parse_system_profiler_displays(json: &str) -> Result<Vec<GpuAdapterFacts>, String> {
    let root: serde_json::Value = serde_json::from_str(json).map_err(|err| format!("not JSON: {err}"))?;
    let items = root.get("SPDisplaysDataType").and_then(serde_json::Value::as_array).ok_or_else(|| "no SPDisplaysDataType array".to_string())?;
    Ok(items.iter().map(system_profiler_adapter).collect())
}

/// One `SPDisplaysDataType` item -> adapter (see [`parse_system_profiler_displays`]).
#[cfg(any(target_os = "macos", test))]
fn system_profiler_adapter(item: &serde_json::Value) -> GpuAdapterFacts {
    let text = |key: &str| item.get(key).and_then(serde_json::Value::as_str).map(str::trim).filter(|value| !value.is_empty());
    let name = text("sppci_model").or_else(|| text("_name")).map(str::to_string);
    let vendor_id = text("spdisplays_vendor-id").and_then(parse_prefixed_hex_u16);
    let device_id = text("spdisplays_device-id").and_then(parse_prefixed_hex_u16);
    let vendor_label = text("spdisplays_vendor").or_else(|| text("sppci_vendor")).unwrap_or_default().to_ascii_lowercase();
    let name_label = name.as_deref().unwrap_or_default().to_ascii_lowercase();
    let vendor = [vendor_label.as_str(), name_label.as_str()]
        .iter()
        .find_map(|label| vendor_from_label(label))
        .unwrap_or(GpuVendor::Other { vendor_id });
    let bus = text("sppci_bus").unwrap_or_default().to_ascii_lowercase();
    let kind = if vendor == GpuVendor::Apple || bus.contains("builtin") {
        GpuKind::Integrated
    } else if bus.contains("pcie") {
        GpuKind::Discrete
    } else {
        GpuKind::Unknown
    };
    let amd_gfx = match (vendor, device_id) {
        (GpuVendor::Amd, Some(device_id)) => crate::amd_adapters::device_id_info(device_id).map(|info| info.gfx),
        _ => None,
    };
    GpuAdapterFacts {
        vendor,
        name,
        pci_id: vendor_id.zip(device_id),
        kind,
        vram_bytes: if kind == GpuKind::Discrete { text("spdisplays_vram").or_else(|| text("_spdisplays_vram")).and_then(parse_size_with_unit) } else { None },
        amd_gfx,
        nvidia_compute_capability: None,
        driver_version: None,
        kernel_driver: None,
        gpu_cores: text("sppci_cores").and_then(|cores| cores.parse::<u32>().ok()),
    }
}

/// The vendor a lower-case `system_profiler` vendor label or name names, if any. Matched on
/// whole alphanumeric words (`sppci_vendor_amd` -> `amd`), so "Intel Corporation" never reads
/// as "ati".
#[cfg(any(target_os = "macos", test))]
fn vendor_from_label(label: &str) -> Option<GpuVendor> {
    label.split(|ch: char| !ch.is_ascii_alphanumeric()).find_map(|word| match word {
        "apple" => Some(GpuVendor::Apple),
        "nvidia" => Some(GpuVendor::Nvidia),
        "amd" | "ati" | "radeon" => Some(GpuVendor::Amd),
        "intel" => Some(GpuVendor::Intel),
        _ => None,
    })
}

/// `"0x1002"` -> `0x1002`.
#[cfg(any(target_os = "macos", test))]
fn parse_prefixed_hex_u16(text: &str) -> Option<u16> {
    let hex = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))?;
    u16::from_str_radix(hex, 16).ok()
}

/// `"8 GB"` / `"4096 MB"` -> bytes (binary units, as macOS reports VRAM); `None` otherwise.
#[cfg(any(target_os = "macos", test))]
fn parse_size_with_unit(text: &str) -> Option<u64> {
    let mut parts = text.split_whitespace();
    let number: u64 = parts.next()?.parse().ok()?;
    let multiplier: u64 = match parts.next()?.to_ascii_uppercase().as_str() {
        "MB" => 1024 * 1024,
        "GB" => 1024 * 1024 * 1024,
        _ => return None,
    };
    number.checked_mul(multiplier).filter(|bytes| *bytes > 0)
}

/// The Windows product name: the registry `ProductName` (`"Windows 10 Pro"`), with its
/// `"Windows 10"` prefix replaced by `"Windows 11"` when `CurrentBuildNumber` is 22000 or
/// later — Windows 11 never updated that value, so it still reads "Windows 10" there. A
/// missing or blank name yields `"Windows"`. Pure.
#[cfg(any(windows, test))]
pub(crate) fn windows_product_name(product_name: Option<&str>, build_number: Option<u32>) -> String {
    let name = product_name.map(str::trim).filter(|name| !name.is_empty()).unwrap_or("Windows");
    match (name.strip_prefix("Windows 10"), build_number) {
        (Some(rest), Some(build)) if build >= 22_000 => format!("Windows 11{rest}"),
        _ => name.to_string(),
    }
}

/// The Windows build identifier `CurrentBuildNumber.UBR` (`"26100.4061"`), or the build number
/// alone without a UBR; `None` without a build number. Pure.
#[cfg(any(windows, test))]
pub(crate) fn windows_build(build_number: Option<&str>, update_build_revision: Option<u32>) -> Option<String> {
    let build = build_number.map(str::trim).filter(|build| !build.is_empty())?;
    Some(update_build_revision.map_or_else(|| build.to_string(), |ubr| format!("{build}.{ubr}")))
}

/// `LOGICAL_PROCESSOR_RELATIONSHIP::RelationProcessorCore` (winnt.h), spelled here so the
/// decoder builds and is tested on every host.
#[cfg(any(windows, test))]
const RELATION_PROCESSOR_CORE: i32 = 0;

/// Counts the `RelationProcessorCore` records of a `GetLogicalProcessorInformationEx` buffer:
/// variable-size `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` records, each starting with
/// `Relationship: i32` and `Size: u32` (little-endian). `None` for a malformed buffer (a record
/// shorter than its header, running past the end) or one without a core record. Pure.
#[cfg(any(windows, test))]
pub(crate) fn count_processor_core_records(buffer: &[u8]) -> Option<usize> {
    let mut offset = 0_usize;
    let mut cores = 0_usize;
    while offset < buffer.len() {
        let header = buffer.get(offset..offset.checked_add(8)?)?;
        let relationship = i32::from_le_bytes(header.get(0..4)?.try_into().ok()?);
        let size = usize::try_from(u32::from_le_bytes(header.get(4..8)?.try_into().ok()?)).ok()?;
        if size < 8 {
            return None;
        }
        if relationship == RELATION_PROCESSOR_CORE {
            cores += 1;
        }
        offset = offset.checked_add(size)?;
    }
    (offset == buffer.len() && cores > 0).then_some(cores)
}
