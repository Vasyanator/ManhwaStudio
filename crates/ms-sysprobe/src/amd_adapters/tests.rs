//! Unit tests of the pure `amd_adapters` contracts: the parsers, the pinned table's sanity,
//! the adapter order and a non-panicking live probe. The KFD fixtures are verbatim node
//! `properties` files captured from a gfx1201 (RX 9070-class) + gfx1036 (Granite Ridge iGPU)
//! machine.

use std::collections::HashSet;

use super::device_ids::DEVICE_IDS;
use super::*;

/// KFD node 0 of the capture: the CPU agent (`simd_count 0`, all GPU fields 0).
const KFD_CPU_NODE: &str = "cpu_cores_count 12
simd_count 0
mem_banks_count 1
caches_count 0
io_links_count 2
p2p_links_count 0
cpu_core_id_base 0
simd_id_base 0
max_waves_per_simd 0
lds_size_in_kb 0
gds_size_in_kb 0
num_gws 0
wave_front_size 0
array_count 0
simd_arrays_per_engine 0
cu_per_simd_array 0
simd_per_cu 0
max_slots_scratch_cu 0
gfx_target_version 0
vendor_id 0
device_id 0
location_id 0
domain 0
drm_render_minor 0
hive_id 0
num_sdma_engines 0
num_sdma_xgmi_engines 0
num_sdma_queues_per_engine 0
num_cp_queues 0
cwsr_size 0
ctl_stack_size 0
max_engine_clk_ccompute 5486
";

/// KFD node 1 of the capture: the discrete gfx1201 GPU (device 0x7551).
const KFD_GFX1201_NODE: &str = "cpu_cores_count 0
simd_count 128
mem_banks_count 1
caches_count 138
io_links_count 1
p2p_links_count 1
cpu_core_id_base 0
simd_id_base 2147487744
max_waves_per_simd 16
lds_size_in_kb 64
gds_size_in_kb 0
num_gws 64
wave_front_size 32
array_count 8
simd_arrays_per_engine 2
cu_per_simd_array 8
simd_per_cu 2
max_slots_scratch_cu 32
gfx_target_version 120001
vendor_id 4098
device_id 30033
location_id 768
domain 0
drm_render_minor 128
hive_id 0
num_sdma_engines 2
num_sdma_xgmi_engines 0
num_sdma_queues_per_engine 6
num_cp_queues 4
cwsr_size 30699520
ctl_stack_size 28672
max_engine_clk_fcompute 2350
local_mem_size 0
fw_version 3290
capability 1752408704
capability2 0
debug_prop 1495
sdma_fw_version 36502
unique_id 6284653937388565550
num_xcc 1
max_engine_clk_ccompute 5486
";

/// KFD node 2 of the capture: the Granite Ridge iGPU (gfx1036, device 0x13c0).
const KFD_GFX1036_NODE: &str = "cpu_cores_count 0
simd_count 4
gfx_target_version 100306
vendor_id 4098
device_id 5056
location_id 4864
drm_render_minor 129
unique_id 0
";

fn adapter(device_id: u16, integrated: Option<bool>) -> AmdAdapter {
    AmdAdapter {
        vendor_id: AMD_PCI_VENDOR_ID,
        device_id,
        revision: None,
        name: format!("{device_id:#06x}"),
        gfx: None,
        gfx_source: None,
        integrated,
        driver_version: None,
    }
}

#[test]
fn kfd_properties_parse_captured_nodes() {
    assert_eq!(parse_kfd_node_properties(KFD_CPU_NODE), KfdNode { gfx_target_version: 0, vendor_id: 0, device_id: 0, simd_count: 0 });
    let dgpu = parse_kfd_node_properties(KFD_GFX1201_NODE);
    assert_eq!(dgpu, KfdNode { gfx_target_version: 120_001, vendor_id: 4098, device_id: 0x7551, simd_count: 128 });
    assert_eq!(GfxArch::from_kfd_target_version(dgpu.gfx_target_version).as_ref().map(GfxArch::as_str), Some("gfx1201"));
    let igpu = parse_kfd_node_properties(KFD_GFX1036_NODE);
    assert_eq!(igpu.device_id, 0x13c0);
    assert_eq!(GfxArch::from_kfd_target_version(igpu.gfx_target_version).as_ref().map(GfxArch::as_str), Some("gfx1036"));
    // The capture's ids agree with the pinned table.
    assert_eq!(device_id_info(0x7551).map(|info| info.gfx.to_string()), Some("gfx1201".to_string()));
    assert_eq!(device_id_info(0x13c0).map(|info| (info.gfx.to_string(), info.integrated)), Some(("gfx1036".to_string(), true)));
}

