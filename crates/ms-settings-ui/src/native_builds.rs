/*
FILE OVERVIEW: crates/ms-settings-ui/src/native_builds.rs
Native-only (the module is declared `cfg(not(target_arch = "wasm32"))` in `lib.rs`).

Purpose:
GUI-free view of the native ONNX build catalog on THIS machine: per build, the native
runtime's own verdict (runnable, or the reason it would fall back to the CPU build), the
Базовые / Специфичные / Недоступные grouping the AI pane's build picker shows, the build's
display label, and the localized wording of a fallback reason. Shared by the AI backend pane,
the settings warnings and the launcher's system-information tab, so none re-derives them.

Key items:
- `NativeBuildStatus`: one catalog build (slug, category, fallback reason or runnable).
- `NativeBuildAvailability::evaluate(&facts)`: every catalog build judged by
  `ms_native_runtime::native_build_fallback_reason` (its headline EP). Pure over the facts.
- `NativeBuildGroups` / `NativeBuildAvailability::groups()`: the picker's three groups.
- `NativeBuildAvailability::display_label()`: catalog label, "(unavailable)" suffix when not
  runnable.
- `native_fallback_reason_text()`: the localized reason fragment of a
  `NativeFallbackReason`; `From<NativeFallbackReason> for FallbackCause`: the one mapping onto
  the wasm-clean warnings mirror whose `label()` owns the wording.

Notes:
Availability is NEVER decided here: it is the runtime's rule over `NativeHardwareFacts`
(`NativeHardwareFacts::probe_all()` or `OnnxCaps::native_facts()`; probing is blocking, worker
threads only). Everything in this file is cheap and allocation-light, safe on the GUI thread.
*/

use ms_native_runtime::{NativeFallbackReason, NativeHardwareFacts};
use ms_onnx_runtime::builds::{self, BuildCategory};

use crate::settings_warnings::FallbackCause;

/// One catalog build and the native runtime's verdict on it for this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeBuildStatus {
    /// Stable catalog slug (`cpu`, `cuda13`, `qnn`, …).
    pub slug: &'static str,
    /// The catalog's picker category (Basic / Specific / Informational).
    pub category: BuildCategory,
    /// `None` = runnable here with its headline EP; otherwise why the runtime would fall back
    /// to the CPU build. The informational QNN entry always carries `NoRunnableProvider`.
    pub fallback: Option<NativeFallbackReason>,
}

impl NativeBuildStatus {
    /// Whether the runtime would run this build here (no fallback reason).
    #[must_use]
    pub fn available(&self) -> bool {
        self.fallback.is_none()
    }
}

/// The verdict on every catalog build for one set of hardware facts, in catalog (panel
/// display) order. Built by [`NativeBuildAvailability::evaluate`]; read by the AI pane's build
/// picker each frame and by the launcher's system-information tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeBuildAvailability {
    statuses: Vec<NativeBuildStatus>,
}

impl NativeBuildAvailability {
    /// Judges every catalog build with the native runtime's rule
    /// ([`ms_native_runtime::native_build_fallback_reason`]: the build with its headline EP —
    /// the EP a build pick resets to — so the result is what the runtime's selection will do).
    /// Pure over `facts` (no probe); manifest lookups are allocation-free.
    #[must_use]
    pub fn evaluate(facts: &NativeHardwareFacts) -> Self {
        let statuses = builds::all_builds()
            .iter()
            .map(|build| NativeBuildStatus {
                slug: build.slug,
                category: build.category,
                fallback: ms_native_runtime::native_build_fallback_reason(build.slug, facts),
            })
            .collect();
        Self { statuses }
    }

    /// Every catalog build's status, in catalog order.
    #[must_use]
    pub fn statuses(&self) -> &[NativeBuildStatus] {
        &self.statuses
    }

    /// Whether the catalog build `slug` is runnable here. An unknown slug is never available;
    /// the informational QNN entry never is either (the rule reports no runnable provider).
    #[must_use]
    pub fn is_available(&self, slug: &str) -> bool {
        self.statuses.iter().any(|status| status.slug == slug && status.available())
    }

