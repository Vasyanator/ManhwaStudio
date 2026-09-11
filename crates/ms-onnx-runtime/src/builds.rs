/*
File: onnx_runtime/builds.rs

Purpose:
The static catalog of user-selectable ONNX Runtime BUILDS. A "build" is a concrete
onnxruntime binary of a specific version exposing a specific execution-provider set
(e.g. "CUDA 13 + TensorRT" on 1.27.0 vs "CUDA 12 + TensorRT" on 1.24.1). This module
is the SINGLE SOURCE OF TRUTH that both the UI (the AI backend panel) and the runtime
consume to enumerate builds, resolve a build's ordered EP set, and pick a per-OS
default. It carries NO download recipe — that lives in `ort_manifest.json`, keyed by
`(os, arch, provider, build)`; a build's on-disk resolvability is expressed here only
as the list of platforms that have a manifest entry.

Key types:
- BuildCategory : how a build is grouped in the panel (Basic / Specific / Informational)
- OrtBuild      : one catalog row (slug, display name, version, category, EP set, platforms)

Key functions:
- all_builds                : the full ordered catalog
- build_by_slug             : find a build by its stable slug
- build_execution_providers : the ordered EP set for a build slug (empty if unknown)
- default_build_for_current_os : the recommended default build for the built target
- default_build_for_provider   : the default build a bare provider maps to (compat shim)

Notes:
Slugs are STABLE identifiers shared with `ort_manifest.json` (the `build` field) and the
per-build cache directory; they must not change. The catalog is const data with no
runtime state. QNN is an INFORMATIONAL-only entry: it has no manifest entry and no EP,
and is not runnable on x86_64 — it exists so the panel can show it under "Недоступные".
*/

use ms_onnx::ExecutionProvider;

/// How a build is grouped in the AI backend panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildCategory {
    /// A mainstream, broadly-recommended backend (CPU / DirectML / WebGPU / CoreML).
    Basic,
    /// A hardware-specific backend that targets a narrower set of devices
    /// (CUDA/TensorRT, OpenVINO).
    Specific,
    /// Display-only: shown for information but not installable/runnable here (QNN on
    /// x86_64). It has no manifest entry and no execution provider.
    Informational,
}

/// One catalog row describing a selectable onnxruntime build.
///
/// A build bundles an onnxruntime `onnx_version` with an ordered `execution_providers`
/// set. `slug` is the stable key shared with `ort_manifest.json`'s `build` field and
/// the per-build cache directory. `platforms` lists the `(os, arch)` pairs that have a
/// manifest entry for this build (empty for an [`BuildCategory::Informational`] build).
#[derive(Debug, Clone, Copy)]
pub struct OrtBuild {
    /// Stable slug, e.g. `"cpu"`, `"cuda13"`. Shared with the manifest `build` field.
    pub slug: &'static str,
    /// Panel display label. For most builds this is a plain product name (never
    /// localized — `"CPU"`, `"CUDA 12 + TensorRT"`, …). When the label needs
    /// localization it instead holds a catalog KEY, resolved by
    /// [`OrtBuild::display_label`] (a plain product name is never a catalog key, so
    /// the lookup misses and falls back to the literal). Read it via `display_label`,
    /// not directly, so the localized case is handled.
    pub display_name: &'static str,
    /// The onnxruntime release version this build ships (e.g. `"1.27.0"`).
    pub onnx_version: &'static str,
    /// Panel grouping.
    pub category: BuildCategory,
    /// Ordered execution providers this build registers (highest priority first);
    /// empty for an informational build.
    pub execution_providers: &'static [ExecutionProvider],
    /// `(os, arch)` platform keys that have a manifest entry for this build; empty for
    /// an informational build.
    pub platforms: &'static [(&'static str, &'static str)],
}

/// The three desktop x86_64 platforms plus Apple Silicon, reused by several builds.
const P_WIN: (&str, &str) = ("windows", "x86_64");
const P_LINUX: (&str, &str) = ("linux", "x86_64");
const P_MAC_ARM: (&str, &str) = ("macos", "aarch64");

