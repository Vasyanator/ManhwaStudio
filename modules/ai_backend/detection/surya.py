"""
FILE OVERVIEW: modules/ai_backend/detection/surya.py
Forward-only Surya text detector service.

Main responsibilities:
- lazy init and health reporting for the Surya detection-only predictor;
- explicit presence check of the detector checkpoint directory Rust downloaded
  (sent per request as `surya_detection_dir`; never downloaded here);
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
and the network. The checkpoint presence check and the settings pointer are
delegated to `engines/surya_checkpoints.py`, shared with the Surya OCR service
(`ocr/surya.py`); the directory is passed as `checkpoint=` so the load stays
offline. This service needs no ROCm mmap staging: it loads the float16
detector checkpoint as float32, and that host-side cast already materializes the
weights in anonymous memory (see `_preferred_detector_dtype`).
"""

from __future__ import annotations

import functools
import logging
import threading
from typing import Any

import numpy as np

from ..runtime.health_snapshot import HealthSnapshot
from ..runtime.model_manager import LoadedModelManager
from ..runtime.torch_device import clear_torch_cache, resolve_selected_backend_device
from ..engines.surya_checkpoints import point_settings_at, require_local_checkpoint
from .forward_maps import quantize_probability_maps, validate_tiles

log = logging.getLogger(__name__)

# Names this service in "not downloaded" errors.
CHECKPOINT_LABEL = "Surya detector"

# Input alignment and output stride of the Surya segformer: the heatmap is
# exactly a quarter of the input on each axis when both sides are a multiple
# of 4 (probed: 1200x1200 -> 300x300, 1000x700 -> 250x175).
SURYA_INPUT_ALIGN = 4
SURYA_MAP_STRIDE = 4


#: `General.ai_device` resolution with this service's device policy (owner:
#: `runtime.torch_device`); patched by name in the test suite.
_resolve_selected_backend_device = resolve_selected_backend_device

#: Allocator-cache release with this service's policy (owner: `runtime.torch_device`);
#: patched by name in the test suite.
_clear_torch_cache = functools.partial(clear_torch_cache, owner="Surya detection")


class SuryaTextDetectorService:
    MODEL_KEY_PREFIX = "surya:detector_only"

    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._predictor = None
        self._device: str | None = None
        # Checkpoint directory the resident predictor was built from; part of
        # its identity next to `_device`.
        self._model_dir: str | None = None
        self._last_error: str | None = None
        # Published copy of the state-derived health fields; `health()` never
        # takes `_lock`, which a load and the forward pass hold (S-014).
        self._health = HealthSnapshot(ready=False, device=None, model_dir=None, last_error=None)

    def health(self) -> dict[str, Any]:
        """Return `{"ready", "device", "model", "model_dir", "last_error"}` for the health snapshot.

        `model_dir` is the directory of the RESIDENT predictor (`None` when
        nothing is loaded): the checkpoint location is Rust's and arrives per
        request, so there is no library cache to probe. Reads only the published
        snapshot, so it answers while a load or a forward pass holds the lock.
        """
        return {"model": "surya_detection", **self._health.read()}

    def _publish_health_locked(self) -> None:
        """Republish the state-derived health fields; caller holds `self._lock`."""
        self._health.publish(
            ready=self._predictor is not None,
            device=self._device,
            model_dir=self._model_dir,
            last_error=self._last_error,
        )

    def forward_tiles(self, tiles: np.ndarray, *, model_dir: str) -> np.ndarray:
        """Run the Surya detector on RGB tiles and return `uint8` maps `[n, 1, h/4, w/4]`.

        `tiles` is `uint8 [n, h, w, 3]`, RGB, both sides a multiple of 4.
        `model_dir` is the absolute detector checkpoint directory Rust
        downloaded. The single channel is the model's text heatmap (channel 0,
        sigmoid already applied inside the model) at its native quarter
        resolution; the library's bilinear upsample to the processor size is
        NOT applied.

        Raises `ValueError` for an invalid batch or path, `FileNotFoundError`
        ("not downloaded") for a missing or incomplete `model_dir`, and
        `RuntimeError` for an unavailable package, a failed load, an unexpected
        output shape or non-finite heatmap values (float16 NaN on some CUDA
        setups).
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
                predictor = self._ensure_predictor_locked(selected_device, model_dir)
                maps = _forward_with_predictor(predictor, tiles)
            if lease.needs_load:
                lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
            self._last_error = None
            # Set outside `_lock` (a forward must not wait for another request's
            # load just to clear its error), so only this field is republished.
            self._health.publish(last_error=None)
            log.info("Surya forward done device=%s map_size=%sx%s", selected_device, maps.shape[3], maps.shape[2])
            return maps
        except Exception as exc:
            log.exception("Surya forward failed device=%s error=%s", selected_device, exc)
            if lease.needs_load:
                lease.mark_load_failed()
            self._last_error = str(exc)
            self._health.publish(last_error=self._last_error)
            raise
        finally:
            lease.release()

    def _ensure_predictor_locked(self, device: str, model_dir: str):
        """Return the predictor for `device` built from `model_dir`, loading it if needed.

        Caller must hold `self._lock`. Reloads when the resident predictor lives
        on another device or came from another directory. Raises what
        `forward_tiles` documents for the load.
        """
        if self._predictor is not None and self._device == device and self._model_dir == model_dir:
            return self._predictor

        if self._device is not None and (self._device != device or self._model_dir != model_dir):
            self._drop_predictor_locked()

        checkpoint = require_local_checkpoint(model_dir, label=CHECKPOINT_LABEL)

        try:
            from surya.detection import DetectionPredictor  # type: ignore
        except Exception as exc:
            self._last_error = f"Surya detection package is not available: {exc}"
            self._publish_health_locked()
            raise RuntimeError(self._last_error) from exc

        try:
            # The settings AND the explicit `checkpoint=`: nothing inside the
            # library may fall back to the `s3://` default and download it.
            point_settings_at(detection_dir=checkpoint)
            predictor_dtype = _preferred_detector_dtype(device)
            if predictor_dtype is None:
                self._predictor = DetectionPredictor(checkpoint=checkpoint, device=device)
                log.info("Surya detection predictor init device=%s dtype=default checkpoint=%s", device, checkpoint)
            else:
                self._predictor = DetectionPredictor(checkpoint=checkpoint, device=device, dtype=predictor_dtype)
                log.info(
                    "Surya detection predictor init device=%s dtype=%s checkpoint=%s",
                    device,
                    predictor_dtype,
                    checkpoint,
                )
        except Exception as exc:
            self._predictor = None
            self._device = None
            self._last_error = f"Surya detection init failed: {exc}"
            self._publish_health_locked()
            raise RuntimeError(self._last_error) from exc

        self._device = device
        self._model_dir = checkpoint
        self._last_error = None
        self._publish_health_locked()
        return self._predictor

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
        """Forget the predictor, its device and directory; caller holds `self._lock`."""
        self._predictor = None
        self._device = None
        self._model_dir = None
        self._publish_health_locked()

    @classmethod
    def _model_key(cls, device: str) -> str:
        return f"{cls.MODEL_KEY_PREFIX}:{device}"


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
