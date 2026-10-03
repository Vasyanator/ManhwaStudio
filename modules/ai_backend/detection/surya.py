"""
FILE OVERVIEW: modules/ai_backend/detection/surya.py
Forward-only Surya text detector service.

Main responsibilities:
- lazy init and health reporting for the Surya detection-only predictor;
- explicit checkpoint presence check and auto-download for the detector model;
- run ONE batched forward pass over equal-size RGB tiles prepared by Rust and
  return the text heatmap (model channel 0, at a quarter of the tile size) as
  `uint8`;
- synchronize model device with backend `General.ai_device`;
- cooperate with `LoadedModelManager` for bounded resident model count.

Key structures:
- SuryaTextDetectorService

Notes:
Backs the `textdetector.surya.forward` IPC method (`ipc/handlers/textdetector.py`).
Resizing, page chunking, stitching and the CRAFT-style post-process (dynamic
thresholds, components, boxes, mask) live in Rust (`crates/ms-text-detect`);
this service only applies the library processor's normalization (no resize)
and the network. Checkpoint presence and download are delegated to
`engines/surya_checkpoints.py`, shared with the Surya OCR service
(`ocr/surya.py`). This service needs no ROCm mmap staging: it loads the float16
detector checkpoint as float32, and that host-side cast already materializes the
weights in anonymous memory (see `_preferred_detector_dtype`).
"""

from __future__ import annotations

import gc
import logging
import threading
from typing import Any

import numpy as np

try:
    from ai_device import AIDevice
except Exception:
    from modules.ai_device import AIDevice

try:
    from config import UserConfig
except Exception:
    UserConfig = None

from ..runtime.model_manager import LoadedModelManager
from ..engines.surya_checkpoints import (
    checkpoint_local_dir,
    checkpoint_ready,
    ensure_checkpoint_downloaded,
)
from .forward_maps import quantize_probability_maps, validate_tiles

log = logging.getLogger(__name__)

# Names this service in checkpoint download errors.
CHECKPOINT_LABEL = "Surya detector"

# Input alignment and output stride of the Surya segformer: the heatmap is
# exactly a quarter of the input on each axis when both sides are a multiple
# of 4 (probed: 1200x1200 -> 300x300, 1000x700 -> 250x175).
SURYA_INPUT_ALIGN = 4
SURYA_MAP_STRIDE = 4


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


class SuryaTextDetectorService:
    MODEL_KEY_PREFIX = "surya:detector_only"

    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._predictor = None
        self._device: str | None = None
        self._last_error: str | None = None

    def health(self) -> dict[str, Any]:
        with self._lock:
            checkpoint = self._checkpoint_name()
            model_dir = checkpoint_local_dir(checkpoint)
            model_exists = bool(model_dir) and checkpoint_ready(model_dir)
            return {
                "ready": self._predictor is not None,
                "device": self._device,
                "model": "surya_detection",
                "checkpoint": checkpoint,
                "model_dir": model_dir,
                "model_exists": model_exists,
                "last_error": self._last_error,
            }

    def forward_tiles(self, tiles: np.ndarray) -> np.ndarray:
        """Run the Surya detector on RGB tiles and return `uint8` maps `[n, 1, h/4, w/4]`.

        `tiles` is `uint8 [n, h, w, 3]`, RGB, both sides a multiple of 4. The
        single channel is the model's text heatmap (channel 0, sigmoid already
        applied inside the model) at its native quarter resolution; the
        library's bilinear upsample to the processor size is NOT applied.

        Raises `ValueError` for an invalid batch and `RuntimeError` for an
        unavailable package, a failed load, an unexpected output shape or
        non-finite heatmap values (float16 NaN on some CUDA setups).
        """
        n, height, width = validate_tiles(tiles, align=SURYA_INPUT_ALIGN, engine="surya")
        selected_device = _resolve_selected_backend_device(self._device or "cpu")
        model_key = self._model_key(selected_device)
        log.info(
            "Surya forward start tiles=%s size=%sx%s device=%s model_key=%s",
            n,
            width,
            height,
            selected_device,
            model_key,
        )
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
        )
        try:
            with self._lock:
                predictor = self._ensure_predictor_locked(selected_device)
                maps = _forward_with_predictor(predictor, tiles)
            if lease.needs_load:
                lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
            self._last_error = None
            log.info("Surya forward done device=%s map_size=%sx%s", selected_device, maps.shape[3], maps.shape[2])
            return maps
        except Exception as exc:
            log.exception("Surya forward failed device=%s error=%s", selected_device, exc)
            if lease.needs_load:
                lease.mark_load_failed()
            self._last_error = str(exc)
            raise
        finally:
            lease.release()

    def _ensure_predictor_locked(self, device: str):
        if self._predictor is not None and self._device == device:
            return self._predictor

        if self._device is not None and self._device != device:
            self._drop_predictor_locked()

        self._ensure_checkpoint_downloaded_locked()

        try:
            from surya.detection import DetectionPredictor  # type: ignore
        except Exception as exc:
            self._last_error = f"Surya detection package is not available: {exc}"
            raise RuntimeError(self._last_error) from exc

        try:
            predictor_dtype = _preferred_detector_dtype(device)
            if predictor_dtype is None:
                self._predictor = DetectionPredictor(device=device)
                log.info("Surya detection predictor init device=%s dtype=default", device)
            else:
                self._predictor = DetectionPredictor(device=device, dtype=predictor_dtype)
                log.info(
                    "Surya detection predictor init device=%s dtype=%s",
                    device,
                    predictor_dtype,
                )
        except Exception as exc:
            self._predictor = None
            self._device = None
            self._last_error = f"Surya detection init failed: {exc}"
            raise RuntimeError(self._last_error) from exc

        self._device = device
        self._last_error = None
        return self._predictor

    def _ensure_checkpoint_downloaded_locked(self) -> None:
        """Fetch the detector checkpoint if it is not complete on disk.

        Caller must hold `self._lock`. Delegates to `engines/surya_checkpoints.py`,
        which the Surya OCR service shares; see that module for why the download
        must finish before the predictor is constructed.

        # Raises
        `FileNotFoundError` / `RuntimeError` as documented on
        `engines.surya_checkpoints.ensure_checkpoint_downloaded`.
        """
        ensure_checkpoint_downloaded(self._checkpoint_name(), label=CHECKPOINT_LABEL)

    def _unload_key(self, model_key: str) -> bool:
        with self._lock:
            current_device = self._device
            if current_device is None or model_key != self._model_key(current_device):
                return False
            self._drop_predictor_locked()
            _clear_torch_cache()
            self._model_manager.mark_unloaded(model_key)
            return True

    def _drop_predictor_locked(self) -> None:
        self._predictor = None
        self._device = None

    @classmethod
    def _model_key(cls, device: str) -> str:
        return f"{cls.MODEL_KEY_PREFIX}:{device}"

    @staticmethod
    def _checkpoint_name() -> str:
        from surya.settings import settings  # type: ignore

        return str(settings.DETECTOR_MODEL_CHECKPOINT)


