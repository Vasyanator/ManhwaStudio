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
to the `cpu` build with a logged notice, never a wrong result; a load-time EP failure is an
error the callers answer by falling back to the backend.

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
- To change provider/device resolution or the fallback ladder, see `decide_selection` and
  its neighbours in `lib.rs`.
- To change the engine cache policy, see the LRU section of `lib.rs`
  (`General.ai_max_loaded_models`).
- To change WHERE the crash-guard markers are written, see
  `crates/ms-config/src/ort_load_guard.rs`, not this crate.
