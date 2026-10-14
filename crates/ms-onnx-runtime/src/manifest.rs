/*
File: onnx_runtime/manifest.rs

Purpose:
The integrity manifest for the native ONNX Runtime loader. It embeds the pinned
`ort_manifest.json` at compile time and exposes typed lookups plus SHA256 helpers
used by `onnx_runtime::mod` to download and verify the official onnxruntime
release archive for the current platform/provider.

Key types:
- ArchiveKind    : how a release archive is packed (Zip / TarGz / TarZst)
- ManifestSource : one download+extract recipe (url, sha256, archive, members)
- ManifestEntry  : one (os, arch, provider, build) -> version + primary source + extras
- BundledEntry   : one (os, arch, provider, build) -> version + the library the app
  package ships (Android APK); no download recipe
- BuildSource    : what `lookup_build` found — a download row or a bundled row
- OrtBuildSourceKind : the public, data-free kind of a `BuildSource`
- Manifest       : the whole parsed document (default version + entries + bundled)

Key functions:
- lookup            : find the DOWNLOAD entry for (os, arch, provider_id, build)
- lookup_build      : find the row for (os, arch, build) in either table — the build-keyed primary
- build_version     : the manifest version for a build on the current platform
- build_shipped_here : whether a build has a row for the current platform (no allocation)
- build_source_kind_here : download or bundled, for the current platform
- ort_downloads_offered_here : whether any build is downloadable on the current platform
- provider_version  : back-compat shim — the version of a provider's DEFAULT build
- current_platform  : the (os, arch) pair this binary was built for
- platform_keys     : pure target_os/target_arch -> manifest (os, arch) key mapping
- sha256_hex        : lowercase hex SHA256 of a byte slice (pure)
- sha256_hex_of_file: streaming lowercase hex SHA256 of a file
- verify_sha256_file: compare a file's SHA256 against an expected hex string

Notes:
The manifest is keyed by `(os, arch, provider, build)`: a "build" (see `builds.rs`) is
a concrete onnxruntime binary of a specific version exposing a specific EP set. Several
builds may share one `provider` but differ by version/binary — e.g. the `cuda12`
(1.24.1) and `cuda13` (1.27.0) builds both carry `provider = "cuda"`. Each entry pins
its OWN ONNX Runtime `version`, so builds coexist on different releases. `ORT_VERSION`
is only a legacy fallback floor used when no build entry exists on a platform.
Non-GPU/non-OpenVINO entries carry a real, end-to-end-verifiable SHA256 (GitHub release
digests / PyPI wheel digests). `sha256` is modeled as `Option` so the large GPU builds
can ship before a maintainer pins their hash; see the unverified-hash handling in
`onnx_runtime::mod` and the module README. An entry may pull additional libraries from
extra archives via `additional_sources` (multi-source entries), each extracted into the
same cache directory as the primary source.
Bundled rows live in their own `bundled` table, so the download schema stays strict
(`url` / `archive` / `dylib_member` required): a bundled build has no recipe, its library
is part of the app package. A `(os, arch, build)` key is in at most one table (test).
*/

use std::fs::File;
use std::path::Path;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use super::OrtRuntimeError;

/// The legacy fallback ONNX Runtime version.
///
/// This is the `ort_version` field of the embedded manifest (enforced by a unit
/// test). It is NOT any build's version — every entry pins its own
/// [`ManifestEntry::version`] (CPU / CoreML / WebGPU on 1.27.0, DirectML on 1.24.4,
/// CUDA 12 / OpenVINO on 1.24.1, CUDA 13 on 1.27.0). It survives only as the floor
/// callers fall back to when a build has no entry on the current platform. ONNX
/// Runtime must stay `>= 1.18` because the pinned `ort` (`2.0.0-rc.12`, `api-18`)
/// hard-errors on older libraries.
pub const ORT_VERSION: &str = "1.20.1";

/// The pinned manifest, embedded into the binary at compile time.
const MANIFEST_JSON: &str = include_str!("ort_manifest.json");

/// How an ONNX Runtime release archive is packed.
///
/// `TarZst` has no Phase-0 CPU entry but is a fully supported extraction format
/// so a maintainer can add zstd-packed provider archives without touching code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveKind {
    /// PKZIP archive (Windows release assets).
    Zip,
    /// gzip-compressed tar (Linux / macOS release assets).
    TarGz,
    /// zstd-compressed tar.
    TarZst,
}

/// One additional download+extract source for a multi-source manifest entry.
///
/// A [`ManifestEntry`] can name extra archives whose libraries must land in the
/// SAME cache directory as the primary onnxruntime library. Each source is
/// downloaded, verified, and extracted independently by `onnx_runtime::mod`, using
/// the exact same download/verify/extract path as the primary source. `members`
/// lists the archive-internal paths to extract; each is flattened to its filename
/// in the cache directory (path traversal is impossible).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ManifestSource {
    /// Absolute download URL of the additional archive.
    pub url: String,
    /// Expected lowercase hex SHA256 of the archive, or `None` if unpinned.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Packing format of the archive at `url`.
    pub archive: ArchiveKind,
    /// Archive-internal paths of libraries to extract into the cache directory.
    pub members: Vec<String>,
}

