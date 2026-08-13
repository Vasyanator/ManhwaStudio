# Module: modules/ai_backend/detection

## Purpose
Text-detection services of the Python AI backend. Each service turns one page image into the same
payload shape — a source size, axis-aligned `blocks`, and a binary `mask_png` — regardless of which
detection engine produced it. The mask is what the Rust side feeds into inpainting; the blocks are
what it turns into text areas.

## Architecture
Three independent service classes, one per engine, plus the vendored ComicTextDetector
implementation they do not share:

- `ctd.py` -> `CtdTextDetectorService` — Torch, weights from `ManhwaStudio_AI_Models/Torch`
  (`config.TEXT_DETECTOR_DIR` / `comictextdetector.pt`). Runs the model in `textdetector/`.
- `paddle.py` -> `PaddleTextDetectorService` — ONNX Runtime, weights from
  `ManhwaStudio_AI_Models/ONNX/PaddleOCR`. Model path and provider settings come from
  `../engines/paddle_onnx.py`, shared with the Paddle OCR service.
- `surya.py` -> `SuryaTextDetectorService` — Torch, checkpoints owned by the `surya` library's own
  cache; presence and download go through `../engines/surya_checkpoints.py`, shared with
  `../ocr/surya.py`.

Nothing in this package imports another service in it. `__init__.py` is deliberately a bare
docstring: each engine drags in a different heavy stack, so importing one detector must never import
the others' dependencies.

Services are constructed once in `server.py` and reach the IPC layer only as fields on `AppState`
(`text_detector_ctd`, `text_detector_paddle`, `text_detector_surya`). The handler
`ipc/handlers/textdetector.py` backs `textdetector.ctd`, `textdetector.paddle` and
`textdetector.surya` and touches services exclusively through `HandlerContext.state` — it never
imports this package, so a service must not expect any handler-side setup.

`ctd.py` and `surya.py` take a `LoadedModelManager` (`../runtime/model_manager.py`) and hold a lease
around every detection so the process-wide resident-model budget can unload them. `paddle.py` has no
lease: ONNX sessions are managed inside `PaddleOnnxRuntime`.

## Files and submodules
- `ctd.py`: CTD service. Normalizes/clamps runtime params, keys the loaded model by
  `ctd:<device>` so a device or size change reloads, applies font-size post-processing and mask
  dilation. Imports `CTDModel` lazily from `textdetector/` at first use.
- `paddle.py`: Paddle ONNX detector service. Runs DB detection, then rebuilds a glyph-shaped mask
  per polygon ROI (saturation / dark / light Otsu cascade) instead of filling the polygon, and
  returns `polys` with scores in addition to `blocks`.
- `surya.py`: Surya detector service. Calls `batch_detection` directly (no OCR wrapper), then
  re-implements heatmap post-processing to keep the binary mask that Surya's own API discards.
  Loads the float16 checkpoint as float32 — see the contract below.
- `base.py`: compatibility `BaseModule` / `DEFAULT_DEVICE` / `DEVICE_SELECTOR` shim for the vendored
  code. **Its only consumer is `textdetector/base.py` (`from ..base import ...`)** — no service and
  no other package imports it. It looks like dead code and is not.
- `textdetector/`: vendored ComicTextDetector (BallonTranslator lineage) with its `ctd/` and
  `yolov5/` subtrees. See `textdetector/MODULE_README.md`.

## Contracts and invariants
- Torch and ONNX model roots stay separate: CTD reads `Torch/`, Paddle reads `ONNX/`. Never write
  one engine's weights under the other's root.
- Missing weights, a missing package, or a failed load surface as an explicit error; no service may
  fall back to another engine or return an empty result to hide a failure.
- Every service resolves its device from `General.ai_device`, treating `not-selected` as "no choice
  yet" and falling back through `AIDevice.detect_available_devices()`. CPU is a fallback, never a
  silent preference.
- The response contract of all three `detect_page` / `detect_image_bytes` methods is
  `source_size`, `blocks` (`x1/y1/x2/y2`, clamped to the image, degenerate boxes dropped) and
  `mask_png` as raw PNG bytes. `surya.py` adds `lines`, `paddle.py` adds `polys`; the handler moves
  `mask_png` into the response blob.
- Images are decoded from bytes (`cv2.imdecode` / PIL over an in-memory buffer), never by handing a
  path to `cv2.imread` — `imread` cannot open non-ASCII paths on Windows.
- MIGraphX constraint: when the selected ONNX provider is `MIGraphXExecutionProvider`, the detection
  session is forced onto `CPUExecutionProvider` (`PaddleOnnxRuntime._det_provider_settings` in
  `../engines/paddle_onnx.py`); only recognition runs on MIGraphX. `paddle.py` must not assume its
  requested provider is the one the detection session actually used.
- `surya.py` needs no ROCm mmap staging. It requests float32 for a float16 checkpoint, so Torch
  casts on the host into fresh anonymous memory and the host->device copy never reads the
  safetensors file mapping (`_preferred_detector_dtype` documents the measurement). Switching it
  back to float16 would reintroduce the multi-second-per-tensor amdkfd stall and would then require
  `runtime.rocm_mmap_transfer.patched_module_to()`.
- `base.py` and `textdetector/` must stay in the same package: the vendored code reaches the shim
  with a relative `..base` import.
- `__init__.py` must stay import-free. Adding a re-export there makes every detector's dependencies
  load whenever any one of them is imported.
- Missing coverage (pre-existing, per CLAUDE.md §16): this package has no unit tests. All three
  services are thin adapters around model runtimes whose behavior cannot be exercised without real
  checkpoints and a GPU, and the pure-Python parts (param clamping, block collection, mask
  encoding) are private methods on classes whose constructors already reach for config and model
  paths. Verification is currently limited to `python3 -m compileall` plus an import smoke test of
  the three service modules. Tests should be added if any of these files grows logic that is
  separable from the model runtime.

## Editing map
- To change what a detector returns to Rust, edit the service's `_detect_*`/`_collect_blocks` and
  keep `ipc/handlers/textdetector.py` in sync (it owns the header/blob split).
- To change CTD detection itself (network, NMS, mask refinement), edit `textdetector/`, not `ctd.py`.
- To change CTD runtime parameters or their clamping, edit `_normalize_params` in `ctd.py`.
- To change Paddle model-path or ONNX provider resolution, edit `../engines/paddle_onnx.py` — it is
  shared with OCR, so a change there affects both.
- To change Surya checkpoint location or download, edit `../engines/surya_checkpoints.py`.
- To change device resolution, edit `_resolve_selected_backend_device` in the affected service.
- To add a fourth detector: add a module here, wire it into `server.py`'s `AppState`, add the method
  constant and handler in `ipc/`, and leave `__init__.py` alone.
