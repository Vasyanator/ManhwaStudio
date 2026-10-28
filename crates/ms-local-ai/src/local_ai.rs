/*
FILE OVERVIEW: crates/ms-local-ai/src/local_ai.rs
Which LOCAL AI runtimes this process may use, and the one rule that turns an AI route into
"can run here" or a typed reason.

Semantics (the `--no-ai` contract):
- `--no-ai` switches off every LOCAL AI runtime: the Python backend, the in-process native
  ONNX runtime AND the Stable-Diffusion.cpp worker runtime. Cloud AI API (OCR and MT) and the plain machine translators
  (Google / Yandex / DeepL) are network code and are NEVER gated by it.
- Independently, the platform decides: the backend needs `PlatformFeature::PythonBackend`,
  native ONNX needs `PlatformFeature::NativeOnnxRuntime`, sd.cpp needs
  `PlatformFeature::SdCppRuntime`. A platform reason wins over the
  flag (restarting without `--no-ai` would not help there).

Key items:
- `LocalAiAvailability`: the per-process answer for the three runtimes, built once at startup
  (`for_process` in the root's `init_full`) and carried by the backend supervisor handle to the
  studio tabs and the settings panes.
- `install_process` / `process`: the same answer as a process-global, installed once by
  `init_full`, for code that has no handle to carry it (`process` falls back to
  `for_process(false)` before or without the install: tests, inert contexts).
- `LocalAiOff` / `LocalRuntime` / `LocalAiOffReason`: why one runtime is off; `user_message`
  is the localized text every refusal shows.
- `AiRouteKind`: what an AI route runs on (cloud, native ONNX, backend, sd.cpp);
  `AiRouteKind::availability` is the ONE availability rule. OCR (`ms_tab_translation`'s `ocr::ocr_route_availability`)
  and text detection (`TextDetectorAlgorithm::availability`) map their routes onto it.
- `effective_ai_runtime`: the runtime the routers use — `AiRuntime::from_user_settings`, except
  that a stored `Backend` choice reads as `Native` where the platform has no Python backend.
*/

use std::sync::OnceLock;

use ms_config::AiRuntime;
use ms_log::runtime_log;
use ms_platform::{PlatformFeature, Unavailable, UnavailableReason};
use serde_json::Value;

/// A local AI runtime that can be switched off for the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalRuntime {
    /// The out-of-process Python AI backend.
    PythonBackend,
    /// The in-process native ONNX runtime.
    NativeOnnx,
    /// The Stable-Diffusion.cpp runtime (a separate worker process of this program).
    SdCpp,
}

impl LocalRuntime {
    /// The platform capability this runtime needs.
    #[must_use]
    pub const fn platform_feature(self) -> PlatformFeature {
        match self {
            LocalRuntime::PythonBackend => PlatformFeature::PythonBackend,
            LocalRuntime::NativeOnnx => PlatformFeature::NativeOnnxRuntime,
            LocalRuntime::SdCpp => PlatformFeature::SdCppRuntime,
        }
    }
}

/// Why a local runtime is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalAiOffReason {
    /// The process was started with `--no-ai`.
    DisabledByFlag,
    /// The platform capability table says no ([`LocalRuntime::platform_feature`]).
    UnavailableOnPlatform(UnavailableReason),
}

/// A local AI runtime is off for this process, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalAiOff {
    /// The runtime that is off.
    pub runtime: LocalRuntime,
    /// Why it is off.
    pub reason: LocalAiOffReason,
}

impl LocalAiOff {
    /// Localized sentence(s) naming the runtime and the reason, for a status line or toast.
    /// The platform case is `ms_platform::Unavailable::user_message` of the runtime's feature.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self.reason {
            LocalAiOffReason::UnavailableOnPlatform(reason) => {
                Unavailable::new(self.runtime.platform_feature(), reason).user_message()
            }
            LocalAiOffReason::DisabledByFlag => match self.runtime {
                LocalRuntime::PythonBackend => t!("translation.local_ai.backend_disabled_by_flag_error").to_string(),
                LocalRuntime::NativeOnnx => t!("translation.local_ai.native_disabled_by_flag_error").to_string(),
                LocalRuntime::SdCpp => t!("translation.local_ai.sdcpp_disabled_by_flag_error").to_string(),
            },
        }
    }
}

