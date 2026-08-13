"""
Package: modules/ai_backend/ocr

OCR service adapters of the Python AI backend: MangaOCR (`manga.py`), EasyOCR
(`easy.py`), PaddleOCR ONNX (`paddle.py`), PaddleOCR-VL (`paddle_vl.py`), Surya
(`surya.py`), plus the PaddleOCR-VL script constraint (`script_constraint.py`).

This module intentionally re-exports NOTHING and imports no submodule. Each
engine drags in a different heavy stack (torch + transformers for MangaOCR
PyTorch / PaddleOCR-VL / Surya, `easyocr` for EasyOCR, `onnxruntime` + `cv2` for
the ONNX paths), so any re-export here would make importing one engine load the
dependencies of all the others - and would pull the whole AI stack into
torch-free consumers such as `modules.ai_backend.ipc`. Import the concrete
module instead: `from modules.ai_backend.ocr.manga import MangaOcrService`.

The parent package (`modules/ai_backend/__init__.py`) keeps itself cheap the
same way, via PEP 562 lazy `__getattr__` resolution.
"""

from __future__ import annotations