#[test]
fn kfd_properties_skip_malformed_lines() {
    let node = parse_kfd_node_properties("simd_count\ngfx_target_version abc\ndevice_id 1 2\nvendor_id 4098\n\n");
    assert_eq!(node, KfdNode { gfx_target_version: 0, vendor_id: 4098, device_id: 0, simd_count: 0 });
}

#[test]
fn pci_hardware_id_parse_table() {
    assert_eq!(parse_pci_hardware_id("PCI\\VEN_1002&DEV_7550&SUBSYS_04161002&REV_C0"), Some((0x1002, 0x7550, Some(0xc0))));
    assert_eq!(parse_pci_hardware_id("pci\\ven_1002&dev_13c0&rev_c6"), Some((0x1002, 0x13c0, Some(0xc6))));
    assert_eq!(parse_pci_hardware_id("PCI\\VEN_10DE&DEV_2684"), Some((0x10de, 0x2684, None)));
    assert_eq!(parse_pci_hardware_id("PCI\\VEN_1002&DEV_7550&REV_ZZ"), Some((0x1002, 0x7550, None)));
    assert_eq!(parse_pci_hardware_id("PCI\\VEN_1002&SUBSYS_04161002"), None);
    assert_eq!(parse_pci_hardware_id("PCI\\VEN_XYZ&DEV_7550"), None);
    assert_eq!(parse_pci_hardware_id("ROOT\\BasicDisplay"), None);
    assert_eq!(parse_pci_hardware_id(""), None);
}

#[test]
fn sysfs_hex_parse_table() {
    assert_eq!(parse_sysfs_hex_u16("0x1002\n"), Some(0x1002));
    assert_eq!(parse_sysfs_hex_u16("0x7551"), Some(0x7551));
    assert_eq!(parse_sysfs_hex_u16("13c0"), Some(0x13c0));
    assert_eq!(parse_sysfs_hex_u16("0x030000"), None);
    assert_eq!(parse_sysfs_hex_u16(""), None);
}

#[test]
fn windows_driver_version_parse_table() {
    assert_eq!(parse_windows_driver_version("32.0.31041.5005"), Some([32, 0, 31041, 5005]));
    assert_eq!(parse_windows_driver_version(" 31.0.24027.1012 "), Some([31, 0, 24027, 1012]));
    assert!(parse_windows_driver_version("31.0.24027.1012") < parse_windows_driver_version("32.0.31041.5005"));
    for bad in ["", "32.0.31041", "32.0.31041.5005.1", "32.0.x.5005", "Adrenalin 26.10"] {
        assert_eq!(parse_windows_driver_version(bad), None, "{bad:?}");
    }
}

#[test]
fn device_table_is_consistent() {
    let mut seen = HashSet::new();
    for row in DEVICE_IDS {
        assert!(seen.insert(row.device_id), "duplicate device id {:#06x}", row.device_id);
        assert!(GfxArch::parse(row.gfx).is_some_and(|gfx| gfx.as_str() == row.gfx), "{:#06x}: {} must be a canonical gfx", row.device_id, row.gfx);
        assert!(!row.product.trim().is_empty(), "{:#06x} has no product label", row.device_id);
    }
}

#[test]
fn device_table_covers_the_wheel_extras_and_the_unsupported_marker_archs() {
    // Every AMD whl-next device extra with a published device id, plus the explicitly
    // unsupported gfx803/900/906 (gfx1153 and gfx1250 have no published id; see the header).
    let covered: HashSet<&str> = DEVICE_IDS.iter().map(|row| row.gfx).collect();
    for gfx in [
        "gfx1010", "gfx1011", "gfx1012", "gfx1030", "gfx1031", "gfx1032", "gfx1033", "gfx1034", "gfx1035", "gfx1036", "gfx1100", "gfx1101",
        "gfx1102", "gfx1103", "gfx1150", "gfx1151", "gfx1152", "gfx1200", "gfx1201", "gfx908", "gfx90a", "gfx942", "gfx950", "gfx803",
        "gfx900", "gfx906",
    ] {
        assert!(covered.contains(gfx), "{gfx} has no device id");
    }
}

