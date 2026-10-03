"""
File: modules/ai_backend/detection/ctd.py

Purpose:
Forward-only Comic Text Detector service for the Python AI backend.

Main responsibilities:
- load the CTD Torch network (`TextDetBase`) lazily, keyed by device;
- synchronize the model device with the backend setting `General.ai_device`;
- run ONE batched forward pass over equal-size RGB tiles prepared by Rust and
  return the `[seg, shrink]` probability maps as `uint8`.

Key structures:
- CtdTextDetectorService

Key functions:
- CtdTextDetectorService.forward_tiles()

Notes:
Planning, resizing, tiling, stitching and every post-process step (DB boxes,
mask refinement, dilation) live in Rust (`crates/ms-text-detect`). The network
code is the vendored `detection/textdetector/ctd/basemodel.py`, imported lazily
so that constructing this service never pulls in Torch.
Backs the `textdetector.ctd.forward` IPC method (`ipc/handlers/textdetector.py`).
"""

from __future__ import annotations

import gc
import logging
import threading
from pathlib import Path
from typing import Any

import numpy as np

try:
    from ai_device import AIDevice
except Exception:
    from modules.ai_device import AIDevice

from config import TEXT_DETECTOR_DIR
try:
    from config import UserConfig
except Exception:
    UserConfig = None

from ..runtime.model_manager import LoadedModelManager
from .forward_maps import quantize_probability_maps, validate_tiles

log = logging.getLogger(__name__)

MODEL_FILENAME = "comictextdetector.pt"

# Input alignment of the CTD network: every side must be a multiple of 64
# (a 1000 px side fails inside the UNet head with "Expected size 64 but got 63").
CTD_INPUT_ALIGN = 64


def _normalize_device(raw: Any, fallback: str) -> str:
    if raw is None:
        return fallback
    value = str(raw).strip().lower()
    if not value:
        return fallback
    if value == "cpu" or value.startswith("cuda"):
        return value
    if fallback == "cpu" or str(fallback).startswith("cuda"):
        return str(fallback)
    return "cpu"


def _default_device() -> str:
    try:
        import torch  # type: ignore

        if hasattr(torch, "cuda") and torch.cuda.is_available():
            return "cuda"
    except Exception:
        return "cpu"
    return "cpu"


def _clear_torch_cache() -> None:
    try:
        import torch  # type: ignore
    except Exception:
        gc.collect()
        return

    gc.collect()
    try:
        if hasattr(torch, "cuda") and torch.cuda.is_available():
            torch.cuda.empty_cache()
            torch.cuda.ipc_collect()
    except Exception:
        pass
    try:
        if hasattr(torch, "mps") and hasattr(torch.mps, "empty_cache"):
            torch.mps.empty_cache()
    except Exception:
        pass


