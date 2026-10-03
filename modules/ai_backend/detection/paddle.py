"""
FILE OVERVIEW: modules/ai_backend/detection/paddle.py
Forward-only text detector service backed by the PaddleOCR ONNX detection model.

Main responsibilities:
- Run the PP-OCRv5 detection network through ONNX Runtime on equal-size RGB
  tiles prepared by Rust, and return the probability maps as `uint8` for the
  `textdetector.paddle.forward` IPC method (`ipc/handlers/textdetector.py`).
- Read detector weights from `ManhwaStudio_AI_Models/ONNX/PaddleOCR`.

Key structures:
- PaddleTextDetectorService

Notes:
Model-path resolution, ONNX Runtime provider selection, the ImageNet input
normalization and the MIGraphX->CPU rule for the detection session are owned by
the shared `engines/paddle_onnx.py` helper, which the Paddle OCR service also
uses. The DB post-process and the glyph mask live in Rust
(`crates/ms-text-detect`).
"""

from __future__ import annotations

import logging
import threading
from typing import Any

import numpy as np

try:
    from config import UserConfig
except Exception:
    UserConfig = None

from ..engines.paddle_onnx import (
    PaddleOnnxRuntime,
    RuntimeFactory,
    resolve_det_model_path,
    resolve_provider_settings,
)
from .forward_maps import quantize_probability_maps, validate_tiles

log = logging.getLogger(__name__)

# Input alignment of the PP-OCR detection network: both sides a multiple of 32
# (other sizes fail inside the graph, e.g. 100x70 errors in `Add.4`).
PADDLE_INPUT_ALIGN = 32


class PaddleTextDetectorService:
    """Paddle detection forward pass; sessions are leased inside `PaddleOnnxRuntime`."""

    def __init__(self, runtime_factory: RuntimeFactory) -> None:
        self._lock = threading.RLock()
        self._runtime = PaddleOnnxRuntime(runtime_factory)
        self._provider: str | None = None
        self._device_id: str | None = None
        self._last_error: str | None = None

    def health(self) -> dict[str, Any]:
        with self._lock:
            try:
                det_model_path = resolve_det_model_path()
                model_exists = det_model_path.is_file()
                model_dir = str(det_model_path.parent)
            except Exception:
                det_model_path = None
                model_exists = False
                model_dir = ""
            return {
                "ready": self._last_error is None and model_exists,
                "model": "PP-OCRv5_server_det",
                "model_dir": model_dir,
                "model_exists": model_exists,
                "provider": self._provider or "CPUExecutionProvider",
                "device_id": self._device_id or "0",
                "last_error": self._last_error,
            }

    def forward_tiles(self, tiles: np.ndarray) -> np.ndarray:
        """Run detection on a batch of RGB tiles and return `uint8` maps `[n, 1, h, w]`.

        `tiles` is `uint8 [n, h, w, 3]`, RGB, both sides a multiple of 32. The
        single channel is the DB probability map at the input resolution.

        Raises `ValueError` for an invalid batch and `RuntimeError` (or the
        model-path errors of `resolve_det_model_path`) for a missing model, a
        failed session or a malformed / non-finite output.
        """
        n, height, width = validate_tiles(tiles, align=PADDLE_INPUT_ALIGN, engine="paddle")
        with self._lock:
            try:
                provider_settings = resolve_provider_settings(UserConfig)
                pred = self._runtime.forward_det(tiles, provider_settings)
                maps = quantize_probability_maps(pred, engine="paddle")
                self._provider = provider_settings.provider
                self._device_id = provider_settings.device_id
                self._last_error = None
                log.info("Paddle det forward done tiles=%s size=%sx%s", n, width, height)
                return maps
            except Exception as exc:
                self._last_error = str(exc)
                log.exception("Paddle det forward failed tiles=%s size=%sx%s", n, width, height)
                raise
