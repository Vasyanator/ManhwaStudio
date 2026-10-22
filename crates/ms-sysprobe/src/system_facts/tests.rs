//! Unit tests of the pure `system_facts` contracts: every parser, the Windows name / build /
//! core-count decoders, the driver-key memory decoding, the PCI row mapping, the `nvidia-smi`
//! merge and the adapter order. No test spawns a process or needs hardware; the live probe test
//! only checks the output contract and prints it (visible with `--nocapture`).

use super::parse::{
    NvidiaSmiGpu, count_processor_core_records, split_csv_line, parse_cpuinfo, parse_lspci_display_names, parse_nvidia_smi_query, parse_os_release,
    parse_system_profiler_displays, windows_build, windows_product_name,
};
use super::*;
use crate::amd_adapters::{choose_registry_vram, parse_registry_memory_size};

fn pci_device(vendor_id: u16, device_id: u16) -> PciDisplayDevice {
    PciDisplayDevice {
        vendor_id,
        device_id,
        revision: None,
        os_name: None,
        kernel_driver: None,
        driver_version: None,
        reported_memory_bytes: Some(8 << 30),
        kfd_gfx: None,
    }
}

fn adapter(vendor: GpuVendor, kind: GpuKind, name: &str) -> GpuAdapterFacts {
    GpuAdapterFacts {
        vendor,
        name: Some(name.to_string()),
        pci_id: None,
        kind,
        vram_bytes: None,
        amd_gfx: None,
        nvidia_compute_capability: None,
        driver_version: None,
        kernel_driver: None,
        gpu_cores: None,
    }
}

fn smi_row(name: &str, device_id: u16) -> NvidiaSmiGpu {
    NvidiaSmiGpu {
        name: Some(name.to_string()),
        memory_bytes: Some(24_564 * 1024 * 1024),
        driver_version: Some("560.94".to_string()),
        device_id: Some(device_id),
        compute_capability: Some(RuntimeVersion::new(8, 9)),
    }
}

#[test]
fn os_release_prefers_pretty_name_and_unquotes() {
    let text = "NAME=\"Ubuntu\"\nVERSION=\"26.04 LTS (Resolute)\"\n# comment\nPRETTY_NAME=\"Ubuntu 26.04 LTS\"\nID=ubuntu\n";
    assert_eq!(parse_os_release(text).as_deref(), Some("Ubuntu 26.04 LTS"));
    assert_eq!(parse_os_release("NAME='Arch Linux'\nBUILD_ID=rolling\n").as_deref(), Some("Arch Linux"));
    assert_eq!(parse_os_release("NAME=Fedora\nVERSION=\"42 (Workstation \\\"Edition\\\")\"\n").as_deref(), Some("Fedora 42 (Workstation \"Edition\")"));
    assert_eq!(parse_os_release("PRETTY_NAME=\"\"\nNAME=Gentoo\n").as_deref(), Some("Gentoo"));
    assert_eq!(parse_os_release("ID=x\ngarbage line\n"), None);
    assert_eq!(parse_os_release(""), None);
}

#[test]
fn cpuinfo_counts_distinct_physical_cores() {
    // 2 cores x 2 threads on one package: 4 processor blocks, 2 physical cores.
    let block = |processor: u32, core: u32| format!("processor\t: {processor}\nmodel name\t: AMD Ryzen 9 9950X 16-Core Processor\nphysical id\t: 0\ncore id\t\t: {core}\n");
    let text = [block(0, 0), block(1, 1), block(2, 0), block(3, 1)].join("\n");
    let summary = parse_cpuinfo(&text);
    assert_eq!(summary.model_name.as_deref(), Some("AMD Ryzen 9 9950X 16-Core Processor"));
    assert_eq!(summary.physical_cores, Some(2));
    assert_eq!(summary.hardware, None);
}

