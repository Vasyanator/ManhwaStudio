# Module: modules/ai_backend

## Purpose
Python AI backend runtime called by the Rust application over HTTP.

## Architecture
`server.py` wires OCR, text detection, inpainting, translation, device selection, version health
metadata, and shared loaded-model management. Torch-backed services load weights from
`ManhwaStudio_AI_Models/Torch`; ONNX-backed services load weights from
`ManhwaStudio_AI_Models/ONNX`. Library-managed model downloads and caches, such as EasyOCR and
Surya, must remain under those libraries' default paths unless a service explicitly owns the
download contract.

## Files and submodules
- `server.py`: HTTP routing and service construction.
- `model_manager.py`: shared resident-model lease and unload manager.
- `device_service.py`: selected Torch/ONNX device state, accelerated defaults, and manual
  selection flags when backend devices need a user choice.
- `test_device_service.py`: unit tests for backend device selection sentinel and fallback
  contracts.
- `*_service.py`: backend service adapters for OCR, detectors, inpaint, and MT.
- `paddle_onnx_runtime.py`: shared ONNX Runtime helpers for PaddleOCR.
- `textdetector/`: ComicTextDetector implementation used by CTD service.

## Contracts and invariants
- Service initialization is lazy and must surface missing packages or weights as explicit errors.
- Torch and ONNX model roots are separate; do not write ONNX weights under `Torch/` or Torch
  checkpoints under `ONNX/`.
- When no device config exists, CPU is only a temporary fallback. PyTorch should report an
  unresolved device choice as soon as CUDA is available, and ONNX should prefer DirectML on Windows
  when that provider exists. Unconfigured DirectML selection is reported through `/device` as
  needing manual confirmation before it is persisted.
- `General.ai_device`, `General.ai_onnx_provider`, and `General.ai_onnx_device_id` use
  `not-selected` as the default config sentinel. Backend services must resolve it to a real runtime
  default before constructing Torch devices or ONNX provider settings.
- EasyOCR and Surya should use their own library model caches because their packages own the
  download behavior.
- Long-running model inference runs outside the Rust GUI thread through backend requests.
- `/health` must include `backend_version` from root `config.VERSION`; Rust uses it to warn when
  the backend package and Studio binary versions do not match.

## Editing map
- To change model root resolution, edit `config.py` and the affected service resolver.
- To change PaddleOCR ONNX layout, edit `paddle_onnx_runtime.py`.
- To change inpaint checkpoint handling, edit the corresponding inpaint service.
