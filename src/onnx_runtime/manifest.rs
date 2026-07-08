/*
File: onnx_runtime/manifest.rs

Purpose:
The integrity manifest for the native ONNX Runtime loader. It embeds the pinned
`ort_manifest.json` at compile time and exposes typed lookups plus SHA256 helpers
used by `onnx_runtime::mod` to download and verify the official onnxruntime
release archive for the current platform/provider.

Key types:
- ArchiveKind   : how a release archive is packed (Zip / TarGz / TarZst)
- ManifestEntry : one (os, arch, provider) -> archive+dylib mapping
- Manifest      : the whole parsed document (version + entries)

Key functions:
- lookup            : find the entry for (os, arch, provider_id)
- current_platform  : the (os, arch) pair this binary was built for, via cfg
- sha256_hex        : lowercase hex SHA256 of a byte slice (pure)
- sha256_hex_of_file: streaming lowercase hex SHA256 of a file
- verify_sha256_file: compare a file's SHA256 against an expected hex string

Notes:
The manifest is pinned to a single ONNX Runtime version (`ORT_VERSION`). Every
CPU entry currently carries a real, end-to-end-verifiable SHA256 (computed from
the official GitHub release assets). `sha256` is modeled as `Option` so future
GPU-provider entries can be added before a maintainer pins their hash; see the
unverified-hash handling in `onnx_runtime::mod` and the module README.
*/

use std::fs::File;
use std::path::Path;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use super::OrtRuntimeError;

/// The single ONNX Runtime version this build downloads and links against.
///
/// Must equal the `ort_version` field of the embedded manifest (enforced by a
/// unit test). ONNX Runtime must stay `>= 1.18` because the pinned `ort`
/// (`2.0.0-rc.12`, `api-18`) hard-errors on older libraries.
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

/// One manifest row: the download + extraction recipe for a single platform and
/// execution provider.
///
/// `dylib_member` is the archive-internal path of the primary onnxruntime library
/// (the one whose resolved on-disk path is returned to the caller).
/// `extra_members` lists additional archive-internal library paths that must be
/// placed next to it (e.g. `onnxruntime_providers_shared.dll` on Windows); it may
/// be empty. `sha256` is a lowercase hex digest of the whole archive, or `None`
/// when a hash has not yet been pinned for this entry.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ManifestEntry {
    /// Normalized OS key: `"windows"`, `"linux"`, or `"macos"`.
    pub os: String,
    /// Normalized architecture key: `"x86_64"` or `"aarch64"`.
    pub arch: String,
    /// Provider id, matching `ms_onnx::ExecutionProvider::id`.
    pub provider: String,
    /// Absolute download URL of the official release archive.
    pub url: String,
    /// Expected lowercase hex SHA256 of the archive, or `None` if unpinned.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Packing format of the archive at `url`.
    pub archive: ArchiveKind,
    /// Archive-internal path of the primary onnxruntime library.
    pub dylib_member: String,
    /// Archive-internal paths of additional libraries to place alongside it.
    #[serde(default)]
    pub extra_members: Vec<String>,
}

/// The parsed manifest document.
#[derive(Debug, Clone, serde::Deserialize)]
struct Manifest {
    /// ONNX Runtime version all entries target; must equal [`ORT_VERSION`].
    ort_version: String,
    /// All known (os, arch, provider) download recipes.
    entries: Vec<ManifestEntry>,
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

/// Finds the manifest entry for `(os, arch, provider_id)`, if one exists.
///
/// Returns `None` when no row matches (e.g. a provider with no Phase-0 archive);
/// the caller maps that to a typed `OrtRuntimeError::NoManifestEntry`.
#[must_use]
pub fn lookup(os: &str, arch: &str, provider_id: &str) -> Option<&'static ManifestEntry> {
    manifest()
        .entries
        .iter()
        .find(|entry| entry.os == os && entry.arch == arch && entry.provider == provider_id)
}

