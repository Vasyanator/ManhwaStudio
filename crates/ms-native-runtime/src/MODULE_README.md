# Module: crates/ms-native-runtime/src (crate `ms-native-runtime`)

## Purpose
The process-global lazy manager for the IN-PROCESS native ONNX Runtime OCR path
(`General.ai_runtime = "native"`): it owns the single `ms_onnx::OrtRuntime`, the one
always-resident `PaddleDetector`, and the LRU-bounded cache of `MangaOcrEngine` /
`PaddleRecognizer` engines, and turns a crop or page image into recognized text or
detected regions without going through the Python backend.

The binary re-exports it as `crate::native_runtime`, so every existing
`crate::native_runtime::…` path keeps working.

## Architecture
One file, `lib.rs`, whose own header carries the full contract (execution-provider
resolution, the availability fallback ladder, the SIGILL guard scope, hot-swap vs
restart, the engine LRU). Everything is behind `OnceLock`/`Mutex` process globals and is
resolved once per process, because the ort environment and the loaded dylib are themselves
process-global and not swappable without an app restart.

Layer: ABOVE `ms-onnx` (sessions), `ms-text-detect` (`ProbMap`), `ms-onnx-runtime` (the dylib resolver/downloader),
`ms-sysprobe` (`ai_models`, `gpu_utils`) and `ms-config`; BELOW the translation tab's OCR /
detector routers and the AI backend panel, which call into it. Native-only: `ms-onnx`/`ort`
load a native shared library, so the binary gates the re-export off wasm.

Selection: ONE unified ONNX selection — `General.ai_onnx_build` (build slug from
`ms_onnx_runtime::builds`), `ai_onnx_provider` (ORT token), `ai_onnx_device_id` — is shared
with the Python backend and resolved once per process. An unavailable accelerator falls back
to the `cpu` build with a logged notice, never a wrong result (where a `cpu` archive ships, see
below); a load-time EP failure is an
error the callers answer by falling back to the backend.
`evaluate_native_selection(cfg)` is the one owner of that resolution: uncached, silent, and
returning a `NativeSelectionReport` (requested and effective triple, fallback reason, guard
scope key of the effective triple). The process cache (`compute_native_selection`, behind
`native_selection()`) calls it once and adds the log lines; an inspection-only caller (the
launcher's settings checks) calls it directly and reads the cache only through
`committed_load_scope_key()`, which never resolves it, so inspecting the config never pins the
process to it.

Availability rule owner: `native_fallback_reason(build, ep, &NativeHardwareFacts)` (pure, uncached;
`NativeFallbackReason` says why) answers "can this build/EP run here" for the runtime's
selection and for the AI backend panel's native build picker. It also rejects a build with no
archive for this OS/arch, asking the manifest owner (`ms_onnx_runtime::build_shipped_here`),
never a platform table of its own. The cached selection calls it over the facts of only the
probes that (build, EP) needs; the panel's build picker calls `native_build_fallback_reason`
(headline EP) over `NativeHardwareFacts::probe_all()` taken on its own probe worker. A new
build-availability decision for the native runtime belongs here, not in a caller.
Known residual outside this rule: the panel's Backend-runtime provider list
(`ms-settings-ui` `build_onnx_provider_options`) keeps a per-provider `native_available` flag
from its own caps (DirectML = an adapter was detected, CUDA = the CUDA 12 runtime probe,
WebGPU = the same WebGPU fact). Its only live consumer is `default_onnx_provider`, the Backend
mode's default-provider seed; the `Native` arm of `provider_runtime_state` that also reads it
is reachable only from tests, because the Native runtime draws the build picker instead.
Routing it through this rule would change the Backend default on Windows without a detected
adapter and cannot serve its tests, which simulate foreign OSes through flags.

No CPU archive: on a target whose manifest ships no `cpu` build (macOS x86_64, aarch64
Linux/Windows), the CPU fallback cannot load either. The selection then logs one warning and
keeps `cpu` (no "falling back" line for a cpu -> cpu no-op); the load fails with
`NoManifestEntry` and callers route to the Python backend, by design.

Background pipeline: this crate owns no thread. Native load and inference run on the
callers' workers — the OCR and text-detector workers in `ms-tab-translation` (`ocr.rs`,
`text_detector/`, the latter also reached from Cleaning mask generation) — while the AI
backend panel in `ms-settings-ui` reads status and resets the load latch.

Entry points: `recognize_manga`, `recognize_paddle` (OCR), `detect_paddle` (whole-page
detection: single 960-px pass + postprocess) and `paddle_det_forward` (forward-only pass on
tiles prepared by `ms_text_detect`'s plan, returning `ms_text_detect::ProbMap`s; the
translation text-detector pipeline wraps it as its native Paddle runner).
`paddle_det_max_batch` is that runner's batch size: 4 tiles on the CPU provider, 1 on any
accelerator provider (a 4 x 960^2 batch can exhaust a small GPU, and an OOM would silently
send every page to the backend).

## Contracts and invariants
- Every op on the ONE shared `PaddleDetector` (`recognize_paddle`, `detect_paddle`,
  `paddle_det_forward`) runs inside `run_guarded`, holds `lock_paddle_op` for its whole
  ensure + inference span, and uses the detector through `with_shared_detector`. A new
  detector op must follow the same sequence.
- The SIGILL-guard scope key is `{build}:{provider}[:{device}]@{version}`
  (`native_load_scope_key`), so a crashed scope never blocks a different
  build/provider/adapter.
- `native_load_scope_key` resolves and caches the selection; code that must not pin it
  (settings inspection, workers that only report, the panel's guard reset) uses
  `evaluate_native_selection` and `committed_load_scope_key` / `next_load_scope_key` instead.
  `next_load_scope_key` (committed scope, else the configured effective one) is the one rule
  for "which guard does the next load read": the settings guard warning and the reset button
  both use it, so they always address the same scope.
- `device_selection_for` is the one parse of `ai_onnx_device_id`; a caller judging a
  persisted id (settings checks, the panel's device reconcile) parses through it.
- Every dylib load is bracketed by the crash guard in `ms_config::ort_load_guard`
  (`mark_ort_load_attempted` before, `mark_ort_load_succeeded` after the first successful
  inference, `reset_ort_load_guard` on a graceful failure). That guard is what makes an
  uncatchable SIGILL survivable across launches; it may not be skipped or reordered.
- `ORT_DYLIB_COMMITTED` is set after the first successful load and NEVER cleared:
  `reset_load_latch` enables a SAME-build retry only. A different build needs a restart.
- Blocking work only. Nothing here may be called from the GUI thread; the callers own the
  worker.
- This crate must never name `tabs`, `app` or `launcher`.

## Editing map
- To change provider/device resolution or the fallback ladder, see `native_fallback_reason`
  (and its pure core `decide_selection`) in `lib.rs`; a new hardware fact goes into
  `NativeHardwareFacts` (both `probe_all` and the lazy `probe_for`). The settings panel picks
  the change up without edits.
- To change the engine cache policy, see the LRU section of `lib.rs`
  (`General.ai_max_loaded_models`).
- To change WHERE the crash-guard markers are written, see
  `crates/ms-config/src/ort_load_guard.rs`, not this crate.