/// One manifest row: the download + extraction recipe for a single platform and
/// execution provider.
///
/// `version` pins the ONNX Runtime release this entry targets; it scopes the cache
/// directory and the versioned library filename, so providers can coexist on
/// different releases. `dylib_member` is the archive-internal path of the primary
/// onnxruntime library (the one whose resolved on-disk path is returned to the
/// caller). `extra_members` lists additional archive-internal library paths from
/// the SAME archive that must be placed next to it (e.g.
/// `onnxruntime_providers_shared.dll`); it may be empty. `additional_sources` names
/// extra archives to also download and extract into the cache directory (empty for
/// single-archive providers). `sha256` is a lowercase hex digest of the whole
/// primary archive, or `None` when a hash has not yet been pinned for this entry.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ManifestEntry {
    /// Normalized OS key: `"windows"`, `"linux"`, or `"macos"`.
    pub os: String,
    /// Normalized architecture key: `"x86_64"` or `"aarch64"`.
    pub arch: String,
    /// Provider id, matching `ms_onnx::ExecutionProvider::id`. Several builds may
    /// share one provider (e.g. `cuda12` and `cuda13` are both `"cuda"`).
    pub provider: String,
    /// Build slug — the stable discriminator that makes the key unique, matching a
    /// [`super::builds::OrtBuild::slug`] (e.g. `"cpu"`, `"cuda12"`, `"cuda13"`,
    /// `"openvino"`). Together with `(os, arch, provider)` it identifies one row.
    pub build: String,
    /// ONNX Runtime version this entry targets (e.g. `"1.24.1"` or `"1.27.0"`).
    ///
    /// Scopes the per-provider cache directory and the versioned library filename;
    /// see `onnx_runtime::ort_dylib_dir`. Required in every row.
    pub version: String,
    /// Absolute download URL of the primary release archive.
    pub url: String,
    /// Expected lowercase hex SHA256 of the primary archive, or `None` if unpinned.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Packing format of the primary archive at `url`.
    pub archive: ArchiveKind,
    /// Archive-internal path of the primary onnxruntime library.
    pub dylib_member: String,
    /// Archive-internal paths of additional libraries to place alongside it.
    #[serde(default)]
    pub extra_members: Vec<String>,
    /// Additional archives whose libraries are also extracted into the same cache
    /// directory (multi-source entry). Empty for single-archive providers.
    #[serde(default)]
    pub additional_sources: Vec<ManifestSource>,
}

/// One bundled-build row: an onnxruntime library that ships INSIDE the app package (the
/// Android APK merges it from the official `onnxruntime-android` AAR) instead of being
/// downloaded. Resolution never touches the network for it: the library is looked up in
/// the package's native library directory.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundledEntry {
    /// Normalized OS key (`"android"` for every shipped row).
    pub os: String,
    /// Normalized architecture key (`"aarch64"`).
    pub arch: String,
    /// Provider id of the build's headline EP, matching `ms_onnx::ExecutionProvider::id`.
    pub provider: String,
    /// Build slug, matching a [`super::builds::OrtBuild::slug`]. Several bundled builds may
    /// name the same library (one binary, different EP sets).
    pub build: String,
    /// ONNX Runtime version of the bundled library. Must equal the catalog build's
    /// version AND the AAR version pinned in `android/app/build.gradle.kts` (both tested).
    pub version: String,
    /// File name of the library in the package's native library directory.
    pub library: String,
}

/// What [`lookup_build`] found for a build on a platform.
#[derive(Debug, Clone, Copy)]
pub enum BuildSource {
    /// A download recipe (official release archive or wheel).
    Download(&'static ManifestEntry),
    /// A library shipped inside the app package.
    Bundled(&'static BundledEntry),
}

impl BuildSource {
    /// The ONNX Runtime version of the row.
    #[must_use]
    pub fn version(self) -> &'static str {
        match self {
            BuildSource::Download(entry) => &entry.version,
            BuildSource::Bundled(entry) => &entry.version,
        }
    }

    /// The provider id of the row.
    #[must_use]
    pub fn provider(self) -> &'static str {
        match self {
            BuildSource::Download(entry) => &entry.provider,
            BuildSource::Bundled(entry) => &entry.provider,
        }
    }

    /// The data-free kind of this source.
    #[must_use]
    pub fn kind(self) -> OrtBuildSourceKind {
        match self {
            BuildSource::Download(_) => OrtBuildSourceKind::Download,
            BuildSource::Bundled(_) => OrtBuildSourceKind::Bundled,
        }
    }
}

/// How a build's onnxruntime library is obtained on the current platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrtBuildSourceKind {
    /// Downloaded on first use, verified and extracted into the per-build cache.
    Download,
    /// Shipped inside the app package; never downloaded.
    Bundled,
}