#[test]
fn device_id_info_known_and_unknown() {
    let info = device_id_info(0x7550);
    assert_eq!(info.as_ref().map(|info| (info.gfx.as_str(), info.integrated)), Some(("gfx1201", false)));
    // Radeon PRO V710: the one id whose sources disagree; the table follows ROCm (gfx1101).
    assert_eq!(device_id_info(0x7460).map(|info| info.gfx.to_string()), Some("gfx1101".to_string()));
    assert_eq!(device_id_info(0x15bf).map(|info| (info.gfx.to_string(), info.integrated)), Some(("gfx1103".to_string(), true)));
    assert_eq!(device_id_info(0x0000), None);
    assert_eq!(device_id_info(0xffff), None);
}

#[test]
fn order_puts_discrete_then_integrated_then_unknown() {
    let ordered = order_adapters(vec![
        adapter(1, Some(true)),
        adapter(2, None),
        adapter(3, Some(false)),
        adapter(4, Some(true)),
        adapter(5, Some(false)),
    ]);
    let ids: Vec<u16> = ordered.iter().map(|adapter| adapter.device_id).collect();
    assert_eq!(ids, vec![3, 5, 1, 4, 2]);
}

#[test]
fn live_detection_is_non_panicking_and_consistent() {
    // Host-dependent: prints what this machine reports (visible with `--nocapture`) and
    // checks only the output contract.
    let adapters = detect_amd_adapters();
    for adapter in &adapters {
        println!("{adapter:?}");
        assert_eq!(adapter.vendor_id, AMD_PCI_VENDOR_ID);
        assert_eq!(adapter.gfx.is_some(), adapter.gfx_source.is_some());
        assert!(!adapter.name.is_empty());
    }
    assert_eq!(order_adapters(adapters.clone()), adapters, "output must already be ordered");
    if !cfg!(any(target_os = "linux", windows)) {
        assert!(adapters.is_empty());
    }
    if !cfg!(target_os = "linux") {
        assert!(!linux_kfd_available());
    }
}

#[test]
fn win32_buffer_growth_is_bounded_and_must_progress() {
    assert_eq!(next_win32_buffer_units(512, 2048), Some(1024));
    assert_eq!(next_win32_buffer_units(512, 2049), Some(1025));
    // No progress (same or smaller request) and oversized requests stop the retry loop.
    assert_eq!(next_win32_buffer_units(512, 1024), None);
    assert_eq!(next_win32_buffer_units(512, 0), None);
    assert_eq!(next_win32_buffer_units(512, u32::MAX), None);
    let max_bytes = u32::try_from(MAX_WIN32_STRING_UNITS * 2).unwrap_or(u32::MAX);
    assert_eq!(next_win32_buffer_units(512, max_bytes), Some(MAX_WIN32_STRING_UNITS));
    assert_eq!(next_win32_buffer_units(512, max_bytes + 2), None);
}

#[test]
fn win32_utf16_decode_splits_and_trims_nuls() {
    let encode = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
    // REG_MULTI_SZ: strings separated by NUL, list ended by a double NUL, slack after it.
    let mut multi = encode("PCI\\VEN_1002&DEV_7550\0PCI\\VEN_1002\0\0");
    multi.extend([0x41, 0x42]);
    let used_bytes = u32::try_from((multi.len() - 2) * 2).unwrap_or(0);
    assert_eq!(decode_win32_utf16_strings(&multi, used_bytes), vec!["PCI\\VEN_1002&DEV_7550".to_string(), "PCI\\VEN_1002".to_string()]);
    // REG_SZ with and without its terminating NUL decodes the same.
    let with_nul = encode("32.0.31041.5005\0");
    assert_eq!(decode_win32_utf16_strings(&with_nul, 32), vec!["32.0.31041.5005".to_string()]);
    assert_eq!(decode_win32_utf16_strings(&with_nul, 30), vec!["32.0.31041.5005".to_string()]);
    // A byte count past the buffer is clamped; an odd count rounds up; empty yields nothing.
    assert_eq!(decode_win32_utf16_strings(&encode("ab"), 1000), vec!["ab".to_string()]);
    assert_eq!(decode_win32_utf16_strings(&encode("ab"), 3), vec!["ab".to_string()]);
    assert!(decode_win32_utf16_strings(&encode("\0\0"), 4).is_empty());
    assert!(decode_win32_utf16_strings(&[], 8).is_empty());
}
