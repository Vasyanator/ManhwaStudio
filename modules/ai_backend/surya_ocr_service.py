"""
FILE OVERVIEW: modules/ai_backend/surya_ocr_service.py
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
"""

from __future__ import annotations

import gc
import io
import threading
from typing import Any

try:
    from ai_device import AIDevice
except Exception:
    from modules.ai_device import AIDevice

try:
    from config import UserConfig
except Exception:
    UserConfig = None

from .model_manager import LoadedModelManager

SURYA_TASK_OCR_WITH_BOXES = "ocr_with_boxes"
SURYA_TASK_OCR_WITHOUT_BOXES = "ocr_without_boxes"
SURYA_TASK_BLOCK_WITHOUT_BOXES = "block_without_boxes"
SURYA_ALLOWED_TASKS = {
    SURYA_TASK_OCR_WITH_BOXES,
    SURYA_TASK_OCR_WITHOUT_BOXES,
    SURYA_TASK_BLOCK_WITHOUT_BOXES,
}


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
        self._last_error: str | None = None

    def health(self) -> dict[str, Any]:
        with self._lock:
            return {
                "ready": self._recognition_predictor is not None,
                "device": self._device,
                "detector_ready": self._detection_predictor is not None,
                "last_error": self._last_error,
            }

    def warmup(
        self,
        *,
        task_name: str = SURYA_TASK_OCR_WITHOUT_BOXES,
        recognize_math: bool = False,
    ) -> None:
        from PIL import Image

        dummy = Image.new("RGB", (32, 32), (255, 255, 255))
        encoded = io.BytesIO()
        dummy.save(encoded, format="PNG")
        self.recognize_image_bytes(
            encoded.getvalue(),
            task_name=task_name,
            recognize_math=recognize_math,
        )

    def recognize_image_bytes(
        self,
        image_bytes: bytes,
        *,
        join_newlines: bool = True,
        reflect_strings: bool = False,
        task_name: str = SURYA_TASK_OCR_WITHOUT_BOXES,
        recognize_math: bool = False,
        sort_lines: bool = False,
        drop_repeated_text: bool = False,
        max_sliding_window: int | None = None,
        max_tokens: int | None = None,
    ) -> dict[str, Any]:
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
                recognition_predictor = self._ensure_recognition_loaded_locked(selected_device)
            if foundation_lease.needs_load:
                foundation_lease.mark_loaded(
                    unload_callback=lambda: self._unload_foundation_key(foundation_key)
                )
            with self._lock:
                detection_predictor = (
                    self._ensure_detection_loaded_locked(selected_device)
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

    def _ensure_recognition_loaded_locked(self, device: str):
        if (
            self._foundation_predictor is not None
            and self._recognition_predictor is not None
            and self._device == device
        ):
            return self._recognition_predictor

        if self._device is not None and self._device != device:
            self._drop_all_predictors_locked()

        try:
            from surya.foundation import FoundationPredictor  # type: ignore
            from surya.recognition import RecognitionPredictor  # type: ignore
        except Exception as exc:
            self._last_error = f"Surya OCR package is not available: {exc}"
            raise RuntimeError(self._last_error) from exc

        try:
            foundation_predictor = FoundationPredictor(device=device)
            recognition_predictor = RecognitionPredictor(foundation_predictor)
        except Exception as exc:
            self._foundation_predictor = None
            self._recognition_predictor = None
            self._device = None
            self._last_error = f"Surya OCR init failed: {exc}"
            raise RuntimeError(self._last_error) from exc

        self._foundation_predictor = foundation_predictor
        self._recognition_predictor = recognition_predictor
        self._device = device
        self._last_error = None
        return recognition_predictor

    def _ensure_detection_loaded_locked(self, device: str):
        if self._detection_predictor is not None and self._device == device:
            return self._detection_predictor

        try:
            from surya.detection import DetectionPredictor  # type: ignore
        except Exception as exc:
            self._last_error = f"Surya detection package is not available: {exc}"
            raise RuntimeError(self._last_error) from exc

        try:
            self._detection_predictor = DetectionPredictor(device=device)
        except Exception as exc:
            self._detection_predictor = None
            self._last_error = f"Surya detection init failed: {exc}"
            raise RuntimeError(self._last_error) from exc

        self._last_error = None
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
        with self._lock:
            current_device = self._device
            if current_device is None or model_key != self._detector_model_key(current_device):
                return False
            if self._detection_predictor is None:
                return False
            self._detection_predictor = None
            _clear_torch_cache()
            self._model_manager.mark_unloaded(model_key)
            return True

    def _drop_all_predictors_locked(self) -> None:
        self._foundation_predictor = None
        self._recognition_predictor = None
        self._detection_predictor = None
        self._device = None

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
