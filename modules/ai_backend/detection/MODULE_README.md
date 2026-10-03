# Module: modules/ai_backend/detection

## Purpose
Forward-only text-detection services of the Python AI backend. Each service runs ONE detector
network on a batch of equal-size RGB tiles that Rust already resized, padded and cut, and returns
the network's probability maps quantized to `uint8`. Everything around the network — the scale and
tiling plan, stitching, DB boxes, mask refinement, CRAFT components, dilation, block finalization —
lives in Rust (`crates/ms-text-detect`).

## Architecture
Three independent service classes, one per engine, plus the vendored ComicTextDetector network:

- `ctd.py` -> `CtdTextDetectorService` — Torch, weights from `ManhwaStudio_AI_Models/Torch`
  (`config.TEXT_DETECTOR_DIR` / `comictextdetector.pt`). Runs `TextDetBase` from
  `textdetector/ctd/basemodel.py`; returns `[seg, shrink]` at the tile resolution.
- `paddle.py` -> `PaddleTextDetectorService` — ONNX Runtime, weights from
  `ManhwaStudio_AI_Models/ONNX/PaddleOCR`. The session, the ImageNet normalization and the
  provider rule come from `../engines/paddle_onnx.py` (`PaddleOnnxRuntime.forward_det`), shared
  with the Paddle OCR service; returns `[prob]` at the tile resolution.
- `surya.py` -> `SuryaTextDetectorService` — Torch, checkpoints owned by the `surya` library's own
  cache; presence and download go through `../engines/surya_checkpoints.py`, shared with
  `../ocr/surya.py`. Normalizes with the library's own processor (no resize) and returns the text
  heatmap (model channel 0) at a quarter of the tile resolution, then releases Torch's CUDA cache
  (the upstream `batch_detection` did, and ONNX Runtime GPU sessions share the device).
- `forward_maps.py` — numpy-only helpers all three share: `validate_tiles` (dtype, shape,
  alignment) and `quantize_probability_maps` (`floor(clip(p, 0, 1) * 255 + 0.5)` in float32, the
  same form as the Rust side; non-finite -> error).

Nothing in this package imports another service in it. `__init__.py` is deliberately a bare
docstring: each engine drags in a different heavy stack, so importing one detector must never import
the others' dependencies.

Services are constructed once in `server.py` and reach the IPC layer only as fields on `AppState`
(`text_detector_ctd`, `text_detector_paddle`, `text_detector_surya`). The handler
`ipc/handlers/textdetector.py` backs `textdetector.{ctd,paddle,surya}.forward`, owns the wire
contract and its validation, and touches services exclusively through `HandlerContext.state` — it
never imports this package. Its only service call is `forward_tiles(tiles)`.

`ctd.py` and `surya.py` take a `LoadedModelManager` (`../runtime/model_manager.py`) and hold a lease
around every forward pass so the process-wide resident-model budget can unload them. `paddle.py` has
no lease of its own: ONNX sessions are leased inside `PaddleOnnxRuntime`.

## Files and submodules
- `ctd.py`: CTD service. Keys the loaded network by `ctd:<device>` so a device change reloads it;
  input `/255`, RGB, NCHW float32; output `[seg, lines[:, 0]]` (the YOLO block head and the DB
  threshold map are discarded). Imports `TextDetBase` lazily at first use.
- `paddle.py`: Paddle detection service; validates the batch and quantizes `forward_det`'s output.
- `surya.py`: Surya detector service; drives `predictor.processor` and `predictor.model` directly
  (not `batch_detection`, which resizes to 1200² and upsamples the map).
- `forward_maps.py`: tile validation and map quantization; `test_forward_maps.py` covers it.
- `test_services.py`: per-engine input/output contract of the three services without weights —
  fake nets and predictors injected at `CtdTextDetectorService._forward_locked`,
  `surya._forward_with_predictor` and the Paddle service's runtime factory (Torch tests skip when
  Torch is absent).
- `textdetector/`: vendored ComicTextDetector network (`ctd/basemodel.py` + `yolov5/`). See
  `textdetector/MODULE_README.md`.

## Contracts and invariants
- `forward_tiles(tiles)` takes `uint8 [n, h, w, 3]` RGB (both sides a multiple of the engine
  align: CTD 64, Paddle 32, Surya 4) and returns a C-contiguous `uint8 [n, C, mh, mw]`. The
  handler re-checks the shape and dtype; the channel order is the wire's (`PROTOCOL.md` §5.3).
- Non-finite network output is an error (`RuntimeError`), never quantized to zeros: a broken
  forward pass must not look like a page without text.
- Torch and ONNX model roots stay separate: CTD reads `Torch/`, Paddle reads `ONNX/`. Never write
  one engine's weights under the other's root.
- Missing weights, a missing package, or a failed load surface as an explicit error; no service may
  fall back to another engine or return an empty result to hide a failure.
- Every Torch service resolves its device from `General.ai_device`, treating `not-selected` as "no
  choice yet" and falling back through `AIDevice.detect_available_devices()`. CPU is a fallback,
  never a silent preference.
- MIGraphX constraint: when the selected ONNX provider is `MIGraphXExecutionProvider`, the detection
  session is forced onto `CPUExecutionProvider` (`PaddleOnnxRuntime._det_provider_settings` in
  `../engines/paddle_onnx.py`); only recognition runs on MIGraphX.
- `surya.py` needs no ROCm mmap staging. It requests float32 for a float16 checkpoint, so Torch
  casts on the host into fresh anonymous memory and the host->device copy never reads the
  safetensors file mapping (`_preferred_detector_dtype` documents the measurement). Switching it
  back to float16 would reintroduce the multi-second-per-tensor amdkfd stall (and fp16 NaN
  heatmaps on some CUDA setups) and would then require
  `runtime.rocm_mmap_transfer.patched_module_to()`.
- `__init__.py` must stay import-free. Adding a re-export there makes every detector's dependencies
  load whenever any one of them is imported.
- Test coverage: what each service feeds its network and which output channel it keeps
  (`test_services.py`), validation and quantization (`test_forward_maps.py`), and the wire contract
  (`../ipc/handlers/test_textdetector.py`). Not covered without checkpoints: model loading, device
  selection and the real networks' numerics; verify those with a manual detection run from the
  app.

## Editing map
- To change what a detector returns to Rust (channels, map size): the service's `forward_tiles`,
  `FORWARD_SPECS` in `../ipc/handlers/textdetector.py`, `ms_backend_ipc::textdetector`,
  `../ipc/PROTOCOL.md` §5.3, and bump `PROTOCOL_VERSION` on both sides.
- To change detection behavior (scale, tiling, boxes, mask): `crates/ms-text-detect`, not here.
- To change the CTD network code: `textdetector/ctd/basemodel.py` (vendored; keep edits minimal).
- To change Paddle model-path, provider resolution or the input normalization, edit
  `../engines/paddle_onnx.py` — it is shared with OCR, so a change there affects both.
- To change Surya checkpoint location or download, edit `../engines/surya_checkpoints.py`.
- To change device resolution, edit `_resolve_selected_backend_device` in the affected service.
- To add a fourth detector: add a module here with `forward_tiles`, wire it into `server.py`'s
  `AppState`, add the method constant, spec row and handler in `ipc/`, mirror it in
  `ms_backend_ipc::textdetector`, and leave `__init__.py` alone.
