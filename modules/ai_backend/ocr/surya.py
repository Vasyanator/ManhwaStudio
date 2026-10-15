"""
FILE OVERVIEW: modules/ai_backend/ocr/surya.py
OCR service for Surya OCR runtime.

Main responsibilities:
- lazy init and health reporting for Surya foundation/recognition predictors;
- optional lazy init of Surya text detector for `ocr_with_boxes` mode;
- OCR recognition from raw image bytes with stable JSON-friendly output;
- synchronization of model device with backend `General.ai_device`;
- cooperation with `LoadedModelManager` for bounded resident model count.

Key runtime options:
- `task_name`: `ocr_without_boxes`, `block_without_boxes`, or `ocr_with_boxes`;
- `recognize_math`: enable Surya math mode;
- `sort_lines`: request Surya line sorting;
- `drop_repeated_text`: suppress repeated decoded text;
- `max_sliding_window` / `max_tokens`: optional decoder limits.

Checkpoints:
Rust downloads both checkpoints (pinned external catalog) and sends their
absolute directories with every request (`surya_recognition_dir`,
`surya_detection_dir`). The service proves each directory complete
(`engines.surya_checkpoints.require_local_checkpoint`), points Surya's
settings at it and passes it as `checkpoint=`, so a load never touches the
network; a missing or incomplete directory is a "not downloaded" error.

Notes:
Surya owns the host->device transfer of its own weights, so every predictor
construction here is wrapped in `runtime.rocm_mmap_transfer.patched_module_to()`;
see `_ensure_recognition_loaded_locked` for why. The presence check runs before
that block, which covers the weight transfer only. Predictor loads are logged
with their duration so a slow start can be attributed to weight loading rather
than package import.
"""

from __future__ import annotations

import functools
import io
import logging
import threading
import time
from typing import Any

from ..engines.surya_checkpoints import point_settings_at, require_local_checkpoint
from ..runtime.health_snapshot import HealthSnapshot
from ..runtime.model_manager import LoadedModelManager
from ..runtime.rocm_mmap_transfer import patched_module_to
from ..runtime.torch_device import clear_torch_cache, resolve_selected_backend_device

log = logging.getLogger(__name__)

# Name the recognition and detection models carry in "not downloaded" errors.
FOUNDATION_CHECKPOINT_LABEL = "Surya OCR recognition"
DETECTION_CHECKPOINT_LABEL = "Surya OCR detection"

SURYA_TASK_OCR_WITH_BOXES = "ocr_with_boxes"
SURYA_TASK_OCR_WITHOUT_BOXES = "ocr_without_boxes"
SURYA_TASK_BLOCK_WITHOUT_BOXES = "block_without_boxes"
SURYA_ALLOWED_TASKS = {
    SURYA_TASK_OCR_WITH_BOXES,
    SURYA_TASK_OCR_WITHOUT_BOXES,
    SURYA_TASK_BLOCK_WITHOUT_BOXES,
}


#: `General.ai_device` resolution with this service's device policy (owner:
#: `runtime.torch_device`); patched by name in the test suite.
_resolve_selected_backend_device = resolve_selected_backend_device

#: Allocator-cache release with this service's policy (owner: `runtime.torch_device`);
#: patched by name in the test suite.
_clear_torch_cache = functools.partial(clear_torch_cache, owner="Surya OCR")