#[test]
fn cpuinfo_without_topology_has_unknown_cores_and_reads_hardware() {
    // Android / ARM: no model name, no physical id, a trailing Hardware block.
    let text = "processor\t: 0\nBogoMIPS\t: 38.40\nCPU part\t: 0xd05\n\nprocessor\t: 1\nBogoMIPS\t: 38.40\n\nHardware\t: Qualcomm Technologies, Inc SM8150\n";
    let summary = parse_cpuinfo(text);
    assert_eq!(summary.model_name, None);
    assert_eq!(summary.hardware.as_deref(), Some("Qualcomm Technologies, Inc SM8150"));
    assert_eq!(summary.physical_cores, None);
    // A topology present on only some blocks is not trusted.
    let mixed = "processor : 0\nphysical id : 0\ncore id : 0\n\nprocessor : 1\n";
    assert_eq!(parse_cpuinfo(mixed).physical_cores, None);
    assert_eq!(parse_cpuinfo("").physical_cores, None);
}

#[test]
fn nvidia_smi_query_rows_parse() {
    let text = "NVIDIA GeForce RTX 4090, 24564, 560.94, 0x268410DE, 8.9\nNVIDIA GeForce GTX 1080, 8192, 560.94, 0x1B8010DE, 6.1\n";
    let rows = parse_nvidia_smi_query(text, true);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], smi_row("NVIDIA GeForce RTX 4090", 0x2684));
    assert_eq!(rows[1].device_id, Some(0x1b80));
    assert_eq!(rows[1].compute_capability, Some(RuntimeVersion::new(6, 1)));
    assert_eq!(rows[1].memory_bytes, Some(8192 * 1024 * 1024));
}

#[test]
fn nvidia_smi_query_handles_na_commas_and_the_short_query() {
    // nvidia-smi quotes a cell that contains a comma (or a quote, doubled); [N/A] is unknown.
    let rows = parse_nvidia_smi_query("\"NVIDIA RTX A6000, \"\"Ada\"\" Board\", [N/A], 535.104.05, 0x26B110DE, [N/A]\n", true);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name.as_deref(), Some("NVIDIA RTX A6000, \"Ada\" Board"));
    assert_eq!(rows[0].memory_bytes, None);
    assert_eq!(rows[0].driver_version.as_deref(), Some("535.104.05"));
    assert_eq!(rows[0].compute_capability, None);
    assert_eq!(rows[0].device_id, Some(0x26b1));
    // A broken quote or a wrong cell count drops the line.
    assert!(parse_nvidia_smi_query("\"unterminated, 1, 2, 0x1, 8.9\n", true).is_empty());
    assert!(parse_nvidia_smi_query("\"A\"x, 1, 2, 0x1, 8.9\n", true).is_empty());
    assert!(parse_nvidia_smi_query("Unquoted, comma, 1, 2, 0x1, 8.9\n", true).is_empty());
    // The fallback query has no compute_cap column.
    let short = parse_nvidia_smi_query("Quadro K2200, 4096, 470.256, 0x13BA10DE\n", false);
    assert_eq!(short.len(), 1);
    assert_eq!(short[0].name.as_deref(), Some("Quadro K2200"));
    assert_eq!(short[0].compute_capability, None);
    // Too few columns and blank lines are skipped.
    assert!(parse_nvidia_smi_query("garbage\n\n24564, 1, 2\n", true).is_empty());
}

#[test]
fn csv_lines_split_rfc4180_style() {
    let cells = |line: &str| split_csv_line(line).map(|cells| cells.into_iter().collect::<Vec<_>>());
    assert_eq!(cells("a, b ,c"), Some(vec!["a".to_string(), "b".to_string(), "c".to_string()]));
    assert_eq!(cells("\"x, y\", 2"), Some(vec!["x, y".to_string(), "2".to_string()]));
    assert_eq!(cells("\"say \"\"hi\"\"\""), Some(vec!["say \"hi\"".to_string()]));
    assert_eq!(cells("a,"), Some(vec!["a".to_string(), String::new()]));
    assert_eq!(cells(""), Some(vec![String::new()]));
    assert_eq!(cells("\"open"), None);
    assert_eq!(cells("\"a\" b, c"), None);
}

