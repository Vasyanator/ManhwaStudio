"""
File: modules/ai_backend/ctd_text_detector_service.py

Purpose:
Comic Text Detector service adapter for the Python AI backend.

Main responsibilities:
- load CTD models lazily;
- synchronize model device with backend AI device settings;
- run detection and return masks/blocks to Rust.
"""

from __future__ import annotations

import gc
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

from .model_manager import LoadedModelManager

MODEL_FILENAME = "comictextdetector.pt"

# ============================================================================
# CTD TEXT DETECTOR SERVICE
# ----------------------------------------------------------------------------
# Что в файле:
# - lazy init и health для CTD-детектора.
# - запуск детекции страницы с постобработкой bbox и mask.
# - нормализация runtime-параметров детектора.
# - синхронизация `device` с backend-настройкой `General.ai_device`.
# ============================================================================


def _to_int(value: Any, default: int) -> int:
    try:
        return int(value)
    except Exception:
        return default


def _to_float(value: Any, default: float) -> float:
    try:
        return float(value)
    except Exception:
        return default


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
    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._detector = None
        self._detector_model_key: str | None = None
        self._cv2 = None
        self._last_error: str | None = None
        self._model_path = Path(TEXT_DETECTOR_DIR) / MODEL_FILENAME
        start_device = _resolve_selected_backend_device(_default_device())
        self._active_params = {
            "device": start_device,
            "detect_size": 1280,
            "det_rearrange_max_batches": 4,
            "font size multiplier": 1.0,
            "font size max": -1.0,
            "font size min": -1.0,
            "mask dilate size": 2,
        }

    def health(self) -> dict[str, Any]:
        with self._lock:
            return {
                "ready": self._detector is not None,
                "model": "ctd",
                "model_path": str(self._model_path),
                "model_exists": self._model_path.exists(),
                "params": dict(self._active_params),
                "last_error": self._last_error,
            }

    def detect_page(
        self, page_path: str, *, params: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        normalized = self._normalize_params(params)
        model_key = self._model_key_for(normalized)
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
        )
        with self._lock:
            try:
                detector = self._ensure_detector_locked(normalized)
                # cv2.imread на Windows не поддерживает не-ASCII пути (кириллица и т.д.),
                # поэтому читаем байты через pathlib и декодируем через imdecode.
                raw = Path(page_path).read_bytes()
                payload = self._detect_from_encoded_image_bytes(raw, detector, normalized)
                if lease.needs_load:
                    lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
                self._last_error = None
                return payload
            except Exception as exc:
                if lease.needs_load:
                    lease.mark_load_failed()
                self._last_error = str(exc)
                raise
            finally:
                lease.release()

    def detect_image_bytes(
        self, image_bytes: bytes, *, params: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        normalized = self._normalize_params(params)
        model_key = self._model_key_for(normalized)
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
        )
        with self._lock:
            try:
                detector = self._ensure_detector_locked(normalized)
                payload = self._detect_from_encoded_image_bytes(
                    image_bytes, detector, normalized
                )
                if lease.needs_load:
                    lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
                self._last_error = None
                return payload
            except Exception as exc:
                if lease.needs_load:
                    lease.mark_load_failed()
                self._last_error = str(exc)
                raise
            finally:
                lease.release()

    def _normalize_params(self, params: dict[str, Any] | None) -> dict[str, Any]:
        merged = dict(self._active_params)
        if isinstance(params, dict):
            merged.update(params)
        merged["device"] = _resolve_selected_backend_device(self._active_params["device"])
        merged["detect_size"] = _to_int(merged.get("detect_size"), 1280)
        merged["detect_size"] = max(896, min(2048, merged["detect_size"]))
        merged["det_rearrange_max_batches"] = _to_int(
            merged.get("det_rearrange_max_batches"), 4
        )
        merged["det_rearrange_max_batches"] = max(
            1, min(64, merged["det_rearrange_max_batches"])
        )
        merged["font size multiplier"] = _to_float(
            merged.get("font size multiplier"), 1.0
        )
        merged["font size multiplier"] = max(
            0.1, min(8.0, merged["font size multiplier"])
        )
        merged["font size max"] = _to_float(merged.get("font size max"), -1.0)
        merged["font size max"] = max(-1.0, min(500.0, merged["font size max"]))
        merged["font size min"] = _to_float(merged.get("font size min"), -1.0)
        merged["font size min"] = max(-1.0, min(500.0, merged["font size min"]))
        merged["mask dilate size"] = _to_int(merged.get("mask dilate size"), 2)
        merged["mask dilate size"] = max(0, min(30, merged["mask dilate size"]))
        return merged

    def _ensure_cv2_locked(self):
        if self._cv2 is None:
            import cv2  # type: ignore

            self._cv2 = cv2
        return self._cv2

    def _ensure_detector_locked(self, params: dict[str, Any]):
        if not self._model_path.exists():
            raise FileNotFoundError(f"CTD model not found: {self._model_path}")

        requested_key = self._model_key_for(params)
        previous_key = self._detector_model_key

        if self._detector is None:
            from .textdetector.ctd import CTDModel  # heavy import; keep lazy

            self._detector = CTDModel(
                str(self._model_path),
                detect_size=int(params["detect_size"]),
                device=str(params["device"]),
                det_rearrange_max_batches=int(params["det_rearrange_max_batches"]),
            )
            self._detector_model_key = requested_key
            self._active_params = dict(params)
            return self._detector

        if previous_key != requested_key:
            self._detector = None
            self._detector_model_key = None
            _clear_torch_cache()
            if previous_key is not None:
                self._model_manager.mark_unloaded(previous_key)
            from .textdetector.ctd import CTDModel  # heavy import; keep lazy

            self._detector = CTDModel(
                str(self._model_path),
                detect_size=int(params["detect_size"]),
                device=str(params["device"]),
                det_rearrange_max_batches=int(params["det_rearrange_max_batches"]),
            )
            self._detector_model_key = requested_key
            self._active_params = dict(params)
            return self._detector

        requested_device = str(params["device"])
        current_device = str(getattr(self._detector, "device", "")).strip().lower()
        if current_device != requested_device:
            set_device = getattr(self._detector, "set_device", None)
            if callable(set_device):
                set_device(requested_device)
            else:
                self._detector.device = requested_device

        self._detector.detect_size = int(params["detect_size"])
        if hasattr(self._detector, "det_rearrange_max_batches"):
            self._detector.det_rearrange_max_batches = int(
                params["det_rearrange_max_batches"]
            )

        self._active_params = dict(params)
        return self._detector

    def _unload_key(self, model_key: str) -> bool:
        with self._lock:
            if self._detector is None or self._detector_model_key != model_key:
                return False
            self._detector = None
            self._detector_model_key = None
            _clear_torch_cache()
            self._model_manager.mark_unloaded(model_key)
            return True

    @staticmethod
    def _model_key_for(params: dict[str, Any]) -> str:
        device = str(params.get("device", "cpu")).strip().lower() or "cpu"
        return f"ctd:{device}"

    def _apply_font_params(self, blocks, params: dict[str, Any]) -> None:
        if not isinstance(blocks, list):
            return
        mul = float(params.get("font size multiplier", 1.0))
        fmax = float(params.get("font size max", -1.0))
        fmin = float(params.get("font size min", -1.0))
        for block in blocks:
            base = getattr(block, "_detected_font_size", -1.0)
            if base is None:
                base = -1.0
            try:
                size = float(base) * mul
            except Exception:
                continue
            if fmax > 0:
                size = min(fmax, size)
            if fmin > 0:
                size = max(fmin, size)
            try:
                block.font_size = size
            except Exception:
                pass
            try:
                block._detected_font_size = size
            except Exception:
                pass

    def _collect_blocks(self, blocks, width: int, height: int) -> list[dict[str, float]]:
        if not isinstance(blocks, list):
            return []

        out: list[dict[str, float]] = []
        for block in blocks:
            xyxy = getattr(block, "xyxy", None)
            if not isinstance(xyxy, (list, tuple)) or len(xyxy) < 4:
                continue
            try:
                x1 = float(xyxy[0])
                y1 = float(xyxy[1])
                x2 = float(xyxy[2])
                y2 = float(xyxy[3])
            except Exception:
                continue

            x1 = max(0.0, min(float(width), x1))
            y1 = max(0.0, min(float(height), y1))
            x2 = max(0.0, min(float(width), x2))
            y2 = max(0.0, min(float(height), y2))
            if x2 <= x1 or y2 <= y1:
                continue

            out.append({"x1": x1, "y1": y1, "x2": x2, "y2": y2})

        out.sort(key=lambda item: (item["y1"], item["x1"], item["y2"], item["x2"]))
        if len(out) > 2500:
            out = out[:2500]
        return out

    def _detect_from_encoded_image_bytes(self, raw_bytes: bytes, detector, params: dict[str, Any]):
        cv2 = self._ensure_cv2_locked()
        raw = np.frombuffer(raw_bytes, dtype=np.uint8)
        image = cv2.imdecode(raw, cv2.IMREAD_COLOR)
        if image is None:
            raise FileNotFoundError("Не удалось открыть изображение.")

        _, mask_refined, blocks = detector(image)
        self._apply_font_params(blocks, params)
        mask_refined = self._apply_mask_dilate(mask_refined, params)

        h, w = image.shape[:2]
        return {
            "source_size": [int(w), int(h)],
            "blocks": self._collect_blocks(blocks, int(w), int(h)),
            "mask_png": self._encode_mask_png_bytes(mask_refined),
        }

    def _apply_mask_dilate(self, mask, params: dict[str, Any]):
        cv2 = self._ensure_cv2_locked()
        if mask is None:
            return None
        try:
            mask = cv2.convertScaleAbs(mask)
        except Exception:
            return None

        ksize = _to_int(params.get("mask dilate size"), 2)
        ksize = max(0, min(30, ksize))
        if ksize <= 0:
            return mask
        element = cv2.getStructuringElement(
            cv2.MORPH_ELLIPSE,
            (2 * ksize + 1, 2 * ksize + 1),
            (ksize, ksize),
        )
        return cv2.dilate(mask, element)

    def _encode_mask_png_bytes(self, mask) -> bytes:
        if mask is None:
            return b""
        cv2 = self._ensure_cv2_locked()
        try:
            binary = cv2.convertScaleAbs(mask)
            _, binary = cv2.threshold(binary, 30, 255, cv2.THRESH_BINARY)
            ok, encoded = cv2.imencode(".png", binary)
            if not ok:
                return b""
            return encoded.tobytes()
        except Exception:
            return b""


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
