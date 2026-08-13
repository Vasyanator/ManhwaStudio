"""
File: modules/ai_backend/detection/__init__.py

Purpose:
Package marker for the AI backend's text-detection services.

Notes:
Intentionally empty of re-exports and submodule imports. Each detector pulls in
a different heavy stack (CTD needs Torch, `paddle.py` needs ONNX Runtime,
`surya.py` needs Torch plus the surya package), so importing one detector must
never drag in the dependencies of the others. Import the concrete module
instead, e.g. `from modules.ai_backend.detection.ctd import
CtdTextDetectorService`.
"""