#[test]
fn lspci_names_display_functions_only() {
    let text = concat!(
        "00:00.0 \"Host bridge [0600]\" \"Intel Corporation [8086]\" \"Raptor Lake-S Host Bridge [a703]\" -r01 \"ASUSTeK Computer Inc. [1043]\" \"Device [8882]\"\n",
        "00:02.0 \"VGA compatible controller [0300]\" \"Intel Corporation [8086]\" \"Raptor Lake-S GT1 [UHD Graphics 770] [a780]\" -r04 -p00 \"ASUSTeK Computer Inc. [1043]\" \"Device [8882]\"\n",
        "01:00.0 \"VGA compatible controller [0300]\" \"NVIDIA Corporation [10de]\" \"AD102 [GeForce RTX 4090] [2684]\" -ra1 -p00 \"ASUSTeK Computer Inc. [1043]\" \"Device [889d]\"\n",
        "02:00.0 \"Display controller [0380]\" \"Matrox Electronics Systems Ltd. [102b]\" \"MGA G200eW WPCM450 [0532]\" -r0a \"Dell [1028]\" \"Device [0486]\"\n",
        "03:00.0 \"3D controller [0302]\" \"Moore Threads [1ed5]\" \"Device [0301]\" -r01 \"\" \"\"\n",
    );
    let names = parse_lspci_display_names(text);
    assert_eq!(
        names,
        vec![
            ((0x8086, 0xa780), "UHD Graphics 770".to_string()),
            ((0x10de, 0x2684), "GeForce RTX 4090".to_string()),
            ((0x102b, 0x0532), "MGA G200eW WPCM450".to_string()),
        ]
    );
    assert!(parse_lspci_display_names("").is_empty());
}

