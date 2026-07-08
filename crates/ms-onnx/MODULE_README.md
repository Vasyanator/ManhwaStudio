# Module: crates/ms-onnx

## Purpose
Native ONNX Runtime inference infrastructure for ManhwaStudio. It is a pure,
GUI-free layer over the `ort` bindings (v2.0.0-rc.12) in **load-dynamic** mode. It
loads the onnxruntime shared library from a caller-supplied path, initializes the
ort environment, and hosts the native inference engines: **MangaOCR**
(`src/manga_ocr/`, exported as `MangaOcrEngine`) and **PaddleOCR** detection +
recognition (`src/paddle_ocr/`, exported as `PaddleOcrEngine` / `PaddleDetector` /
`PaddleRecognizer`). The crate never downloads anything and never reads app config:
callers pass the dylib path, the execution provider, and model/vocabulary/dict paths.

## Architecture
The crate root (`src/lib.rs`) owns the runtime handle and error surface; the
inference engines live in `src/manga_ocr/` and `src/paddle_ocr/`. The root wraps
these `ort` entry points:

- `ort::init_from(path)` — dlopens the onnxruntime library from the given path
  (load-dynamic; nothing is linked or downloaded at build time) and verifies its
  ABI/version. This is the real loader code path.
- `EnvironmentBuilder::commit()` — registers the process-global environment
  options (name/providers). The ort environment is a singleton owned by `ort`.
- `ort::environment::current()` — creates the actual `OrtEnv` (onnxruntime
  `CreateEnv` + default allocator). Called from `warmup`, it executes real
  onnxruntime code.
- `OrtRuntime::build_session(model_path)` — builds a session with all graph
  optimizations and applies the committed execution provider: CPU registers
  nothing (byte-identical to the historical CPU-only builder);
  DirectML/CoreML/CUDA/WebGPU/OpenVINO/TensorRT register their EP with
  `error_on_failure()` (no silent CPU fallback at EP-registration time). Device
  selection is carried by `NativeDeviceSelection`: index-based EPs
  (DirectML/CUDA/TensorRT/WebGPU) read `Index(id)` and pass it via
  `with_device_id(id)`, falling back to the EP `default()` (adapter 0) otherwise;
  OpenVINO reads `OpenVinoDeviceType(s)` (a device-TYPE string like
  `"CPU"`/`"GPU"`/`"GPU.0"`/`"NPU"`/`"HETERO:..."`) and passes it via
  `with_device_type(s)`, otherwise letting OpenVINO pick its own default device.
  CoreML/CPU ignore the selection. TensorRT is registered alone: onnxruntime still
  falls back per-node to CPU for ops TensorRT cannot run (not an EP failure).
  WebGPU/OpenVINO/TensorRT are distinct backends (different GPU/accelerator
  kernels); their numeric output is NOT asserted byte-equal to the CPU reference —
  each is treated as a separate backend, and only CPU stays byte-identical to the
  historical builder.

Load/warmup are split so the app can dlopen early (`load`) and later force real
environment initialization on a controlled thread (`warmup`), where an
unsupported-CPU SIGILL would surface.

## Files and submodules
- `src/lib.rs`: the crate root public surface — `ExecutionProvider` (`id` /
  `is_available_on_current_platform`), `NativeDeviceSelection`
  (`Default`/`Index`/`OpenVinoDeviceType` + `index()`), `OrtError`, `OrtRuntime`
  (`load` / `warmup` / `provider` / `device` / `device_id` / `build_session`) — plus
  re-exports of `MangaOcrEngine`, the PaddleOCR types, and the free
  `paddle_recognize` pipeline function. Edit here for runtime/error-surface changes
  and execution-provider registration.
- `src/manga_ocr/`: the native MangaOCR engine (`MangaOcrEngine`) — preprocess,
  tokenizer, beam search, and post-process. See its `MODULE_README.md`. This is a
  faithful port of `modules/ai_backend/manga_ocr_service.py`.
- `src/paddle_ocr/`: the native PaddleOCR detection + recognition engines — DB
  post-process, perspective crop, CTC decode, character table, and glyph mask. See
  its `MODULE_README.md`. Faithful port of `modules/ai_backend/paddle_onnx_runtime.py`
  and `paddle_text_detector_service.py`.
- `tests/manga_ocr_e2e.rs`: `#[ignore]` end-to-end parity test (needs real models +
  an onnxruntime dylib).

## Contracts and invariants
- **Pure inference crate.** No egui/eframe, no application config, no path
  resolution, no download logic. The caller supplies the onnxruntime dylib path
  and the `ExecutionProvider`. The crate never downloads anything and never reads
  app config.
