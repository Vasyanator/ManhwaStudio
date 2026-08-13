"""
Package: modules/ai_backend/engines

Model-family runtime shared by more than one service domain: the PaddleOCR
ONNX Runtime engine (`paddle_onnx`, used by both `ocr/` and `detection/`) and
the Surya checkpoint helper (`surya_checkpoints`, used by both Surya services).

This package intentionally exposes NOTHING here - no re-exports and no imports
of its own submodules. `paddle_onnx` pulls in cv2/numpy/onnxruntime at import
time, so a re-export would make every `modules.ai_backend.engines.*` import pay
for the AI stack. Keeping this file empty of imports is what lets the torch-free
`ipc/` layer and its tests stay importable without those dependencies, and it is
the same reason the parent package (`modules/ai_backend/__init__.py`) resolves
`run_server` lazily via PEP 562.

Import submodules directly, e.g. `from ..engines.paddle_onnx import
resolve_models_root`.
"""
