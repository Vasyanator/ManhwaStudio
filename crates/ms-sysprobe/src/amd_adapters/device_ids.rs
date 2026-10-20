/*
File: crates/ms-sysprobe/src/amd_adapters/device_ids.rs

Purpose:
The ONE pinned AMD PCI device-id -> LLVM processor (`gfxNNNN`) table of the workspace, with an
integrated/discrete flag and a short product label per id. On Linux it is the fallback behind
the kernel's own KFD answer; on Windows it is the only gfx source (C3: no OpenCL probe).

Key items:
- DeviceIdRow / DEVICE_IDS: the table, one row per PCI device id (vendor 0x1002).
- lookup_row(): exact-id lookup used by `amd_adapters::device_id_info`.

Sources (fetched as data, 2026-10-20; regenerate by re-reading the same files at newer pins):
- ROCm/rocminfo `rocm_agent_enumerator`, `ISA_TO_ID`, commit
  0a86860df4d6a730a6d65882f47560ed6aa85934 (branch amd-staging):
  https://github.com/ROCm/rocminfo/blob/0a86860df4d6a730a6d65882f47560ed6aa85934/rocm_agent_enumerator
  — the base id -> gfx mapping for gfx803..gfx1102, gfx1200, gfx1201 (gfx700/701/801/802 are left
  out: never a torch target, they stay "unknown id").
- pciutils/pciids `pci.ids`, commit ad4bc5c5d708dc5086581646f30374553cd06e8c:
  https://github.com/pciutils/pciids/blob/ad4bc5c5d708dc5086581646f30374553cd06e8c/pci.ids
  — chip codenames for ids the enumerator lacks: Navi 31/32/33/24/22 extras, Aqua Vanjaram
  (MI300 = gfx942, MI350 = gfx950), Raphael / Granite Ridge (gfx1036), Mendocino (gfx1037),
  Phoenix / Hawk Point (gfx1103), Strix (gfx1150), Strix Halo (gfx1151), Krackan (gfx1152).
- mesa/drm `data/amdgpu.ids`, commit b97cbde15c5c3abfe44d78e8f57139e50f612fec:
  https://gitlab.freedesktop.org/mesa/drm/-/blob/b97cbde15c5c3abfe44d78e8f57139e50f612fec/data/amdgpu.ids
  — the product labels (first two distinct marketing names) and the MI3xx VF ids.
- llvm/llvm-project `llvm/docs/AMDGPUUsage.rst`, commit be7aacdc8c4ba1c0db220a0d63ea999584d4d2a7,
  processor table: codename/product -> gfx and the APU (integrated) column.
- ROCm system requirements (Linux), accessed 2026-10-20:
  https://rocm.docs.amd.com/projects/install-on-linux/en/latest/reference/system-requirements.html
  — resolves the one conflict: the enumerator lists 0x7460/0x7461 (Radeon PRO V710) under
  gfx1100, pci.ids names them Navi 32 and the ROCm table says V710 = gfx1101; the table uses
  gfx1101. Also MI350X/MI355X = gfx950, MI300A = gfx942.

Notes:
- No id is published at these pins for gfx1153 or gfx1250 (both are in the AMD wheel-extra
  list), nor for Krackan2 (0x1902, codename only): they are absent, so such a card is an
  "unknown device id" on Windows (Linux still gets its gfx from KFD).
- `integrated` follows the LLVM APU column, except MI300A (0x74a0), which is an APU although its
  processor gfx942 is listed as dGPU.
- Product labels are hardware names, not UI prose; they are never translated.
*/

/// One table row: a PCI device id of vendor 0x1002 and what it is.
#[derive(Debug)]
pub(crate) struct DeviceIdRow {
    /// PCI device id (`DEV_xxxx` / sysfs `device`).
    pub(crate) device_id: u16,
    /// LLVM processor name; every row parses as `ms_config::torch_choice::GfxArch` (tested).
    pub(crate) gfx: &'static str,
    /// Whether the id is an APU / integrated GPU.
    pub(crate) integrated: bool,
    /// Short product label for display when the OS gives no name.
    pub(crate) product: &'static str,
}

const fn row(device_id: u16, gfx: &'static str, integrated: bool, product: &'static str) -> DeviceIdRow {
    DeviceIdRow { device_id, gfx, integrated, product }
}