/// The parsed manifest document.
#[derive(Debug, Clone, serde::Deserialize)]
struct Manifest {
    /// Legacy/default ONNX Runtime version; must equal [`ORT_VERSION`]. Individual
    /// entries pin their own [`ManifestEntry::version`] and may differ from this.
    ort_version: String,
    /// All known (os, arch, provider) download recipes.
    entries: Vec<ManifestEntry>,
    /// Builds shipped inside the app package (no download recipe).
    bundled: Vec<BundledEntry>,
}

/// Parses (once) and returns the embedded manifest.
///
/// # Panics
/// Panics if the compiled-in `ort_manifest.json` is not valid JSON for the typed
/// `Manifest` model. This is embedded, version-controlled data validated by the
/// `manifest_parses` unit test, so a failure here is an impossible-in-a-tested-build
/// programmer error, not a runtime/user condition. Failing loudly is deliberate:
/// silently degrading (e.g. returning an empty manifest) would hide a broken build.
fn manifest() -> &'static Manifest {
    static CACHE: OnceLock<Manifest> = OnceLock::new();
    CACHE.get_or_init(|| match serde_json::from_str::<Manifest>(MANIFEST_JSON) {
        Ok(parsed) => parsed,
        Err(err) => panic!("embedded ort_manifest.json is invalid: {err}"),
    })
}

/// Finds the manifest entry for `(os, arch, provider_id, build)`, if one exists.
///
/// The full four-part key. Returns `None` when no row matches; the caller maps that
/// to a typed `OrtRuntimeError::NoManifestEntry`. Most resolution paths know only the
/// build slug — prefer [`lookup_build`] there.
#[must_use]
pub fn lookup(
    os: &str,
    arch: &str,
    provider_id: &str,
    build: &str,
) -> Option<&'static ManifestEntry> {
    manifest().entries.iter().find(|entry| {
        entry.os == os
            && entry.arch == arch
            && entry.provider == provider_id
            && entry.build == build
    })
}

/// Finds the row for `(os, arch, build)` in the download table or the bundled table.
///
/// The primary lookup for resolution: a build slug is globally unique and a key is in at
/// most one table, so the `(os, arch, build)` triple identifies exactly one row. Returns
/// `None` when the build is not shipped on this platform; the caller maps that to a typed
/// `OrtRuntimeError::NoManifestEntry`.
#[must_use]
pub fn lookup_build(os: &str, arch: &str, build: &str) -> Option<BuildSource> {
    lookup_download(os, arch, build).map(BuildSource::Download).or_else(|| lookup_bundled(os, arch, build).map(BuildSource::Bundled))
}

/// The download row for `(os, arch, build)`, if any.
#[must_use]
fn lookup_download(os: &str, arch: &str, build: &str) -> Option<&'static ManifestEntry> {
    manifest()
        .entries
        .iter()
        .find(|entry| entry.os == os && entry.arch == arch && entry.build == build)
}

/// The bundled row for `(os, arch, build)`, if any.
#[must_use]
fn lookup_bundled(os: &str, arch: &str, build: &str) -> Option<&'static BundledEntry> {
    manifest().bundled.iter().find(|entry| entry.os == os && entry.arch == arch && entry.build == build)
}

/// The ONNX Runtime version pinned for build `build` on the CURRENT platform.
///
/// Returns the matching entry's [`ManifestEntry::version`] for the built target's
/// `(os, arch)`, or `None` when the build has no entry there. Callers use this to
/// scope per-build state to the exact version that will be resolved.
#[must_use]
pub fn build_version(build: &str) -> Option<String> {
    let (os, arch) = current_platform();
    lookup_build(os, arch, build).map(|source| source.version().to_string())
}

/// Whether build `build` has a row (download or bundled) for the CURRENT platform's
/// `(os, arch)`.
///
/// Answers the same question as `build_version(build).is_some()` without cloning the
/// version, so per-frame callers (the AI panel's build picker, through the native
/// availability rule) stay allocation-free. [`current_platform`] remains the one owner
/// of the os/arch mapping.
#[must_use]
pub fn build_shipped_here(build: &str) -> bool {
    let (os, arch) = current_platform();
    lookup_build(os, arch, build).is_some()
}

/// How build `build`'s library is obtained on the CURRENT platform, or `None` when the
/// build is not shipped here. Allocation-free (per-frame callers: the AI panel hides the
/// download affordances of a bundled build).
#[must_use]
pub fn build_source_kind_here(build: &str) -> Option<OrtBuildSourceKind> {
    let (os, arch) = current_platform();
    lookup_build(os, arch, build).map(BuildSource::kind)
}

/// Whether any build can be DOWNLOADED on the current platform. `false` where every
/// shipped build is bundled in the app package (Android): there, a build that is not
/// shipped can never be obtained, so the AI panel lists only the shipped builds.
#[must_use]
pub fn ort_downloads_offered_here() -> bool {
    let (os, arch) = current_platform();
    manifest().entries.iter().any(|entry| entry.os == os && entry.arch == arch)
}

