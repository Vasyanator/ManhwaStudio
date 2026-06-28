"""
FILE OVERVIEW: modules/ai_backend/paddle_text_detector_service.py
Text detector service backed by PaddleOCR ONNX detection model.

Main responsibilities:
- Run PP-OCRv5 detection through ONNX Runtime without Paddle dependencies.
- Return polygon blocks and a glyph-shaped binary mask compatible with the
  existing `/textdetector/paddle/detect` endpoint.
- Read detector weights from `ManhwaStudio_AI_Models/ONNX/PaddleOCR`.
"""

from __future__ import annotations

import threading
from pathlib import Path
from typing import Any

import cv2
import numpy as np

try:
    from config import UserConfig
except Exception:
    UserConfig = None

from .paddle_onnx_runtime import (
    PaddleOnnxRuntime,
    RuntimeFactory,
    resolve_det_model_path,
    resolve_provider_settings,
)


def _extract_text_mask_in_roi(roi_bgr: np.ndarray, poly_mask: np.ndarray) -> np.ndarray:
    h, w = roi_bgr.shape[:2]
    if h == 0 or w == 0:
        return np.zeros((h, w), dtype=np.uint8)

    hsv = cv2.cvtColor(roi_bgr, cv2.COLOR_BGR2HSV)
    sat = hsv[:, :, 1]
    mean_sat = float(cv2.mean(sat, mask=poly_mask)[0])
    if mean_sat > 20:
        _, sat_bin = cv2.threshold(sat, 0, 255, cv2.THRESH_BINARY + cv2.THRESH_OTSU)
        fill = float(cv2.countNonZero(cv2.bitwise_and(sat_bin, poly_mask))) / (h * w)
        if 0.01 < fill < 0.85:
            return cv2.bitwise_and(sat_bin, poly_mask)

    gray = cv2.cvtColor(roi_bgr, cv2.COLOR_BGR2GRAY)
    _, dark_bin = cv2.threshold(gray, 0, 255, cv2.THRESH_BINARY_INV + cv2.THRESH_OTSU)
    dark_bin = cv2.bitwise_and(dark_bin, poly_mask)
    fill_dark = float(cv2.countNonZero(dark_bin)) / (h * w)
    if 0.01 < fill_dark < 0.85:
        return dark_bin

    _, light_bin = cv2.threshold(gray, 0, 255, cv2.THRESH_BINARY + cv2.THRESH_OTSU)
    return cv2.bitwise_and(light_bin, poly_mask)


def _build_glyph_mask(source_bgr: np.ndarray, polys: list[np.ndarray]) -> np.ndarray:
    img_h, img_w = source_bgr.shape[:2]
    mask = np.zeros((img_h, img_w), dtype=np.uint8)
    kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (3, 3))

    for poly in polys:
        pts = np.array(poly, dtype=np.int32)
        x, y, bw, bh = cv2.boundingRect(pts)
        x1 = max(0, x)
        y1 = max(0, y)
        x2 = min(img_w, x + bw)
        y2 = min(img_h, y + bh)
        if x2 <= x1 or y2 <= y1:
            continue

        roi = source_bgr[y1:y2, x1:x2]
        roi_poly_mask = np.zeros((y2 - y1, x2 - x1), dtype=np.uint8)
        shifted = pts.copy()
        shifted[:, 0] -= x1
        shifted[:, 1] -= y1
        cv2.fillPoly(roi_poly_mask, [shifted], 255)

        roi_text_mask = _extract_text_mask_in_roi(roi, roi_poly_mask)
        roi_text_mask = cv2.morphologyEx(roi_text_mask, cv2.MORPH_CLOSE, kernel)
        mask[y1:y2, x1:x2] = cv2.bitwise_or(mask[y1:y2, x1:x2], roi_text_mask)

    return mask


class PaddleTextDetectorService:
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

    def detect_page(self, page_path: str) -> dict[str, Any]:
        with self._lock:
            try:
                raw = Path(page_path).read_bytes()
                result = self._detect_from_encoded_bytes(raw)
                self._last_error = None
                return result
            except Exception as exc:
                self._last_error = str(exc)
                raise

    def detect_image_bytes(self, image_bytes: bytes) -> dict[str, Any]:
        with self._lock:
            try:
                result = self._detect_from_encoded_bytes(image_bytes)
                self._last_error = None
                return result
            except Exception as exc:
                self._last_error = str(exc)
                raise

    def _detect_from_encoded_bytes(self, raw_bytes: bytes) -> dict[str, Any]:
        arr = np.frombuffer(raw_bytes, dtype=np.uint8)
        image = cv2.imdecode(arr, cv2.IMREAD_COLOR)
        if image is None:
            raise ValueError("Не удалось декодировать изображение.")

        provider_settings = resolve_provider_settings(UserConfig)
        result = self._runtime.detect(image, provider_settings)
        boxes = result["boxes"]
        polys = [box.astype(float).tolist() for box in boxes]
        glyph_mask = _build_glyph_mask(image, boxes)

        self._provider = provider_settings.provider
        self._device_id = provider_settings.device_id

        img_h, img_w = image.shape[:2]
        return {
            "source_size": [img_w, img_h],
            "blocks": self._collect_blocks(boxes, img_w, img_h),
            "mask_png": self._encode_mask_png_bytes(glyph_mask),
            "polys": [
                {
                    "points": poly,
                    "score": float(score),
                }
                for poly, score in zip(polys, result["scores"])
            ],
        }

    @staticmethod
    def _collect_blocks(polys: list[np.ndarray], img_w: int, img_h: int) -> list[dict[str, int]]:
        blocks: list[dict[str, int]] = []
        for poly in polys:
            poly_np = np.asarray(poly, dtype=np.float32)
            if poly_np.shape != (4, 2):
                continue
            min_x = int(np.clip(np.floor(poly_np[:, 0].min()), 0, img_w))
            min_y = int(np.clip(np.floor(poly_np[:, 1].min()), 0, img_h))
            max_x = int(np.clip(np.ceil(poly_np[:, 0].max()), 0, img_w))
            max_y = int(np.clip(np.ceil(poly_np[:, 1].max()), 0, img_h))
            if max_x <= min_x or max_y <= min_y:
                continue
            blocks.append({"x1": min_x, "y1": min_y, "x2": max_x, "y2": max_y})
        return blocks

    @staticmethod
    def _encode_mask_png_bytes(mask: np.ndarray) -> bytes:
        ok, encoded = cv2.imencode(".png", mask)
        if not ok:
            raise RuntimeError("Не удалось закодировать mask PNG.")
        return encoded.tobytes()
