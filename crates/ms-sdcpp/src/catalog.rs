/*
File: crates/ms-sdcpp/src/catalog.rs

Purpose:
The pinned stable-diffusion.cpp release and its package rows: which archive (URL, size,
sha256), which members of it, which libraries are the entry points and which host
requirements apply, per OS x architecture x build. The one owner of these facts.

Key items:
- PinnedRelease / PINNED : tag, commit and the sha256 of the C header the bindings match
- SdCppBuild             : Vulkan / Cpu, with the frozen wire slugs persisted in `user_config`
- PackageRow / ROWS      : one pinned zip per (os, arch, build)
- row_here / builds_here / default_build_here / build_from_choice : the per-machine answers

Notes:
- Provenance of every pin (2026-10-28): sizes and sha256 are GitHub's asset `digest`
  (`gh api repos/leejet/stable-diffusion.cpp/releases/tags/master-1017-be48649`), and each was
  re-hashed locally (`sha256sum` of the downloaded zip) with an identical result. Member lists
  come from `unzip -l` of the same zips; every listed entry is a regular file (the Linux zips
  store the soname "symlinks" as plain copies). Library dependencies were read statically
  (`readelf -d` / `objdump -p`); no downloaded binary was executed.
- Linux ships SEPARATE zips per backend: `…-x86_64.zip` (CPU only) and `…-x86_64-vulkan.zip`
  (the same files plus `libggml-vulkan.so`). Windows likewise (`win-cpu-x64` / `win-vulkan-x64`).
- Members taken: the sd.cpp library, ggml's dispatcher and base library under the names the
  loader resolves (Linux: the sonames `libggml.so.0` / `libggml-base.so.0` from DT_NEEDED), every
  CPU variant (`ggml_backend_load_all_from_path` picks the best one at run time by the
  `<prefix>ggml-cpu-*` name pattern), the Vulkan backend for the Vulkan build, and both licence
  texts. Skipped: `sd-cli` / `sd-server` and their scripts, webp / webm / sharpyuv (only the CLI
  image I/O needs them), and the duplicate unversioned / fully versioned `.so` copies.
- Host requirements: Linux libraries need GLIBC_2.38 (max symbol version of
  `libstable-diffusion.so` / `libggml-base.so.0`); Windows DLLs are MSVC 14.44-linked and import
  MSVCP140, MSVCP140_CODECVT_IDS, VCRUNTIME140, VCRUNTIME140_1 and VCOMP140, which are not
  bundled; the Vulkan backend needs the system loader (`libvulkan.so.1` / `vulkan-1.dll`).
- A new release means: new `PINNED`, new rows (fresh digests, re-listed members), a new
  vendored header (its sha256 is `PINNED.header_sha256`), and a re-check of the floors.
*/

use ms_config::SdCppChoice;
use ms_download::unpack::ArchiveKind;
use ms_log::runtime_log;

/// Identity of the pinned stable-diffusion.cpp release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedRelease {
    /// GitHub release tag; also a path component of the package directory and part of the
    /// busy key and package marker.
    pub tag: &'static str,
    /// Full commit hash the tag points at.
    pub commit: &'static str,
    /// The short commit `sd_commit()` reports (the bindings compare it after loading).
    pub short_commit: &'static str,
    /// sha256 of `include/stable-diffusion.h` at `commit`: the header the hand-written
    /// bindings were written against.
    pub header_sha256: &'static str,
}

/// The one release the app runs.
pub const PINNED: PinnedRelease = PinnedRelease {
    tag: "master-1017-be48649",
    commit: "be486498eadc29dc3cd234d944c2180ed42b1415",
    short_commit: "be48649",
    header_sha256: "d58b35a7c2b3069caa184e7cc8567c78a86d67f2246139ab2dc01a2d5f12a71d",
};