/// Which local AI runtimes this process may use. `Ok(())` = allowed (a backend may still be
/// stopped or offline: that is the health snapshot's business, not this type's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalAiAvailability {
    /// The Python backend: may it be started and used at all.
    pub backend: Result<(), LocalAiOff>,
    /// The native ONNX runtime: may it be loaded and used at all.
    pub native_onnx: Result<(), LocalAiOff>,
    /// The Stable-Diffusion.cpp runtime: may its worker process be started and used at all.
    pub sdcpp: Result<(), LocalAiOff>,
}

impl LocalAiAvailability {
    /// The process answer: `no_ai_flag` is `Cli::no_ai`; the platform half comes from the
    /// capability table (never from `cfg`).
    #[must_use]
    pub fn for_process(no_ai_flag: bool) -> Self {
        Self::resolve(
            no_ai_flag,
            PlatformFeature::PythonBackend.require(),
            PlatformFeature::NativeOnnxRuntime.require(),
            PlatformFeature::SdCppRuntime.require(),
        )
    }

    /// Pure core of [`Self::for_process`]: a platform `Err` wins over the flag for its runtime;
    /// otherwise `no_ai_flag` switches the runtime off with [`LocalAiOffReason::DisabledByFlag`].
    /// The three platform answers are the `require()` results of the runtimes' features.
    #[must_use]
    pub fn resolve(
        no_ai_flag: bool,
        backend_platform: Result<(), Unavailable>,
        native_platform: Result<(), Unavailable>,
        sdcpp_platform: Result<(), Unavailable>,
    ) -> Self {
        let decide = |runtime: LocalRuntime, platform: Result<(), Unavailable>| match platform {
            Err(unavailable) => {
                Err(LocalAiOff { runtime, reason: LocalAiOffReason::UnavailableOnPlatform(unavailable.reason) })
            }
            Ok(()) if no_ai_flag => Err(LocalAiOff { runtime, reason: LocalAiOffReason::DisabledByFlag }),
            Ok(()) => Ok(()),
        };
        Self {
            backend: decide(LocalRuntime::PythonBackend, backend_platform),
            native_onnx: decide(LocalRuntime::NativeOnnx, native_platform),
            sdcpp: decide(LocalRuntime::SdCpp, sdcpp_platform),
        }
    }
}

/// The process's [`LocalAiAvailability`], installed once by [`install_process`].
static PROCESS_LOCAL_AI: OnceLock<LocalAiAvailability> = OnceLock::new();

/// Installs the process answer (the root's `init_full`, right where it builds it with
/// [`LocalAiAvailability::for_process`]). Only the first call counts: a second one is logged
/// and ignored, so the answer never changes while code may already have read it.
pub fn install_process(availability: LocalAiAvailability) {
    install_into(&PROCESS_LOCAL_AI, availability);
}

/// The process answer [`install_process`] installed; before or without an install (tests,
/// inert contexts, a process that never ran `init_full`) it is
/// `LocalAiAvailability::for_process(false)`: the platform alone decides.
#[must_use]
pub fn process() -> LocalAiAvailability {
    read_from(&PROCESS_LOCAL_AI)
}

/// Core of [`install_process`] over an explicit slot; `true` when this call installed it.
fn install_into(slot: &OnceLock<LocalAiAvailability>, availability: LocalAiAvailability) -> bool {
    match slot.set(availability) {
        Ok(()) => true,
        Err(rejected) => {
            runtime_log::log_warn(format!(
                "[local-ai] the process local-AI availability was already installed; second install ignored. Kept: {:?}; rejected: {rejected:?}",
                slot.get()
            ));
            false
        }
    }
}

