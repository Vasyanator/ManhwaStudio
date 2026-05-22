"""
FILE OVERVIEW: modules/ai_backend/paddle_ocr_service.py
OCR service for PaddleOCR-compatible ONNX models without Paddle runtime.

Main responsibilities:
- Expose lazy health/warmup/recognition API used by `modules/ai_backend/server.py`.
- Resolve selected ONNX provider/device from backend settings.
- Run PP-OCR detector + recognizer through shared ONNX runtime helpers.

Notes:
- The request field `paddle_lang` is kept for backward compatibility, but it now
  carries a PaddleOCR model key like `korean_v5`.
- Models are read from `ManhwaStudio_AI_Models/ONNX/PaddleOCR`.
"""

from __future__ import annotations

import threading
from typing import Any

import cv2
import numpy as np

try:
    from config import UserConfig
except Exception:
    UserConfig = None

from .paddle_onnx_runtime import (
    DEFAULT_REC_MODEL_KEY,
    PaddleOnnxRuntime,
    RuntimeFactory,
    normalize_model_key,
    resolve_model_paths,
    resolve_models_root,
    resolve_provider_settings,
)


class PaddleOcrService:
    def __init__(self, runtime_factory: RuntimeFactory) -> None:
        self._lock = threading.Lock()
        self._runtime = PaddleOnnxRuntime(runtime_factory)
        self._model_key: str | None = None
        self._provider: str | None = None
        self._device_id: str | None = None
        self._last_error: str | None = None

    def health(self) -> dict[str, Any]:
        with self._lock:
            model_key = self._model_key or DEFAULT_REC_MODEL_KEY
            provider = self._provider or "CPUExecutionProvider"
            device_id = self._device_id or "0"
            try:
                model_paths = resolve_model_paths(model_key)
                model_exists = model_paths.rec_model_path.is_file()
            except Exception:
                model_exists = False
            return {
                "ready": self._model_key is not None and self._last_error is None,
                "model": model_key,
                "provider": provider,
                "device_id": device_id,
                "models_root": str(resolve_models_root()),
                "model_exists": model_exists,
                "last_error": self._last_error,
            }

    def warmup(self, *, lang: str = DEFAULT_REC_MODEL_KEY, device: str | None = None) -> None:
        dummy = np.zeros((16, 16, 3), dtype="uint8")
        ok, encoded = cv2.imencode(".png", dummy)
        if not ok:
            raise RuntimeError("Не удалось закодировать warmup PNG для PaddleOCR ONNX.")
        self.recognize_image_bytes(encoded.tobytes(), lang=lang, device=device)

    def recognize_image_bytes(
        self,
        image_bytes: bytes,
        *,
        join_newlines: bool = True,
        reflect_strings: bool = False,
        lang: str = DEFAULT_REC_MODEL_KEY,
        device: str | None = None,
    ) -> dict[str, Any]:
        image_bgr = self._decode_image_bgr(image_bytes)
        model_key = normalize_model_key(lang)
        provider_settings = resolve_provider_settings(UserConfig, device)

        with self._lock:
            try:
                result = self._runtime.recognize(image_bgr, model_key, provider_settings)
                lines = [
                    line["text"]
                    for line in result.get("lines", [])
                    if isinstance(line, dict) and isinstance(line.get("text"), str)
                ]
                if reflect_strings:
                    lines.reverse()
                output_text = "\n".join(lines) if join_newlines else " ".join(lines)

                self._model_key = model_key
                self._provider = provider_settings.provider
                self._device_id = provider_settings.device_id
                self._last_error = None
                return {
                    "lines": lines,
                    "text": output_text.strip(),
                }
            except Exception as exc:
                self._last_error = str(exc)
                raise

    @staticmethod
    def _decode_image_bgr(image_bytes: bytes) -> np.ndarray:
        encoded = np.frombuffer(image_bytes, dtype=np.uint8)
        bgr = cv2.imdecode(encoded, cv2.IMREAD_COLOR)
        if bgr is None:
            raise RuntimeError("PaddleOCR ONNX: cv2.imdecode returned None.")
        return bgr