/// A build (compute backend set) of the pinned release. Serialized as its [`Self::wire`] slug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SdCppBuild {
    /// Vulkan GPU backend plus the CPU backends.
    Vulkan,
    /// CPU backends only.
    Cpu,
}

impl SdCppBuild {
    /// Every build, in display order.
    pub const ALL: [Self; 2] = [Self::Vulkan, Self::Cpu];

    /// The frozen slug persisted in `user_config` `General.sdcpp_build` and used in the package
    /// path, the marker and the busy key. Never rename one.
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Vulkan => "vulkan",
            Self::Cpu => "cpu",
        }
    }

    /// Parses [`Self::wire`]; `None` for an unknown slug.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|build| build.wire() == value)
    }

    /// Every build, in display order (the catalog may not offer each one on this machine; see
    /// [`builds_here`]).
    #[must_use]
    pub const fn all() -> [Self; 2] {
        Self::ALL
    }

    /// The i18n key of the build's display name.
    #[must_use]
    pub const fn label_key(self) -> &'static str {
        match self {
            Self::Vulkan => "sdcpp.build.vulkan_label",
            Self::Cpu => "sdcpp.build.cpu_label",
        }
    }

    /// The localized display name ([`Self::label_key`] resolved in the active locale).
    #[must_use]
    pub fn label(self) -> &'static str {
        ms_i18n::resolve_key(self.label_key())
    }
}

/// One pinned archive of the release for one (os, arch, build).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageRow {
    /// `std::env::consts::OS` value the row applies to.
    pub os: &'static str,
    /// `std::env::consts::ARCH` value the row applies to.
    pub arch: &'static str,
    /// The build this row installs.
    pub build: SdCppBuild,
    /// Download URL of the release asset (embeds `PINNED.tag`).
    pub url: &'static str,
    /// Exact archive size in bytes.
    pub size: u64,
    /// Lower-case hex sha256 of the archive.
    pub sha256: &'static str,
    /// Archive format (matches `ms_download::unpack::ArchiveKind`).
    pub archive: ArchiveKind,
    /// Archive members to install. Every one sits at the archive root, so its archive path is
    /// also its file name in the package directory.
    pub members: &'static [&'static str],
    /// The sd.cpp library the worker loads (a member).
    pub sd_library: &'static str,
    /// The ggml dispatcher library exporting `ggml_backend_load_all_from_path` (a member).
    pub ggml_library: &'static str,
    /// Minimum VC++ runtime (major, minor) on Windows; `None` elsewhere.
    pub min_vc_runtime: Option<(u16, u16)>,
    /// Minimum glibc (major, minor) on Linux; `None` elsewhere.
    pub min_glibc: Option<(u32, u32)>,
    /// Whether the build needs the system Vulkan loader.
    pub needs_vulkan_loader: bool,
}

impl PackageRow {
    /// File name of the archive (the URL's last path segment).
    #[must_use]
    pub fn archive_file_name(&self) -> &'static str {
        self.url.rsplit('/').next().unwrap_or(self.url)
    }
}

/// Base URL of the pinned release's assets; must stay in sync with `PINNED.tag` (a test checks
/// that every row URL embeds the tag).
macro_rules! release_url {
    ($asset:literal) => {
        concat!("https://github.com/leejet/stable-diffusion.cpp/releases/download/master-1017-be48649/", $asset)
    };
}

/// Linux members shared by both builds (see the file header for the selection rule).
macro_rules! linux_common_members {
    ($($extra:literal),*) => {
        &[
            "libstable-diffusion.so",
            "libggml.so.0",
            "libggml-base.so.0",
            "libggml-cpu-alderlake.so",
            "libggml-cpu-cannonlake.so",
            "libggml-cpu-cascadelake.so",
            "libggml-cpu-cooperlake.so",
            "libggml-cpu-haswell.so",
            "libggml-cpu-icelake.so",
            "libggml-cpu-ivybridge.so",
            "libggml-cpu-piledriver.so",
            "libggml-cpu-sandybridge.so",
            "libggml-cpu-sapphirerapids.so",
            "libggml-cpu-skylakex.so",
            "libggml-cpu-sse42.so",
            "libggml-cpu-x64.so",
            "libggml-cpu-zen4.so",
            $($extra,)*
            "ggml.txt",
            "stable-diffusion.cpp.txt",
        ]
    };
}