- **Cargo features are fixed:** `ort` with `default-features = false` +
  `load-dynamic` + `api-18`. `api-18` is the lowest level rc.12 compiles at (its
  built-in EP modules unconditionally reference `OrtApi` fields gated at `api-18`),
  and it sets the floor for the runtime dylib: **ONNX Runtime >= 1.18.x**. Do NOT
  add `download-binaries`/`copy-dylibs` (they fetch or copy a binary at build time)
  or a linking feature — that would break the runtime-resolved-dylib contract. Do
  NOT add ndarray/tokio/config deps. **Do NOT add the per-EP cargo features**
  (`directml`/`coreml`/`cuda`/`webgpu`/`openvino`/`tensorrt`): `ort::ep::*`
  registration is gated on `any(feature = "load-dynamic", feature = "<ep>")` (WebGPU
  also on `target_arch = "wasm32"`), so load-dynamic already satisfies it; adding an
  `<ep>` feature pulls `ort-sys/<ep>`, which conflicts with load-dynamic's
  `disable-linking`. The `ExecutionProvider` types compile on all targets; platform
  gating happens in `OrtRuntime::load`, not `build_session`.
- **Extra dep:** `imageproc` 0.25-compatible (0.27) — pure Rust, used only by the
  PaddleOCR path (contours, min-area-rect, perspective warp). No OpenCV/Clipper.
- **No panics on bad input.** `load` maps a missing file to
  `OrtError::LibraryNotFound`, a load/ABI failure to `OrtError::LoadFailed`, and a
  provider unavailable on the current `cfg(target_os)` to
  `OrtError::UnsupportedProvider`. `warmup` maps env-creation failure to
  `OrtError::WarmupFailed`. No `.unwrap()`/`.expect()` in these paths.
- **`ExecutionProvider::id`** returns a stable lowercase id
  (`cpu`/`directml`/`coreml`/`cuda`/`webgpu`/`openvino`/`tensorrt`) used by higher
  layers for per-provider config scoping. Keep these values stable.
- **Provider platform gating:** DirectML = Windows only, Core ML = macOS only,
  CUDA = not macOS, WebGPU = Windows/Linux/macOS (Dawn), OpenVINO = x86_64
  Windows/Linux (Intel device + OpenVINO runtime: self-contained on the Linux
  wheel, system SDK on Windows), TensorRT = Windows/Linux (mirrors CUDA; bundled in
  the CUDA "gpu" build, needs the NVIDIA/CUDA stack), CPU = everywhere.
  `ExecutionProvider::is_available_on_current_platform` exposes this predicate
  publicly so higher layers query availability without duplicating the
  `cfg(target_os)` logic; `OrtRuntime::load` uses it to gate. Note: rc.12's own
  `ort::ep::WebGPU::supported_by_platform` excludes macOS, but that only controls
  ort's log verbosity — registration still works on macOS with a WebGPU-capable
  dylib, so this crate reports macOS as available.
- **Global-singleton caveat:** ort's environment options are process-global.
  Only the first `load` commits its options; later loads reuse them (logged). A
  second `load` with a *different* dylib does not replace the first environment.
- **Logging:** load/warmup start/finish are emitted via `ms-log`'s `trace_log!`
  under `cat::STARTUP`. No secrets or large payloads are logged.

## Editing map
- To add a new execution provider, extend `ExecutionProvider` (add an `id` arm, a
  platform-gating arm, and a `build_session` registration arm — all three matches
  are exhaustive, no `_`).
- To change how sessions apply the provider, edit `OrtRuntime::build_session`.
- The accelerator selection is threaded through `OrtRuntime` as `device`
  (`NativeDeviceSelection`, set at `load`, read back via `device()`; `device_id()`
  is a convenience projecting only `Index(id)` to `Some(id)`). To change which
  providers honor it or how it is passed to the EP, edit the
  DirectML/CUDA/TensorRT/WebGPU (numeric `Index`) or OpenVINO (`OpenVinoDeviceType`
  string) arms of `build_session`.
- To change MangaOCR inference (preprocess/generation/decode/post-process), edit
  `src/manga_ocr/` (see its `MODULE_README.md`).
- To change PaddleOCR detection/recognition, edit `src/paddle_ocr/` (see its
  `MODULE_README.md`).
- To change error surfacing, edit `OrtError` (session/inference/tensor-shape/vocab/
  image-preprocess/paddle-dict variants live alongside the load/warmup variants).
