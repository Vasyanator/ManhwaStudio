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

Layer: ABOVE `ms-onnx` (sessions), `ms-onnx-runtime` (the dylib resolver/downloader),
`ms-sysprobe` (`ai_models`, `gpu_utils`) and `ms-config`; BELOW the translation tab's OCR /
detector routers and the AI backend panel, which call into it. Native-only: `ms-onnx`/`ort`
load a native shared library, so the binary gates the re-export off wasm.

## Contracts and invariants
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