    /// Partitions the catalog into Базовые / Специфичные / Недоступные for the build picker.
    ///
    /// An AVAILABLE `Basic` build lands in `basic`, an available `Specific` build in
    /// `specific`; every build unavailable on this machine — plus the informational QNN entry —
    /// lands in `unavailable`. With `downloads_offered == false`
    /// (`ms_onnx_runtime::ort_downloads_offered_here`: every build here is bundled, Android) an
    /// unavailable build can never be obtained, so it and QNN are not listed at all: the picker
    /// offers exactly the builds shipped here.
    #[must_use]
    pub fn groups(&self, downloads_offered: bool) -> NativeBuildGroups {
        let mut groups = NativeBuildGroups::default();
        for status in &self.statuses {
            let available = status.available();
            if !available && !downloads_offered {
                continue;
            }
            match status.category {
                BuildCategory::Basic if available => groups.basic.push(status.slug),
                BuildCategory::Specific if available => groups.specific.push(status.slug),
                // Unavailable Basic / Specific builds, and the informational QNN entry (never
                // runnable here), all go to the bottom group.
                BuildCategory::Basic | BuildCategory::Specific | BuildCategory::Informational => {
                    groups.unavailable.push(status.slug);
                }
            }
        }
        groups
    }

    /// The build's localized label as the picker shows it: the catalog display name, suffixed
    /// " (недоступно)" when the build is not runnable here. An unknown slug shows itself.
    #[must_use]
    pub fn display_label(&self, slug: &str) -> String {
        let name = builds::build_by_slug(slug).map(|build| build.display_label()).unwrap_or(slug);
        if self.is_available(slug) {
            name.to_string()
        } else {
            tf!("ai_backend.device_unavailable_suffix", name = name)
        }
    }
}

/// The build picker's three groups (slugs in catalog order), from
/// [`NativeBuildAvailability::groups`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NativeBuildGroups {
    /// Available `Basic`-category builds.
    pub basic: Vec<&'static str>,
    /// Available `Specific`-category builds.
    pub specific: Vec<&'static str>,
    /// Every unavailable build plus the informational QNN entry (bottom group).
    pub unavailable: Vec<&'static str>,
}

/// Maps the native-only reason onto its wasm-clean mirror in the settings warnings model.
/// One-to-one; the one place this mapping is written.
impl From<NativeFallbackReason> for FallbackCause {
    fn from(reason: NativeFallbackReason) -> Self {
        match reason {
            NativeFallbackReason::NoRunnableProvider => FallbackCause::NoRunnableProvider,
            NativeFallbackReason::CudaRuntimeMissing => FallbackCause::CudaRuntimeMissing,
            NativeFallbackReason::OpenVinoRuntimeMissing => FallbackCause::OpenVinoRuntimeMissing,
            NativeFallbackReason::WebGpuUnavailable => FallbackCause::WebGpuUnavailable,
            NativeFallbackReason::UnsupportedOnPlatform => FallbackCause::UnsupportedOnPlatform,
            NativeFallbackReason::BuildNotShippedForPlatform => FallbackCause::BuildNotShipped,
        }
    }
}

/// The localized reason fragment for a native CPU fallback (e.g. "matching CUDA runtime not
/// found"), the same text the settings warnings' fallback tooltip embeds
/// (`FallbackCause::label`, the one owner of that wording).
#[must_use]
pub fn native_fallback_reason_text(reason: NativeFallbackReason) -> &'static str {
    FallbackCause::from(reason).label()
}

#[cfg(test)]
mod tests {
    use super::{NativeBuildAvailability, NativeBuildStatus};
    use ms_native_runtime::{NativeFallbackReason, NativeHardwareFacts};
    use ms_onnx_runtime::builds;

    /// Availability where exactly `runnable` slugs run; every other catalog build carries a
    /// fallback reason (its kind does not matter to the grouping).
    fn availability_where(runnable: &[&str]) -> NativeBuildAvailability {
        let statuses = builds::all_builds()
            .iter()
            .map(|build| NativeBuildStatus {
                slug: build.slug,
                category: build.category,
                fallback: (!runnable.contains(&build.slug)).then_some(NativeFallbackReason::NoRunnableProvider),
            })
            .collect();
        NativeBuildAvailability { statuses }
    }