/// The full, ordered ONNX Runtime build catalog.
///
/// Order is the panel display order. The EP order in each `execution_providers` slice
/// is the registration priority passed to onnxruntime (first = highest priority), with
/// `Cpu` last as the always-present fallback for accelerated builds.
static BUILDS: &[OrtBuild] = &[
    OrtBuild {
        slug: "cpu",
        display_name: "CPU",
        onnx_version: "1.27.0",
        category: BuildCategory::Basic,
        execution_providers: &[ExecutionProvider::Cpu],
        // onnxruntime 1.27.0 ships no osx-x86_64 archive, so the CPU build targets
        // Apple Silicon only on macOS (Intel Macs are unsupported at 1.27).
        platforms: &[P_WIN, P_LINUX, P_MAC_ARM],
    },
    OrtBuild {
        slug: "directml",
        display_name: "DirectML",
        onnx_version: "1.24.4",
        category: BuildCategory::Basic,
        execution_providers: &[ExecutionProvider::DirectMl, ExecutionProvider::Cpu],
        platforms: &[P_WIN],
    },
    OrtBuild {
        slug: "webgpu",
        display_name: "WebGPU",
        onnx_version: "1.27.0",
        category: BuildCategory::Basic,
        execution_providers: &[ExecutionProvider::WebGpu, ExecutionProvider::Cpu],
        platforms: &[P_WIN, P_LINUX, P_MAC_ARM],
    },
    OrtBuild {
        slug: "coreml",
        display_name: "CoreML",
        onnx_version: "1.27.0",
        category: BuildCategory::Basic,
        execution_providers: &[ExecutionProvider::CoreMl, ExecutionProvider::Cpu],
        platforms: &[P_MAC_ARM],
    },
    OrtBuild {
        slug: "cuda13",
        display_name: "CUDA 13 + TensorRT",
        onnx_version: "1.27.0",
        category: BuildCategory::Specific,
        execution_providers: &[
            ExecutionProvider::Cuda,
            ExecutionProvider::TensorRt,
            ExecutionProvider::Cpu,
        ],
        platforms: &[P_WIN, P_LINUX],
    },
    OrtBuild {
        slug: "cuda12",
        display_name: "CUDA 12 + TensorRT",
        onnx_version: "1.24.1",
        category: BuildCategory::Specific,
        execution_providers: &[
            ExecutionProvider::Cuda,
            ExecutionProvider::TensorRt,
            ExecutionProvider::Cpu,
        ],
        platforms: &[P_WIN, P_LINUX],
    },
    OrtBuild {
        slug: "openvino",
        display_name: "OpenVINO",
        onnx_version: "1.24.1",
        category: BuildCategory::Specific,
        execution_providers: &[ExecutionProvider::OpenVino, ExecutionProvider::Cpu],
        platforms: &[P_WIN, P_LINUX],
    },
    OrtBuild {
        slug: "qnn",
        // A catalog KEY, not a literal: `t!` cannot run in this const array, so the
        // label is localized at read time by `OrtBuild::display_label`.
        display_name: "onnx_runtime.builds.qnn_label",
        // Informational: the QNN EP ships only for Windows-on-ARM (Snapdragon) and is
        // NOT runnable on x86_64. No binary is offered here (no manifest entry, no EP);
        // this row exists solely so the panel can list it under "Недоступные".
        onnx_version: "1.27.0",
        category: BuildCategory::Informational,
        execution_providers: &[],
        platforms: &[],
    },
];

impl OrtBuild {
    /// The localized panel label for this build.
    ///
    /// `display_name` is either a plain product name or a catalog key (see the field
    /// docs). Resolving through `ms_i18n::lookup` with a fallback to the literal
    /// handles both: a product name is never a catalog key, so it falls through
    /// unchanged, while a key resolves to its localized value.
    #[must_use]
    pub fn display_label(&self) -> &'static str {
        ms_i18n::lookup(self.display_name).unwrap_or(self.display_name)
    }
}

/// The full ONNX Runtime build catalog in panel display order.
#[must_use]
pub fn all_builds() -> &'static [OrtBuild] {
    BUILDS
}

/// Finds the catalog build with stable slug `slug`, if any.
#[must_use]
pub fn build_by_slug(slug: &str) -> Option<&'static OrtBuild> {
    BUILDS.iter().find(|build| build.slug == slug)
}

/// The ordered execution-provider set for `slug`.
///
/// Returns the build's `execution_providers` (first = highest registration priority),
/// or an empty slice when `slug` is unknown or informational (e.g. `"qnn"`). Callers
/// treat an empty slice as "no runnable provider set for this build".
#[must_use]
pub fn build_execution_providers(slug: &str) -> &'static [ExecutionProvider] {
    match build_by_slug(slug) {
        Some(build) => build.execution_providers,
        None => &[],
    }
}

/// The recommended default build slug for the target this binary was built for.
///
/// Policy (accelerated-by-default, CPU as the universal fallback):
/// - Windows → `"directml"` (D3D12 GPU acceleration available on all vendors);
/// - Linux   → `"webgpu"` (cross-vendor GPU acceleration without a CUDA toolchain);
/// - macOS   → `"coreml"` (Apple Neural Engine / GPU);
/// - any other target → `"cpu"`.
///
/// The returned slug is always present in [`all_builds`]; the caller is responsible
/// for checking that the build has a manifest entry for the current `(os, arch)`
/// before resolving it.
#[must_use]
pub fn default_build_for_current_os() -> &'static str {
    if cfg!(target_os = "windows") {
        "directml"
    } else if cfg!(target_os = "macos") {
        "coreml"
    } else if cfg!(target_os = "linux") {
        "webgpu"
    } else {
        "cpu"
    }
}