/// The pinned table, grouped by gfx. Ids are unique (tested).
pub(crate) const DEVICE_IDS: &[DeviceIdRow] = &[
    row(0x67c0, "gfx803", false, "Radeon Pro WX 7100 Graphics / Radeon E9550"),
    row(0x67c1, "gfx803", false, "Polaris / Fiji"),
    row(0x67c2, "gfx803", false, "Radeon Pro V7300X / V7350x2 / Radeon Pro V7350x2"),
    row(0x67c4, "gfx803", false, "Radeon Pro WX 7100 Graphics / Radeon E9560 / E9565 Graphics"),
    row(0x67c7, "gfx803", false, "Radeon Pro WX 5100 Graphics / Radeon E9390 Graphics"),
    row(0x67c8, "gfx803", false, "Polaris / Fiji"),
    row(0x67c9, "gfx803", false, "Polaris / Fiji"),
    row(0x67ca, "gfx803", false, "Ellesmere [Polaris10]"),
    row(0x67cc, "gfx803", false, "Ellesmere [Polaris10]"),
    row(0x67cf, "gfx803", false, "Ellesmere [Polaris10]"),
    row(0x67d0, "gfx803", false, "Radeon Pro V7300X / V7350x2 / Radeon Pro V7350x2"),
    row(0x67df, "gfx803", false, "Radeon Pro 580X / Radeon RX 580 Series"),
    row(0x67e0, "gfx803", false, "Radeon Pro WX Series"),
    row(0x67e1, "gfx803", false, "Baffin [Polaris11]"),
    row(0x67e3, "gfx803", false, "Radeon Pro WX 4100"),
    row(0x67e7, "gfx803", false, "Polaris / Fiji"),
    row(0x67e8, "gfx803", false, "Radeon Pro WX Series / Radeon E9260 Graphics"),
    row(0x67e9, "gfx803", false, "Baffin [Polaris11]"),
    row(0x67eb, "gfx803", false, "Radeon Pro V5300X"),
    row(0x67ef, "gfx803", false, "Radeon RX Graphics / Radeon RX 460 Graphics"),
    row(0x67ff, "gfx803", false, "Radeon Pro 465 / Radeon RX 560 Series"),
    row(0x694c, "gfx803", false, "Radeon RX Vega M GH Graphics"),
    row(0x694e, "gfx803", false, "Radeon RX Vega M GL Graphics"),
    row(0x694f, "gfx803", false, "Polaris 22 MGL XL [Radeon Pro WX Vega M GL]"),
    row(0x6980, "gfx803", false, "Radeon Pro WX 3100"),
    row(0x6981, "gfx803", false, "Radeon Pro WX 3200 Series"),
    row(0x6985, "gfx803", false, "Radeon Pro WX 3100"),
    row(0x6986, "gfx803", false, "Radeon Pro WX 2100"),
    row(0x6987, "gfx803", false, "Embedded Radeon E9171 / Radeon 550X Series"),
    row(0x6995, "gfx803", false, "Radeon Pro WX 2100"),
    row(0x6997, "gfx803", false, "Radeon Pro WX 2100"),
    row(0x699f, "gfx803", false, "Embedded Radeon E9170 Series / Radeon 500 Series"),
    row(0x6fdf, "gfx803", false, "Radeon RX 590 GME / Radeon RX 580 2048SP"),
    row(0x7300, "gfx803", false, "FirePro S9300 x2 / Radeon R9 Fury Series"),
    row(0x730f, "gfx803", false, "Polaris / Fiji"),
    row(0x6860, "gfx900", false, "Radeon Instinct MI25 / Radeon Pro V340"),
    row(0x6861, "gfx900", false, "Radeon Pro WX 9100"),
    row(0x6862, "gfx900", false, "Radeon Pro SSG"),
    row(0x6863, "gfx900", false, "Radeon Vega Frontier Edition"),
    row(0x6864, "gfx900", false, "Radeon Pro V340 / Radeon Instinct MI25x2"),
    row(0x6867, "gfx900", false, "Radeon Pro Vega 56"),
    row(0x6868, "gfx900", false, "Radeon Pro WX 8200"),
    row(0x6869, "gfx900", false, "Vega 10 XGA [Radeon Pro Vega 48]"),
    row(0x686a, "gfx900", false, "Vega 10 LEA"),
    row(0x686b, "gfx900", false, "Vega 10 XTXA [Radeon Pro Vega 64X]"),
    row(0x686c, "gfx900", false, "Radeon Instinct MI25 MxGPU / Radeon Pro V340 MxGPU"),
    row(0x686d, "gfx900", false, "Vega 10 GLXTA"),
    row(0x686e, "gfx900", false, "Vega 10 GLXLA"),
    row(0x686f, "gfx900", false, "Vega 10"),
    row(0x687f, "gfx900", false, "Radeon RX Vega"),
    row(0x15d8, "gfx902", true, "Radeon RX Vega 8 Graphics WS / Ryzen Embedded R1606G with Radeon Vega Gfx"),
    row(0x15dd, "gfx902", true, "Ryzen Embedded V1807B with Radeon Vega Gfx / Ryzen Embedded V1756B with Radeon Vega Gfx"),
    row(0x69a0, "gfx904", false, "Radeon Vega 12"),
    row(0x69a1, "gfx904", false, "Radeon Vega 12"),
    row(0x69a2, "gfx904", false, "Radeon Vega 12"),
    row(0x69a3, "gfx904", false, "Radeon Vega 12"),
    row(0x69af, "gfx904", false, "Radeon Pro Vega 20 / Radeon Pro Vega 16"),
    row(0x66a0, "gfx906", false, "Vega 20 [Radeon Pro/Radeon Instinct]"),
    row(0x66a1, "gfx906", false, "Instinct MI60 / MI50 / Radeon Pro VII"),
    row(0x66a2, "gfx906", false, "Vega 20"),
    row(0x66a3, "gfx906", false, "Vega 20 [Radeon Pro Vega II/Radeon Pro Vega II Duo]"),
    row(0x66a4, "gfx906", false, "Vega 20"),
    row(0x66a7, "gfx906", false, "Vega 20 [Radeon Pro Vega 20]"),
    row(0x66af, "gfx906", false, "Radeon VII"),
    row(0x7388, "gfx908", false, "Arcturus GL-XL"),
    row(0x738c, "gfx908", false, "Instinct MI100"),
    row(0x738e, "gfx908", false, "Arcturus GL-XL [Instinct MI100]"),
    row(0x7390, "gfx908", false, "Arcturus"),
    row(0x7408, "gfx90a", false, "Instinct MI250X"),
    row(0x740c, "gfx90a", false, "Instinct MI250X / MI250"),
    row(0x740f, "gfx90a", false, "Instinct MI210"),
    row(0x7410, "gfx90a", false, "Instinct MI210 VF"),
    row(0x15e7, "gfx90c", true, "Barcelo"),
    row(0x1636, "gfx90c", true, "Renoir [Radeon Vega Series / Radeon Vega Mobile Series]"),
    row(0x1638, "gfx90c", true, "Cezanne [Radeon Vega Series / Radeon Vega Mobile Series]"),
    row(0x164c, "gfx90c", true, "Lucienne"),
    row(0x74a0, "gfx942", true, "Instinct MI300A"),
    row(0x74a1, "gfx942", false, "Instinct MI300X"),
    row(0x74a2, "gfx942", false, "Instinct MI308X / Instinct MI300X"),
    row(0x74a5, "gfx942", false, "Instinct MI325X"),
    row(0x74a8, "gfx942", false, "Instinct MI308X HF"),
    row(0x74a9, "gfx942", false, "Instinct MI300X HF"),
    row(0x74b5, "gfx942", false, "Instinct MI300X VF"),
    row(0x74b6, "gfx942", false, "Instinct MI308X / Instinct MI300X"),
    row(0x74b9, "gfx942", false, "Instinct MI325X VF"),
    row(0x74bd, "gfx942", false, "Instinct MI300X HF"),
    row(0x75a0, "gfx950", false, "Instinct MI350X"),
    row(0x75a3, "gfx950", false, "Instinct MI355X"),
    row(0x75a8, "gfx950", false, "Instinct MI350P"),
    row(0x75b0, "gfx950", false, "Instinct MI350X VF"),
    row(0x75b3, "gfx950", false, "Instinct MI355X VF"),
    row(0x75b8, "gfx950", false, "Instinct MI350P VF"),
    row(0x7310, "gfx1010", false, "Navi 10 [Radeon Pro W5700X]"),
    row(0x7312, "gfx1010", false, "Radeon Pro W5700"),
    row(0x7318, "gfx1010", false, "Navi 10"),
    row(0x7319, "gfx1010", false, "Navi 10 [Radeon Pro 5700 XT]"),
    row(0x731a, "gfx1010", false, "Navi 10"),
    row(0x731b, "gfx1010", false, "Navi 10 [Radeon Pro 5700]"),
    row(0x731e, "gfx1010", false, "Radeon RX 5700XTB / Radeon RX 5700B"),
    row(0x731f, "gfx1010", false, "Radeon RX 5700 XT 50th Anniversary / Radeon RX 5700 XT"),
    row(0x7360, "gfx1011", false, "Radeon Pro 5600M / Radeon Pro V520"),
    row(0x7362, "gfx1011", false, "Radeon Pro V540 / Radeon Pro V520"),
    row(0x7340, "gfx1012", false, "Radeon RX 5500M / Radeon RX 5300M"),
    row(0x7341, "gfx1012", false, "Radeon Pro W5500"),
    row(0x7347, "gfx1012", false, "Radeon Pro W5500M"),
    row(0x734f, "gfx1012", false, "Navi 14 [Radeon Pro W5300M]"),
    row(0x13f3, "gfx1013", true, "Cyan Skillfish"),
    row(0x13f9, "gfx1013", true, "Oberon/Navi12Lite"),
    row(0x13fa, "gfx1013", true, "Cyan Skillfish"),
    row(0x13fb, "gfx1013", true, "Cyan Skillfish [PlayStation 5 APU]"),
    row(0x13fc, "gfx1013", true, "Cyan Skillfish"),
    row(0x73a0, "gfx1030", false, "Navi 21"),
    row(0x73a1, "gfx1030", false, "Radeon Pro V620"),
    row(0x73a2, "gfx1030", false, "Navi 21 Pro-XTA [Radeon Pro W6900X]"),
    row(0x73a3, "gfx1030", false, "Radeon Pro W6800"),
    row(0x73a5, "gfx1030", false, "Radeon RX 6950 XT"),
    row(0x73a8, "gfx1030", false, "Navi 21"),
    row(0x73a9, "gfx1030", false, "Navi 21"),
    row(0x73ab, "gfx1030", false, "Navi 21 Pro-XLA [Radeon Pro W6800X/Radeon Pro W6800X Duo]"),
    row(0x73ac, "gfx1030", false, "Navi 21"),
    row(0x73ad, "gfx1030", false, "Navi 21"),
    row(0x73ae, "gfx1030", false, "Radeon Pro V620 MxGPU"),
    row(0x73af, "gfx1030", false, "Radeon RX 6900 XT"),
    row(0x73bf, "gfx1030", false, "Radeon RX 6900 XT / Radeon RX 6800 XT"),
    row(0x73c0, "gfx1031", false, "Navi 22"),
    row(0x73c1, "gfx1031", false, "Navi 22"),
    row(0x73c3, "gfx1031", false, "Navi 22"),
    row(0x73ce, "gfx1031", false, "Navi 22-XL SRIOV MxGPU"),
    row(0x73da, "gfx1031", false, "Navi 22"),
    row(0x73db, "gfx1031", false, "Navi 22"),
    row(0x73dc, "gfx1031", false, "Navi 22"),
    row(0x73dd, "gfx1031", false, "Navi 22"),
    row(0x73de, "gfx1031", false, "Navi 22"),
    row(0x73df, "gfx1031", false, "Radeon RX 6750 XT / Radeon RX 6700 XT"),
    row(0x73e0, "gfx1032", false, "Radeon RX 6600M"),
    row(0x73e1, "gfx1032", false, "Radeon Pro W6600M"),
    row(0x73e2, "gfx1032", false, "Navi 23"),
    row(0x73e3, "gfx1032", false, "Radeon Pro W6600"),
    row(0x73e8, "gfx1032", false, "Navi 23"),
    row(0x73e9, "gfx1032", false, "Navi 23"),
    row(0x73ea, "gfx1032", false, "Navi 23"),
    row(0x73eb, "gfx1032", false, "Navi 23"),
    row(0x73ec, "gfx1032", false, "Navi 23"),
    row(0x73ed, "gfx1032", false, "Navi 23"),
    row(0x73ef, "gfx1032", false, "Radeon RX 6800S / Radeon RX 6650 XT"),
    row(0x73ff, "gfx1032", false, "Radeon RX 6600 XT / Radeon RX 6600M"),
    row(0x163f, "gfx1033", true, "Custom GPU 0405"),
    row(0x7420, "gfx1034", false, "Navi 24"),
    row(0x7421, "gfx1034", false, "Radeon Pro W6500M"),
    row(0x7422, "gfx1034", false, "Radeon Pro W6400"),
    row(0x7423, "gfx1034", false, "Radeon Pro W6300M / Radeon Pro W6300"),
    row(0x7424, "gfx1034", false, "Radeon RX 6300"),
    row(0x743f, "gfx1034", false, "Radeon RX 6500 XT / Radeon RX 6500M"),
    row(0x164d, "gfx1035", true, "Rembrandt"),
    row(0x1681, "gfx1035", true, "Radeon 680M / Radeon 660M"),
    row(0x13c0, "gfx1036", true, "Granite Ridge [Radeon 610M]"),
    row(0x164e, "gfx1036", true, "Radeon 610M"),
    row(0x1506, "gfx1037", true, "Radeon 610M"),
    row(0x7448, "gfx1100", false, "Radeon Pro W7900"),
    row(0x7449, "gfx1100", false, "Radeon Pro W7800 48GB"),
    row(0x744a, "gfx1100", false, "Radeon Pro W7900 Dual Slot"),
    row(0x744b, "gfx1100", false, "Radeon Pro W7900D"),
    row(0x744c, "gfx1100", false, "Radeon RX 7900 XTX / Radeon RX 7900 XT"),
    row(0x745e, "gfx1100", false, "Radeon Pro W7800"),
    row(0x7460, "gfx1101", false, "Radeon Pro V710"),
    row(0x7461, "gfx1101", false, "Radeon Pro V710 MxGPU"),
    row(0x7470, "gfx1101", false, "Radeon Pro W7700"),
    row(0x747e, "gfx1101", false, "Radeon RX 7800 XT / Radeon RX 7800M"),
    row(0x73f0, "gfx1102", false, "Navi 33 [Radeon RX 7600M XT]"),
    row(0x7480, "gfx1102", false, "Radeon Pro W7600 / Radeon RX 7600 XT"),
    row(0x7481, "gfx1102", false, "Navi 33 [Steam Machine]"),
    row(0x7483, "gfx1102", false, "Radeon RX 7600M"),
    row(0x7487, "gfx1102", false, "Navi 33 [Radeon Graphics]"),
    row(0x7489, "gfx1102", false, "Radeon Pro W7500"),
    row(0x748b, "gfx1102", false, "Navi 33 [Radeon Graphics]"),
    row(0x7499, "gfx1102", false, "Radeon Pro W7400 / Radeon RX 7400"),
    row(0x749f, "gfx1102", false, "Navi 33 [Radeon RX 7500]"),
    row(0x15bf, "gfx1103", true, "Radeon 780M Graphics / Radeon 760M Graphics"),
    row(0x15c8, "gfx1103", true, "Radeon 740M Graphics"),
    row(0x1900, "gfx1103", true, "Radeon 780M Graphics / Radeon 760M Graphics"),
    row(0x1901, "gfx1103", true, "Radeon 740M Graphics"),
    row(0x150e, "gfx1150", true, "Radeon 890M Graphics / Radeon 880M Graphics"),
    row(0x1586, "gfx1151", true, "Radeon 8060S Graphics / Radeon 8050S Graphics"),
    row(0x1114, "gfx1152", true, "Radeon 860M Graphics / Radeon 840M Graphics"),
    row(0x7590, "gfx1200", false, "Radeon RX 9060 XT / Radeon RX 9060 XT LP"),
    row(0x7550, "gfx1201", false, "Radeon RX 9070 XT / Radeon RX 9070 GRE"),
    row(0x7551, "gfx1201", false, "Radeon AI Pro R9700 / Radeon AI Pro R9700S"),
];

/// The row of `device_id`, if the table knows it.
pub(crate) fn lookup_row(device_id: u16) -> Option<&'static DeviceIdRow> {
    DEVICE_IDS.iter().find(|row| row.device_id == device_id)
}