class SuryaOcrService:
    FOUNDATION_MODEL_KEY_PREFIX = "suryaocr:foundation"
    DETECTOR_MODEL_KEY_PREFIX = "suryaocr:detector"

    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.Lock()
        self._model_manager = model_manager
        self._foundation_predictor = None
        self._recognition_predictor = None
        self._detection_predictor = None
        self._device: str | None = None
        # Checkpoint directories the resident predictors were built from; part
        # of their identity next to `_device`.
        self._recognition_dir: str | None = None
        self._detection_dir: str | None = None
        self._last_error: str | None = None
        # Published copy of the health fields; `health()` never takes `_lock`,
        # which a predictor load holds (S-014).
        self._health = HealthSnapshot(ready=False, device=None, detector_ready=False, last_error=None)

    def health(self) -> dict[str, Any]:
        """Return `{"ready", "device", "detector_ready", "last_error"}` for the health snapshot.

        Reads only the published snapshot, so it answers while a predictor load
        holds the instance lock.
        """
        return self._health.read()

    def _publish_health_locked(self) -> None:
        """Republish the health fields from the guarded state; caller holds `self._lock`."""
        self._health.publish(
            ready=self._recognition_predictor is not None,
            device=self._device,
            detector_ready=self._detection_predictor is not None,
            last_error=self._last_error,
        )

    def warmup(
        self,
        *,
        recognition_dir: str,
        detection_dir: str,
        task_name: str = SURYA_TASK_OCR_WITHOUT_BOXES,
        recognize_math: bool = False,
    ) -> None:
        """Load the predictors for `task_name` by recognizing a blank image.

        The directories are those of `recognize_image_bytes`; raises what it raises.
        """
        from PIL import Image

        dummy = Image.new("RGB", (32, 32), (255, 255, 255))
        encoded = io.BytesIO()
        dummy.save(encoded, format="PNG")
        self.recognize_image_bytes(
            encoded.getvalue(),
            recognition_dir=recognition_dir,
            detection_dir=detection_dir,
            task_name=task_name,
            recognize_math=recognize_math,
        )

    def recognize_image_bytes(
        self,
        image_bytes: bytes,
        *,
        recognition_dir: str,
        detection_dir: str,
        join_newlines: bool = True,
        reflect_strings: bool = False,
        task_name: str = SURYA_TASK_OCR_WITHOUT_BOXES,
        recognize_math: bool = False,
        sort_lines: bool = False,
        drop_repeated_text: bool = False,
        max_sliding_window: int | None = None,
        max_tokens: int | None = None,
    ) -> dict[str, Any]:
        """Recognize `image_bytes` and return `{"lines", "text"}`.

        `recognition_dir` (foundation + recognition checkpoint) and
        `detection_dir` (line detector, read only for `ocr_with_boxes`) are
        absolute directories Rust downloaded. Raises `ValueError` /
        `FileNotFoundError` ("not downloaded") for a bad directory and
        `RuntimeError` for a missing package or a failed load.
        """
        normalized_task = _normalize_task_name(task_name)
        image = self._decode_image(image_bytes)
        selected_device = _resolve_selected_backend_device(self._device or "cpu")
        foundation_key = self._foundation_model_key(selected_device)
        detector_needed = normalized_task == SURYA_TASK_OCR_WITH_BOXES
        detector_key = self._detector_model_key(selected_device)

        foundation_lease = self._model_manager.begin_model_use(
            foundation_key,
            unload_callback=lambda: self._unload_foundation_key(foundation_key),
        )
        detector_lease = None
        if detector_needed:
            detector_lease = self._model_manager.begin_model_use(
                detector_key,
                unload_callback=lambda: self._unload_detector_key(detector_key),
            )

        try:
            with self._lock:
                recognition_predictor = self._ensure_recognition_loaded_locked(selected_device, recognition_dir)
            if foundation_lease.needs_load:
                foundation_lease.mark_loaded(
                    unload_callback=lambda: self._unload_foundation_key(foundation_key)
                )
            with self._lock:
                detection_predictor = (
                    self._ensure_detection_loaded_locked(selected_device, detection_dir)
                    if detector_needed
                    else None
                )
            if detector_lease is not None and detector_lease.needs_load:
                detector_lease.mark_loaded(
                    unload_callback=lambda: self._unload_detector_key(detector_key)
                )

            result = self._recognize_with_predictors(
                image=image,
                recognition_predictor=recognition_predictor,
                detection_predictor=detection_predictor,
                task_name=normalized_task,
                recognize_math=recognize_math,
                sort_lines=sort_lines,
                drop_repeated_text=drop_repeated_text,
                max_sliding_window=max_sliding_window,
                max_tokens=max_tokens,
            )
        except Exception:
            if foundation_lease.needs_load:
                foundation_lease.mark_load_failed()
            if detector_lease is not None and detector_lease.needs_load:
                detector_lease.mark_load_failed()
            raise
        finally:
            if detector_lease is not None:
                detector_lease.release()
            foundation_lease.release()

        lines = [
            _normalize_surya_text(
                str(getattr(line, "text", "")),
                join_newlines=join_newlines,
            ).strip()
            for line in getattr(result, "text_lines", [])
            if _normalize_surya_text(
                str(getattr(line, "text", "")),
                join_newlines=join_newlines,
            ).strip()
        ]
        if reflect_strings:
            lines.reverse()

        output_text = "\n".join(lines) if join_newlines else " ".join(lines)
        return {
            "lines": lines,
            "text": output_text.strip(),
        }

    def _ensure_recognition_loaded_locked(self, device: str, recognition_dir: str):
        """Return the recognition predictor for `device` and `recognition_dir`, loading it if needed.

        Caller must hold `self._lock`. Reloads from scratch when the previously
        loaded predictors live on another device or came from another directory.

        # Raises
        `ValueError` / `FileNotFoundError` ("not downloaded") for an invalid or
        incomplete `recognition_dir`, `RuntimeError` if the Surya package is
        missing or the load fails; in every case the service is left with no
        recognition predictors and `_last_error` set.
        """
        if (
            self._foundation_predictor is not None
            and self._recognition_predictor is not None
            and self._device == device
            and self._recognition_dir == recognition_dir
        ):
            return self._recognition_predictor

        if self._device is not None and (self._device != device or self._recognition_dir != recognition_dir):
            self._drop_all_predictors_locked()

        try:
            checkpoint = require_local_checkpoint(recognition_dir, label=FOUNDATION_CHECKPOINT_LABEL)
        except Exception as exc:
            self._last_error = str(exc)
            self._publish_health_locked()
            raise

        try:
            from surya.foundation import FoundationPredictor  # type: ignore
            from surya.recognition import RecognitionPredictor  # type: ignore
        except Exception as exc:
            self._last_error = f"Surya OCR package is not available: {exc}"
            self._publish_health_locked()
            raise RuntimeError(self._last_error) from exc

        log.info("Surya OCR recognition predictor load start device=%s checkpoint=%s", device, checkpoint)
        started_at = time.perf_counter()
        try:
            # The settings AND the explicit `checkpoint=`: the loader reads the
            # argument, and the settings cover any library code that reads its
            # default instead (an `s3://` default would be downloaded).
            point_settings_at(recognition_dir=checkpoint)
            # Surya loads the checkpoint and moves it to the GPU inside its own
            # loader (`surya/foundation/loader.py`: `from_pretrained(...).to(dev)`),
            # so there is no CPU-resident module we could hand to
            # `move_module_to` — the only seam is the temporary `nn.Module.to`
            # patch. It also covers `PreTrainedModel.to`, because that override
            # ends in `super().to(...)` and `super()` resolves the (already
            # patched) `nn.Module.to` class attribute at call time.
            # Why it is needed: on a ROCm build a host->device copy whose source
            # is the writable private file mapping handed out by safetensors
            # stalls ~1-2 s per tensor of >=1 MiB (measured here: 271.06 s ->
            # 0.28 s for the 1372 MiB foundation checkpoint).
            # Constructing on CPU and moving the module ourselves is not an
            # option: the loader makes device-dependent decisions while still on
            # CPU (attention implementation) and keeps private device-resident
            # token tensors that a later move would leave behind.
            # The patch is process-global, so it is held around loading only.
            with patched_module_to():
                foundation_predictor = FoundationPredictor(checkpoint=checkpoint, device=device)
                recognition_predictor = RecognitionPredictor(foundation_predictor)
        except Exception as exc:
            self._foundation_predictor = None
            self._recognition_predictor = None
            self._device = None
            self._last_error = f"Surya OCR init failed: {exc}"
            self._publish_health_locked()
            log.error(
                "Surya OCR recognition predictor load failed device=%s duration_s=%.2f error=%s",
                device,
                time.perf_counter() - started_at,
                exc,
            )
            raise RuntimeError(self._last_error) from exc

        log.info(
            "Surya OCR recognition predictor load done device=%s duration_s=%.2f",
            device,
            time.perf_counter() - started_at,
        )
        self._foundation_predictor = foundation_predictor
        self._recognition_predictor = recognition_predictor
        self._device = device
        self._recognition_dir = checkpoint
        self._last_error = None
        self._publish_health_locked()
        return recognition_predictor

    def _ensure_detection_loaded_locked(self, device: str, detection_dir: str):
        """Return the `ocr_with_boxes` line detector, loading it if needed.

        Caller must hold `self._lock`. The detector shares `self._device` with
        the recognition predictors, so it is only reused when both agree and it
        came from `detection_dir`.

        # Raises
        `ValueError` / `FileNotFoundError` ("not downloaded") for an invalid or
        incomplete `detection_dir`, `RuntimeError` if the Surya detection package
        is missing or the load fails; `_last_error` is set and no detector is
        retained.
        """
        if (
            self._detection_predictor is not None
            and self._device == device
            and self._detection_dir == detection_dir
        ):
            return self._detection_predictor
        self._detection_predictor = None
        self._detection_dir = None

        try:
            checkpoint = require_local_checkpoint(detection_dir, label=DETECTION_CHECKPOINT_LABEL)
        except Exception as exc:
            self._last_error = str(exc)
            self._publish_health_locked()
            raise

        try:
            from surya.detection import DetectionPredictor  # type: ignore
        except Exception as exc:
            self._last_error = f"Surya detection package is not available: {exc}"
            self._publish_health_locked()
            raise RuntimeError(self._last_error) from exc

        log.info("Surya OCR detection predictor load start device=%s checkpoint=%s", device, checkpoint)
        started_at = time.perf_counter()
        try:
            point_settings_at(detection_dir=checkpoint)
            # Same ROCm mmap->GPU stall as in `_ensure_recognition_loaded_locked`:
            # `surya/detection/loader.py` also moves the checkpoint to the device
            # itself, so the transfer is only reachable through the patch.
            with patched_module_to():
                self._detection_predictor = DetectionPredictor(checkpoint=checkpoint, device=device)
        except Exception as exc:
            self._detection_predictor = None
            self._last_error = f"Surya detection init failed: {exc}"
            self._publish_health_locked()
            log.error(
                "Surya OCR detection predictor load failed device=%s duration_s=%.2f error=%s",
                device,
                time.perf_counter() - started_at,
                exc,
            )
            raise RuntimeError(self._last_error) from exc

        log.info(
            "Surya OCR detection predictor load done device=%s duration_s=%.2f",
            device,
            time.perf_counter() - started_at,
        )
        self._detection_dir = checkpoint
        self._last_error = None
        self._publish_health_locked()
        return self._detection_predictor

    def _recognize_with_predictors(
        self,
        *,
        image,
        recognition_predictor,
        detection_predictor,
        task_name: str,
        recognize_math: bool,
        sort_lines: bool,
        drop_repeated_text: bool,
        max_sliding_window: int | None,
        max_tokens: int | None,
    ):
        image_w, image_h = image.size
        kwargs: dict[str, Any] = {
            "task_names": [task_name],
            "sort_lines": sort_lines,
            "math_mode": recognize_math,
            "drop_repeated_text": drop_repeated_text,
            "max_sliding_window": max_sliding_window,
            "max_tokens": max_tokens,
        }
        if task_name == SURYA_TASK_OCR_WITH_BOXES:
            kwargs["det_predictor"] = detection_predictor
            kwargs["highres_images"] = [image]
        else:
            kwargs["bboxes"] = [[[0, 0, image_w, image_h]]]

        predictions = recognition_predictor([image], **kwargs)
        if not predictions:
            raise RuntimeError("Surya OCR returned no predictions.")
        return predictions[0]

    @staticmethod
    def _decode_image(image_bytes: bytes):
        from PIL import Image

        with Image.open(io.BytesIO(image_bytes)) as img:
            rgb = img.convert("RGB")
            width, height = rgb.size
            if width >= 2 and height >= 2:
                return rgb

            resampling = getattr(getattr(Image, "Resampling", Image), "NEAREST")
            target_size = (max(2, width), max(2, height))
            return rgb.resize(target_size, resample=resampling)

    def _unload_foundation_key(self, model_key: str) -> bool:
        """Drop every predictor when `model_key` names the resident foundation.

        Returns `False` when another key is resident, which is what tells
        `LoadedModelManager` the eviction did not happen.
        """
        with self._lock:
            current_device = self._device
            if current_device is None or model_key != self._foundation_model_key(current_device):
                return False
            detector_key = self._detector_model_key(current_device)
            self._drop_all_predictors_locked()
            _clear_torch_cache()
            self._model_manager.mark_unloaded(model_key)
            self._model_manager.mark_unloaded(detector_key)
            return True

    def _unload_detector_key(self, model_key: str) -> bool:
        """Drop only the `ocr_with_boxes` detector; `False` if it is not resident."""
        with self._lock:
            current_device = self._device
            if current_device is None or model_key != self._detector_model_key(current_device):
                return False
            if self._detection_predictor is None:
                return False
            self._detection_predictor = None
            self._detection_dir = None
            self._publish_health_locked()
            _clear_torch_cache()
            self._model_manager.mark_unloaded(model_key)
            return True

    def _drop_all_predictors_locked(self) -> None:
        """Forget every predictor and the device; caller holds `self._lock`."""
        self._foundation_predictor = None
        self._recognition_predictor = None
        self._detection_predictor = None
        self._device = None
        self._recognition_dir = None
        self._detection_dir = None
        self._publish_health_locked()

    @classmethod
    def _foundation_model_key(cls, device: str) -> str:
        return f"{cls.FOUNDATION_MODEL_KEY_PREFIX}:{device}"

    @classmethod
    def _detector_model_key(cls, device: str) -> str:
        return f"{cls.DETECTOR_MODEL_KEY_PREFIX}:{device}"


def _normalize_task_name(raw: str) -> str:
    normalized = str(raw or "").strip().lower()
    if normalized not in SURYA_ALLOWED_TASKS:
        return SURYA_TASK_OCR_WITHOUT_BOXES
    return normalized


def _normalize_surya_text(raw: str, *, join_newlines: bool) -> str:
    text = str(raw or "")
    replacement = "\n" if join_newlines else " "
    return text.replace("<br>", replacement)