    /// Characterization of the per-build availability for an empty probe (no runtimes) and a
    /// full one (every runtime present). Platform-gated builds follow the target OS.
    #[test]
    fn availability_per_facts_shape() {
        // Every expectation is tied to the embedded manifest (does the build ship for this
        // OS/arch?), so the test holds on every target, including ones with no archive.
        let ships = ms_onnx_runtime::build_shipped_here;
        let empty = NativeBuildAvailability::evaluate(&NativeHardwareFacts::default());
        assert_eq!(empty.is_available("cpu"), ships("cpu"));
        // DirectML needs no probed adapter, so it is available wherever it ships (Windows); a
        // build without an archive for this OS/arch is unavailable. The manifest ships
        // DirectML only for Windows and CoreML only for macOS, where their headline EPs run.
        assert_eq!(empty.is_available("directml"), ships("directml"), "DirectML with zero adapters");
        assert!(!empty.is_available("webgpu"));
        assert_eq!(empty.is_available("coreml"), ships("coreml"));
        assert!(!empty.is_available("cuda13") && !empty.is_available("cuda12") && !empty.is_available("openvino"));

        let full = NativeBuildAvailability::evaluate(&NativeHardwareFacts {
            cuda12_available: true,
            cuda13_available: true,
            openvino_available: true,
            webgpu_available: true,
        });
        for slug in ["cpu", "directml", "webgpu", "coreml", "cuda13", "cuda12", "openvino", "xnnpack"] {
            assert_eq!(full.is_available(slug), ships(slug), "{slug}");
        }

        // The informational QNN entry and an unknown slug are never available.
        assert!(!full.is_available("qnn"));
        assert!(!full.is_available("no-such-build"));
        // Every catalog build has exactly one status, in catalog order.
        let slugs: Vec<&str> = full.statuses().iter().map(|status| status.slug).collect();
        let catalog: Vec<&str> = builds::all_builds().iter().map(|build| build.slug).collect();
        assert_eq!(slugs, catalog);
    }

    /// Availability partitions builds into Базовые / Специфичные / Недоступные: available
    /// Basic -> basic, available Specific -> specific, everything else + QNN -> unavailable,
    /// with no slug in two groups.
    #[test]
    fn partition_groups_by_category_and_availability() {
        let groups = availability_where(&["cpu", "directml", "webgpu", "coreml", "cuda13"]).groups(true);
        for slug in ["cpu", "directml", "webgpu", "coreml"] {
            assert!(groups.basic.contains(&slug), "{slug} should be Базовые");
        }
        assert!(groups.specific.contains(&"cuda13"));
        // Unavailable Specific builds and the informational QNN land at the bottom.
        for slug in ["cuda12", "openvino", "qnn"] {
            assert!(groups.unavailable.contains(&slug), "{slug} should be Недоступные");
        }
        // A slug never appears in more than one group.
        for slug in &groups.basic {
            assert!(!groups.specific.contains(slug) && !groups.unavailable.contains(slug));
        }
    }

    /// An unavailable Basic build (e.g. DirectML off Windows) drops from Базовые to
    /// Недоступные instead of vanishing; CPU is the only Basic build available on every
    /// shipped platform.
    #[test]
    fn partition_unavailable_basic_falls_to_bottom() {
        let groups = availability_where(&["cpu"]).groups(true);
        assert_eq!(groups.basic, vec!["cpu"]);
        assert!(groups.specific.is_empty());
        for slug in ["directml", "webgpu", "coreml", "cuda13", "cuda12", "openvino", "qnn"] {
            assert!(groups.unavailable.contains(&slug), "{slug} should be Недоступные");
        }
    }

    /// Where nothing can be downloaded (every build bundled, Android) the picker lists only
    /// the builds that run here; QNN and unavailable builds are not offered at all.
    #[test]
    fn partition_without_downloads_lists_only_runnable_builds() {
        let groups = availability_where(&["cpu", "xnnpack"]).groups(false);
        assert_eq!(groups.basic, vec!["cpu", "xnnpack"]);
        assert!(groups.specific.is_empty());
        assert!(groups.unavailable.is_empty(), "{:?}", groups.unavailable);
    }

    /// The label carries the unavailable suffix exactly when the build is not runnable.
    #[test]
    fn display_label_marks_unavailable_builds() {
        let availability = availability_where(&["cpu"]);
        let name = |slug: &str| builds::build_by_slug(slug).map_or_else(|| slug.to_string(), |build| build.display_label().to_string());
        assert_eq!(availability.display_label("cpu"), name("cpu"));
        assert_eq!(availability.display_label("cuda13"), tf!("ai_backend.device_unavailable_suffix", name = name("cuda13").as_str()));
    }
}