def _forward_with_predictor(predictor, tiles: np.ndarray) -> np.ndarray:
    """One batched forward pass through a loaded `DetectionPredictor`.

    Normalizes each tile with the predictor's own processor (rescale 1/255, then
    the checkpoint's mean/std from `preprocessor_config.json`; the processor does
    not resize), casts to the model dtype (float32 on CUDA, see
    `_preferred_detector_dtype`) and returns quantized channel 0. Releases
    Torch's CUDA cache afterwards when CUDA (or ROCm) is available, as upstream
    `batch_detection` does.
    """
    import torch  # type: ignore
    from surya.settings import settings  # type: ignore

    n, height, width = (int(v) for v in tiles.shape[:3])
    pixel_values = [
        torch.from_numpy(np.asarray(predictor.processor(tile)["pixel_values"][0]))
        for tile in tiles
    ]
    batch = torch.stack(pixel_values, dim=0).to(predictor.model.dtype)
    with settings.INFERENCE_MODE():
        pred = predictor.model(pixel_values=batch.to(predictor.model.device))
    heat = pred.logits[:, 0:1].to(torch.float32).cpu().numpy()
    # Upstream `DetectionPredictor.batch_detection` ends every call with
    # `torch.cuda.empty_cache()`; this path bypasses it, so release the batch's
    # cached activation memory here. Torch could reuse it, but the non-Torch GPU
    # users of this process (ONNX Runtime CUDA/ROCm sessions) cannot. A bare call
    # rather than `_clear_torch_cache`: that one also runs `gc.collect()`, too
    # costly once per tile batch.
    if torch.cuda.is_available():
        torch.cuda.empty_cache()
    expected = (n, 1, height // SURYA_MAP_STRIDE, width // SURYA_MAP_STRIDE)
    if tuple(heat.shape) != expected:
        raise RuntimeError(f"Surya detector output has shape {tuple(heat.shape)}, expected {expected}.")
    return quantize_probability_maps(heat, engine="surya")


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
    normalized = value.strip().lower()
    if normalized == "not-selected":
        return None
    return normalized or None


def _safe_available_devices() -> set[str]:
    try:
        return set(AIDevice.detect_available_devices())
    except Exception:
        return {"cpu"}


def _normalize_backend_device(raw: str, fallback: str) -> str:
    normalized = str(raw or "").strip().lower()
    if normalized == "cpu" or normalized == "cuda" or normalized.startswith("cuda:"):
        return normalized
    if normalized == "mps":
        return normalized
    return str(fallback or "cpu").strip().lower() or "cpu"


def _resolve_selected_backend_device(fallback: str) -> str:
    fallback_norm = _normalize_backend_device(fallback, "cpu")
    configured = _read_configured_device()
    if configured is None:
        configured = fallback_norm

    normalized = _normalize_backend_device(configured, fallback_norm)
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


def _preferred_detector_dtype(device: str):
    """Dtype to load the Surya detector with, or `None` for Surya's default.

    `device` is a backend device string (`cpu`, `cuda`, `cuda:N`, `mps`).
    Returns `torch.float32` for CUDA/ROCm targets and `None` when Torch is
    unavailable or the target is not CUDA.
    """
    normalized = str(device or "").strip().lower()
    if normalized.startswith("cuda"):
        try:
            import torch  # type: ignore
        except Exception:
            return None
        # Surya detection on some CUDA setups emits NaN heatmaps in float16.
        # Prefer float32 here to preserve correctness.
        #
        # As a side effect this keeps the service clear of the ROCm mmap->GPU
        # stall handled by `runtime/rocm_mmap_transfer.py` (see `ocr/surya.py`): the
        # detector checkpoint is stored in float16, so requesting float32 makes
        # transformers cast on the host into freshly allocated anonymous memory
        # and the host->device copy no longer reads from the safetensors file
        # mapping. Measured with `rocm_mmap_transfer._is_file_backed` on the CPU
        # copy of this checkpoint: 29 of 29 parameters >=1 MiB are file-backed
        # at float16, 0 of 43 at float32. Changing this back to float16 would
        # reintroduce the stall, and the load would then have to be wrapped in
        # `runtime.rocm_mmap_transfer.patched_module_to()`.
        return torch.float32
    return None