#[test]
fn system_profiler_apple_silicon_and_intel_mac() {
    let apple = r#"{"SPDisplaysDataType":[{"_name":"Apple M2","sppci_bus":"sppci_builtin","sppci_cores":"10","sppci_device_type":"sppci_device_gpu","sppci_model":"Apple M2","spdisplays_vendor":"sppci_vendor_Apple"}]}"#;
    let adapters = parse_system_profiler_displays(apple).unwrap_or_default();
    assert_eq!(adapters.len(), 1);
    assert_eq!(adapters[0].vendor, GpuVendor::Apple);
    assert_eq!(adapters[0].kind, GpuKind::Integrated);
    assert_eq!(adapters[0].name.as_deref(), Some("Apple M2"));
    assert_eq!(adapters[0].gpu_cores, Some(10));
    assert_eq!(adapters[0].vram_bytes, None);
    assert_eq!(adapters[0].pci_id, None);

    let intel_mac = r#"{"SPDisplaysDataType":[
        {"_name":"Intel UHD Graphics 630","sppci_bus":"spdisplays_builtin","sppci_model":"Intel UHD Graphics 630","spdisplays_vendor":"Intel","spdisplays_vram_shared":"1536 MB","spdisplays_vendor-id":"0x8086","spdisplays_device-id":"0x3e9b"},
        {"_name":"AMD Radeon Pro 5500M","sppci_bus":"spdisplays_pcie_device","sppci_model":"AMD Radeon Pro 5500M","spdisplays_vendor":"sppci_vendor_amd","spdisplays_vram":"8 GB","spdisplays_vendor-id":"0x1002","spdisplays_device-id":"0x7340"}
    ]}"#;
    let adapters = parse_system_profiler_displays(intel_mac).unwrap_or_default();
    assert_eq!(adapters.len(), 2);
    assert_eq!((adapters[0].vendor, adapters[0].kind, adapters[0].vram_bytes), (GpuVendor::Intel, GpuKind::Integrated, None));
    assert_eq!(adapters[0].pci_id, Some((0x8086, 0x3e9b)));
    assert_eq!((adapters[1].vendor, adapters[1].kind), (GpuVendor::Amd, GpuKind::Discrete));
    assert_eq!(adapters[1].vram_bytes, Some(8 << 30));
    assert_eq!(adapters[1].amd_gfx, crate::amd_adapters::device_id_info(0x7340).map(|info| info.gfx));

    assert!(parse_system_profiler_displays("not json").is_err());
    assert!(parse_system_profiler_displays("{}").is_err());
    assert_eq!(parse_system_profiler_displays(r#"{"SPDisplaysDataType":[]}"#), Ok(Vec::new()));
}

#[test]
fn windows_product_name_corrects_windows_11() {
    assert_eq!(windows_product_name(Some("Windows 10 Pro"), Some(26_100)), "Windows 11 Pro");
    assert_eq!(windows_product_name(Some("Windows 10 Home"), Some(22_000)), "Windows 11 Home");
    assert_eq!(windows_product_name(Some("Windows 10 Pro"), Some(19_045)), "Windows 10 Pro");
    assert_eq!(windows_product_name(Some("Windows 10 Pro"), None), "Windows 10 Pro");
    assert_eq!(windows_product_name(Some("Windows Server 2022 Datacenter"), Some(20_348)), "Windows Server 2022 Datacenter");
    assert_eq!(windows_product_name(None, Some(26_100)), "Windows");
    assert_eq!(windows_product_name(Some("  "), None), "Windows");
}

#[test]
fn windows_build_joins_build_and_ubr() {
    assert_eq!(windows_build(Some("26100"), Some(4061)).as_deref(), Some("26100.4061"));
    assert_eq!(windows_build(Some("19045"), None).as_deref(), Some("19045"));
    assert_eq!(windows_build(None, Some(1)), None);
    assert_eq!(windows_build(Some(""), Some(1)), None);
}

#[test]
fn processor_core_records_are_counted_and_validated() {
    let record = |relationship: i32, size: u32| {
        let mut bytes = relationship.to_le_bytes().to_vec();
        bytes.extend(size.to_le_bytes());
        bytes.resize(usize::try_from(size).unwrap_or(8), 0);
        bytes
    };
    let buffer: Vec<u8> = [record(0, 48), record(0, 48), record(0, 48)].concat();
    assert_eq!(count_processor_core_records(&buffer), Some(3));
    // Other relationships are not cores.
    let mixed: Vec<u8> = [record(0, 48), record(2, 80)].concat();
    assert_eq!(count_processor_core_records(&mixed), Some(1));
    // Malformed: a size below the header, a record running past the end, no core at all.
    let mut undersized = 0_i32.to_le_bytes().to_vec();
    undersized.extend(4_u32.to_le_bytes());
    assert_eq!(count_processor_core_records(&undersized), None);
    assert_eq!(count_processor_core_records(&record(0, 48)[..40]), None);
    assert_eq!(count_processor_core_records(&record(2, 48)), None);
    assert_eq!(count_processor_core_records(&[]), None);
}

#[test]
fn driver_key_memory_size_decoding_and_choice() {
    // REG_QWORD 12 GiB, REG_DWORD 4 GiB - 1, 4-byte REG_BINARY of older drivers.
    assert_eq!(parse_registry_memory_size(11, &(12_u64 << 30).to_le_bytes()), Some(12 << 30));
    assert_eq!(parse_registry_memory_size(4, &u32::MAX.to_le_bytes()), Some(u64::from(u32::MAX)));
    assert_eq!(parse_registry_memory_size(3, &(512_u32 << 20).to_le_bytes()), Some(512 << 20));
    assert_eq!(parse_registry_memory_size(3, &(16_u64 << 30).to_le_bytes()), Some(16 << 30));
    // Wrong type or length, and zero, are unknown.
    assert_eq!(parse_registry_memory_size(1, &[0; 8]), None);
    assert_eq!(parse_registry_memory_size(4, &[1, 2, 3]), None);
    assert_eq!(parse_registry_memory_size(11, &0_u64.to_le_bytes()), None);
    // The 64-bit value wins (the DWORD saturates at 4 GiB).
    assert_eq!(choose_registry_vram(Some(24 << 30), Some(u64::from(u32::MAX))), Some(24 << 30));
    assert_eq!(choose_registry_vram(None, Some(2 << 30)), Some(2 << 30));
    assert_eq!(choose_registry_vram(None, None), None);
}

#[test]
fn pci_rows_map_vendor_kind_and_vram() {
    let nvidia = adapter_from_pci(&pci_device(0x10de, 0x2684));
    assert_eq!((nvidia.vendor, nvidia.kind, nvidia.vram_bytes), (GpuVendor::Nvidia, GpuKind::Discrete, Some(8 << 30)));
    assert_eq!(nvidia.pci_id, Some((0x10de, 0x2684)));
    // Intel: kind unknown, so the driver-reported (carve-out) memory is not shown as VRAM.
    let intel = adapter_from_pci(&pci_device(0x8086, 0xa780));
    assert_eq!((intel.vendor, intel.kind, intel.vram_bytes), (GpuVendor::Intel, GpuKind::Unknown, None));
    // AMD discrete / integrated from the pinned table, gfx and name from it too.
    let rx = adapter_from_pci(&pci_device(0x1002, 0x7550));
    assert_eq!((rx.vendor, rx.kind, rx.vram_bytes), (GpuVendor::Amd, GpuKind::Discrete, Some(8 << 30)));
    assert_eq!(rx.amd_gfx.as_ref().map(GfxArch::as_str), Some("gfx1201"));
    assert!(rx.name.is_some());
    let apu = adapter_from_pci(&pci_device(0x1002, 0x13c0));
    assert_eq!((apu.kind, apu.vram_bytes), (GpuKind::Integrated, None));
    // Unknown AMD id: nothing guessed.
    let unknown = adapter_from_pci(&pci_device(0x1002, 0xffff));
    assert_eq!((unknown.kind, unknown.amd_gfx.clone(), unknown.name.clone()), (GpuKind::Unknown, None, None));
    assert_eq!(adapter_from_pci(&pci_device(0x1ed5, 0x0301)).vendor, GpuVendor::Other { vendor_id: Some(0x1ed5) });
}

#[test]
fn nvidia_smi_rows_enrich_matching_pci_rows_in_order() {
    let pci = vec![adapter_from_pci(&pci_device(0x1002, 0x13c0)), adapter_from_pci(&pci_device(0x10de, 0x2684))];
    let merged = merge_nvidia_smi(pci, vec![smi_row("NVIDIA GeForce RTX 4090", 0x2684)]);
    assert_eq!(merged.len(), 2);
    let nvidia = &merged[1];
    assert_eq!(nvidia.name.as_deref(), Some("NVIDIA GeForce RTX 4090"));
    assert_eq!(nvidia.vram_bytes, Some(24_564 * 1024 * 1024));
    assert_eq!(nvidia.nvidia_compute_capability, Some(RuntimeVersion::new(8, 9)));
    assert_eq!(nvidia.driver_version.as_deref(), Some("560.94"));
    assert_eq!(nvidia.pci_id, Some((0x10de, 0x2684)));
    assert_eq!(merged[0].vendor, GpuVendor::Amd);
}

#[test]
fn nvidia_smi_rows_replace_pci_rows_when_counts_differ() {
    // The walk saw nothing (WSL): the rows become the adapters.
    let merged = merge_nvidia_smi(Vec::new(), vec![smi_row("A", 0x2684), smi_row("B", 0x1b80)]);
    assert_eq!(merged.iter().map(|adapter| adapter.name.as_deref()).collect::<Vec<_>>(), vec![Some("A"), Some("B")]);
    assert!(merged.iter().all(|adapter| adapter.kind == GpuKind::Discrete && adapter.vendor == GpuVendor::Nvidia));
    assert_eq!(merged[1].pci_id, Some((0x10de, 0x1b80)));
    // Two PCI rows, one smi row: the smi row wins, other vendors are kept.
    let pci = vec![adapter_from_pci(&pci_device(0x10de, 1)), adapter_from_pci(&pci_device(0x10de, 2)), adapter_from_pci(&pci_device(0x8086, 3))];
    let merged = merge_nvidia_smi(pci, vec![smi_row("only", 1)]);
    assert_eq!(merged.len(), 2);
    assert_eq!(merged.iter().filter(|adapter| adapter.vendor == GpuVendor::Nvidia).count(), 1);
    // No rows: unchanged.
    let pci = vec![adapter_from_pci(&pci_device(0x10de, 1))];
    assert_eq!(merge_nvidia_smi(pci.clone(), Vec::new()), pci);
}

#[test]
fn nvidia_smi_row_without_a_pci_row_is_appended() {
    // Headless compute card: no DRM card for the NVIDIA GPU, only the Intel iGPU was walked.
    let pci = vec![adapter_from_pci(&pci_device(0x8086, 0xa780))];
    let merged = merge_nvidia_smi(pci, vec![smi_row("NVIDIA A100", 0x20b0)]);
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].vendor, GpuVendor::Intel);
    assert_eq!((merged[1].vendor, merged[1].kind), (GpuVendor::Nvidia, GpuKind::Discrete));
    assert_eq!(merged[1].name.as_deref(), Some("NVIDIA A100"));
    assert_eq!(merged[1].pci_id, Some((0x10de, 0x20b0)));
    assert_eq!(merged[1].nvidia_compute_capability, Some(RuntimeVersion::new(8, 9)));
}

