/*
FILE OVERVIEW: crates/ms-local-ai/src/local_ai.rs
Which LOCAL AI runtimes this process may use, and the one rule that turns an AI route into
"can run here" or a typed reason.

Semantics (the `--no-ai` contract):
- `--no-ai` switches off every LOCAL AI runtime: the Python backend AND the in-process native
  ONNX runtime. Cloud AI API (OCR and MT) and the plain machine translators
  (Google / Yandex / DeepL) are network code and are NEVER gated by it.
- Independently, the platform decides: the backend needs `PlatformFeature::PythonBackend`,
  native ONNX needs `PlatformFeature::NativeOnnxRuntime`. A platform reason wins over the
  flag (restarting without `--no-ai` would not help there).

Key items:
- `LocalAiAvailability`: the per-process answer for both runtimes, built once in `run_main`
  (`for_process`) and carried by the backend supervisor handle to the studio tabs and the
  settings panes.
- `LocalAiOff` / `LocalRuntime` / `LocalAiOffReason`: why one runtime is off; `user_message`
  is the localized text every refusal shows.
- `AiRouteKind`: what an AI route runs on (cloud, native ONNX, backend);
  `AiRouteKind::availability` is the ONE availability rule. OCR (`ms_tab_translation`'s `ocr::ocr_route_availability`)
  and text detection (`TextDetectorAlgorithm::availability`) map their routes onto it.
- `effective_ai_runtime`: the runtime the routers use — `AiRuntime::from_user_settings`, except
  that a stored `Backend` choice reads as `Native` where the platform has no Python backend.
*/

use ms_config::AiRuntime;
use ms_platform::{PlatformFeature, Unavailable, UnavailableReason};
use serde_json::Value;

/// A local AI runtime that can be switched off for the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalRuntime {
    /// The out-of-process Python AI backend.
    PythonBackend,
    /// The in-process native ONNX runtime.
    NativeOnnx,
}

impl LocalRuntime {
    /// The platform capability this runtime needs.
    #[must_use]
    pub const fn platform_feature(self) -> PlatformFeature {
        match self {
            LocalRuntime::PythonBackend => PlatformFeature::PythonBackend,
            LocalRuntime::NativeOnnx => PlatformFeature::NativeOnnxRuntime,
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
        )
    }

    /// Pure core of [`Self::for_process`]: a platform `Err` wins over the flag for its runtime;
    /// otherwise `no_ai_flag` switches the runtime off with [`LocalAiOffReason::DisabledByFlag`].
    #[must_use]
    pub fn resolve(
        no_ai_flag: bool,
        backend_platform: Result<(), Unavailable>,
        native_platform: Result<(), Unavailable>,
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
        }
    }
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
}

impl AiRouteKind {
    /// The one availability rule: a cloud route always runs; a native route needs native
    /// ONNX allowed; a backend route needs the backend allowed. Backend HEALTH (connected or
    /// not) is checked separately by the caller after an `Ok(Backend)`.
    ///
    /// # Errors
    /// The [`LocalAiOff`] of the runtime the route needs.
    pub fn availability(self, local_ai: &LocalAiAvailability) -> Result<(), LocalAiOff> {
        match self {
            AiRouteKind::Cloud => Ok(()),
            AiRouteKind::Native => local_ai.native_onnx,
            AiRouteKind::Backend => local_ai.backend,
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

    #[test]
    fn resolve_platform_wins_over_flag_per_runtime() {
        for no_ai in [false, true] {
            for backend_platform in [false, true] {
                for native_platform in [false, true] {
                    let local = LocalAiAvailability::resolve(
                        no_ai,
                        if backend_platform { Ok(()) } else { absent(PlatformFeature::PythonBackend) },
                        if native_platform { Ok(()) } else { absent(PlatformFeature::NativeOnnxRuntime) },
                    );
                    let expect = |runtime, on_platform: bool| {
                        if !on_platform {
                            Err(LocalAiOff { runtime, reason: PLATFORM_OFF })
                        } else if no_ai {
                            Err(LocalAiOff { runtime, reason: LocalAiOffReason::DisabledByFlag })
                        } else {
                            Ok(())
                        }
                    };
                    let case = format!("no_ai={no_ai} backend={backend_platform} native={native_platform}");
                    assert_eq!(local.backend, expect(LocalRuntime::PythonBackend, backend_platform), "{case}");
                    assert_eq!(local.native_onnx, expect(LocalRuntime::NativeOnnx, native_platform), "{case}");
                }
            }
        }
    }

    #[test]
    fn route_kind_availability_all_combinations() {
        for no_ai in [false, true] {
            for backend_platform in [false, true] {
                for native_platform in [false, true] {
                    let local = LocalAiAvailability::resolve(
                        no_ai,
                        if backend_platform { Ok(()) } else { absent(PlatformFeature::PythonBackend) },
                        if native_platform { Ok(()) } else { absent(PlatformFeature::NativeOnnxRuntime) },
                    );
                    let case = format!("no_ai={no_ai} backend={backend_platform} native={native_platform}");
                    // Cloud is never gated, not even by the flag.
                    assert_eq!(AiRouteKind::Cloud.availability(&local), Ok(()), "{case}");
                    assert_eq!(
                        AiRouteKind::Native.availability(&local).is_ok(),
                        native_platform && !no_ai,
                        "{case}"
                    );
                    assert_eq!(
                        AiRouteKind::Backend.availability(&local).is_ok(),
                        backend_platform && !no_ai,
                        "{case}"
                    );
                    if let Err(off) = AiRouteKind::Native.availability(&local) {
                        assert_eq!(off.runtime, LocalRuntime::NativeOnnx, "{case}");
                    }
                    if let Err(off) = AiRouteKind::Backend.availability(&local) {
                        assert_eq!(off.runtime, LocalRuntime::PythonBackend, "{case}");
                    }
                }
            }
        }
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
        for runtime in [LocalRuntime::PythonBackend, LocalRuntime::NativeOnnx] {
            for reason in [LocalAiOffReason::DisabledByFlag, PLATFORM_OFF] {
                assert!(!LocalAiOff { runtime, reason }.user_message().trim().is_empty(), "{runtime:?} {reason:?}");
            }
        }
    }
}
