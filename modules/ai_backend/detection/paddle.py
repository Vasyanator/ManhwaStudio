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
from ..runtime.health_snapshot import HealthSnapshot
from .forward_maps import quantize_probability_maps, validate_tiles

log = logging.getLogger(__name__)

# Input alignment of the PP-OCR detection network: both sides a multiple of 32
# (other sizes fail inside the graph, e.g. 100x70 errors in `Add.4`).
PADDLE_INPUT_ALIGN = 32


class PaddleTextDetectorService:
    """Paddle detection forward pass; the session is leased (`PaddleOnnxRuntime.det_session`) before `_lock`."""

    def __init__(self, runtime_factory: RuntimeFactory) -> None:
        self._lock = threading.RLock()
        self._runtime = PaddleOnnxRuntime(runtime_factory)
        self._provider: str | None = None
        self._device_id: str | None = None
        self._last_error: str | None = None
        # Published copy of the state-derived health fields; `health()` never
        # takes `_lock`, which the forward pass holds (S-014).
        self._health = HealthSnapshot()
        self._publish_health_locked()

    def health(self) -> dict[str, Any]:
        """Return the Paddle detector health entry for the backend snapshot.

        `provider`/`device_id`/`last_error` come from the published snapshot;
        the model-path probe runs here, outside any service lock, so this
        answers while a forward pass holds the instance lock.
        """
        fields = self._health.read()
        try:
            det_model_path = resolve_det_model_path()
            model_exists = det_model_path.is_file()
            model_dir = str(det_model_path.parent)
        except Exception:
            model_exists = False
            model_dir = ""
        return {
            "ready": fields["last_error"] is None and model_exists,
            "model": "PP-OCRv5_server_det",
            "model_dir": model_dir,
            "model_exists": model_exists,
            "provider": fields["provider"],
            "device_id": fields["device_id"],
            "last_error": fields["last_error"],
        }

    def _publish_health_locked(self) -> None:
        """Republish the state-derived health fields; caller holds `self._lock` (or is `__init__`)."""
        self._health.publish(
            provider=self._provider or "CPUExecutionProvider",
            device_id=self._device_id or "0",
            last_error=self._last_error,
        )

    def forward_tiles(self, tiles: np.ndarray) -> np.ndarray:
        """Run detection on a batch of RGB tiles and return `uint8` maps `[n, 1, h, w]`.

        `tiles` is `uint8 [n, h, w, 3]`, RGB, both sides a multiple of 32. The
        single channel is the DB probability map at the input resolution.

        Raises `ValueError` for an invalid batch and `RuntimeError` (or the
        model-path errors of `resolve_det_model_path`) for a missing model, a
        failed session or a malformed / non-finite output.
        """
        n, height, width = validate_tiles(tiles, align=PADDLE_INPUT_ALIGN, engine="paddle")
        try:
            provider_settings = resolve_provider_settings(UserConfig)
            # The lease first, then the instance lock: acquiring may block in
            # the residency arbiter, and no service may hold its lock across
            # that without a lease (owner-rule premise,
            # `runtime/residency/MODULE_README.md`).
            with self._runtime.det_session(provider_settings) as leased:
                with self._lock:
                    pred = self._runtime.forward_det(leased, tiles, provider_settings)
                    maps = quantize_probability_maps(pred, engine="paddle")
                    self._provider = provider_settings.provider
                    self._device_id = provider_settings.device_id
                    self._last_error = None
                    self._publish_health_locked()
            log.info("Paddle det forward done tiles=%s size=%sx%s", n, width, height)
            return maps
        except Exception as exc:
            with self._lock:
                self._last_error = str(exc)
                self._publish_health_locked()
            log.exception("Paddle det forward failed tiles=%s size=%sx%s", n, width, height)
            raise