#[test]
fn adapters_are_ordered_discrete_integrated_unknown() {
    let ordered = order_gpu_adapters(vec![
        adapter(GpuVendor::Intel, GpuKind::Unknown, "u1"),
        adapter(GpuVendor::Amd, GpuKind::Integrated, "i1"),
        adapter(GpuVendor::Nvidia, GpuKind::Discrete, "d1"),
        adapter(GpuVendor::Amd, GpuKind::Discrete, "d2"),
        adapter(GpuVendor::Apple, GpuKind::Integrated, "i2"),
    ]);
    let names: Vec<_> = ordered.iter().filter_map(|adapter| adapter.name.as_deref()).collect();
    assert_eq!(names, vec!["d1", "d2", "i1", "i2", "u1"]);
    assert_eq!(GpuFacts::new(GpuEnumeration::Supported, ordered.clone(), None).adapters, ordered);
}

#[test]
fn live_probe_is_non_panicking_and_consistent() {
    // Host-dependent: prints this machine's report (visible with `--nocapture`) and checks only
    // the output contract. It may run `nvidia-smi` / `lspci` (bounded) on a machine that has them.
    let facts = probe_system_facts();
    println!("{facts:#?}");
    assert!(!facts.os.name.is_empty());
    assert_eq!(facts.os.arch, std::env::consts::ARCH);
    assert_eq!(order_gpu_adapters(facts.gpus.adapters.clone()), facts.gpus.adapters, "adapters must already be ordered");
    for adapter in &facts.gpus.adapters {
        if adapter.vram_bytes.is_some() && adapter.gpu_cores.is_none() {
            assert_eq!(adapter.kind, GpuKind::Discrete, "VRAM is reported for discrete adapters only");
        }
    }
    if let Some(memory) = facts.memory {
        assert!(memory.available_bytes <= memory.total_bytes);
    }
}