/// The `(os, arch)` platform keys for the target this binary was built for.
///
/// `os` is one of `"windows"`, `"macos"`, `"linux"`; `arch` is `"aarch64"` or
/// `"x86_64"`. These match the manifest key scheme. Pure and const-foldable.
#[must_use]
pub fn current_platform() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        // The only supported non-macOS unix target is Linux; other unixes fall
        // through here intentionally (no manifest entry -> typed no-entry error).
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
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
        OrtRuntimeError::Io(format!(
            "не удалось открыть файл для проверки контрольной суммы '{}': {err}",
            path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    // `Sha256` implements `std::io::Write`, so the file is hashed in a streaming
    // pass without buffering the whole (up to ~65 MB) archive in memory.
    std::io::copy(&mut file, &mut hasher).map_err(|err| {
        OrtRuntimeError::Io(format!(
            "не удалось прочитать файл при проверке контрольной суммы '{}': {err}",
            path.display()
        ))
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
    fn manifest_has_three_cpu_platforms() {
        // All three shipping desktop platforms must have a CPU archive entry.
        assert!(lookup("linux", "x86_64", "cpu").is_some());
        assert!(lookup("windows", "x86_64", "cpu").is_some());
        assert!(
            lookup("macos", "x86_64", "cpu").is_some()
                || lookup("macos", "aarch64", "cpu").is_some()
        );
    }

    #[test]
    fn cpu_entries_have_real_pinned_hashes() {
        // Phase 0 pins verified hashes for every CPU archive; none are null.
        for entry in &manifest().entries {
            if entry.provider == "cpu" {
                let hash = entry
                    .sha256
                    .as_deref()
                    .unwrap_or_else(|| panic!("CPU entry {}/{} lacks a hash", entry.os, entry.arch));
                assert_eq!(hash.len(), 64, "SHA256 hex must be 64 chars");
            }
        }
    }

    #[test]
    fn lookup_misses_return_none() {
        // Unknown OS misses.
        assert!(lookup("plan9", "x86_64", "cpu").is_none());
        // DirectML is Windows-only: there is no linux/directml archive.
        assert!(lookup("linux", "x86_64", "directml").is_none());
        // CUDA is offered on Windows/Linux only: there is no macOS/cuda archive.
        assert!(lookup("macos", "x86_64", "cuda").is_none());
        assert!(lookup("macos", "aarch64", "cuda").is_none());
    }

    #[test]
    fn cuda_entries_present_with_expected_extras() {
        // The CUDA EP ships in the onnxruntime GPU build (Windows zip / Linux tgz).
        // Both entries must extract the CUDA + shared provider libraries next to the
        // primary onnxruntime library. The hashes are intentionally unpinned (the
        // GPU archives are hundreds of MB); the logged-unverified path in
        // `onnx_runtime::mod` computes and logs the digest instead. Removal
        // condition: paste those digests into `ort_manifest.json` (see MODULE_README).
        let win = lookup("windows", "x86_64", "cuda").expect("windows cuda entry");
        assert_eq!(win.archive, ArchiveKind::Zip);
        assert_eq!(
            win.dylib_member,
            "onnxruntime-win-x64-gpu-1.20.1/lib/onnxruntime.dll"
        );
        assert!(win.sha256.is_none(), "cuda windows hash stays unpinned");
        assert!(
            win.extra_members
                .iter()
                .any(|m| m.ends_with("onnxruntime_providers_cuda.dll")),
            "cuda windows entry must extract onnxruntime_providers_cuda.dll"
        );
        assert!(
            win.extra_members
                .iter()
                .any(|m| m.ends_with("onnxruntime_providers_shared.dll")),
            "cuda windows entry must extract onnxruntime_providers_shared.dll"
        );

        let linux = lookup("linux", "x86_64", "cuda").expect("linux cuda entry");
        assert_eq!(linux.archive, ArchiveKind::TarGz);
        assert_eq!(
            linux.dylib_member,
            "onnxruntime-linux-x64-gpu-1.20.1/lib/libonnxruntime.so.1.20.1"
        );
        assert!(linux.sha256.is_none(), "cuda linux hash stays unpinned");
        assert!(
            linux
                .extra_members
                .iter()
                .any(|m| m.ends_with("libonnxruntime_providers_cuda.so")),
            "cuda linux entry must extract libonnxruntime_providers_cuda.so"
        );
        assert!(
            linux
                .extra_members
                .iter()
                .any(|m| m.ends_with("libonnxruntime_providers_shared.so")),
            "cuda linux entry must extract libonnxruntime_providers_shared.so"
        );
    }

    #[test]
    fn directml_entry_is_present_with_extra_dlls() {
        // The DirectML EP ships in the PyPI `onnxruntime-directml` wheel and needs
        // `DirectML.dll` extracted next to `onnxruntime.dll`.
        let entry = lookup("windows", "x86_64", "directml").expect("directml windows entry");
        assert_eq!(entry.archive, ArchiveKind::Zip);
        assert_eq!(entry.dylib_member, "onnxruntime/capi/onnxruntime.dll");
        assert_eq!(
            entry.sha256.as_deref().map(str::len),
            Some(64),
            "directml archive must carry a pinned 64-char SHA256"
        );
        assert!(
            entry
                .extra_members
                .iter()
                .any(|member| member.ends_with("DirectML.dll")),
            "directml entry must extract DirectML.dll alongside onnxruntime.dll"
        );
        assert!(
            entry
                .extra_members
                .iter()
                .any(|member| member.ends_with("onnxruntime_providers_shared.dll")),
            "directml entry must extract onnxruntime_providers_shared.dll"
        );
    }

    #[test]
    fn coreml_reuses_the_cpu_osx_archive() {
        // CoreML is built into the standard macOS onnxruntime archive, so the
        // coreml entry reuses the same URL/sha256/dylib as the cpu osx entry.
        for arch in ["x86_64", "aarch64"] {
            let (Some(coreml), Some(cpu)) = (
                lookup("macos", arch, "coreml"),
                lookup("macos", arch, "cpu"),
            ) else {
                // Only one macOS arch must exist per host; skip the absent one.
                continue;
            };
            assert_eq!(coreml.url, cpu.url, "coreml must reuse the cpu osx archive");
            assert_eq!(coreml.sha256, cpu.sha256);
            assert_eq!(coreml.dylib_member, cpu.dylib_member);
        }
        assert!(
            lookup("macos", "x86_64", "coreml").is_some()
                || lookup("macos", "aarch64", "coreml").is_some(),
            "at least one macOS coreml entry must exist"
        );
    }

    #[test]
    fn current_platform_is_linux_x86_64_on_test_host() {
        // The test host and the primary check target are linux x86_64.
        assert_eq!(current_platform(), ("linux", "x86_64"));
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
