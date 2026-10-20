/*
File: crates/ms-sysprobe/src/amd_adapters/linux.rs

Purpose:
Linux half of `amd_adapters`: enumerates AMD display adapters from sysfs and takes each one's
gfx from KFD topology, with the pinned device-id table as the fallback. Reads files only — no
rocminfo, no lspci, no system ROCm needed.

Key functions:
- enumerate(): `/sys/class/drm/card<N>` -> `AmdAdapter`s (unordered; the caller orders them).

Notes:
- A card is kept when its PCI vendor is 0x1002, its class is display (`0x03xxxx`) and its
  driver is `amdgpu` (a `radeon`-driven or driverless card cannot run ROCm and is logged and
  skipped). `cardN-<connector>` entries are not cards; cards are deduplicated by the canonical
  path of their `device` link.
- KFD matching is by PCI device id; KFD nodes with `simd_count 0` are CPU agents.
*/

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use ms_config::torch_choice::GfxArch;
use ms_log::runtime_log;

use super::{AMD_PCI_VENDOR_ID, AmdAdapter, GfxSource, device_id_info, parse_kfd_node_properties, parse_sysfs_hex_u16};

const DRM_CLASS_DIR: &str = "/sys/class/drm";
const KFD_NODES_DIR: &str = "/sys/class/kfd/kfd/topology/nodes";

/// Enumerates the amdgpu-driven AMD display adapters in `cardN` order. Never panics; an
/// unreadable directory or attribute is logged and the card (or the whole probe) skipped.
pub(super) fn enumerate() -> Vec<AmdAdapter> {
    let cards = match drm_cards() {
        Ok(cards) => cards,
        Err(err) => {
            // No DRM class at all (container, no GPU driver): nothing to enumerate.
            runtime_log::log_info(format!("[amd-adapters] cannot list {DRM_CLASS_DIR}: {err}"));
            return Vec::new();
        }
    };
    let kfd = kfd_gfx_by_device_id();
    let mut seen_devices: Vec<PathBuf> = Vec::new();
    let mut adapters = Vec::new();
    for card in cards {
        let device_dir = card.join("device");
        let canonical = match fs::canonicalize(&device_dir) {
            Ok(path) => path,
            Err(err) => {
                runtime_log::log_warn(format!("[amd-adapters] cannot resolve {}: {err}", device_dir.display()));
                continue;
            }
        };
        if seen_devices.contains(&canonical) {
            continue;
        }
        seen_devices.push(canonical);
        if let Some(adapter) = adapter_from_device_dir(&device_dir, &kfd) {
            adapters.push(adapter);
        }
    }
    adapters
}

/// `cardN` entries of the DRM class, sorted by N.
fn drm_cards() -> std::io::Result<Vec<PathBuf>> {
    let mut cards: Vec<(u32, PathBuf)> = fs::read_dir(DRM_CLASS_DIR)?
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            Err(err) => {
                runtime_log::log_warn(format!("[amd-adapters] cannot read an entry of {DRM_CLASS_DIR}: {err}"));
                None
            }
        })
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let index = card_index(&name)?;
            Some((index, entry.path()))
        })
        .collect();
    cards.sort_by_key(|(index, _)| *index);
    Ok(cards.into_iter().map(|(_, path)| path).collect())
}