/// Back-compat shim: the version of `provider`'s DEFAULT build on the CURRENT platform.
///
/// Bridges the old provider-centric API (still used by the native-runtime guard-scope
/// key) to the build catalog by mapping the provider to its default build
/// (`builds::default_build_for_provider`) and returning that build's
/// [`build_version`]. Returns `None` when that build has no entry on this platform.
/// The build-selection task replaces the remaining callers with direct
/// [`build_version`] calls.
#[must_use]
pub fn provider_version(provider: ms_onnx::ExecutionProvider) -> Option<String> {
    build_version(super::builds::default_build_for_provider(provider))
}

/// The `(os, arch)` platform keys for the target this binary was built for.
///
/// [`platform_keys`] applied to the build target (`std::env::consts::{OS, ARCH}` are the
/// compile-time `target_os` / `target_arch` names), so it is const-foldable.
#[must_use]
pub fn current_platform() -> (&'static str, &'static str) {
    platform_keys(std::env::consts::OS, std::env::consts::ARCH)
}

/// Maps a `target_os` / `target_arch` pair to the manifest's `(os, arch)` key scheme.
///
/// `os` is `"windows"`, `"macos"`, `"android"` or `"linux"`; `arch` is `"aarch64"` or
/// `"x86_64"`. Android gets its own key, never `"linux"`: the official Linux archives are
/// glibc builds that cannot load on Android's bionic; Android's rows are the `bundled`
/// ones (the library inside the APK). Any other unix falls through to `"linux"`
/// intentionally (no matching archive -> the typed no-entry miss). Pure.
#[must_use]
pub fn platform_keys(target_os: &str, target_arch: &str) -> (&'static str, &'static str) {
    let os = match target_os {
        "windows" => "windows",
        "macos" => "macos",
        "android" => "android",
        _ => "linux",
    };
    let arch = if target_arch == "aarch64" { "aarch64" } else { "x86_64" };
    (os, arch)
}

/// Lowercase hex SHA256 of `data`.
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex_lower(&hasher.finalize())
}

/// Streaming lowercase hex SHA256 of the file at `path`.
///
/// # Errors
/// Returns [`OrtRuntimeError::Io`] if the file cannot be opened or read.
pub fn sha256_hex_of_file(path: &Path) -> Result<String, OrtRuntimeError> {
    let mut file = File::open(path).map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.manifest.checksum_open_error", path = path.display(), err = err))
    })?;
    let mut hasher = Sha256::new();
    // `Sha256` implements `std::io::Write`, so the file is hashed in a streaming
    // pass without buffering the whole (up to ~65 MB) archive in memory.
    std::io::copy(&mut file, &mut hasher).map_err(|err| {
        OrtRuntimeError::Io(tf!("onnx_runtime.manifest.checksum_read_error", path = path.display(), err = err))
    })?;
    Ok(hex_lower(&hasher.finalize()))
}

/// Verifies that the file at `path` has SHA256 equal to `expected_hex`.
///
/// Comparison is case-insensitive. On mismatch the file is left in place for the
/// caller to remove.
///
/// # Errors
/// - [`OrtRuntimeError::Io`] if the file cannot be read.
/// - [`OrtRuntimeError::ChecksumMismatch`] if the digest differs from `expected_hex`.
pub fn verify_sha256_file(path: &Path, expected_hex: &str) -> Result<(), OrtRuntimeError> {
    let actual = sha256_hex_of_file(path)?;
    if actual.eq_ignore_ascii_case(expected_hex) {
        Ok(())
    } else {
        Err(OrtRuntimeError::ChecksumMismatch {
            expected: expected_hex.to_string(),
            actual,
        })
    }
}