/// Core of [`process`] over an explicit slot.
fn read_from(slot: &OnceLock<LocalAiAvailability>) -> LocalAiAvailability {
    slot.get().copied().unwrap_or_else(|| LocalAiAvailability::for_process(false))
}

/// What an AI route runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiRouteKind {
    /// Network-only (cloud AI API): needs no local runtime and is never gated here.
    Cloud,
    /// The in-process native ONNX runtime.
    Native,
    /// The Python backend.
    Backend,
    /// The Stable-Diffusion.cpp worker runtime.
    SdCpp,
}

impl AiRouteKind {
    /// The one availability rule: a cloud route always runs; a native route needs native
    /// ONNX allowed; a backend route needs the backend allowed; an sd.cpp route needs sd.cpp
    /// allowed. Backend HEALTH (connected or
    /// not) is checked separately by the caller after an `Ok(Backend)`.
    ///
    /// # Errors
    /// The [`LocalAiOff`] of the runtime the route needs.
    pub fn availability(self, local_ai: &LocalAiAvailability) -> Result<(), LocalAiOff> {
        match self {
            AiRouteKind::Cloud => Ok(()),
            AiRouteKind::Native => local_ai.native_onnx,
            AiRouteKind::Backend => local_ai.backend,
            AiRouteKind::SdCpp => local_ai.sdcpp,
        }
    }
}

/// The EFFECTIVE AI runtime the routers use: [`AiRuntime::from_user_settings`], except that a
/// stored `Backend` choice reads as `Native` where the platform has no Python backend (a
/// config carried over from desktop must not route everything to a backend that cannot
/// exist). Lives here, not in `ms-config`, because that crate does not see `ms-platform`.
/// Every router (OCR, detector, the settings pane and its checks) reads the runtime through it.
#[must_use]
pub fn effective_ai_runtime(cfg: &Value) -> AiRuntime {
    effective_runtime_for(AiRuntime::from_user_settings(cfg), PlatformFeature::PythonBackend.is_available())
}

