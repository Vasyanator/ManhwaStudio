/*
FILE OVERVIEW: src/native_runtime.rs
Process-global lazy manager for the in-process native ONNX Runtime path.

Purpose:
Owns the single, lazily-loaded `ms_onnx::OrtRuntime` plus a small, LRU-bounded cache
of native OCR engines. Exactly ONE `PaddleDetector` is kept resident and shared by
the `textdetector.paddle` op AND every PaddleOCR-language recognition (through
`ms_onnx::paddle_recognize`), so no per-language detector session is duplicated.
`MangaOcrEngine`s (Base / 2025) and per-language `PaddleRecognizer`s live in a
capacity-bounded LRU keyed by `NativeModelId`. Turns a crop/page image into
recognized text or detected regions without going through the Python backend.
Selected via `General.ai_runtime = "native"`.

Execution provider:
The provider + adapter index are read once from the UNIFIED ONNX selection
(`General.ai_onnx_provider` ORT token + `General.ai_onnx_device_id`) — the SAME keys
the Python backend uses, so one selection drives both runtimes — and cached for the
process (the ort environment + dylib are process-global singletons committed once
and not swappable without an app restart). An impossible provider for the current OS
(e.g. DirectML on Linux) logs a notice and falls back to CPU. When `Cuda` is
selected, a system CUDA 12.x / cuDNN 9.x probe (`gpu_utils::native_cuda_runtime_available`)
runs BEFORE any load; if the runtime is missing, it falls back to DirectML (Windows,
when a DirectML accelerator exists) or CPU — never a wrong result, never a hard
failure. A genuine CUDA registration failure at load time surfaces as an OrtLoad
error and callers fall back to the Python backend with a log. The SIGILL guard scope
includes the provider id AND the adapter index, so a failed accelerator attempt
never blocks a working CPU config or a different, healthy adapter.

Engine LRU:
Capacity comes from `General.ai_max_loaded_models` (read once; clamped to at least 1;
default 3 when absent or non-numeric — e.g. the `"not-selected"` sentinel). The
shared `PaddleDetector` is always resident and does NOT count against the LRU. On
inserting a model that exceeds capacity, the least-recently-used engine is evicted
and dropped (freeing its `Session`). Engines are touched on use.

Key items:
- MangaVariant          : which MangaOCR ONNX export to run (alias of the
  `ai_models` enum so the router and this module share one type).
- NativeModelId         : identity key for an LRU-cached engine (MangaBase / Manga2025
  / PaddleRec(lang)).
- LruCache              : the pure, capacity-bounded LRU behind the engine cache.
- NativeRuntimeError    : typed failure surface (guard-disabled, dylib resolve,
  ORT load, model ensure, engine load, inference, guard write).
- recognize_manga       : native MangaOCR entry point (guard -> load -> recognize).
- recognize_paddle      : native PaddleOCR OCR entry point (shared detector + per-lang
  recognizer).
- detect_paddle         : native PaddleOCR text-detector entry point (shared detector).
- execution_provider_from_ort_token : maps a shared ORT provider token -> ExecutionProvider.
- native_load_scope_key : the effective provider:device@version SIGILL-guard scope key.
- reset_load_latch      : clears the in-process ORT load latch + cached runtime/engines.

SIGILL crash-guard:
The onnxruntime library can abort the process with an uncatchable SIGILL on CPUs
missing required instructions. Before the first dlopen we persist an fsync'd attempt
marker (`General.ort_load_state[<provider[:device]>@<ver>]`) via
`settings::mark_ort_load_attempted`; only after the FIRST successful inference do we
mark it succeeded. A crash during load OR first inference leaves `succeeded = false`,
which the next launch reads as `Suspect` and refuses to re-trigger. A GRACEFUL error
(the process survived, so it was not a SIGILL) clears the marker again so it is not
falsely treated as Suspect. `run_guarded` factors this load/guard sequence once and
shares it across the manga/paddle/detector ops.

Because multiple worker threads share this module concurrently, a graceful-error
reset must not clear the marker while ANOTHER guarded op is still mid-load/inference
(that op could still SIGILL). A process-global in-flight counter (`IN_FLIGHT`) tracks
how many guarded ops are running the unproven runtime; `reset_guard_if_unproven`
clears the marker only when the count is back to 0 and success is still unproven.

Concurrency:
Called from several worker threads (the OCR worker, the text-detector worker, and
the Cleaning-tools detector path) — never the GUI thread. It performs blocking
downloads, dlopen, and inference. The global STATE lock is held only for O(1) state
reads/writes — never across a download, ORT load, or inference (an engine is taken
out, run, and put back). All PaddleOCR detector ops (detect + recognize, which share
the single resident `PaddleDetector`) are serialized by `PADDLE_OP_LOCK` so a second
caller blocks on the first instead of finding the detector taken and needlessly
falling back to the backend or rebuilding a duplicate session. Native-only: gated off
wasm in `main.rs` because it depends on `ms-onnx`/`ort`.
*/

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use ms_onnx::{
    ExecutionProvider, MangaOcrEngine, OrtError, OrtRuntime, PaddleDetection, PaddleDetector,
    PaddleRecognizer,
};

use crate::ai_models;
use crate::config;
use crate::gpu_utils;
use crate::onnx_runtime::{
    self, OrtDownloadProgress, OrtDownloadStage, OrtRuntimeError, resolve_or_download_ort_dylib,
};
use crate::runtime_log;
use crate::tabs::settings;

/// Which MangaOCR ONNX export to run natively. Reuses the app-managed model enum
/// so the OCR router and this manager agree on the variant identity.
pub type MangaVariant = ai_models::MangaOcrOnnxModel;

/// Default engine-LRU capacity when `General.ai_max_loaded_models` is absent or
/// unusable (matches the config default and the Python backend LRU default).
const DEFAULT_MAX_LOADED_MODELS: usize = 3;

/// Minimum engine-LRU capacity: at least one engine must stay resident to run.
const MIN_MAX_LOADED_MODELS: usize = 1;