/// `Some(N)` for a name matching `^card\d+$` (so `card1-DP-1` connectors are excluded).
fn card_index(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("card")?;
    if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Builds the adapter of one PCI device directory, or `None` when it is not an
/// amdgpu-driven AMD display device.
fn adapter_from_device_dir(device_dir: &Path, kfd: &HashMap<u16, GfxArch>) -> Option<AmdAdapter> {
    let vendor_id = read_attr(device_dir, "vendor").as_deref().and_then(parse_sysfs_hex_u16)?;
    if vendor_id != AMD_PCI_VENDOR_ID {
        return None;
    }
    let class = read_attr(device_dir, "class").and_then(|text| parse_sysfs_hex_u32(&text))?;
    // PCI base class 0x03 = display controller (VGA 0x0300, 3D 0x0302, other 0x0380).
    if class >> 16 != 0x03 {
        return None;
    }
    let Some(device_id) = read_attr(device_dir, "device").as_deref().and_then(parse_sysfs_hex_u16) else {
        runtime_log::log_warn(format!("[amd-adapters] AMD display device without a readable device id: {}", device_dir.display()));
        return None;
    };
    let driver = fs::read_link(device_dir.join("driver"))
        .ok()
        .and_then(|link| link.file_name().map(|name| name.to_string_lossy().into_owned()));
    if driver.as_deref() != Some("amdgpu") {
        runtime_log::log_info(format!(
            "[amd-adapters] skipping AMD display device {device_id:#06x} at {}: driver {} is not amdgpu",
            device_dir.display(),
            driver.as_deref().unwrap_or("<none>")
        ));
        return None;
    }
    let revision = read_attr(device_dir, "revision").and_then(|text| parse_sysfs_hex_u8(&text));
    let table = device_id_info(device_id);
    let kfd_gfx = kfd.get(&device_id).cloned();
    if let (Some(kfd_gfx), Some(table)) = (&kfd_gfx, &table)
        && *kfd_gfx != table.gfx
    {
        runtime_log::log_warn(format!(
            "[amd-adapters] device {device_id:#06x}: KFD reports {kfd_gfx}, the device-id table says {}; using KFD",
            table.gfx
        ));
    }
    let (gfx, gfx_source) = match (kfd_gfx, &table) {
        (Some(gfx), _) => (Some(gfx), Some(GfxSource::Kfd)),
        (None, Some(info)) => (Some(info.gfx.clone()), Some(GfxSource::DeviceIdTable)),
        (None, None) => (None, None),
    };
    let name = read_attr(device_dir, "product_name")
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .or_else(|| table.as_ref().map(|info| info.product.to_string()))
        .unwrap_or_else(|| format!("AMD [{AMD_PCI_VENDOR_ID:04x}:{device_id:04x}]"));
    Some(AmdAdapter {
        vendor_id,
        device_id,
        revision,
        name,
        gfx,
        gfx_source,
        integrated: table.as_ref().map(|info| info.integrated),
        driver_version: None,
    })
}

/// Reads KFD topology and maps each GPU node's PCI device id to its processor. A missing KFD
/// (no `/dev/kfd` driver support) yields an empty map, so the table fallback applies.
fn kfd_gfx_by_device_id() -> HashMap<u16, GfxArch> {
    let mut map = HashMap::new();
    let entries = match fs::read_dir(KFD_NODES_DIR) {
        Ok(entries) => entries,
        Err(err) => {
            runtime_log::log_info(format!("[amd-adapters] no KFD topology at {KFD_NODES_DIR}: {err}"));
            return map;
        }
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path().join("properties"),
            Err(err) => {
                runtime_log::log_warn(format!("[amd-adapters] cannot read an entry of {KFD_NODES_DIR}: {err}"));
                continue;
            }
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                runtime_log::log_warn(format!("[amd-adapters] cannot read {}: {err}", path.display()));
                continue;
            }
        };
        let node = parse_kfd_node_properties(&text);
        if node.simd_count == 0 || node.vendor_id != u32::from(AMD_PCI_VENDOR_ID) {
            continue;
        }
        let (Ok(device_id), Some(gfx)) = (u16::try_from(node.device_id), GfxArch::from_kfd_target_version(node.gfx_target_version)) else {
            runtime_log::log_warn(format!(
                "[amd-adapters] KFD node {} has an unusable device id {} or gfx_target_version {}",
                path.display(),
                node.device_id,
                node.gfx_target_version
            ));
            continue;
        };
        map.insert(device_id, gfx);
    }
    map
}

/// Reads one sysfs attribute of `device_dir`; `None` when missing or unreadable (sysfs
/// attributes are optional per driver, so absence is not logged).
fn read_attr(device_dir: &Path, name: &str) -> Option<String> {
    fs::read_to_string(device_dir.join(name)).ok()
}

/// Parses a sysfs hex attribute wider than 16 bits (the 24-bit PCI `class`).
fn parse_sysfs_hex_u32(text: &str) -> Option<u32> {
    let trimmed = text.trim();
    u32::from_str_radix(trimmed.strip_prefix("0x").unwrap_or(trimmed), 16).ok()
}

/// Parses the 8-bit PCI `revision` attribute (`"0xc0\n"`).
fn parse_sysfs_hex_u8(text: &str) -> Option<u8> {
    let trimmed = text.trim();
    u8::from_str_radix(trimmed.strip_prefix("0x").unwrap_or(trimmed), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_index_accepts_only_bare_cards() {
        assert_eq!(card_index("card0"), Some(0));
        assert_eq!(card_index("card12"), Some(12));
        assert_eq!(card_index("card1-DP-1"), None);
        assert_eq!(card_index("card"), None);
        assert_eq!(card_index("renderD128"), None);
    }

    #[test]
    fn wide_and_narrow_sysfs_hex() {
        assert_eq!(parse_sysfs_hex_u32("0x030000\n"), Some(0x03_0000));
        assert_eq!(parse_sysfs_hex_u8("0xc0\n"), Some(0xc0));
        assert_eq!(parse_sysfs_hex_u8("0x1c0"), None);
    }
}