/// Windows members shared by both builds (9 CPU variants; there is no zen4 build on Windows).
macro_rules! windows_common_members {
    ($($extra:literal),*) => {
        &[
            "stable-diffusion.dll",
            "ggml.dll",
            "ggml-base.dll",
            "ggml-cpu-alderlake.dll",
            "ggml-cpu-cannonlake.dll",
            "ggml-cpu-cascadelake.dll",
            "ggml-cpu-haswell.dll",
            "ggml-cpu-icelake.dll",
            "ggml-cpu-sandybridge.dll",
            "ggml-cpu-skylakex.dll",
            "ggml-cpu-sse42.dll",
            "ggml-cpu-x64.dll",
            $($extra,)*
            "ggml.txt",
            "stable-diffusion.cpp.txt",
        ]
    };
}

/// glibc floor of the Linux zips (max `GLIBC_` symbol version they import).
const LINUX_MIN_GLIBC: (u32, u32) = (2, 38);
/// VC++ runtime floor of the Windows zips (their PE linker version).
const WINDOWS_MIN_VC_RUNTIME: (u16, u16) = (14, 44);

/// Every pinned row (see the file header for the provenance of each value).
pub const ROWS: &[PackageRow] = &[
    PackageRow {
        os: "linux",
        arch: "x86_64",
        build: SdCppBuild::Vulkan,
        url: release_url!("sd-master-be48649-bin-Linux-Ubuntu-24.04-x86_64-vulkan.zip"),
        size: 37_646_815,
        sha256: "794c913d077aaf72025e772b2acfb6a30f49156e92d072461ae15ce958d95c14",
        archive: ArchiveKind::Zip,
        members: linux_common_members!("libggml-vulkan.so"),
        sd_library: "libstable-diffusion.so",
        ggml_library: "libggml.so.0",
        min_vc_runtime: None,
        min_glibc: Some(LINUX_MIN_GLIBC),
        needs_vulkan_loader: true,
    },
    PackageRow {
        os: "linux",
        arch: "x86_64",
        build: SdCppBuild::Cpu,
        url: release_url!("sd-master-be48649-bin-Linux-Ubuntu-24.04-x86_64.zip"),
        size: 27_322_693,
        sha256: "1016ea927e00b0176ca23aa9f5dc5de68290ea7e115eb3fe6c2399acca41701f",
        archive: ArchiveKind::Zip,
        members: linux_common_members!(),
        sd_library: "libstable-diffusion.so",
        ggml_library: "libggml.so.0",
        min_vc_runtime: None,
        min_glibc: Some(LINUX_MIN_GLIBC),
        needs_vulkan_loader: false,
    },
    PackageRow {
        os: "windows",
        arch: "x86_64",
        build: SdCppBuild::Vulkan,
        url: release_url!("sd-master-be48649-bin-win-vulkan-x64.zip"),
        size: 29_744_133,
        sha256: "833414dfe255d9709c6184e699017028ee8d5522058ad53b201d9602805c6b41",
        archive: ArchiveKind::Zip,
        members: windows_common_members!("ggml-vulkan.dll"),
        sd_library: "stable-diffusion.dll",
        ggml_library: "ggml.dll",
        min_vc_runtime: Some(WINDOWS_MIN_VC_RUNTIME),
        min_glibc: None,
        needs_vulkan_loader: true,
    },
    PackageRow {
        os: "windows",
        arch: "x86_64",
        build: SdCppBuild::Cpu,
        url: release_url!("sd-master-be48649-bin-win-cpu-x64.zip"),
        size: 18_080_575,
        sha256: "f0f76c8176a90cafc431ca7d5049cbe8c5cea61ca7cb92b78e07361d41d8fb21",
        archive: ArchiveKind::Zip,
        members: windows_common_members!(),
        sd_library: "stable-diffusion.dll",
        ggml_library: "ggml.dll",
        min_vc_runtime: Some(WINDOWS_MIN_VC_RUNTIME),
        min_glibc: None,
        needs_vulkan_loader: false,
    },
];