/// The default build slug a bare [`ExecutionProvider`] maps to.
///
/// Used by the provider-keyed compatibility shims (`manifest::provider_version` and the
/// pre-build-selection call sites) to bridge the old provider-centric API to the new
/// build catalog until the full build-selection UI lands. CUDA and TensorRT map to the
/// newest CUDA build (`"cuda13"`), since TensorRT ships inside the CUDA builds.
#[must_use]
pub fn default_build_for_provider(provider: ExecutionProvider) -> &'static str {
    match provider {
        ExecutionProvider::Cpu => "cpu",
        ExecutionProvider::DirectMl => "directml",
        ExecutionProvider::CoreMl => "coreml",
        ExecutionProvider::Cuda => "cuda13",
        ExecutionProvider::WebGpu => "webgpu",
        ExecutionProvider::OpenVino => "openvino",
        ExecutionProvider::TensorRt => "cuda13",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_build_has_a_unique_slug() {
        let mut slugs: Vec<&str> = all_builds().iter().map(|b| b.slug).collect();
        slugs.sort_unstable();
        let count = slugs.len();
        slugs.dedup();
        assert_eq!(slugs.len(), count, "build slugs must be unique");
    }

    #[test]
    fn build_by_slug_finds_and_misses() {
        assert_eq!(build_by_slug("cuda13").map(|b| b.onnx_version), Some("1.27.0"));
        assert_eq!(build_by_slug("cuda12").map(|b| b.onnx_version), Some("1.24.1"));
        assert_eq!(build_by_slug("openvino").map(|b| b.onnx_version), Some("1.24.1"));
        assert_eq!(build_by_slug("directml").map(|b| b.onnx_version), Some("1.24.4"));
        assert!(build_by_slug("nope").is_none());
    }

    #[test]
    fn execution_provider_sets_match_the_contract() {
        assert_eq!(build_execution_providers("cpu"), &[ExecutionProvider::Cpu]);
        assert_eq!(
            build_execution_providers("directml"),
            &[ExecutionProvider::DirectMl, ExecutionProvider::Cpu]
        );
        assert_eq!(
            build_execution_providers("webgpu"),
            &[ExecutionProvider::WebGpu, ExecutionProvider::Cpu]
        );
        assert_eq!(
            build_execution_providers("coreml"),
            &[ExecutionProvider::CoreMl, ExecutionProvider::Cpu]
        );
        for slug in ["cuda13", "cuda12"] {
            assert_eq!(
                build_execution_providers(slug),
                &[
                    ExecutionProvider::Cuda,
                    ExecutionProvider::TensorRt,
                    ExecutionProvider::Cpu
                ],
                "{slug} EP set"
            );
        }
        assert_eq!(
            build_execution_providers("openvino"),
            &[ExecutionProvider::OpenVino, ExecutionProvider::Cpu]
        );
    }

    #[test]
    fn qnn_is_informational_with_no_ep_and_no_platform() {
        let qnn = build_by_slug("qnn").expect("qnn catalog entry");
        assert_eq!(qnn.category, BuildCategory::Informational);
        assert!(qnn.execution_providers.is_empty(), "qnn has no EP");
        assert!(qnn.platforms.is_empty(), "qnn has no manifest platform");
        assert!(build_execution_providers("qnn").is_empty());
    }

    #[test]
    fn default_build_for_current_os_is_present_in_the_catalog() {
        let slug = default_build_for_current_os();
        assert!(build_by_slug(slug).is_some());
        // The test/check host is linux x86_64 → webgpu.
        assert_eq!(slug, "webgpu");
    }

    #[test]
    fn provider_maps_to_its_default_build() {
        assert_eq!(default_build_for_provider(ExecutionProvider::Cpu), "cpu");
        assert_eq!(default_build_for_provider(ExecutionProvider::Cuda), "cuda13");
        assert_eq!(default_build_for_provider(ExecutionProvider::TensorRt), "cuda13");
        assert_eq!(default_build_for_provider(ExecutionProvider::OpenVino), "openvino");
        // Every mapped slug must resolve to a real catalog build.
        for provider in [
            ExecutionProvider::Cpu,
            ExecutionProvider::DirectMl,
            ExecutionProvider::CoreMl,
            ExecutionProvider::Cuda,
            ExecutionProvider::WebGpu,
            ExecutionProvider::OpenVino,
            ExecutionProvider::TensorRt,
        ] {
            assert!(build_by_slug(default_build_for_provider(provider)).is_some());
        }
    }
}