/// Lowercase hex encoding of a byte buffer without any fallible formatting call.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: [u8; 16] = *b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_and_matches_pinned_version() {
        let m = manifest();
        assert_eq!(m.ort_version, ORT_VERSION);
        assert!(!m.entries.is_empty());
    }

    #[test]
    fn every_entry_carries_a_version_and_build() {
        // Every row must pin a non-empty `version` AND a non-empty `build` slug (the
        // discriminator that makes `(os, arch, provider, build)` unique).
        for entry in &manifest().entries {
            assert!(
                !entry.version.is_empty(),
                "entry {}/{}/{}/{} lacks a version",
                entry.os,
                entry.arch,
                entry.provider,
                entry.build
            );
            assert!(
                !entry.build.is_empty(),
                "entry {}/{}/{} lacks a build slug",
                entry.os,
                entry.arch,
                entry.provider
            );
        }
    }

    #[test]
    fn build_versions_match_the_catalog() {
        // Each entry's `version` must equal its catalog build's `onnx_version`
        // (the manifest and the `builds.rs` catalog are one source of truth).
        for entry in &manifest().entries {
            let build = super::super::builds::build_by_slug(&entry.build)
                .unwrap_or_else(|| panic!("no catalog build for slug '{}'", entry.build));
            assert_eq!(
                entry.version, build.onnx_version,
                "version mismatch for build '{}'",
                entry.build
            );
        }
    }

    #[test]
    fn webgpu_entries_present_on_all_desktop_platforms() {
        // WebGPU ships the onnxruntime-webgpu 1.27.0 wheel per platform (Dawn is
        // statically linked into the main library — there is NO separate
        // `webgpu_dawn` sidecar in the 1.27.0 wheels).
        let win = lookup_download("windows", "x86_64", "webgpu").expect("windows webgpu entry");
        assert_eq!(win.version, "1.27.0");
        assert_eq!(win.archive, ArchiveKind::Zip);
        assert_eq!(win.dylib_member, "onnxruntime/capi/onnxruntime.dll");
        assert_eq!(win.sha256.as_deref().map(str::len), Some(64));
        // The 1.27.0 Windows wheel already bundles the DirectX Shader Compiler
        // runtime (dxil.dll + dxcompiler.dll) that Dawn's D3D12 backend needs, so
        // they are extracted from the wheel itself — no separate DXC download.
        assert!(win.extra_members.iter().any(|m| m.ends_with("dxil.dll")));
        assert!(win.extra_members.iter().any(|m| m.ends_with("dxcompiler.dll")));
        assert!(
            win.extra_members
                .iter()
                .any(|m| m.ends_with("onnxruntime_providers_shared.dll"))
        );

        let linux = lookup_download("linux", "x86_64", "webgpu").expect("linux webgpu entry");
        assert_eq!(linux.version, "1.27.0");
        assert_eq!(linux.archive, ArchiveKind::Zip);
        assert_eq!(
            linux.dylib_member,
            "onnxruntime/capi/libonnxruntime.so.1.27.0"
        );
        assert_eq!(linux.sha256.as_deref().map(str::len), Some(64));

        let mac = lookup_download("macos", "aarch64", "webgpu").expect("macos webgpu entry");
        assert_eq!(mac.version, "1.27.0");
        assert_eq!(mac.archive, ArchiveKind::Zip);
        assert_eq!(mac.dylib_member, "onnxruntime/capi/libonnxruntime.1.27.0.dylib");
        assert_eq!(mac.sha256.as_deref().map(str::len), Some(64));
    }

    #[test]
    fn build_version_reports_per_build_version_on_current_platform() {
        // The test host is linux x86_64.
        assert_eq!(build_version("cpu").as_deref(), Some("1.27.0"));
        assert_eq!(build_version("webgpu").as_deref(), Some("1.27.0"));
        assert_eq!(build_version("cuda13").as_deref(), Some("1.27.0"));
        assert_eq!(build_version("cuda12").as_deref(), Some("1.24.1"));
        assert_eq!(build_version("openvino").as_deref(), Some("1.24.1"));
        // DirectML/CoreML have no linux entry; QNN has no entry at all.
        assert!(build_version("directml").is_none());
        assert!(build_version("coreml").is_none());
        assert!(build_version("qnn").is_none());
    }

    #[test]
    fn provider_version_shim_reports_default_build_version() {
        // The shim maps a provider to its default build, then reads that build's
        // version. On the linux test host: CPU -> cpu@1.27.0, WebGPU -> webgpu@1.27.0,
        // CUDA -> cuda13@1.27.0, OpenVINO -> openvino@1.24.1.
        assert_eq!(
            provider_version(ms_onnx::ExecutionProvider::Cpu).as_deref(),
            Some("1.27.0")
        );
        assert_eq!(
            provider_version(ms_onnx::ExecutionProvider::WebGpu).as_deref(),
            Some("1.27.0")
        );
        assert_eq!(
            provider_version(ms_onnx::ExecutionProvider::Cuda).as_deref(),
            Some("1.27.0")
        );
        assert_eq!(
            provider_version(ms_onnx::ExecutionProvider::OpenVino).as_deref(),
            Some("1.24.1")
        );
        // DirectML/CoreML default builds have no linux entry.
        assert!(provider_version(ms_onnx::ExecutionProvider::DirectMl).is_none());
        assert!(provider_version(ms_onnx::ExecutionProvider::CoreMl).is_none());
    }

    #[test]
    fn additional_sources_default_to_empty_and_parse_when_present() {
        // Existing rows omit `additional_sources` -> it defaults to empty (serde
        // back-compat with the pre-multi-source JSON).
        let cpu = lookup_download("linux", "x86_64", "cpu").expect("linux cpu entry");
        assert!(cpu.additional_sources.is_empty());

        // A multi-source entry deserializes its extra archives with members.
        let json = r#"{
            "os": "windows", "arch": "x86_64", "provider": "example", "build": "example",
            "version": "9.9.9",
            "url": "https://example/primary.zip",
            "sha256": null,
            "archive": "zip",
            "dylib_member": "lib/primary.dll",
            "extra_members": [],
            "additional_sources": [
                {
                    "url": "https://example/extra.zip",
                    "sha256": "0011223344556677889900112233445566778899001122334455667788990011",
                    "archive": "zip",
                    "members": ["bin/x64/a.dll", "bin/x64/b.dll"]
                }
            ]
        }"#;
        let entry: ManifestEntry = serde_json::from_str(json).expect("multi-source entry parses");
        assert_eq!(entry.build, "example");
        assert_eq!(entry.additional_sources.len(), 1);
        let source = &entry.additional_sources[0];
        assert_eq!(source.archive, ArchiveKind::Zip);
        assert_eq!(source.members, vec!["bin/x64/a.dll", "bin/x64/b.dll"]);
        assert_eq!(source.sha256.as_deref().map(str::len), Some(64));
    }

    #[test]
    fn cpu_build_is_present_on_supported_platforms() {
        // The CPU build ships for linux/windows x86_64 and macOS on Apple Silicon.
        // onnxruntime 1.27.0 has NO osx-x86_64 archive, so Intel macOS is dropped.
        assert!(lookup_download("linux", "x86_64", "cpu").is_some());
        assert!(lookup_download("windows", "x86_64", "cpu").is_some());
        assert!(lookup_download("macos", "aarch64", "cpu").is_some());
        assert!(lookup_download("macos", "x86_64", "cpu").is_none());
    }

    #[test]
    fn cpu_and_coreml_entries_have_real_pinned_hashes() {
        // The CPU and CoreML builds pin verified GitHub release digests; none null.
        for entry in &manifest().entries {
            if entry.build == "cpu" || entry.build == "coreml" {
                let hash = entry.sha256.as_deref().unwrap_or_else(|| {
                    panic!("{} entry {}/{} lacks a hash", entry.build, entry.os, entry.arch)
                });
                assert_eq!(hash.len(), 64, "SHA256 hex must be 64 chars");
            }
        }
    }

    #[test]
    fn lookup_misses_return_none() {
        // Unknown OS misses.
        assert!(lookup_download("plan9", "x86_64", "cpu").is_none());
        // DirectML is Windows-only: there is no linux/directml archive.
        assert!(lookup_download("linux", "x86_64", "directml").is_none());
        // CUDA builds are Windows/Linux only: there is no macOS/cuda archive.
        assert!(lookup_download("macos", "x86_64", "cuda13").is_none());
        assert!(lookup_download("macos", "aarch64", "cuda12").is_none());
        // The four-key `lookup` also rejects a valid provider with the wrong build.
        assert!(lookup("linux", "x86_64", "cpu", "cuda13").is_none());
        assert!(lookup("linux", "x86_64", "cuda", "cuda13").is_some());
    }

    #[test]
    fn cuda_builds_present_with_expected_extras() {
        // Two CUDA builds share provider "cuda" but differ by build slug + version:
        // cuda13 (1.27.0) and cuda12 (1.24.1). Each extracts the CUDA + TensorRT +
        // shared provider libs next to the primary library. Hashes are intentionally
        // unpinned (the GPU archives are hundreds of MB); `onnx_runtime::mod` computes
        // and logs the digest instead (removal condition in MODULE_README).
        let cases = [
            (
                "cuda13",
                "1.27.0",
                "onnxruntime-win-x64-gpu_cuda13-1.27.0/lib/onnxruntime.dll",
                "onnxruntime-linux-x64-gpu_cuda13-1.27.0/lib/libonnxruntime.so.1.27.0",
            ),
            (
                "cuda12",
                "1.24.1",
                "onnxruntime-win-x64-gpu-1.24.1/lib/onnxruntime.dll",
                "onnxruntime-linux-x64-gpu-1.24.1/lib/libonnxruntime.so.1.24.1",
            ),
        ];
        for (build, version, win_member, linux_member) in cases {
            let win = lookup_download("windows", "x86_64", build)
                .unwrap_or_else(|| panic!("windows {build} entry"));
            assert_eq!(win.provider, "cuda");
            assert_eq!(win.version, version);
            assert_eq!(win.archive, ArchiveKind::Zip);
            assert_eq!(win.dylib_member, win_member);
            assert!(win.sha256.is_none(), "{build} windows hash stays unpinned");
            for needle in [
                "onnxruntime_providers_cuda.dll",
                "onnxruntime_providers_tensorrt.dll",
                "onnxruntime_providers_shared.dll",
            ] {
                assert!(
                    win.extra_members.iter().any(|m| m.ends_with(needle)),
                    "{build} windows entry must extract {needle}"
                );
            }

            let linux = lookup_download("linux", "x86_64", build)
                .unwrap_or_else(|| panic!("linux {build} entry"));
            assert_eq!(linux.version, version);
            assert_eq!(linux.archive, ArchiveKind::TarGz);
            assert_eq!(linux.dylib_member, linux_member);
            assert!(linux.sha256.is_none(), "{build} linux hash stays unpinned");
            for needle in [
                "libonnxruntime_providers_cuda.so",
                "libonnxruntime_providers_tensorrt.so",
                "libonnxruntime_providers_shared.so",
            ] {
                assert!(
                    linux.extra_members.iter().any(|m| m.ends_with(needle)),
                    "{build} linux entry must extract {needle}"
                );
            }
        }
    }

    #[test]
    fn directml_build_is_present_with_extra_dlls() {
        // The DirectML EP ships in the PyPI `onnxruntime-directml` 1.24.4 wheel and
        // needs `DirectML.dll` extracted next to `onnxruntime.dll`.
        let entry = lookup_download("windows", "x86_64", "directml").expect("directml windows entry");
        assert_eq!(entry.version, "1.24.4");
        assert_eq!(entry.archive, ArchiveKind::Zip);
        assert_eq!(entry.dylib_member, "onnxruntime/capi/onnxruntime.dll");
        assert_eq!(
            entry.sha256.as_deref().map(str::len),
            Some(64),
            "directml archive must carry a pinned 64-char SHA256"
        );
        assert!(
            entry.extra_members.iter().any(|m| m.ends_with("DirectML.dll")),
            "directml entry must extract DirectML.dll alongside onnxruntime.dll"
        );
        assert!(
            entry
                .extra_members
                .iter()
                .any(|m| m.ends_with("onnxruntime_providers_shared.dll")),
            "directml entry must extract onnxruntime_providers_shared.dll"
        );
    }

    #[test]
    fn openvino_build_is_present_on_windows_and_linux() {
        // OpenVINO ships the PyPI `onnxruntime-openvino` 1.24.1 wheels with pinned
        // digests. The Windows wheel does NOT bundle the OpenVINO runtime (needs a
        // system install); the Linux wheel bundles the full runtime (libopenvino*).
        let win = lookup_download("windows", "x86_64", "openvino").expect("openvino windows entry");
        assert_eq!(win.version, "1.24.1");
        assert_eq!(win.dylib_member, "onnxruntime/capi/onnxruntime.dll");
        assert_eq!(win.sha256.as_deref().map(str::len), Some(64));
        assert!(
            win.extra_members
                .iter()
                .any(|m| m.ends_with("onnxruntime_providers_openvino.dll"))
        );
        // No bundled OpenVINO runtime DLL on Windows.
        assert!(!win.extra_members.iter().any(|m| m.contains("libopenvino")));

        let linux = lookup_download("linux", "x86_64", "openvino").expect("openvino linux entry");
        assert_eq!(linux.version, "1.24.1");
        assert_eq!(linux.dylib_member, "onnxruntime/capi/libonnxruntime.so.1.24.1");
        assert_eq!(linux.sha256.as_deref().map(str::len), Some(64));
        assert!(
            linux
                .extra_members
                .iter()
                .any(|m| m.ends_with("libonnxruntime_providers_openvino.so"))
        );
        // The Linux wheel bundles the OpenVINO core runtime.
        assert!(
            linux.extra_members.iter().any(|m| m.ends_with("libopenvino.so.2541")),
            "openvino linux entry must bundle the OpenVINO core runtime"
        );
    }

    #[test]
    fn coreml_reuses_the_cpu_osx_archive() {
        // CoreML is built into the standard macOS onnxruntime archive, so the
        // coreml build reuses the same URL/sha256/dylib as the cpu osx entry.
        let coreml = lookup_download("macos", "aarch64", "coreml").expect("macos coreml entry");
        let cpu = lookup_download("macos", "aarch64", "cpu").expect("macos cpu entry");
        assert_eq!(coreml.url, cpu.url, "coreml must reuse the cpu osx archive");
        assert_eq!(coreml.sha256, cpu.sha256);
        assert_eq!(coreml.dylib_member, cpu.dylib_member);
    }

    #[test]
    fn platform_keys_maps_desktop_targets_unchanged() {
        assert_eq!(platform_keys("windows", "x86_64"), ("windows", "x86_64"));
        assert_eq!(platform_keys("windows", "aarch64"), ("windows", "aarch64"));
        assert_eq!(platform_keys("macos", "x86_64"), ("macos", "x86_64"));
        assert_eq!(platform_keys("macos", "aarch64"), ("macos", "aarch64"));
        assert_eq!(platform_keys("linux", "x86_64"), ("linux", "x86_64"));
        assert_eq!(platform_keys("linux", "aarch64"), ("linux", "aarch64"));
        // Other unixes keep falling through to the linux key.
        assert_eq!(platform_keys("freebsd", "x86_64"), ("linux", "x86_64"));
    }

    #[test]
    fn android_has_its_own_key_and_only_bundled_rows() {
        let (os, arch) = platform_keys("android", "aarch64");
        assert_eq!((os, arch), ("android", "aarch64"));
        // Nothing is downloadable on Android: the official archives are glibc builds.
        assert!(!manifest().entries.iter().any(|entry| entry.os == "android"));
        // The APK bundles one library that serves the `cpu` and `xnnpack` builds.
        for build in ["cpu", "xnnpack"] {
            match lookup_build(os, arch, build) {
                Some(BuildSource::Bundled(entry)) => {
                    assert_eq!(entry.library, "libonnxruntime.so", "{build}");
                    assert_eq!(entry.version, "1.27.0", "{build}");
                }
                other => panic!("android/{build} must be a bundled row, got {other:?}"),
            }
        }
        // Every other catalog build is absent on Android.
        for build in crate::builds::all_builds() {
            if build.slug != "cpu" && build.slug != "xnnpack" {
                assert!(lookup_build(os, arch, build.slug).is_none(), "{}", build.slug);
            }
        }
    }

    #[test]
    fn bundled_rows_are_android_only_and_match_the_catalog() {
        // `resolve_or_download_ort_dylib` reads bundled libraries from the Android package
        // directory only; a bundled row for another OS would be unresolvable.
        for entry in &manifest().bundled {
            assert_eq!(entry.os, "android", "bundled row {}", entry.build);
            let build = crate::builds::build_by_slug(&entry.build).unwrap_or_else(|| panic!("no catalog build for bundled slug '{}'", entry.build));
            assert_eq!(entry.version, build.onnx_version, "version mismatch for bundled build '{}'", entry.build);
            assert!(build.platforms.iter().any(|&(os, arch)| os == entry.os && arch == entry.arch), "catalog platforms of '{}' lack {}/{}", entry.build, entry.os, entry.arch);
            assert_eq!(build.execution_providers.first().map(|ep| ep.id()), Some(entry.provider.as_str()), "provider of bundled '{}' is not its headline EP", entry.build);
        }
    }

    #[test]
    fn a_key_is_in_at_most_one_table() {
        for bundled in &manifest().bundled {
            assert!(
                lookup_download(&bundled.os, &bundled.arch, &bundled.build).is_none(),
                "{}/{}/{} is both downloadable and bundled",
                bundled.os,
                bundled.arch,
                bundled.build
            );
        }
    }

    #[test]
    fn bundled_version_matches_the_gradle_aar_pin() {
        // Two owners of one fact: the AAR the APK bundles (Gradle) and the version the
        // manifest reports for it (guard scope key, panel). They must never drift.
        const GRADLE: &str = include_str!("../../../android/app/build.gradle.kts");
        const COORDINATE: &str = "com.microsoft.onnxruntime:onnxruntime-android:";
        let start = GRADLE.find(COORDINATE).map(|index| index + COORDINATE.len()).unwrap_or_else(|| panic!("build.gradle.kts has no {COORDINATE} dependency"));
        let rest = &GRADLE[start..];
        let pinned = &rest[..rest.find('"').unwrap_or_else(|| panic!("unterminated AAR coordinate in build.gradle.kts"))];
        assert!(!manifest().bundled.is_empty());
        for entry in &manifest().bundled {
            assert_eq!(entry.version, pinned, "bundled build '{}' vs the Gradle AAR pin", entry.build);
        }
    }

    #[test]
    fn host_offers_downloads_and_no_bundled_build() {
        // The test host is linux x86_64: every shipped build there is a download.
        assert!(ort_downloads_offered_here());
        for build in crate::builds::all_builds() {
            assert_ne!(build_source_kind_here(build.slug), Some(OrtBuildSourceKind::Bundled), "{}", build.slug);
        }
        assert_eq!(build_source_kind_here("cpu"), Some(OrtBuildSourceKind::Download));
        assert_eq!(build_source_kind_here("xnnpack"), None);
    }

    #[test]
    fn current_platform_is_linux_x86_64_on_test_host() {
        // The test host and the primary check target are linux x86_64.
        assert_eq!(current_platform(), ("linux", "x86_64"));
    }

    /// `build_shipped_here` agrees with `build_version` for every catalog build plus an
    /// unknown slug, on whatever platform the test runs.
    #[test]
    fn build_shipped_here_matches_build_version() {
        for build in crate::builds::all_builds() {
            assert_eq!(
                build_shipped_here(build.slug),
                build_version(build.slug).is_some(),
                "{}",
                build.slug
            );
        }
        assert!(!build_shipped_here("no-such-build"));
    }

    #[test]
    fn sha256_matches_known_vector() {
        // NIST test vector: SHA256("abc").
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_sha256_file_accepts_and_rejects() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ort_manifest_test_{}.bin", std::process::id()));
        std::fs::write(&path, b"abc").expect("write temp fixture");

        let ok = verify_sha256_file(
            &path,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
        assert!(ok.is_ok());

        let bad = verify_sha256_file(&path, &"0".repeat(64));
        assert!(matches!(bad, Err(OrtRuntimeError::ChecksumMismatch { .. })));

        std::fs::remove_file(&path).ok();
    }
}