/// The row of `build` for (`os`, `arch`), if the catalog has one.
#[must_use]
pub fn row_for(os: &str, arch: &str, build: SdCppBuild) -> Option<&'static PackageRow> {
    ROWS.iter().find(|row| row.os == os && row.arch == arch && row.build == build)
}

/// The row of `build` for this machine's OS and architecture, if the catalog has one. Pure:
/// it does not consult `PlatformFeature::SdCppRuntime` (macOS and Android simply have no row).
#[must_use]
pub fn row_here(build: SdCppBuild) -> Option<&'static PackageRow> {
    row_for(std::env::consts::OS, std::env::consts::ARCH, build)
}

/// The builds the catalog offers on this machine, in display order.
#[must_use]
pub fn builds_here() -> Vec<SdCppBuild> {
    SdCppBuild::ALL.into_iter().filter(|build| row_here(*build).is_some()).collect()
}

/// Whether the catalog offers ANY build on this machine: `!builds_here().is_empty()` without
/// the allocation, for per-frame gates (`SdCppStatus::catalog_problem`, the `AiButton` gate).
#[must_use]
pub fn offers_a_build_here() -> bool {
    SdCppBuild::ALL.into_iter().any(|build| row_here(build).is_some())
}

/// The default build on this machine: Vulkan where it is offered, else CPU, else `None`.
#[must_use]
pub fn default_build_here() -> Option<SdCppBuild> {
    default_among(&builds_here())
}

/// [`default_build_here`] over an explicit list of offered builds.
fn default_among(offered: &[SdCppBuild]) -> Option<SdCppBuild> {
    [SdCppBuild::Vulkan, SdCppBuild::Cpu].into_iter().find(|build| offered.contains(build))
}

/// The build the persisted `choice` selects on this machine: the chosen slug when the catalog
/// offers it here, otherwise (absent, unknown or not offered here — the last two logged) the
/// [`default_build_here`]. `None` only when nothing is offered here.
#[must_use]
pub fn build_from_choice(choice: &SdCppChoice) -> Option<SdCppBuild> {
    choose_among(choice, &builds_here())
}