/// Errors from the native ONNX Runtime OCR path.
///
/// Each variant carries a user-facing Russian message; wrapped values add
/// diagnostic context for logs.
#[derive(Debug, thiserror::Error)]
pub enum NativeRuntimeError {
    /// A prior ORT load began but never confirmed success (likely a SIGILL), so
    /// loading is refused for this scope until the guard is reset. Callers fall
    /// back to the Python backend.
    #[error(
        "Нативный рантайм ONNX (провайдер «{provider}») отключён защитой: в прошлый раз загрузка \
         прервалась аварийно. Используется Python-бэкенд. Нажмите «Повторить попытку ORT» в настройках, \
         чтобы попробовать снова."
    )]
    GuardDisabled {
        /// Execution-provider id the guard is scoped to (e.g. `"cpu"`).
        provider: &'static str,
    },

    /// The onnxruntime dynamic library could not be resolved/downloaded/extracted.
    #[error("Не удалось подготовить библиотеку ONNX Runtime. {0}")]
    DylibResolve(#[from] OrtRuntimeError),

    /// Loading (dlopen + committing the environment) the onnxruntime library failed.
    #[error("Не удалось загрузить нативный ONNX Runtime. {0}")]
    OrtLoad(#[source] OrtError),

    /// Resolving/downloading the MangaOCR model files failed.
    #[error("Не удалось подготовить файлы модели MangaOCR. {0}")]
    ModelEnsure(String),

    /// Building the native MangaOCR engine (encoder/decoder sessions + vocab) failed.
    #[error("Не удалось создать нативный движок MangaOCR. {0}")]
    EngineLoad(#[source] OrtError),

    /// Native MangaOCR inference failed.
    #[error("Ошибка нативного распознавания MangaOCR. {0}")]
    Inference(#[source] OrtError),

    /// Building the native PaddleOCR recognizer (recognizer session + dict) failed.
    #[error("Не удалось создать нативный движок PaddleOCR. {0}")]
    PaddleEngineLoad(#[source] OrtError),

    /// Native PaddleOCR recognition failed.
    #[error("Ошибка нативного распознавания PaddleOCR. {0}")]
    PaddleInference(#[source] OrtError),

    /// Building the native PaddleOCR text detector failed.
    #[error("Не удалось создать нативный детектор текста PaddleOCR. {0}")]
    PaddleDetectorLoad(#[source] OrtError),

    /// Native PaddleOCR text detection failed.
    #[error("Ошибка нативной детекции текста PaddleOCR. {0}")]
    PaddleDetect(#[source] OrtError),

    /// Persisting the SIGILL load-guard marker failed.
    #[error("Не удалось записать состояние защиты ONNX Runtime. {0}")]
    GuardWrite(String),

    /// The cached engine/detector disappeared between load and inference. For the
    /// shared PaddleOCR detector this cannot happen under contention (paddle ops are
    /// serialized by `PADDLE_OP_LOCK`); it is surfaced instead of panicking.
    #[error("Внутренняя ошибка: нативный движок недоступен после загрузки.")]
    EngineUnavailable,
}

/// The effective native execution provider + accelerator adapter index for this
/// process, resolved once from the unified ONNX selection and cached.
///
/// `provider` never exceeds what the current OS supports (fallbacks applied);
/// `device_id` is the DirectML/CUDA adapter index (`None` = the provider's default
/// device, and always `None` for CPU/CoreML which ignore it).
#[derive(Debug, Clone, Copy)]
struct ProviderSelection {
    provider: ExecutionProvider,
    device_id: Option<i32>,
}

/// Process-global cache of the effective provider selection (the ort environment is
/// committed once and cannot be swapped without an app restart).
static SELECTED_PROVIDER: OnceLock<ProviderSelection> = OnceLock::new();

/// The effective provider selection for this process (computed once, then cached).
fn native_selection() -> ProviderSelection {
    *SELECTED_PROVIDER.get_or_init(compute_native_selection)
}

/// Maps a shared ORT execution-provider TOKEN (as stored by BOTH the Python backend
/// and the native path under `General.ai_onnx_provider`) to an [`ExecutionProvider`].
///
/// Recognizes the exact tokens the backend writes: `CPUExecutionProvider`,
/// `DmlExecutionProvider`, `CUDAExecutionProvider`, `CoreMLExecutionProvider`. Any
/// unknown, absent, or `"not-selected"` token maps to [`ExecutionProvider::Cpu`] —
/// the only universally available provider — so a stray config never picks a GPU the
/// machine cannot run.
#[must_use]
pub fn execution_provider_from_ort_token(token: &str) -> ExecutionProvider {
    match token.trim() {
        "DmlExecutionProvider" => ExecutionProvider::DirectMl,
        "CUDAExecutionProvider" => ExecutionProvider::Cuda,
        "CoreMLExecutionProvider" => ExecutionProvider::CoreMl,
        // "CPUExecutionProvider", "not-selected", and any unknown token -> CPU.
        _ => ExecutionProvider::Cpu,
    }
}

/// Computes the effective provider selection from the unified ONNX config keys
/// (`General.ai_onnx_provider` token + `General.ai_onnx_device_id`) plus the CUDA
/// probe. See [`ProviderSelection`].
///
/// A provider impossible on this OS falls back to CPU; a `Cuda` selection without a
/// system CUDA 12.x/cuDNN 9.x runtime falls back to DirectML (Windows with a DirectML
/// adapter) or CPU. The adapter index is kept only for accelerator providers
/// (DirectML/CUDA) and dropped for CPU/CoreML. Changing the selection takes effect
/// only after an app restart.
fn compute_native_selection() -> ProviderSelection {
    let cfg = config::load_raw_user_settings_for_startup().unwrap_or_else(|err| {
        runtime_log::log_warn(format!(
            "[native-runtime] could not read user config for the native provider ({err}); using CPU."
        ));
        serde_json::Value::Null
    });
    let token = config::ai_onnx_provider_token_from_user_settings(&cfg);
    let device_id = config::ai_onnx_device_id_from_user_settings(&cfg)
        .and_then(|value| value.trim().parse::<i32>().ok());
    let requested = execution_provider_from_ort_token(token.as_deref().unwrap_or("CPUExecutionProvider"));

    if !provider_supported_on_platform(requested) {
        runtime_log::log_warn(format!(
            "[native-runtime] configured native provider '{}' is not available on this OS; \
             falling back to CPU.",
            requested.id()
        ));
        return ProviderSelection {
            provider: ExecutionProvider::Cpu,
            device_id: None,
        };
    }

    let provider = match requested {
        ExecutionProvider::Cuda => resolve_effective_cuda_provider(),
        provider @ (ExecutionProvider::Cpu
        | ExecutionProvider::DirectMl
        | ExecutionProvider::CoreMl) => provider,
    };
    // The adapter index only applies to DirectML/CUDA; CPU/CoreML ignore it, so drop
    // it there to keep the guard scope clean (`cpu@ver`, not `cpu:0@ver`).
    let device_id = match provider {
        ExecutionProvider::DirectMl | ExecutionProvider::Cuda => device_id,
        ExecutionProvider::Cpu | ExecutionProvider::CoreMl => None,
    };
    runtime_log::log_info(format!(
        "[native-runtime] native execution provider = '{}' device_id={device_id:?}.",
        provider.id()
    ));
    ProviderSelection { provider, device_id }
}

/// Resolves the effective provider when the user selected CUDA.
///
/// Runs the system CUDA 12.x / cuDNN 9.x probe off the (worker) thread; if the
/// runtime is missing, falls back to DirectML on Windows (when a DirectML accelerator
/// is present) or to CPU, so OCR still works instead of hard-failing.
fn resolve_effective_cuda_provider() -> ExecutionProvider {
    let cuda_available = gpu_utils::native_cuda_runtime_available();
    let has_directml = cfg!(target_os = "windows") && gpu_utils::has_directml_accelerator_windows();
    let effective = resolve_cuda_fallback(cuda_available, has_directml, cfg!(target_os = "windows"));
    match effective {
        ExecutionProvider::Cuda => runtime_log::log_info(
            "[native-runtime] system CUDA 12.x + cuDNN 9.x detected; using the CUDA provider.",
        ),
        ExecutionProvider::DirectMl => runtime_log::log_warn(
            "[native-runtime] CUDA selected but system CUDA 12.x/cuDNN 9.x not detected; \
             falling back to DirectML.",
        ),
        ExecutionProvider::Cpu => runtime_log::log_warn(
            "[native-runtime] CUDA selected but system CUDA 12.x/cuDNN 9.x not detected; \
             falling back to CPU.",
        ),
        // `resolve_cuda_fallback` never returns CoreML; kept for exhaustiveness.
        ExecutionProvider::CoreMl => runtime_log::log_warn(
            "[native-runtime] unexpected CoreML fallback for CUDA; proceeding as-is.",
        ),
    }
    effective
}

/// Chooses the effective provider when CUDA was requested (pure, testable).
///
/// Returns [`ExecutionProvider::Cuda`] when the system CUDA runtime is available;
/// otherwise [`ExecutionProvider::DirectMl`] on Windows with a DirectML accelerator,
/// or [`ExecutionProvider::Cpu`] as the always-available last resort. The
/// environment facts are passed in so the decision can be unit-tested without touching
/// the real system.
fn resolve_cuda_fallback(
    cuda_available: bool,
    has_directml: bool,
    is_windows: bool,
) -> ExecutionProvider {
    if cuda_available {
        ExecutionProvider::Cuda
    } else if is_windows && has_directml {
        ExecutionProvider::DirectMl
    } else {
        ExecutionProvider::Cpu
    }
}

/// The SIGILL load-guard scope key for the effective native provider + adapter of
/// this process (`provider[:device]@version`).
///
/// Used by the OCR/detector routers AND the "Повторить попытку ORT" reset control to
/// scope the guard to exactly the provider/adapter this module will load, so the
/// pre-check guard read, the load-time marker, and the reset all agree. Reads the
/// cached selection (first call resolves it, which does disk I/O + a CUDA probe);
/// call off the GUI thread.
#[must_use]
pub fn native_load_scope_key() -> String {
    let selection = native_selection();
    config::ort_load_scope_key(
        selection.provider.id(),
        selection.device_id,
        onnx_runtime::ORT_VERSION,
    )
}

/// Whether `provider` can run on the OS this binary was built for.
///
/// Mirrors `ms_onnx::ExecutionProvider`'s platform gating: DirectML is Windows-only,
/// Core ML is macOS-only, CUDA is unavailable on macOS, and CPU is always available.
fn provider_supported_on_platform(provider: ExecutionProvider) -> bool {
    match provider {
        ExecutionProvider::Cpu => true,
        ExecutionProvider::DirectMl => cfg!(target_os = "windows"),
        ExecutionProvider::CoreMl => cfg!(target_os = "macos"),
        ExecutionProvider::Cuda => !cfg!(target_os = "macos"),
    }
}

/// The engine-LRU capacity for this process, read once from
/// `General.ai_max_loaded_models`.
fn native_max_loaded_models() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        let cfg = config::load_raw_user_settings_for_startup().unwrap_or(serde_json::Value::Null);
        let cap = read_max_loaded_models(&cfg);
        runtime_log::log_info(format!(
            "[native-runtime] native engine cache capacity = {cap} (General.ai_max_loaded_models)."
        ));
        cap
    })
}

/// Reads `General.ai_max_loaded_models` as a clamped engine-LRU capacity.
///
/// A positive integer is clamped to at least [`MIN_MAX_LOADED_MODELS`]; anything
/// absent, non-numeric (e.g. the `"not-selected"` sentinel), or non-positive resolves
/// to [`DEFAULT_MAX_LOADED_MODELS`].
fn read_max_loaded_models(cfg: &serde_json::Value) -> usize {
    cfg.get("General")
        .and_then(serde_json::Value::as_object)
        .and_then(|general| general.get("ai_max_loaded_models"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|&value| value >= MIN_MAX_LOADED_MODELS)
        .unwrap_or(DEFAULT_MAX_LOADED_MODELS)
}

/// Identity of an LRU-cached native OCR engine.
///
/// The two MangaOCR exports and each PaddleOCR language are distinct cache entries.
/// The shared `PaddleDetector` is NOT represented here — it is always resident and
/// tracked separately.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NativeModelId {
    /// MangaOCR Base export.
    MangaBase,
    /// MangaOCR 2025 export.
    Manga2025,
    /// PaddleOCR recognizer for the given (trimmed) language key.
    PaddleRec(String),
}

/// A loadable native engine held in the LRU (a MangaOCR engine or a PaddleOCR
/// recognizer). The shared detector lives outside the LRU.
#[derive(Debug)]
enum CachedEngine {
    /// A MangaOCR encoder+decoder engine.
    Manga(MangaOcrEngine),
    /// A PaddleOCR recognizer (used with the shared detector via `paddle_recognize`).
    PaddleRec(PaddleRecognizer),
}

/// A small, capacity-bounded LRU map: least-recently-used entries are evicted first.
///
/// Entries are ordered from least-recently-used (index 0) to most-recently-used
/// (last). `capacity` is the maximum number of resident entries and is clamped to at
/// least 1 at construction. This structure is pure (no I/O, no locks) so its
/// insertion/eviction/touch ordering can be unit-tested with a fake payload.
#[derive(Debug)]
struct LruCache<K, V> {
    /// Maximum number of resident entries (>= 1).
    capacity: usize,
    /// `(key, value)` entries, least- to most-recently-used.
    entries: Vec<(K, V)>,
}

impl<K: PartialEq, V> LruCache<K, V> {
    /// Creates an empty LRU with the given capacity (clamped to at least 1).
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: Vec::new(),
        }
    }

    /// The maximum number of resident entries.
    fn capacity(&self) -> usize {
        self.capacity
    }

    /// The index of `key`, if present.
    fn position(&self, key: &K) -> Option<usize> {
        self.entries.iter().position(|(existing, _)| existing == key)
    }

    /// Whether `key` is resident.
    fn contains(&self, key: &K) -> bool {
        self.position(key).is_some()
    }

    /// Moves `key` to most-recently-used, if present. Returns whether it existed.
    fn touch(&mut self, key: &K) -> bool {
        if let Some(idx) = self.position(key) {
            let entry = self.entries.remove(idx);
            self.entries.push(entry);
            true
        } else {
            false
        }
    }

    /// Removes and returns the value for `key`, if present.
    fn take(&mut self, key: &K) -> Option<V> {
        self.position(key).map(|idx| self.entries.remove(idx).1)
    }

    /// Inserts `value` for `key` as most-recently-used (replacing any existing value)
    /// and evicts the least-recently-used entries that exceed capacity.
    ///
    /// Returns the evicted `(key, value)` pairs in eviction order (LRU first) so the
    /// caller can log and drop them.
    fn insert(&mut self, key: K, value: V) -> Vec<(K, V)> {
        if let Some(idx) = self.position(&key) {
            self.entries.remove(idx);
        }
        self.entries.push((key, value));

        let mut evicted = Vec::new();
        while self.entries.len() > self.capacity {
            evicted.push(self.entries.remove(0));
        }
        evicted
    }

    /// Removes all entries.
    fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Process-global cached runtime, shared detector, and engine LRU.
#[derive(Debug)]
struct NativeState {
    /// The committed ORT runtime handle (kept alive so sessions stay valid).
    ort: Option<Arc<OrtRuntime>>,
    /// The single, always-resident PaddleOCR detector shared by the detector op and
    /// every PaddleOCR-language recognition. Does NOT count against the engine LRU.
    paddle_detector: Option<PaddleDetector>,
    /// LRU-bounded cache of loadable engines (MangaOCR engines + PaddleOCR
    /// recognizers), capacity from `General.ai_max_loaded_models`.
    engines: LruCache<NativeModelId, CachedEngine>,
}

impl Default for NativeState {
    fn default() -> Self {
        Self {
            ort: None,
            paddle_detector: None,
            engines: LruCache::new(native_max_loaded_models()),
        }
    }
}

static STATE: OnceLock<Mutex<NativeState>> = OnceLock::new();

/// Whether `mark_ort_load_attempted` was written this process (and not yet reset).
static ATTEMPT_MARKED: AtomicBool = AtomicBool::new(false);
/// Whether the first successful inference this process has been persisted.
static SUCCEEDED_MARKED: AtomicBool = AtomicBool::new(false);
/// Count of guarded native ops currently in flight (loading or running inference)
/// on a runtime that has not yet been proven safe. Consulted by
/// `reset_guard_if_unproven` so a graceful-failure reset of the on-disk attempt
/// marker never fires while another op could still SIGILL. See [`InFlightGuard`].
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Serializes PaddleOCR detector operations (`detect_paddle` + `recognize_paddle`),
/// which all share the single resident `PaddleDetector`.
static PADDLE_OP_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// RAII counter guard for [`IN_FLIGHT`]: increments on [`InFlightGuard::enter`] and
/// decrements on drop, so the in-flight count stays exact across every `run_guarded`
/// exit path (success, graceful error, early load failure, and panic unwind).
struct InFlightGuard;

impl InFlightGuard {
    /// Enters the in-flight region, incrementing [`IN_FLIGHT`].
    fn enter() -> Self {
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Locks [`PADDLE_OP_LOCK`], recovering from poisoning.
///
/// Held for the whole ensure+inference span of a PaddleOCR op so a second caller
/// blocks on the first rather than finding the shared detector `take()`n (which
/// would spuriously fall back to the backend) or rebuilding a duplicate detector
/// session. NEVER acquired while the global STATE mutex is held, and the STATE
/// mutex is only ever taken briefly inside, so there is no lock-order cycle.
fn lock_paddle_op() -> MutexGuard<'static, ()> {
    PADDLE_OP_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Decides whether a graceful-failure reset of the on-disk attempt marker is safe.
///
/// Returns `true` only when this process has an unconfirmed attempt
/// (`attempted && !succeeded`) AND no other guarded op is still in flight
/// (`in_flight == 0`). If another op could still be mid-load/inference it might
/// still SIGILL, so clearing the marker would wrongly let the next launch treat the
/// crashing runtime as Safe. Pure so the decision is unit-testable.
fn should_reset_unproven_guard(attempted: bool, succeeded: bool, in_flight: usize) -> bool {
    attempted && !succeeded && in_flight == 0
}

/// Locks the global state, recovering from lock poisoning (a prior panic while
/// holding the lock leaves the data usable here).
fn lock_state() -> MutexGuard<'static, NativeState> {
    STATE
        .get_or_init(|| Mutex::new(NativeState::default()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Inserts `engine` into the LRU, logging and dropping any evicted engines.
fn insert_engine(state: &mut NativeState, id: NativeModelId, engine: CachedEngine) {
    for (evicted_id, _evicted) in state.engines.insert(id, engine) {
        runtime_log::log_info(format!(
            "[native-runtime] evicted least-recently-used native engine {evicted_id:?} to honor \
             the cache capacity {} (General.ai_max_loaded_models).",
            state.engines.capacity()
        ));
        // `_evicted` drops here, freeing its ONNX Runtime session(s).
    }
}

/// The LRU identity of a MangaOCR variant.
fn manga_model_id(variant: MangaVariant) -> NativeModelId {
    match variant {
        MangaVariant::Base => NativeModelId::MangaBase,
        MangaVariant::Model2025 => NativeModelId::Manga2025,
    }
}

/// Model file paths needed to build a [`MangaOcrEngine`].
struct MangaModelPaths {
    encoder: PathBuf,
    decoder: PathBuf,
    vocab: PathBuf,
}

/// Recognizes MangaOCR text in `image` using the native ONNX Runtime path.
///
/// Runs the full SIGILL-guarded sequence: consult the on-disk load guard, resolve the
/// onnxruntime dylib, persist the fsync'd attempt marker BEFORE dlopen, load the
/// runtime + engine, run inference, and mark the runtime proven-safe after the first
/// successful inference. `progress` reports dylib/model download activity and is
/// invoked on the calling (worker) thread.
///
/// # Threading
/// Worker-thread only: performs blocking download, dlopen, and inference.
///
/// # Errors
/// - [`NativeRuntimeError::GuardDisabled`] if a prior load aborted (Suspect guard).
/// - [`NativeRuntimeError::DylibResolve`] / [`NativeRuntimeError::OrtLoad`] /
///   [`NativeRuntimeError::ModelEnsure`] / [`NativeRuntimeError::EngineLoad`] /
///   [`NativeRuntimeError::Inference`] / [`NativeRuntimeError::GuardWrite`] on the
///   corresponding stage failing.
pub fn recognize_manga(
    variant: MangaVariant,
    image: &image::RgbaImage,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<String, NativeRuntimeError> {
    run_guarded(progress, |ort, progress| {
        ensure_engine(variant, ort, progress)?;
        run_inference(variant, image)
    })
}

/// Recognizes PaddleOCR text in `image` for `paddle_lang` using the native path.
///
/// Ensures the shared detector and the per-language recognizer via `ai_models`,
/// builds/caches a [`PaddleRecognizer`] for the language (LRU-bounded), and runs the
/// full detect -> recognize pipeline (`ms_onnx::paddle_recognize`) over the ONE shared
/// detector. Shares the SIGILL guard/first-success machinery with [`recognize_manga`]
/// via [`run_guarded`].
///
/// # Threading
/// Worker-thread only: performs blocking download, dlopen, and inference.
///
/// # Errors
/// Same guard/dylib/ORT-load surface as [`recognize_manga`], plus
/// [`NativeRuntimeError::PaddleDetectorLoad`] / [`NativeRuntimeError::PaddleEngineLoad`]
/// / [`NativeRuntimeError::PaddleInference`].
pub fn recognize_paddle(
    paddle_lang: &str,
    image: &image::RgbaImage,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<Vec<String>, NativeRuntimeError> {
    let lang = paddle_lang.trim().to_string();
    run_guarded(progress, |ort, progress| {
        // Serialize all PaddleOCR ops on the single shared detector: a concurrent
        // caller blocks here instead of racing to `take()` the detector (spurious
        // backend fallback) or rebuilding a duplicate detector session.
        let _paddle_op = lock_paddle_op();
        ensure_paddle_detector(ort, progress)?;
        ensure_paddle_recognizer(&lang, ort, progress)?;
        run_paddle_inference(&lang, image)
    })
}

/// Detects text regions in `image` using the native PaddleOCR detector.
///
/// Ensures the shared detector model via `ai_models`, builds/caches the ONE shared
/// [`PaddleDetector`], and runs detection. Shares the SIGILL guard/first-success
/// machinery with the OCR paths via [`run_guarded`].
///
/// # Threading
/// Worker-thread only: performs blocking download, dlopen, and inference.
///
/// # Errors
/// Same guard/dylib/ORT-load surface as [`recognize_manga`], plus
/// [`NativeRuntimeError::PaddleDetectorLoad`] / [`NativeRuntimeError::PaddleDetect`].
pub fn detect_paddle(
    image: &image::RgbaImage,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<PaddleDetection, NativeRuntimeError> {
    run_guarded(progress, |ort, progress| {
        // Serialize with `recognize_paddle` on the shared detector (see there); a
        // concurrent caller blocks rather than finding the detector taken.
        let _paddle_op = lock_paddle_op();
        ensure_paddle_detector(ort, progress)?;
        run_paddle_detect(image)
    })
}

/// Runs `work` under the shared SIGILL-guarded native-runtime sequence.
///
/// Consults the on-disk load guard for the effective provider scope, resolves + loads
/// the process-global ORT runtime (persisting the fsync'd attempt marker BEFORE
/// dlopen), then runs `work` with the runtime and the progress callback. A graceful
/// failure at any stage clears the unconfirmed attempt marker (the process survived,
/// so it was not a SIGILL); the first full success marks the runtime proven-safe.
/// Factored here so the manga/paddle/detector ops share one guard implementation.
fn run_guarded<T>(
    progress: &mut dyn FnMut(OrtDownloadProgress),
    work: impl FnOnce(
        &Arc<OrtRuntime>,
        &mut dyn FnMut(OrtDownloadProgress),
    ) -> Result<T, NativeRuntimeError>,
) -> Result<T, NativeRuntimeError> {
    let selection = native_selection();
    let provider = selection.provider;
    let cfg_path = config::user_config_path();
    let scope = config::ort_load_scope_key(provider.id(), selection.device_id, onnx_runtime::ORT_VERSION);

    // Enter the in-flight region for the WHOLE guarded op (load + inference). The
    // SIGILL can arrive during the dlopen/load as well as the first inference, and
    // the attempt marker is written before the dlopen, so this op must be counted
    // from before the marker write through to completion. Otherwise a concurrent
    // op's graceful-error reset could clear the marker while THIS op is still mid
    // load/inference and about to SIGILL. The guard is balanced by an explicit
    // `drop` on every exit path below (so the count is 0 again before we decide on a
    // reset), and by its `Drop` on a panic unwind.
    let in_flight = InFlightGuard::enter();

    // 1. Ensure the process-global ORT runtime is loaded (SIGILL-guarded).
    let ort = match ensure_ort_runtime(provider, selection.device_id, &cfg_path, &scope, progress) {
        Ok(ort) => ort,
        Err(err) => {
            // A graceful failure means the process survived (not a SIGILL); clear the
            // attempt marker so it is not read as Suspect next launch — but only if no
            // other op is still in flight (checked inside `reset_guard_if_unproven`).
            // A GuardDisabled error is returned before any marker was written, so this
            // is a no-op there and the Suspect marker is preserved. Drop the in-flight
            // guard first so this op's own contribution is removed before the check.
            drop(in_flight);
            reset_guard_if_unproven(&cfg_path, &scope);
            return Err(err);
        }
    };

    // 2. Run the op-specific work (engine build + inference/detection).
    match work(&ort, progress) {
        Ok(value) => {
            // 3. First full success this process -> mark the runtime proven-safe.
            //    Doing this only after real compute (not after load) means a SIGILL
            //    during the first inference still leaves the guard unconfirmed for
            //    the next launch.
            mark_succeeded_once(&cfg_path, &scope);
            drop(in_flight);
            Ok(value)
        }
        Err(err) => {
            // Drop the in-flight guard BEFORE the reset check so "after this op
            // finishes, the count is 0" holds for a lone graceful failure, while a
            // concurrent in-flight op still blocks the reset.
            drop(in_flight);
            reset_guard_if_unproven(&cfg_path, &scope);
            Err(err)
        }
    }
}

/// Clears the in-process ORT load latch and drops the cached runtime/detector/engines
/// so the next OCR call re-attempts loading from scratch.
///
/// Used together with `settings::reset_ort_load_guard` by the "Повторить попытку ORT"
/// control so a retry does not require restarting the app. Dropping the cached
/// `OrtRuntime` does not un-commit the process-global ort environment; a subsequent
/// load reuses it, which is expected.
pub fn reset_load_latch() {
    ATTEMPT_MARKED.store(false, Ordering::SeqCst);
    SUCCEEDED_MARKED.store(false, Ordering::SeqCst);
    let mut state = lock_state();
    state.ort = None;
    state.paddle_detector = None;
    state.engines.clear();
    runtime_log::log_info(
        "[native-runtime] load latch reset; native ORT will be re-attempted on next OCR.",
    );
}

/// Ensures the process-global ORT runtime is loaded, returning a shared handle.
///
/// On the first load this process it consults the on-disk SIGILL guard and refuses a
/// Suspect scope. The heavy work (download, dlopen) runs with the global lock
/// released; the lock is taken only to read/store the cached handle.
fn ensure_ort_runtime(
    provider: ExecutionProvider,
    device_id: Option<i32>,
    cfg_path: &std::path::Path,
    scope: &str,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<Arc<OrtRuntime>, NativeRuntimeError> {
    // Fast path: already loaded.
    {
        let state = lock_state();
        if let Some(ort) = state.ort.as_ref() {
            return Ok(ort.clone());
        }
    }

    // First real load this process: consult the SIGILL crash-guard on disk.
    let cfg = config::load_raw_user_settings_for_startup().unwrap_or_else(|err| {
        runtime_log::log_warn(format!(
            "[native-runtime] could not read user config for the ORT guard ({err}); treating scope as Safe."
        ));
        serde_json::Value::Null
    });
    let guard = config::read_ort_load_guard(&cfg, scope);
    if config::ort_load_decision(guard) == config::OrtLoadDecision::Suspect {
        runtime_log::log_warn(format!(
            "[native-runtime] ORT load guard is Suspect for scope '{scope}'; refusing native load, using backend."
        ));
        return Err(NativeRuntimeError::GuardDisabled {
            provider: provider.id(),
        });
    }

    // Resolve/download the onnxruntime dylib (blocking; worker-thread only).
    let dylib = resolve_or_download_ort_dylib(provider, progress)?;

    // Persist the aborted-attempt marker (fsync'd) BEFORE the dlopen so a SIGILL
    // during load or first inference leaves a Suspect marker for the next launch.
    settings::mark_ort_load_attempted(cfg_path, scope).map_err(NativeRuntimeError::GuardWrite)?;
    ATTEMPT_MARKED.store(true, Ordering::SeqCst);

    runtime_log::log_info(format!(
        "[native-runtime] loading ONNX Runtime for provider '{}' device_id={device_id:?} from {}",
        provider.id(),
        dylib.display()
    ));
    let ort = OrtRuntime::load(&dylib, provider, device_id).map_err(NativeRuntimeError::OrtLoad)?;
    let ort = Arc::new(ort);

    // Store it, preferring any handle another caller committed in the meantime.
    let mut state = lock_state();
    if let Some(existing) = state.ort.as_ref() {
        return Ok(existing.clone());
    }
    state.ort = Some(ort.clone());
    Ok(ort)
}

/// Ensures the native engine for `variant` is built and cached (LRU). Model download
/// + engine build run with the global lock released.
fn ensure_engine(
    variant: MangaVariant,
    ort: &OrtRuntime,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<(), NativeRuntimeError> {
    let id = manga_model_id(variant);
    {
        let mut state = lock_state();
        if state.engines.contains(&id) {
            state.engines.touch(&id);
            return Ok(());
        }
    }

    let paths = ensure_model_paths(variant, progress)?;
    runtime_log::log_info(format!(
        "[native-runtime] building MangaOCR engine (variant={variant:?}) enc={} dec={} vocab={}",
        paths.encoder.display(),
        paths.decoder.display(),
        paths.vocab.display()
    ));
    let engine = MangaOcrEngine::load(ort, &paths.encoder, &paths.decoder, &paths.vocab)
        .map_err(NativeRuntimeError::EngineLoad)?;

    let mut state = lock_state();
    // Keep an engine another worker committed in the meantime, so a concurrent build
    // never drops a live engine on a race.
    if !state.engines.contains(&id) {
        insert_engine(&mut state, id, CachedEngine::Manga(engine));
    }
    Ok(())
}

/// Resolves the encoder/decoder/vocab paths for `variant`, downloading model files on
/// first use.
///
/// The Base export ships NO `vocab.txt`; the shared WordPiece vocabulary lives in the
/// 2025 export directory and is used for BOTH variants.
fn ensure_model_paths(
    variant: MangaVariant,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<MangaModelPaths, NativeRuntimeError> {
    let models_root = config::models_dir();

    // The `ai_models` reporter is a zero-arg `FnMut` fired once when a download
    // starts; adapt it to a coarse "Downloading" progress so the OCR UI shows
    // activity. Scoped in a block so the mutable borrow of `progress` is released
    // before the shared-vocab step reuses it.
    let model_dir = {
        let mut reported = false;
        let mut reporter = || {
            if !reported {
                reported = true;
                emit_downloading(progress);
            }
        };
        ai_models::ensure_manga_ocr_onnx_with_reporter(&models_root, variant, Some(&mut reporter))
            .map_err(NativeRuntimeError::ModelEnsure)?
    };

    let encoder = model_dir.join("encoder_model.onnx");
    let decoder = model_dir.join("decoder_model.onnx");
    let vocab = ensure_shared_vocab(&models_root, progress)?;
    Ok(MangaModelPaths {
        encoder,
        decoder,
        vocab,
    })
}

/// Resolves the shared MangaOCR `vocab.txt` (from the 2025 export), used by both
/// variants.
///
/// Short-circuits when the file already exists so the Base variant does not pull the
/// 2025 model. When absent it ensures the full 2025 export (whose file set includes
/// `vocab.txt`).
///
/// NOTE: pulling the whole 2025 model purely for its shared vocabulary is wasteful for
/// the Base variant. A targeted `ai_models::ensure_manga_ocr_vocab` helper (downloading
/// only `ONNX/MangaOCR/2025/vocab.txt`) would avoid it; see the task report. Kept
/// correct-but-heavy here rather than reaching into `ai_models` internals.
fn ensure_shared_vocab(
    models_root: &std::path::Path,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<PathBuf, NativeRuntimeError> {
    let vocab = models_root
        .join("ONNX")
        .join("MangaOCR")
        .join("2025")
        .join("vocab.txt");
    if vocab.is_file() {
        return Ok(vocab);
    }

    {
        let mut reported = false;
        let mut reporter = || {
            if !reported {
                reported = true;
                emit_downloading(progress);
            }
        };
        ai_models::ensure_manga_ocr_onnx_with_reporter(
            models_root,
            MangaVariant::Model2025,
            Some(&mut reporter),
        )
        .map_err(NativeRuntimeError::ModelEnsure)?;
    }

    if !vocab.is_file() {
        return Err(NativeRuntimeError::ModelEnsure(format!(
            "Файл словаря отсутствует после загрузки модели 2025: {}",
            vocab.display()
        )));
    }
    Ok(vocab)
}

/// Runs inference for `variant` with the global lock RELEASED during the call.
///
/// The engine is taken out of the LRU, used, then reinserted as most-recently-used.
/// If a concurrent worker takes the same MangaOCR engine first, this call surfaces
/// `EngineUnavailable` and the caller falls back to the backend rather than blocking
/// (MangaOCR engines, unlike the shared Paddle detector, are not op-serialized).
fn run_inference(
    variant: MangaVariant,
    image: &image::RgbaImage,
) -> Result<String, NativeRuntimeError> {
    let id = manga_model_id(variant);
    let mut engine = take_manga_engine(&id)?;

    // `recognize` takes `&mut self` and can be slow; run it without the lock held.
    let result = engine.recognize(image);

    // Return the engine to the cache regardless of the outcome (it stays reusable).
    {
        let mut state = lock_state();
        insert_engine(&mut state, id, CachedEngine::Manga(engine));
    }

    result.map_err(NativeRuntimeError::Inference)
}

/// Takes the MangaOCR engine for `id` out of the LRU, or fails.
fn take_manga_engine(id: &NativeModelId) -> Result<MangaOcrEngine, NativeRuntimeError> {
    let mut state = lock_state();
    match state.engines.take(id) {
        Some(CachedEngine::Manga(engine)) => Ok(engine),
        Some(other) => {
            // Wrong payload type for this id: put it back, surface an internal error.
            insert_engine(&mut state, id.clone(), other);
            Err(NativeRuntimeError::EngineUnavailable)
        }
        None => Err(NativeRuntimeError::EngineUnavailable),
    }
}

/// Ensures the PaddleOCR recognizer for `lang` is built and cached (LRU). Model
/// download + recognizer build run with the global lock released.
fn ensure_paddle_recognizer(
    lang: &str,
    ort: &OrtRuntime,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<(), NativeRuntimeError> {
    let id = NativeModelId::PaddleRec(lang.to_string());
    {
        let mut state = lock_state();
        if state.engines.contains(&id) {
            state.engines.touch(&id);
            return Ok(());
        }
    }

    let models_root = config::models_dir();
    let paths = {
        let mut reported = false;
        let mut reporter = || {
            if !reported {
                reported = true;
                emit_downloading(progress);
            }
        };
        ai_models::ensure_paddle_ocr_full_paths_with_reporter(
            &models_root,
            lang,
            Some(&mut reporter),
        )
        .map_err(NativeRuntimeError::ModelEnsure)?
    };
    runtime_log::log_info(format!(
        "[native-runtime] building PaddleOCR recognizer (lang='{lang}') rec={} dict={} \
         (shared det={})",
        paths.rec.display(),
        paths.dict.display(),
        paths.det.display()
    ));
    let recognizer = PaddleRecognizer::load(ort, &paths.rec, &paths.dict)
        .map_err(NativeRuntimeError::PaddleEngineLoad)?;

    let mut state = lock_state();
    if !state.engines.contains(&id) {
        insert_engine(&mut state, id, CachedEngine::PaddleRec(recognizer));
    }
    Ok(())
}

/// Runs PaddleOCR recognition for `lang` with the global lock RELEASED during the
/// call.
///
/// Takes BOTH the shared detector and the language recognizer out, runs the full
/// detect -> recognize pipeline (`ms_onnx::paddle_recognize`) over the ONE shared
/// detector, then returns both to the cache.
fn run_paddle_inference(
    lang: &str,
    image: &image::RgbaImage,
) -> Result<Vec<String>, NativeRuntimeError> {
    let id = NativeModelId::PaddleRec(lang.to_string());

    // Take the shared detector and the language recognizer under one lock.
    let (mut detector, mut recognizer) = {
        let mut state = lock_state();
        let detector = state.paddle_detector.take();
        let recognizer = match state.engines.take(&id) {
            Some(CachedEngine::PaddleRec(rec)) => Some(rec),
            Some(other) => {
                // Wrong payload type for this id: put it back.
                insert_engine(&mut state, id.clone(), other);
                None
            }
            None => None,
        };
        match (detector, recognizer) {
            (Some(detector), Some(recognizer)) => (detector, recognizer),
            (detector, recognizer) => {
                // Put back whatever was taken; report the missing piece.
                if let Some(detector) = detector {
                    state.paddle_detector = Some(detector);
                }
                if let Some(recognizer) = recognizer {
                    insert_engine(&mut state, id, CachedEngine::PaddleRec(recognizer));
                }
                return Err(NativeRuntimeError::EngineUnavailable);
            }
        }
    };

    // Run detect -> recognize with the lock released (both calls take `&mut self`).
    let result = ms_onnx::paddle_recognize(&mut detector, &mut recognizer, image);

    // Return the shared detector and the recognizer to the cache regardless of outcome.
    {
        let mut state = lock_state();
        state.paddle_detector.get_or_insert(detector);
        insert_engine(&mut state, id, CachedEngine::PaddleRec(recognizer));
    }

    result.map_err(NativeRuntimeError::PaddleInference)
}

/// Ensures the shared PaddleOCR text detector is built and cached. Model download +
/// detector build run with the global lock released.
fn ensure_paddle_detector(
    ort: &OrtRuntime,
    progress: &mut dyn FnMut(OrtDownloadProgress),
) -> Result<(), NativeRuntimeError> {
    {
        let state = lock_state();
        if state.paddle_detector.is_some() {
            return Ok(());
        }
    }

    let models_root = config::models_dir();
    let det_path = {
        let mut reported = false;
        let mut reporter = || {
            if !reported {
                reported = true;
                emit_downloading(progress);
            }
        };
        ai_models::ensure_paddle_ocr_detector_with_reporter(&models_root, Some(&mut reporter))
            .map_err(NativeRuntimeError::ModelEnsure)?
    };
    runtime_log::log_info(format!(
        "[native-runtime] building shared PaddleOCR text detector det={}",
        det_path.display()
    ));
    let detector =
        PaddleDetector::load(ort, &det_path).map_err(NativeRuntimeError::PaddleDetectorLoad)?;

    let mut state = lock_state();
    state.paddle_detector.get_or_insert(detector);
    Ok(())
}

/// Runs PaddleOCR text detection with the global lock RELEASED during the call. The
/// shared detector is taken out of the cache, used, then put back.
fn run_paddle_detect(image: &image::RgbaImage) -> Result<PaddleDetection, NativeRuntimeError> {
    let mut detector = {
        let mut state = lock_state();
        state.paddle_detector.take()
    }
    .ok_or(NativeRuntimeError::EngineUnavailable)?;

    // `detect` takes `&mut self` and can be slow; run it without the lock held.
    let result = detector.detect(image);

    // Return the detector to the cache regardless of the outcome (still reusable).
    {
        let mut state = lock_state();
        state.paddle_detector.get_or_insert(detector);
    }

    result.map_err(NativeRuntimeError::PaddleDetect)
}

/// Persists the succeeded marker exactly once per process, after the first successful
/// inference.
fn mark_succeeded_once(cfg_path: &std::path::Path, scope: &str) {
    if SUCCEEDED_MARKED.swap(true, Ordering::SeqCst) {
        return;
    }
    match settings::mark_ort_load_succeeded(cfg_path, scope) {
        Ok(()) => runtime_log::log_info(format!(
            "[native-runtime] ORT runtime proven safe; marked succeeded for scope '{scope}'."
        )),
        // Leave SUCCEEDED_MARKED = true so we do not retry the write every recognize.
        Err(err) => runtime_log::log_warn(format!(
            "[native-runtime] failed to persist ORT succeeded marker for '{scope}': {err}"
        )),
    }
}

/// Clears the on-disk attempt marker if a load was attempted but never confirmed safe
/// (the process survived a graceful error, so it was not a SIGILL) AND no other
/// guarded op is still in flight.
///
/// The in-flight check is essential with multiple worker threads: if another op is
/// still mid load/first-inference (unproven), it could still SIGILL, so the marker
/// must stay. That op will run its own reset (or mark succeeded) when it finishes.
/// Callers drop their own [`InFlightGuard`] before invoking this, so `IN_FLIGHT == 0`
/// means genuinely no other op is running the unproven runtime.
fn reset_guard_if_unproven(cfg_path: &std::path::Path, scope: &str) {
    let attempted = ATTEMPT_MARKED.load(Ordering::SeqCst);
    let succeeded = SUCCEEDED_MARKED.load(Ordering::SeqCst);
    let in_flight = IN_FLIGHT.load(Ordering::SeqCst);
    if !should_reset_unproven_guard(attempted, succeeded, in_flight) {
        return;
    }
    match settings::reset_ort_load_guard(cfg_path, scope) {
        Ok(()) => {
            ATTEMPT_MARKED.store(false, Ordering::SeqCst);
            runtime_log::log_info(format!(
                "[native-runtime] cleared ORT attempt marker for '{scope}' after a graceful native \
                 failure (process survived, not a SIGILL)."
            ));
        }
        Err(err) => runtime_log::log_warn(format!(
            "[native-runtime] failed to clear ORT attempt marker for '{scope}': {err}"
        )),
    }
}

/// Emits a coarse "Downloading" progress snapshot (no byte counters known).
fn emit_downloading(progress: &mut dyn FnMut(OrtDownloadProgress)) {
    progress(OrtDownloadProgress {
        downloaded: 0,
        total: None,
        stage: OrtDownloadStage::Downloading,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn read_max_loaded_models_defaults_and_clamps() {
        // Absent key -> default.
        assert_eq!(read_max_loaded_models(&json!({})), DEFAULT_MAX_LOADED_MODELS);
        assert_eq!(
            read_max_loaded_models(&json!({"General": {}})),
            DEFAULT_MAX_LOADED_MODELS
        );
        // The "not-selected" sentinel (non-numeric) -> default.
        assert_eq!(
            read_max_loaded_models(&json!({"General": {"ai_max_loaded_models": "not-selected"}})),
            DEFAULT_MAX_LOADED_MODELS
        );
        // Zero is below the floor -> default.
        assert_eq!(
            read_max_loaded_models(&json!({"General": {"ai_max_loaded_models": 0}})),
            DEFAULT_MAX_LOADED_MODELS
        );
        // A positive integer is honored.
        assert_eq!(
            read_max_loaded_models(&json!({"General": {"ai_max_loaded_models": 5}})),
            5
        );
        assert_eq!(
            read_max_loaded_models(&json!({"General": {"ai_max_loaded_models": 1}})),
            1
        );
    }

    #[test]
    fn ort_token_maps_to_execution_provider() {
        // The exact tokens the Python backend + native path share.
        assert_eq!(
            execution_provider_from_ort_token("CPUExecutionProvider"),
            ExecutionProvider::Cpu
        );
        assert_eq!(
            execution_provider_from_ort_token("DmlExecutionProvider"),
            ExecutionProvider::DirectMl
        );
        assert_eq!(
            execution_provider_from_ort_token("CUDAExecutionProvider"),
            ExecutionProvider::Cuda
        );
        assert_eq!(
            execution_provider_from_ort_token("CoreMLExecutionProvider"),
            ExecutionProvider::CoreMl
        );
        // Whitespace is trimmed.
        assert_eq!(
            execution_provider_from_ort_token(" DmlExecutionProvider "),
            ExecutionProvider::DirectMl
        );
        // Unknown / sentinel / empty -> the always-available CPU provider, never a
        // GPU the machine may be unable to run.
        assert_eq!(
            execution_provider_from_ort_token("not-selected"),
            ExecutionProvider::Cpu
        );
        assert_eq!(
            execution_provider_from_ort_token("MIGraphXExecutionProvider"),
            ExecutionProvider::Cpu
        );
        assert_eq!(
            execution_provider_from_ort_token(""),
            ExecutionProvider::Cpu
        );
    }

    #[test]
    fn resolve_cuda_fallback_prefers_cuda_then_directml_then_cpu() {
        // CUDA available -> CUDA regardless of platform/DirectML.
        assert_eq!(
            resolve_cuda_fallback(true, false, false),
            ExecutionProvider::Cuda
        );
        assert_eq!(
            resolve_cuda_fallback(true, true, true),
            ExecutionProvider::Cuda
        );
        // CUDA missing on Windows with a DirectML accelerator -> DirectML.
        assert_eq!(
            resolve_cuda_fallback(false, true, true),
            ExecutionProvider::DirectMl
        );
        // CUDA missing on Windows without a DirectML accelerator -> CPU.
        assert_eq!(
            resolve_cuda_fallback(false, false, true),
            ExecutionProvider::Cpu
        );
        // CUDA missing on non-Windows (e.g. Linux) -> CPU (DirectML is Windows-only).
        assert_eq!(
            resolve_cuda_fallback(false, true, false),
            ExecutionProvider::Cpu
        );
        assert_eq!(
            resolve_cuda_fallback(false, false, false),
            ExecutionProvider::Cpu
        );
    }

    #[test]
    fn lru_inserts_evicts_and_touches_in_order() {
        let mut lru: LruCache<u32, &'static str> = LruCache::new(2);
        assert_eq!(lru.capacity(), 2);

        // Fill to capacity: no eviction.
        assert!(lru.insert(1, "a").is_empty());
        assert!(lru.insert(2, "b").is_empty());
        assert!(lru.contains(&1) && lru.contains(&2));

        // Inserting a third entry evicts the least-recently-used (key 1).
        let evicted = lru.insert(3, "c");
        assert_eq!(evicted, vec![(1, "a")]);
        assert!(!lru.contains(&1));
        assert!(lru.contains(&2) && lru.contains(&3));

        // Touching key 2 makes it most-recently-used, so the next insert evicts key 3.
        assert!(lru.touch(&2));
        let evicted = lru.insert(4, "d");
        assert_eq!(evicted, vec![(3, "c")]);
        assert!(lru.contains(&2) && lru.contains(&4));
    }

    #[test]
    fn lru_take_removes_and_reinsert_is_mru() {
        let mut lru: LruCache<u32, u32> = LruCache::new(2);
        lru.insert(1, 10);
        lru.insert(2, 20);

        // Taking key 1 removes it (dropping length below capacity).
        assert_eq!(lru.take(&1), Some(10));
        assert!(!lru.contains(&1));
        assert_eq!(lru.take(&1), None);

        // Reinserting it puts it back as MRU; capacity is respected, no eviction.
        assert!(lru.insert(1, 11).is_empty());
        // Now key 2 is LRU: inserting a new key evicts key 2, not the just-used key 1.
        let evicted = lru.insert(3, 30);
        assert_eq!(evicted, vec![(2, 20)]);
        assert!(lru.contains(&1) && lru.contains(&3));
    }

    #[test]
    fn lru_insert_replaces_existing_value_without_eviction() {
        let mut lru: LruCache<u32, u32> = LruCache::new(2);
        lru.insert(1, 10);
        lru.insert(2, 20);
        // Replacing an existing key updates its value and refreshes recency.
        assert!(lru.insert(1, 100).is_empty());
        assert_eq!(lru.take(&1), Some(100));
    }

    #[test]
    fn lru_capacity_is_clamped_to_one() {
        let lru: LruCache<u32, u32> = LruCache::new(0);
        assert_eq!(lru.capacity(), 1);
    }

    #[test]
    fn reset_decision_only_when_unproven_and_idle() {
        // Lone graceful error: attempted, not yet succeeded, nothing else in flight
        // -> the marker is safe to clear.
        assert!(should_reset_unproven_guard(true, false, 0));

        // A concurrent guarded op is still in flight -> do NOT reset: that op could
        // still SIGILL, and clearing the marker would lose the crash protection.
        assert!(!should_reset_unproven_guard(true, false, 1));
        assert!(!should_reset_unproven_guard(true, false, 7));

        // Already proven safe this process -> never reset (a real SIGILL cannot
        // happen anymore, and the marker records the proven-safe state).
        assert!(!should_reset_unproven_guard(true, true, 0));
        assert!(!should_reset_unproven_guard(true, true, 3));

        // No attempt marker was ever written (e.g. Suspect refusal) -> nothing to
        // reset; leave any on-disk Suspect marker in place.
        assert!(!should_reset_unproven_guard(false, false, 0));
        assert!(!should_reset_unproven_guard(false, true, 0));
    }

    #[test]
    fn in_flight_guard_increments_and_decrements() {
        // The counter is process-global; capture the baseline so the test is robust
        // to any concurrently running guarded op in the same test binary.
        let base = IN_FLIGHT.load(Ordering::SeqCst);
        {
            let _g1 = InFlightGuard::enter();
            assert_eq!(IN_FLIGHT.load(Ordering::SeqCst), base + 1);
            {
                let _g2 = InFlightGuard::enter();
                assert_eq!(IN_FLIGHT.load(Ordering::SeqCst), base + 2);
            }
            // Inner guard dropped -> back to one above the baseline.
            assert_eq!(IN_FLIGHT.load(Ordering::SeqCst), base + 1);
        }
        // Both dropped -> back to the baseline.
        assert_eq!(IN_FLIGHT.load(Ordering::SeqCst), base);
    }
}