class CtdTextDetectorService:
    """CTD forward pass behind a `LoadedModelManager` lease.

    Thread-safety: `forward_tiles` serializes model load and inference on one
    re-entrant lock; the lease is taken before the lock so the resident-model
    budget can ask this service to unload while it waits.
    """

    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._net = None
        self._net_model_key: str | None = None
        self._device = _resolve_selected_backend_device(_default_device())
        self._last_error: str | None = None
        self._model_path = Path(TEXT_DETECTOR_DIR) / MODEL_FILENAME

    def health(self) -> dict[str, Any]:
        with self._lock:
            return {
                "ready": self._net is not None,
                "model": "ctd",
                "model_path": str(self._model_path),
                "model_exists": self._model_path.exists(),
                "device": self._device,
                "last_error": self._last_error,
            }

    def forward_tiles(self, tiles: np.ndarray) -> np.ndarray:
        """Run CTD on a batch of RGB tiles and return `uint8` maps `[n, 2, h, w]`.

        `tiles` is `uint8 [n, h, w, 3]`, RGB, both sides a multiple of 64.
        Channel 0 is the segmentation map (`seg`), channel 1 the DB shrink map
        (`lines[:, 0]`); both have the input's resolution. The YOLO block head
        and the DB threshold map are discarded.

        Raises `ValueError` for an invalid batch, `FileNotFoundError` when the
        checkpoint is missing, and `RuntimeError` for a failed load, an
        unexpected output shape or non-finite probabilities.
        """
        n, height, width = validate_tiles(tiles, align=CTD_INPUT_ALIGN, engine="ctd")
        device = _resolve_selected_backend_device(self._device)
        model_key = self._model_key_for(device)
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
        )
        with self._lock:
            try:
                net = self._ensure_net_locked(device, model_key)
                maps = self._forward_locked(net, tiles, device)
                if lease.needs_load:
                    lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
                self._last_error = None
                log.info("CTD forward done device=%s tiles=%s size=%sx%s", device, n, width, height)
                return maps
            except Exception as exc:
                if lease.needs_load:
                    lease.mark_load_failed()
                self._last_error = str(exc)
                log.exception("CTD forward failed device=%s tiles=%s size=%sx%s", device, n, width, height)
                raise
            finally:
                lease.release()

    def _ensure_net_locked(self, device: str, model_key: str):
        """Return the network for `device`, (re)loading it when the key changed.

        Caller holds `self._lock`.
        """
        if self._net is not None and self._net_model_key == model_key:
            return self._net
        if not self._model_path.exists():
            raise FileNotFoundError(f"CTD model not found: {self._model_path}")

        previous_key = self._net_model_key
        if self._net is not None:
            self._net = None
            self._net_model_key = None
            _clear_torch_cache()
            if previous_key is not None:
                self._model_manager.mark_unloaded(previous_key)

        from .textdetector.ctd.basemodel import TextDetBase  # heavy import; keep lazy

        self._net = TextDetBase(str(self._model_path), device=device, act="leaky")
        self._net_model_key = model_key
        self._device = device
        log.info("CTD network loaded device=%s path=%s", device, self._model_path)
        return self._net

    @staticmethod
    def _forward_locked(net, tiles: np.ndarray, device: str) -> np.ndarray:
        """One batched forward pass; returns quantized `[seg, shrink]` maps."""
        import torch  # type: ignore

        n, height, width = (int(v) for v in tiles.shape[:3])
        # NHWC RGB u8 -> NCHW float32 in [0, 1]: the normalization the network
        # was trained with (upstream `preprocess_img`: /255, RGB order).
        batch = np.ascontiguousarray(tiles.transpose(0, 3, 1, 2)).astype(np.float32) / np.float32(255.0)
        with torch.no_grad():
            _blocks, seg, lines = net(torch.from_numpy(batch).to(device))
            seg_np = seg.float().cpu().numpy()
            shrink_np = lines[:, 0:1].float().cpu().numpy()
        if seg_np.shape != (n, 1, height, width):
            raise RuntimeError(
                f"CTD segmentation output has shape {tuple(seg_np.shape)}, expected {(n, 1, height, width)}."
            )
        if shrink_np.shape != (n, 1, height, width):
            raise RuntimeError(
                f"CTD DB output has shape {tuple(shrink_np.shape)}, expected {(n, 1, height, width)}."
            )
        return quantize_probability_maps(np.concatenate([seg_np, shrink_np], axis=1), engine="ctd")

    def _unload_key(self, model_key: str) -> bool:
        with self._lock:
            if self._net is None or self._net_model_key != model_key:
                return False
            self._net = None
            self._net_model_key = None
            _clear_torch_cache()
            self._model_manager.mark_unloaded(model_key)
            return True

    @staticmethod
    def _model_key_for(device: str) -> str:
        normalized = str(device).strip().lower() or "cpu"
        return f"ctd:{normalized}"


def _resolve_selected_backend_device(fallback: str) -> str:
    fallback_norm = _normalize_device(fallback, "cpu")
    configured = _read_configured_device()
    if configured is None:
        configured = fallback_norm

    normalized = _normalize_device(configured, fallback_norm)
    available = _safe_available_devices()

    if normalized in available:
        return normalized
    if normalized.startswith("cuda") and "cuda" in available:
        return "cuda"
    if fallback_norm in available:
        return fallback_norm
    if "cuda" in available:
        return "cuda"
    return "cpu"


def _read_configured_device() -> str | None:
    config_root = getattr(UserConfig, "config", None)
    if not isinstance(config_root, dict):
        return None
    general = config_root.get("General")
    if not isinstance(general, dict):
        return None
    value = general.get("ai_device")
    if not isinstance(value, str):
        return None
    value = value.strip().lower()
    if value == "not-selected":
        return None
    return value or None


def _safe_available_devices() -> set[str]:
    try:
        return set(AIDevice.detect_available_devices())
    except Exception:
        return {"cpu"}