/// [`build_from_choice`] over an explicit list of offered builds.
fn choose_among(choice: &SdCppChoice, offered: &[SdCppBuild]) -> Option<SdCppBuild> {
    if let Some(slug) = choice.build.as_deref() {
        match SdCppBuild::from_wire(slug) {
            Some(build) if offered.contains(&build) => return Some(build),
            Some(build) => runtime_log::log_warn(format!("[sdcpp] chosen build '{}' is not offered on this machine; using the default", build.wire())),
            None => runtime_log::log_warn(format!("[sdcpp] unknown build slug '{slug}' in General.sdcpp_build; using the default")),
        }
    }
    default_among(offered)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn is_hex64(value: &str) -> bool {
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    #[test]
    fn every_row_pins_size_sha_and_tag() {
        assert!(is_hex64(PINNED.header_sha256));
        assert!(PINNED.commit.starts_with(PINNED.short_commit));
        assert!(PINNED.tag.ends_with(PINNED.short_commit));
        for row in ROWS {
            assert!(row.size > 0, "{}", row.url);
            assert!(is_hex64(row.sha256), "{}", row.url);
            assert!(row.url.contains(&format!("/download/{}/", PINNED.tag)), "{}", row.url);
            assert!(row.url.starts_with("https://github.com/leejet/stable-diffusion.cpp/releases/download/"));
            assert!(std::path::Path::new(row.archive_file_name()).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("zip")));
            assert_eq!(row.archive, ArchiveKind::Zip);
        }
    }

    #[test]
    fn members_are_unique_flat_names_holding_the_primary_libraries() {
        for row in ROWS {
            let unique: HashSet<&str> = row.members.iter().copied().collect();
            assert_eq!(unique.len(), row.members.len(), "{}", row.url);
            for member in row.members {
                assert!(!member.is_empty() && !member.contains('/') && !member.contains('\\') && *member != "." && *member != "..", "{member}");
            }
            assert!(unique.contains(row.sd_library), "{}", row.url);
            assert!(unique.contains(row.ggml_library), "{}", row.url);
            let vulkan_backend = if row.os == "windows" { "ggml-vulkan.dll" } else { "libggml-vulkan.so" };
            assert_eq!(unique.contains(vulkan_backend), row.build == SdCppBuild::Vulkan, "{}", row.url);
            assert_eq!(row.needs_vulkan_loader, row.build == SdCppBuild::Vulkan);
        }
    }

    #[test]
    fn linux_and_windows_have_both_builds_with_their_floors() {
        for os in ["linux", "windows"] {
            for build in SdCppBuild::ALL {
                let row = row_for(os, "x86_64", build).unwrap_or_else(|| panic!("no {os} {build:?} row"));
                assert_eq!(row.min_glibc.is_some(), os == "linux");
                assert_eq!(row.min_vc_runtime.is_some(), os == "windows");
            }
        }
        assert!(row_for("macos", "aarch64", SdCppBuild::Cpu).is_none());
        assert!(row_for("android", "aarch64", SdCppBuild::Cpu).is_none());
        let pairs: HashSet<(&str, &str, SdCppBuild)> = ROWS.iter().map(|row| (row.os, row.arch, row.build)).collect();
        assert_eq!(pairs.len(), ROWS.len(), "one row per (os, arch, build)");
    }

    #[test]
    fn wire_round_trips_and_labels_are_distinct() {
        for build in SdCppBuild::all() {
            assert_eq!(SdCppBuild::from_wire(build.wire()), Some(build));
        }
        assert_eq!(SdCppBuild::from_wire("cuda"), None);
        for build in SdCppBuild::all() {
            assert_eq!(serde_json::to_string(&build).expect("serialize build"), format!("\"{}\"", build.wire()));
        }
        assert_ne!(SdCppBuild::Vulkan.label_key(), SdCppBuild::Cpu.label_key());
    }

    #[test]
    fn choice_resolution_prefers_offered_choice_then_vulkan_then_cpu() {
        let both = [SdCppBuild::Vulkan, SdCppBuild::Cpu];
        let choose = |slug: Option<&str>, offered: &[SdCppBuild]| choose_among(&SdCppChoice { build: slug.map(str::to_owned), ..SdCppChoice::default() }, offered);
        assert_eq!(choose(None, &both), Some(SdCppBuild::Vulkan));
        assert_eq!(choose(Some("cpu"), &both), Some(SdCppBuild::Cpu));
        assert_eq!(choose(Some("bogus"), &both), Some(SdCppBuild::Vulkan));
        assert_eq!(choose(Some("vulkan"), &[SdCppBuild::Cpu]), Some(SdCppBuild::Cpu));
        assert_eq!(choose(Some("cpu"), &[]), None);
    }

    #[test]
    fn this_machine_answers_match_the_rows() {
        let here = builds_here();
        if cfg!(all(any(target_os = "linux", target_os = "windows"), target_arch = "x86_64")) {
            assert_eq!(here, vec![SdCppBuild::Vulkan, SdCppBuild::Cpu]);
            assert_eq!(default_build_here(), Some(SdCppBuild::Vulkan));
        } else {
            assert_eq!(here, Vec::<SdCppBuild>::new());
            assert_eq!(default_build_here(), None);
        }
    }
}