/// Pure core of [`effective_ai_runtime`].
fn effective_runtime_for(stored: AiRuntime, backend_on_platform: bool) -> AiRuntime {
    match stored {
        AiRuntime::Backend if !backend_on_platform => AiRuntime::Native,
        AiRuntime::Backend | AiRuntime::Native => stored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absent(feature: PlatformFeature) -> Result<(), Unavailable> {
        Err(Unavailable::new(feature, UnavailableReason::NotOnThisPlatform))
    }

    const PLATFORM_OFF: LocalAiOffReason = LocalAiOffReason::UnavailableOnPlatform(UnavailableReason::NotOnThisPlatform);

    /// Every `(no_ai, backend, native, sdcpp)` platform combination of the truth table.
    fn all_cases() -> Vec<(bool, bool, bool, bool)> {
        let mut cases = Vec::new();
        for no_ai in [false, true] {
            for backend in [false, true] {
                for native in [false, true] {
                    for sdcpp in [false, true] {
                        cases.push((no_ai, backend, native, sdcpp));
                    }
                }
            }
        }
        cases
    }

    fn resolve_case(no_ai: bool, backend: bool, native: bool, sdcpp: bool) -> LocalAiAvailability {
        LocalAiAvailability::resolve(
            no_ai,
            if backend { Ok(()) } else { absent(PlatformFeature::PythonBackend) },
            if native { Ok(()) } else { absent(PlatformFeature::NativeOnnxRuntime) },
            if sdcpp { Ok(()) } else { absent(PlatformFeature::SdCppRuntime) },
        )
    }

    #[test]
    fn resolve_platform_wins_over_flag_per_runtime() {
        for (no_ai, backend_platform, native_platform, sdcpp_platform) in all_cases() {
            let local = resolve_case(no_ai, backend_platform, native_platform, sdcpp_platform);
            let expect = |runtime, on_platform: bool| {
                if !on_platform {
                    Err(LocalAiOff { runtime, reason: PLATFORM_OFF })
                } else if no_ai {
                    Err(LocalAiOff { runtime, reason: LocalAiOffReason::DisabledByFlag })
                } else {
                    Ok(())
                }
            };
            let case = format!("no_ai={no_ai} backend={backend_platform} native={native_platform} sdcpp={sdcpp_platform}");
            assert_eq!(local.backend, expect(LocalRuntime::PythonBackend, backend_platform), "{case}");
            assert_eq!(local.native_onnx, expect(LocalRuntime::NativeOnnx, native_platform), "{case}");
            assert_eq!(local.sdcpp, expect(LocalRuntime::SdCpp, sdcpp_platform), "{case}");
        }
    }

    #[test]
    fn route_kind_availability_all_combinations() {
        for (no_ai, backend_platform, native_platform, sdcpp_platform) in all_cases() {
            let local = resolve_case(no_ai, backend_platform, native_platform, sdcpp_platform);
            let case = format!("no_ai={no_ai} backend={backend_platform} native={native_platform} sdcpp={sdcpp_platform}");
            // Cloud is never gated, not even by the flag.
            assert_eq!(AiRouteKind::Cloud.availability(&local), Ok(()), "{case}");
            for (kind, runtime, on_platform) in [
                (AiRouteKind::Native, LocalRuntime::NativeOnnx, native_platform),
                (AiRouteKind::Backend, LocalRuntime::PythonBackend, backend_platform),
                (AiRouteKind::SdCpp, LocalRuntime::SdCpp, sdcpp_platform),
            ] {
                let result = kind.availability(&local);
                assert_eq!(result.is_ok(), on_platform && !no_ai, "{kind:?} {case}");
                if let Err(off) = result {
                    assert_eq!(off.runtime, runtime, "{kind:?} {case}");
                }
            }
        }
    }

    #[test]
    fn sdcpp_runtime_needs_the_sdcpp_platform_feature() {
        assert_eq!(LocalRuntime::SdCpp.platform_feature(), PlatformFeature::SdCppRuntime);
        let local = LocalAiAvailability::for_process(false);
        assert_eq!(local.sdcpp.is_ok(), PlatformFeature::SdCppRuntime.is_available());
    }

    #[test]
    fn install_keeps_the_first_answer_and_ignores_a_second() {
        let slot = OnceLock::new();
        // Before an install the platform alone decides.
        assert_eq!(read_from(&slot), LocalAiAvailability::for_process(false));
        let first = resolve_case(true, true, true, true);
        let second = resolve_case(false, true, true, true);
        assert!(install_into(&slot, first));
        assert_eq!(read_from(&slot), first);
        assert!(!install_into(&slot, second));
        assert_eq!(read_from(&slot), first);
    }

    #[test]
    fn stored_backend_reads_as_native_without_a_backend_platform() {
        assert_eq!(effective_runtime_for(AiRuntime::Backend, false), AiRuntime::Native);
        assert_eq!(effective_runtime_for(AiRuntime::Backend, true), AiRuntime::Backend);
        assert_eq!(effective_runtime_for(AiRuntime::Native, false), AiRuntime::Native);
        assert_eq!(effective_runtime_for(AiRuntime::Native, true), AiRuntime::Native);
    }

    #[test]
    fn every_off_reason_has_a_message() {
        for runtime in [LocalRuntime::PythonBackend, LocalRuntime::NativeOnnx, LocalRuntime::SdCpp] {
            for reason in [LocalAiOffReason::DisabledByFlag, PLATFORM_OFF] {
                assert!(!LocalAiOff { runtime, reason }.user_message().trim().is_empty(), "{runtime:?} {reason:?}");
            }
        }
    }
}
