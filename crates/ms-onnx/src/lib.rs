/*
File: crates/ms-onnx/src/lib.rs

Purpose:
Crate root of `ms-onnx` — ManhwaStudio's native ONNX Runtime inference
infrastructure (Phase 0). It wraps the `ort` bindings in load-dynamic mode: the
onnxruntime shared library is resolved at RUNTIME from a caller-supplied path and
is never linked or downloaded at build time.

Key types:
- ExecutionProvider     : the inference backend to request
  (CPU/DirectML/CoreML/CUDA/WebGPU/OpenVINO/TensorRT)
- NativeDeviceSelection : accelerator selection (default / numeric index /
  OpenVINO device-type string) applied to the EP when a session is built
- OrtError              : typed error surface for load/warmup and inference failures
- OrtRuntime        : a loaded, dylib-backed ort environment handle
- MangaOcrEngine    : native MangaOCR encoder+decoder inference (see `manga_ocr`)
- PaddleOcrEngine   : native PaddleOCR detection+recognition (see `paddle_ocr`)

Key functions:
- OrtRuntime::load          : dlopen the onnxruntime library and commit the ort environment options
- OrtRuntime::warmup        : create the ort environment (executes real onnxruntime code)
- OrtRuntime::build_session : build an inference session applying the committed execution provider

Notes:
Pure inference infrastructure: no egui/eframe, no application config, no paths,
and no download logic. The caller owns dylib resolution and provider selection.
The ort environment is a process-global singleton owned by the `ort` crate; an
`OrtRuntime` is the app-side handle for the committed, dylib-backed environment.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]
// The crate is intentionally named after the domain concept it exports.
#![allow(clippy::module_name_repetitions)]
// `doc_markdown` systematically false-positives on the domain acronyms this crate's
// docs must name literally (MangaOCR, WordPiece, NCHW, ViTImageProcessor, ONNX
// export names, NumPy, EOS); backticking every occurrence would harm readability
// without adding meaning. Suppressed crate-wide as a not-applicable style lint.
#![allow(clippy::doc_markdown)]

pub mod manga_ocr;
pub mod paddle_ocr;

pub use manga_ocr::MangaOcrEngine;
pub use paddle_ocr::{
    PaddleDetection, PaddleDetector, PaddleLine, PaddleOcrEngine, PaddleRecognizer,
    paddle_recognize,
};

use std::path::{Path, PathBuf};

use ms_log::trace::cat;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;

/// Inference backend requested from ONNX Runtime.
///
/// The stable [`ExecutionProvider::id`] string is used by higher layers to scope
/// per-provider configuration; the ids must stay stable across releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionProvider {
    /// Portable CPU backend; available on every platform.
    Cpu,
    /// `DirectML` (`Direct3D` 12) GPU backend; Windows only.
    DirectMl,
    /// Core ML backend; macOS only.
    CoreMl,
    /// NVIDIA CUDA GPU backend; Linux and Windows.
    Cuda,
    /// WebGPU GPU backend via Dawn (D3D12 on Windows, Vulkan on Linux, Metal on
    /// macOS); a distinct cross-vendor GPU backend registered through ort's
    /// standard append path.
    WebGpu,
    /// Intel OpenVINO backend (x86_64 Windows/Linux). Targets Intel CPUs, iGPUs,
    /// dGPUs, and NPUs and needs an Intel device plus the OpenVINO runtime
    /// available to onnxruntime (self-contained in the Linux OpenVINO wheel; a
    /// system OpenVINO SDK on Windows). Its device is selected by a device-TYPE
    /// string (`"CPU"`/`"GPU"`/`"GPU.0"`/`"NPU"`/`"HETERO:..."`), not a numeric
    /// index — see [`NativeDeviceSelection::OpenVinoDeviceType`].
    OpenVino,
    /// NVIDIA TensorRT backend (Windows/Linux). Bundled in the CUDA ("gpu") build
    /// and needs the same NVIDIA/CUDA stack as [`ExecutionProvider::Cuda`].
    /// onnxruntime falls back per-node to CPU for TensorRT-unsupported ops. Its
    /// adapter is selected by a numeric device id
    /// ([`NativeDeviceSelection::Index`]).
    TensorRt,
}

impl ExecutionProvider {
    /// Stable, lowercase identifier for this provider.
    ///
    /// Used as a scoping key for per-provider configuration; the returned values
    /// (`"cpu"`, `"directml"`, `"coreml"`, `"cuda"`, `"webgpu"`, `"openvino"`,
    /// `"tensorrt"`) are part of the crate's contract and must not change.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            ExecutionProvider::Cpu => "cpu",
            ExecutionProvider::DirectMl => "directml",
            ExecutionProvider::CoreMl => "coreml",
            ExecutionProvider::Cuda => "cuda",
            ExecutionProvider::WebGpu => "webgpu",
            ExecutionProvider::OpenVino => "openvino",
            ExecutionProvider::TensorRt => "tensorrt",
        }
    }

    /// Whether this provider can run on the platform this binary was built for.
    ///
    /// `DirectML` is Windows-only, Core ML is macOS-only, and CUDA is unavailable
    /// on macOS; the CPU provider is always available. WebGPU runs on all three
    /// desktop targets via Dawn (D3D12/Vulkan/Metal), so it is available on
    /// Windows, Linux, and macOS. Higher layers use this to query provider
    /// availability without duplicating the `cfg(target_os)` logic (e.g. to decide
    /// which providers to offer). The result reflects the target this binary was
    /// compiled for, evaluated at compile time.
    ///
    /// Note: rc.12's own `ort::ep::WebGPU::supported_by_platform` returns `false`
    /// on macOS (it only counts Windows/Linux/wasm), but that predicate merely
    /// governs ort's internal log verbosity — EP registration still goes through
    /// the standard append path and works on macOS given a WebGPU-capable dylib.
    /// This crate therefore reports macOS as available rather than mirroring ort's
    /// log-only predicate.
    ///
    /// OpenVINO is limited to x86_64 Windows/Linux (mirroring ort's own
    /// `OpenVINO::supported_by_platform`) and additionally requires an Intel device
    /// plus the OpenVINO runtime reachable by onnxruntime (self-contained in the
    /// Linux OpenVINO wheel; a system OpenVINO SDK on Windows) — availability here
    /// only reflects the build target, not the presence of Intel hardware/runtime.
    /// TensorRT mirrors CUDA (Windows/Linux, not macOS): it is bundled in the CUDA
    /// ("gpu") build and needs the NVIDIA/CUDA stack; this predicate does not probe
    /// for a working NVIDIA driver.
    #[must_use]
    pub fn is_available_on_current_platform(self) -> bool {
        match self {
            ExecutionProvider::Cpu => true,
            ExecutionProvider::DirectMl => cfg!(target_os = "windows"),
            ExecutionProvider::CoreMl => cfg!(target_os = "macos"),
            ExecutionProvider::Cuda => !cfg!(target_os = "macos"),
            ExecutionProvider::WebGpu => {
                cfg!(any(target_os = "windows", target_os = "linux", target_os = "macos"))
            }
            ExecutionProvider::OpenVino => {
                cfg!(all(target_arch = "x86_64", any(target_os = "windows", target_os = "linux")))
            }
            ExecutionProvider::TensorRt => {
                cfg!(any(target_os = "windows", target_os = "linux"))
            }
        }
    }
}

/// Accelerator selection applied to the execution provider when a session is built.
///
/// Different EPs identify a device differently, so this enum carries the shapes the
/// crate supports rather than a single `i32`:
/// - [`NativeDeviceSelection::Default`] — no explicit device; the EP picks its own
///   default (adapter 0 for index-based EPs, OpenVINO's own default device).
/// - [`NativeDeviceSelection::Index`] — a numeric adapter index, used by
///   DirectML/CUDA/TensorRT/WebGPU via `with_device_id(i32)`.
/// - [`NativeDeviceSelection::OpenVinoDeviceType`] — an OpenVINO device-TYPE string
///   (`"CPU"`/`"GPU"`/`"GPU.0"`/`"NPU"`/`"HETERO:..."`), used only by OpenVINO via
///   `with_device_type(&str)`.
///
/// Each `build_session` arm applies only the variant meaningful for its provider and
/// ignores the others (e.g. CUDA ignores `OpenVinoDeviceType`; OpenVINO ignores
/// `Index`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NativeDeviceSelection {
    /// The execution provider's own default device (no explicit selection).
    #[default]
    Default,
    /// A numeric accelerator adapter index (DirectML/CUDA/TensorRT/WebGPU).
    Index(i32),
    /// An OpenVINO device-TYPE string (e.g. `"GPU.0"`, `"NPU"`, `"HETERO:GPU,CPU"`).
    OpenVinoDeviceType(String),
}

impl NativeDeviceSelection {
    /// The numeric adapter index if this selection is [`NativeDeviceSelection::Index`],
    /// otherwise `None`.
    ///
    /// Convenience for index-based providers (DirectML/CUDA/TensorRT/WebGPU); it
    /// intentionally maps both [`NativeDeviceSelection::Default`] and
    /// [`NativeDeviceSelection::OpenVinoDeviceType`] to `None` (no numeric adapter).
    #[must_use]
    pub fn index(&self) -> Option<i32> {
        match self {
            NativeDeviceSelection::Index(id) => Some(*id),
            NativeDeviceSelection::Default | NativeDeviceSelection::OpenVinoDeviceType(_) => None,
        }
    }
}

/// Errors from loading or warming up ONNX Runtime.
#[derive(Debug, thiserror::Error)]
pub enum OrtError {
    /// The onnxruntime shared library does not exist at the given path.
    #[error("Библиотека ONNX Runtime не найдена по пути: {0}")]
    LibraryNotFound(PathBuf),

    /// Loading the shared library or committing the ort environment failed.
    /// Wraps the underlying `ort` error text.
    #[error("Не удалось загрузить ONNX Runtime: {0}")]
    LoadFailed(String),

    /// Creating/initializing the ort environment during warmup failed.
    /// Wraps the underlying `ort` error text.
    #[error("Не удалось инициализировать ONNX Runtime: {0}")]
    WarmupFailed(String),

    /// The requested execution provider is not available on this platform.
    #[error("Провайдер выполнения ONNX Runtime «{0}» недоступен на этой платформе")]
    UnsupportedProvider(&'static str),

    /// Building an inference session from a model file failed (missing file,
    /// unreadable model, or ort/session-builder error). `reason` carries the
    /// underlying ort error text for diagnostics.
    #[error("Не удалось создать сессию ONNX Runtime для модели «{path}»: {reason}")]
    SessionBuild {
        /// Path of the model file the session build was attempted from.
        path: PathBuf,
        /// Underlying ort error text.
        reason: String,
    },

    /// Running a model (encoder or decoder) failed. `stage` names which model
    /// call failed; `reason` carries the underlying ort error text.
    #[error("Ошибка инференса ONNX Runtime на этапе «{stage}»: {reason}")]
    Inference {
        /// Inference stage that failed (`"encoder"` / `"decoder"`).
        stage: &'static str,
        /// Underlying ort error text.
        reason: String,
    },

    /// A model input/output had an unexpected shape, dtype, or was missing.
    /// `detail` describes the mismatch for diagnosis.
    #[error("Непредвиденная форма тензора ONNX Runtime ({detail})")]
    TensorShape {
        /// Human-readable description of the shape/dtype mismatch.
        detail: String,
    },

    /// Loading or parsing the tokenizer vocabulary (`vocab.txt`) failed.
    #[error("Не удалось загрузить словарь MangaOCR «{path}»: {detail}")]
    VocabLoad {
        /// Path of the `vocab.txt` file that could not be loaded.
        path: PathBuf,
        /// Reason the vocabulary could not be loaded/parsed.
        detail: String,
    },

    /// The input image could not be prepared for inference (e.g. empty image).
    #[error("Не удалось подготовить изображение для MangaOCR: {detail}")]
    ImagePreprocess {
        /// Reason preprocessing failed.
        detail: String,
    },

    /// Loading or parsing the PaddleOCR character dictionary (`dict.txt`) failed.
    #[error("Не удалось загрузить словарь PaddleOCR «{path}»: {detail}")]
    PaddleDictLoad {
        /// Path of the `dict.txt` file that could not be loaded.
        path: PathBuf,
        /// Reason the dictionary could not be loaded/parsed.
        detail: String,
    },
}

/// A loaded, dylib-backed ONNX Runtime environment handle.
///
/// Construct with [`OrtRuntime::load`], then call [`OrtRuntime::warmup`] to force
/// real onnxruntime environment initialization. The underlying ort environment is
/// a process-global singleton owned by the `ort` crate; this handle records the
/// dylib path and provider that were committed.
#[derive(Debug)]
pub struct OrtRuntime {
    /// Path the onnxruntime library was loaded from (kept for diagnostics).
    dylib_path: PathBuf,
    /// Execution provider this runtime was loaded for; applied to sessions later.
    provider: ExecutionProvider,
    /// Accelerator selection applied to the EP when it is registered.
    ///
    /// Meaningful only for the index-based EPs (DirectML/CUDA/TensorRT/WebGPU, which
    /// read [`NativeDeviceSelection::Index`]) and OpenVINO (which reads
    /// [`NativeDeviceSelection::OpenVinoDeviceType`]); ignored by CPU/CoreML.
    /// [`NativeDeviceSelection::Default`] requests each provider's default device.
    device: NativeDeviceSelection,
}

impl OrtRuntime {
    /// Loads onnxruntime from `dylib_path` and commits the ort environment options.
    ///
    /// `dylib_path` must point to the onnxruntime shared library (`libonnxruntime.so`
    /// / `onnxruntime.dll` / `libonnxruntime.dylib`). The library is dlopened here
    /// and its ABI/version is verified, but the environment itself is created lazily
    /// in [`OrtRuntime::warmup`]. This never panics on a missing or invalid library:
    /// every failure is mapped to an [`OrtError`].
    ///
    /// `device` selects the accelerator applied to the execution provider when the
    /// session is built (see [`OrtRuntime::build_session`]). It is honored only by
    /// the index-based EPs (DirectML/CUDA/TensorRT/WebGPU, via
    /// [`NativeDeviceSelection::Index`]) and OpenVINO (via
    /// [`NativeDeviceSelection::OpenVinoDeviceType`]); it is ignored by CPU/CoreML.
    /// [`NativeDeviceSelection::Default`] selects each provider's default device. The
    /// value is stored on the returned runtime and does not affect dylib loading here.
    ///
    /// # Errors
    /// - [`OrtError::UnsupportedProvider`] if `provider` cannot run on this platform.
    /// - [`OrtError::LibraryNotFound`] if `dylib_path` does not exist.
    /// - [`OrtError::LoadFailed`] if the library cannot be loaded or its ABI/version
    ///   is incompatible.
    pub fn load(
        dylib_path: &Path,
        provider: ExecutionProvider,
        device: NativeDeviceSelection,
    ) -> Result<Self, OrtError> {
        ms_log::trace_log!(
            cat::STARTUP,
            "ms-onnx load start provider={} path={}",
            provider.id(),
            dylib_path.display()
        );

        // Reject providers that cannot run on this platform before touching ort,
        // so callers get a precise error instead of a later session failure.
        if !provider.is_available_on_current_platform() {
            return Err(OrtError::UnsupportedProvider(provider.id()));
        }

        // Distinguish "missing file" from "load failed" up front. `ort::init_from`
        // would otherwise surface a generic dlopen error, and for a relative path
        // it also probes the executable directory; we require the caller-supplied
        // path to exist as given.
        if !dylib_path.exists() {
            return Err(OrtError::LibraryNotFound(dylib_path.to_path_buf()));
        }

        // load-dynamic: dlopen the onnxruntime library from `dylib_path`, verify
        // its `OrtGetApiBase`/version, then commit the environment options. This
        // runs real loader code (any ABI/version mismatch fails here) but does NOT
        // yet create the ort environment — that happens in `warmup`.
        let committed = ort::init_from(dylib_path)
            .map_err(|e| OrtError::LoadFailed(e.to_string()))?
            .with_name("manhwastudio")
            .commit();
        if !committed {
            // ort's environment options are a process-global set: a prior `load`
            // already committed them. The dylib is loaded, but the earlier options
            // (name/providers) stay in effect. Record it for diagnostics.
            ms_log::trace_log!(
                cat::STARTUP,
                "ms-onnx load: ort environment already committed; reusing existing options"
            );
        }

        ms_log::trace_log!(cat::STARTUP, "ms-onnx load done provider={}", provider.id());
        Ok(OrtRuntime {
            dylib_path: dylib_path.to_path_buf(),
            provider,
            device,
        })
    }

    /// Execution provider this runtime was loaded for.
    #[must_use]
    pub fn provider(&self) -> ExecutionProvider {
        self.provider
    }

    /// Accelerator selection this runtime applies when building sessions.
    ///
    /// [`NativeDeviceSelection::Default`] means each provider's default device; the
    /// value is ignored for the CPU and CoreML providers.
    #[must_use]
    pub fn device(&self) -> &NativeDeviceSelection {
        &self.device
    }

    /// Convenience: the numeric adapter index for index-based providers, or `None`.
    ///
    /// Equivalent to `self.device().index()`; returns `None` for
    /// [`NativeDeviceSelection::Default`] and [`NativeDeviceSelection::OpenVinoDeviceType`].
    #[must_use]
    pub fn device_id(&self) -> Option<i32> {
        self.device.index()
    }

    /// Test-only constructor building an `OrtRuntime` handle without touching a
    /// dylib, so accessor contracts (`provider`/`device`) can be asserted
    /// deterministically without a real onnxruntime binary.
    #[cfg(test)]
    fn for_test(provider: ExecutionProvider, device: NativeDeviceSelection) -> Self {
        OrtRuntime {
            dylib_path: PathBuf::from("/test/only/no-dylib"),
            provider,
            device,
        }
    }

    /// Builds an inference session from `model_path` for this runtime's provider.
    ///
    /// The session is created with all graph optimizations enabled
    /// (`GraphOptimizationLevel::All`), matching the Python reference's
    /// `ORT_ENABLE_ALL`. The execution provider recorded at [`OrtRuntime::load`] is
    /// then applied:
    /// - [`ExecutionProvider::Cpu`] registers NO execution provider, so ONNX
    ///   Runtime uses its built-in CPU backend (byte-identical to the historical
    ///   MangaOCR session builder).
    /// - [`ExecutionProvider::DirectMl`] / [`ExecutionProvider::CoreMl`] /
    ///   [`ExecutionProvider::Cuda`] / [`ExecutionProvider::WebGpu`] /
    ///   [`ExecutionProvider::OpenVino`] / [`ExecutionProvider::TensorRt`] register
    ///   the matching EP with `error_on_failure()`, so a broken GPU/accelerator
    ///   setup surfaces as an [`OrtError::SessionBuild`] instead of silently falling
    ///   back to CPU. (TensorRT is registered alone; onnxruntime still falls back
    ///   per-node to CPU for ops TensorRT cannot run — that is not an EP failure.)
    ///
    /// The runtime's [`OrtRuntime::device`] selects the accelerator:
    /// - DirectML/CUDA/TensorRT/WebGPU read [`NativeDeviceSelection::Index`]: when
    ///   present, the `i32` adapter index is passed via `with_device_id(id)`;
    ///   otherwise the EP is registered with `default()` (adapter 0 / the provider's
    ///   default device-selection path).
    /// - OpenVINO reads [`NativeDeviceSelection::OpenVinoDeviceType`]: when present,
    ///   the device-TYPE string is passed via `with_device_type(s)`; otherwise
    ///   OpenVINO chooses its own default device.
    /// - CoreML and CPU ignore the device selection entirely.
    ///
    /// WebGPU additionally pins the Dawn backend to the platform's native GPU API
    /// (D3D12 on Windows, Vulkan on Linux); on macOS no backend is set and Dawn
    /// defaults to Metal (rc.12 exposes no `Metal` `DawnBackendType` variant).
    ///
    /// The EP registration wrappers (`ort::ep::*`) are available under the
    /// `load-dynamic` cargo feature alone; the per-EP cargo features
    /// (`directml`/`coreml`/`cuda`/`openvino`/`tensorrt`) are intentionally NOT
    /// enabled (they conflict with load-dynamic's `disable-linking`). The provider
    /// was already gated to the current platform by [`OrtRuntime::load`], so no
    /// re-check is done here.
    ///
    /// # Errors
    /// [`OrtError::SessionBuild`] if the builder cannot be created, the execution
    /// provider registration fails, or the model file is missing/unreadable.
    pub fn build_session(&self, model_path: &Path) -> Result<Session, OrtError> {
        // The ort builder methods return DISTINCT error generics (`ort::Error<T>`),
        // so each fallible step maps its own error inline (Display -> String) rather
        // than sharing one closure — a single closure would fix `T` and not typecheck.
        let builder = Session::builder().map_err(|e| OrtError::SessionBuild {
            path: model_path.to_path_buf(),
            reason: e.to_string(),
        })?;
        let builder = builder
            .with_optimization_level(GraphOptimizationLevel::All)
            .map_err(|e| OrtError::SessionBuild {
                path: model_path.to_path_buf(),
                reason: e.to_string(),
            })?;

        // Register the committed provider (if any) and commit the session. CPU
        // registers nothing, keeping the historical CPU-only builder byte-identical.
        let mut builder = builder;
        if let Some(ep) = self.execution_provider_dispatch() {
            builder =
                builder.with_execution_providers([ep]).map_err(|e| OrtError::SessionBuild {
                    path: model_path.to_path_buf(),
                    reason: e.to_string(),
                })?;
        }

        builder.commit_from_file(model_path).map_err(|e| OrtError::SessionBuild {
            path: model_path.to_path_buf(),
            reason: e.to_string(),
        })
    }

    /// Builds the ort execution-provider dispatch for this runtime's provider.
    ///
    /// Returns `None` for [`ExecutionProvider::Cpu`] (no EP is registered, so ONNX
    /// Runtime uses its built-in CPU backend). For every other provider it returns
    /// the matching `ort::ep::*` dispatch with `error_on_failure()` set and the
    /// runtime's [`NativeDeviceSelection`] applied per the provider's device model
    /// (numeric `Index` for DirectML/CUDA/TensorRT/WebGPU, device-TYPE string for
    /// OpenVINO). See [`OrtRuntime::build_session`] for the full contract. The match
    /// is exhaustive: a new provider variant must force a decision here.
    fn execution_provider_dispatch(&self) -> Option<ort::ep::ExecutionProviderDispatch> {
        match self.provider {
            ExecutionProvider::Cpu => None,
            ExecutionProvider::DirectMl => {
                // `with_device_id` selects a specific adapter via the `_DML` append
                // path; `default()` alone keeps the `_DML2` device-filter path, so
                // only set the id when the caller requested a concrete adapter.
                Some(match self.device.index() {
                    Some(id) => ort::ep::DirectMLExecutionProvider::default()
                        .with_device_id(id)
                        .build()
                        .error_on_failure(),
                    None => ort::ep::DirectMLExecutionProvider::default().build().error_on_failure(),
                })
            }
            ExecutionProvider::CoreMl => {
                Some(ort::ep::CoreMLExecutionProvider::default().build().error_on_failure())
            }
            ExecutionProvider::Cuda => {
                // CUDA takes the adapter as its `device_id` provider option; leave it
                // unset (`default()`) to fall back to onnxruntime's default device 0.
                Some(match self.device.index() {
                    Some(id) => ort::ep::CUDAExecutionProvider::default()
                        .with_device_id(id)
                        .build()
                        .error_on_failure(),
                    None => ort::ep::CUDAExecutionProvider::default().build().error_on_failure(),
                })
            }
            ExecutionProvider::TensorRt => {
                // TensorRT takes the adapter as its `device_id` provider option; leave
                // it unset (`default()`) to fall back to onnxruntime's default device.
                // Registered alone: onnxruntime falls back per-node to CPU for ops
                // TensorRT does not support (that is not an EP-registration failure).
                Some(match self.device.index() {
                    Some(id) => ort::ep::TensorRT::default()
                        .with_device_id(id)
                        .build()
                        .error_on_failure(),
                    None => ort::ep::TensorRT::default().build().error_on_failure(),
                })
            }
            ExecutionProvider::OpenVino => {
                // OpenVINO selects its device by a device-TYPE string, not a numeric
                // index; apply it only when the caller provided one, otherwise leave
                // OpenVINO to choose its own default device.
                Some(match &self.device {
                    NativeDeviceSelection::OpenVinoDeviceType(device_type) => {
                        ort::ep::OpenVINO::default()
                            .with_device_type(device_type)
                            .build()
                            .error_on_failure()
                    }
                    NativeDeviceSelection::Default | NativeDeviceSelection::Index(_) => {
                        ort::ep::OpenVINO::default().build().error_on_failure()
                    }
                })
            }
            ExecutionProvider::WebGpu => {
                let mut ep = ort::ep::WebGPUExecutionProvider::default();
                // WebGPU takes the adapter as its `deviceId` provider option; leave
                // it unset (`default()`) to fall back to the default adapter.
                if let Some(id) = self.device.index() {
                    ep = ep.with_device_id(id);
                }
                // Pin the Dawn backend to the platform's native GPU API. On macOS no
                // backend is set: rc.12's `DawnBackendType` has only `Vulkan`/`D3D12`
                // (no `Metal`), and Dawn defaults to Metal there.
                #[cfg(target_os = "windows")]
                {
                    ep = ep.with_dawn_backend_type(ort::ep::webgpu::DawnBackendType::D3D12);
                }
                #[cfg(target_os = "linux")]
                {
                    ep = ep.with_dawn_backend_type(ort::ep::webgpu::DawnBackendType::Vulkan);
                }
                Some(ep.build().error_on_failure())
            }
        }
    }

    /// Forces ONNX Runtime environment initialization, executing real onnxruntime code.
    ///
    /// # Errors
    /// [`OrtError::WarmupFailed`] if the ort environment cannot be created.
    pub fn warmup(&self) -> Result<(), OrtError> {
        ms_log::trace_log!(
            cat::STARTUP,
            "ms-onnx warmup start provider={} path={}",
            self.provider.id(),
            self.dylib_path.display()
        );

        // Phase 0: force creation of the ort environment (onnxruntime `CreateEnv`
        // + default allocator). This executes real onnxruntime code — this is
        // where CPU-feature-dispatch SIGILL on an unsupported CPU would trigger.
        // Phase 1 will extend this to a real minimal 1x1 inference so the compute
        // kernels (MLAS dispatch) are exercised too, not just environment setup.
        ort::environment::current().map_err(|e| OrtError::WarmupFailed(e.to_string()))?;

        ms_log::trace_log!(cat::STARTUP, "ms-onnx warmup done provider={}", self.provider.id());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_dylib_returns_typed_error_without_panic() {
        // A path that cannot exist on either check target. `load` must map this to
        // a typed error and never panic, and must not require a real onnxruntime.
        let missing = Path::new("/nonexistent/ms-onnx/does-not-exist/libonnxruntime.so");
        let result =
            OrtRuntime::load(missing, ExecutionProvider::Cpu, NativeDeviceSelection::Default);
        assert!(matches!(result, Err(OrtError::LibraryNotFound(_))));
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn load_rejects_coreml_off_macos() {
        // Provider gating happens before any dylib access, so no onnxruntime binary
        // is needed. Both check targets (linux-gnu, windows-gnu) are non-macOS.
        let any = Path::new("libonnxruntime.so");
        assert!(matches!(
            OrtRuntime::load(any, ExecutionProvider::CoreMl, NativeDeviceSelection::Default),
            Err(OrtError::UnsupportedProvider("coreml"))
        ));
    }

    #[test]
    fn device_accessor_round_trips_stored_selection() {
        // `device()` must return exactly what was stored for every variant, and the
        // `device_id()` convenience must project only `Index` to a numeric adapter.
        let cpu = OrtRuntime::for_test(ExecutionProvider::Cpu, NativeDeviceSelection::Default);
        assert_eq!(cpu.device(), &NativeDeviceSelection::Default);
        assert_eq!(cpu.device_id(), None);

        let cuda =
            OrtRuntime::for_test(ExecutionProvider::Cuda, NativeDeviceSelection::Index(2));
        assert_eq!(cuda.device(), &NativeDeviceSelection::Index(2));
        assert_eq!(cuda.device_id(), Some(2));

        // A negative index is stored verbatim (interpretation is the EP's concern).
        let dml = OrtRuntime::for_test(ExecutionProvider::DirectMl, NativeDeviceSelection::Index(-1));
        assert_eq!(dml.device_id(), Some(-1));
        assert_eq!(dml.provider(), ExecutionProvider::DirectMl);

        // OpenVINO's device-type string round-trips through `device()` and does NOT
        // project to a numeric adapter id.
        let ov = OrtRuntime::for_test(
            ExecutionProvider::OpenVino,
            NativeDeviceSelection::OpenVinoDeviceType("GPU.0".to_string()),
        );
        assert_eq!(
            ov.device(),
            &NativeDeviceSelection::OpenVinoDeviceType("GPU.0".to_string())
        );
        assert_eq!(ov.device_id(), None);
    }

    #[test]
    fn tensorrt_round_trips_numeric_index() {
        // TensorRT is an index-based provider: its adapter selection projects through
        // the `device_id()` convenience like DirectML/CUDA.
        let trt =
            OrtRuntime::for_test(ExecutionProvider::TensorRt, NativeDeviceSelection::Index(1));
        assert_eq!(trt.device(), &NativeDeviceSelection::Index(1));
        assert_eq!(trt.device_id(), Some(1));
        assert_eq!(trt.provider(), ExecutionProvider::TensorRt);
    }

    #[test]
    fn provider_ids_are_stable() {
        assert_eq!(ExecutionProvider::Cpu.id(), "cpu");
        assert_eq!(ExecutionProvider::DirectMl.id(), "directml");
        assert_eq!(ExecutionProvider::CoreMl.id(), "coreml");
        assert_eq!(ExecutionProvider::Cuda.id(), "cuda");
        assert_eq!(ExecutionProvider::WebGpu.id(), "webgpu");
        assert_eq!(ExecutionProvider::OpenVino.id(), "openvino");
        assert_eq!(ExecutionProvider::TensorRt.id(), "tensorrt");
    }

    #[test]
    fn cpu_is_always_available() {
        // CPU is the portable backend; it must be available on every target.
        assert!(ExecutionProvider::Cpu.is_available_on_current_platform());
    }

    #[test]
    fn provider_availability_matches_current_target() {
        // The predicate reflects the compile-time target. Assert each provider
        // against this build's `cfg(target_os)` so both GNU check targets are
        // covered (Windows: DirectML on, CoreML off, CUDA on, WebGPU on; Linux:
        // DirectML off, CoreML off, CUDA on, WebGPU on).
        assert_eq!(
            ExecutionProvider::DirectMl.is_available_on_current_platform(),
            cfg!(target_os = "windows")
        );
        assert_eq!(
            ExecutionProvider::CoreMl.is_available_on_current_platform(),
            cfg!(target_os = "macos")
        );
        assert_eq!(
            ExecutionProvider::Cuda.is_available_on_current_platform(),
            !cfg!(target_os = "macos")
        );
        // WebGPU runs on all three desktop targets via Dawn.
        assert_eq!(
            ExecutionProvider::WebGpu.is_available_on_current_platform(),
            cfg!(any(target_os = "windows", target_os = "linux", target_os = "macos"))
        );
        // OpenVINO: x86_64 Windows/Linux only (mirrors ort's own predicate).
        assert_eq!(
            ExecutionProvider::OpenVino.is_available_on_current_platform(),
            cfg!(all(target_arch = "x86_64", any(target_os = "windows", target_os = "linux")))
        );
        // TensorRT: Windows/Linux, not macOS (mirrors CUDA).
        assert_eq!(
            ExecutionProvider::TensorRt.is_available_on_current_platform(),
            cfg!(any(target_os = "windows", target_os = "linux"))
        );
    }

    #[test]
    fn native_device_selection_index_projection() {
        // The `index()` helper projects only `Index`; `Default` and
        // `OpenVinoDeviceType` carry no numeric adapter.
        assert_eq!(NativeDeviceSelection::Default.index(), None);
        assert_eq!(NativeDeviceSelection::Index(3).index(), Some(3));
        assert_eq!(
            NativeDeviceSelection::OpenVinoDeviceType("NPU".to_string()).index(),
            None
        );
        // The default is `Default` (no explicit device).
        assert_eq!(NativeDeviceSelection::default(), NativeDeviceSelection::Default);
    }
}
