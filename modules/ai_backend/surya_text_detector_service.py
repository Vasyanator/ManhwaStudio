"""
FILE OVERVIEW: modules/ai_backend/surya_text_detector_service.py
Low-level Surya text detector service.

Main responsibilities:
- lazy init and health reporting for Surya detection-only predictor;
- explicit checkpoint presence check and auto-download for the detector model;
- low-level heatmap-based text detection without OCR wrappers;
- return line blocks and a binary mask derived from Surya detector heatmaps;
- synchronize model device with backend `General.ai_device`;
- cooperate with `LoadedModelManager` for bounded resident model count.
"""

from __future__ import annotations

import base64
import gc
import io
import logging
import threading
from pathlib import Path
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

from .model_manager import LoadedModelManager

log = logging.getLogger(__name__)


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
            model_dir = _resolve_checkpoint_local_dir(checkpoint)
            model_exists = bool(model_dir) and _check_checkpoint_ready(model_dir)
            return {
                "ready": self._predictor is not None,
                "device": self._device,
                "model": "surya_detection",
                "checkpoint": checkpoint,
                "model_dir": model_dir,
                "model_exists": model_exists,
                "last_error": self._last_error,
            }

    def detect_page(self, page_path: str) -> dict[str, Any]:
        log.info("Surya detect_page start path=%s", page_path)
        raw = Path(page_path).read_bytes()
        return self.detect_image_bytes(raw)

    def detect_image_bytes(self, image_bytes: bytes) -> dict[str, Any]:
        selected_device = _resolve_selected_backend_device(self._device or "cpu")
        model_key = self._model_key(selected_device)
        checkpoint = self._checkpoint_name()
        model_dir = _resolve_checkpoint_local_dir(checkpoint)
        log.info(
            "Surya detect_image_bytes start bytes=%s device=%s model_key=%s checkpoint=%s model_dir=%s",
            len(image_bytes),
            selected_device,
            model_key,
            checkpoint,
            model_dir,
        )
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
        )
        try:
            with self._lock:
                predictor = self._ensure_predictor_locked(selected_device)
            payload = self._detect_with_predictor(image_bytes, predictor)
            log.info(
                "Surya detect_image_bytes done device=%s blocks=%s lines=%s mask_b64_len=%s",
                selected_device,
                len(payload.get("blocks", [])),
                len(payload.get("lines", [])),
                len(str(payload.get("mask_png_base64", ""))),
            )
            if lease.needs_load:
                lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
            self._last_error = None
            return payload
        except Exception as exc:
            log.exception("Surya detect_image_bytes failed device=%s error=%s", selected_device, exc)
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
        checkpoint = self._checkpoint_name()
        local_dir = _resolve_checkpoint_local_dir(checkpoint)
        if local_dir and _check_checkpoint_ready(local_dir):
            return
        if not checkpoint.startswith("s3://"):
            raise FileNotFoundError(f"Surya detector checkpoint not found: {checkpoint}")

        try:
            from surya.common.s3 import download_directory  # type: ignore
        except Exception as exc:
            raise RuntimeError(
                f"Surya detector download helpers are not available: {exc}"
            ) from exc

        remote_dir = checkpoint.removeprefix("s3://")
        if not local_dir:
            raise RuntimeError(
                f"Surya detector checkpoint path is invalid for download: {checkpoint}"
            )
        download_directory(remote_dir, local_dir)
        if not _check_checkpoint_ready(local_dir):
            raise RuntimeError(
                f"Surya detector checkpoint download finished but manifest is incomplete: {local_dir}"
            )

    def _detect_with_predictor(self, image_bytes: bytes, predictor) -> dict[str, Any]:
        from surya.common.util import clean_boxes  # type: ignore
        from surya.common.polygon import PolygonBox  # type: ignore
        from surya.settings import settings  # type: ignore

        cv2 = self._ensure_cv2()
        image = self._decode_image(image_bytes)
        image_rgb = image.convert("RGB")
        image_w, image_h = image_rgb.size
        image_np = np.asarray(image_rgb, dtype=np.uint8)
        log.info("Surya predictor input image_size=%sx%s", image_w, image_h)
        log.info(
            "Surya predictor input pixels min=%s max=%s mean=%.3f std=%.3f",
            int(np.min(image_np)),
            int(np.max(image_np)),
            float(np.mean(image_np)),
            float(np.std(image_np)),
        )

        detection_batches = list(
            predictor.batch_detection(
                [image_rgb], batch_size=1, static_cache=settings.DETECTOR_STATIC_CACHE
            )
        )
        if not detection_batches:
            raise RuntimeError("Surya detector returned no predictions.")

        preds, orig_sizes = detection_batches[0]
        if not preds or not orig_sizes:
            raise RuntimeError("Surya detector returned empty prediction payload.")

        heatmap = preds[0][0]
        if heatmap.dtype != np.float32:
            heatmap = heatmap.astype(np.float32)
        finite_mask = np.isfinite(heatmap)
        finite_count = int(np.count_nonzero(finite_mask))
        total_count = int(heatmap.size)
        nonfinite_count = total_count - finite_count
        log.info(
            "Surya heatmap stats shape=%s dtype=%s min=%.6f max=%.6f mean=%.6f finite=%s/%s nonfinite=%s",
            tuple(int(v) for v in heatmap.shape),
            heatmap.dtype,
            float(np.nanmin(heatmap)),
            float(np.nanmax(heatmap)),
            float(np.nanmean(heatmap)),
            finite_count,
            total_count,
            nonfinite_count,
        )
        if nonfinite_count > 0:
            raise RuntimeError(
                "Surya detector returned non-finite heatmap values. "
                f"device={self._device or 'unknown'} nonfinite={nonfinite_count}/{total_count}"
            )

        processor_size = list(reversed(heatmap.shape))
        boxes, confidences, proc_mask, debug_stats = _extract_mask_and_boxes(
            cv2=cv2,
            linemap=heatmap,
            text_threshold=float(settings.DETECTOR_TEXT_THRESHOLD),
            low_text=float(settings.DETECTOR_BLANK_THRESHOLD),
        )
        log.info(
            "Surya postprocess raw processor_size=%s labels=%s accepted=%s "
            "text_threshold=%.6f low_text=%.6f max_confidence=%.6f proc_mask_nonzero=%s",
            processor_size,
            debug_stats["label_count"],
            len(boxes),
            debug_stats["text_threshold"],
            debug_stats["low_text"],
            debug_stats["max_confidence"],
            debug_stats["proc_mask_nonzero"],
        )

        polygon_boxes = [
            PolygonBox(polygon=box, confidence=confidence)
            for box, confidence in zip(boxes, confidences)
        ]
        for box in polygon_boxes:
            box.rescale(processor_size, (image_w, image_h))
            box.fit_to_bounds([0, 0, image_w, image_h])

        polygon_boxes = clean_boxes(polygon_boxes)
        for box in polygon_boxes:
            if box.height < 3 * box.width:
                box.expand(
                    x_margin=0,
                    y_margin=float(settings.DETECTOR_BOX_Y_EXPAND_MARGIN),
                )
                box.fit_to_bounds([0, 0, image_w, image_h])

        source_mask = cv2.resize(
            proc_mask,
            (image_w, image_h),
            interpolation=cv2.INTER_NEAREST,
        )
        log.info(
            "Surya postprocess cleaned_boxes=%s source_mask_nonzero=%s source_mask_size=%sx%s",
            len(polygon_boxes),
            int(np.count_nonzero(source_mask)),
            image_w,
            image_h,
        )

        lines = []
        blocks = []
        for box in sorted(
            polygon_boxes,
            key=lambda item: (
                float(item.bbox[1]),
                float(item.bbox[0]),
                float(item.bbox[3]),
                float(item.bbox[2]),
            ),
        ):
            bbox = box.bbox
            x1 = int(bbox[0])
            y1 = int(bbox[1])
            x2 = int(bbox[2])
            y2 = int(bbox[3])
            if x2 <= x1 or y2 <= y1:
                continue
            blocks.append({"x1": x1, "y1": y1, "x2": x2, "y2": y2})
            lines.append(
                {
                    "polygon": [[float(x), float(y)] for x, y in box.polygon],
                    "bbox": [x1, y1, x2, y2],
                    "confidence": float(box.confidence or 0.0),
                }
            )
        log.info("Surya final payload blocks=%s lines=%s", len(blocks), len(lines))

        return {
            "source_size": [image_w, image_h],
            "blocks": blocks,
            "lines": lines,
            "mask_png_base64": _encode_mask_png_base64(cv2, source_mask),
        }

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

    @staticmethod
    def _ensure_cv2():
        import cv2  # type: ignore

        return cv2

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


def _extract_mask_and_boxes(
    *,
    cv2,
    linemap: np.ndarray,
    text_threshold: float,
    low_text: float,
) -> tuple[list[np.ndarray], list[float], np.ndarray, dict[str, float | int]]:
    from surya.detection.heatmap import get_dynamic_thresholds  # type: ignore

    img_h, img_w = linemap.shape
    text_threshold, low_text = get_dynamic_thresholds(linemap, text_threshold, low_text)
    text_score_comb = (linemap > low_text).astype(np.uint8)
    label_count, labels, stats, _ = cv2.connectedComponentsWithStats(
        text_score_comb, connectivity=4
    )

    det: list[np.ndarray] = []
    confidences: list[float] = []
    max_confidence = 0.0
    binary_mask = np.zeros((img_h, img_w), dtype=np.uint8)

    for label_idx in range(1, label_count):
        size = int(stats[label_idx, cv2.CC_STAT_AREA])
        if size < 10:
            continue

        x, y, width, height = [
            int(value)
            for value in stats[
                label_idx,
                [
                    cv2.CC_STAT_LEFT,
                    cv2.CC_STAT_TOP,
                    cv2.CC_STAT_WIDTH,
                    cv2.CC_STAT_HEIGHT,
                ],
            ]
        ]

        try:
            niter = int(np.sqrt(min(width, height)))
        except ValueError:
            niter = 0

        buffer = 1
        sx = max(0, x - niter - buffer)
        sy = max(0, y - niter - buffer)
        ex = min(img_w, x + width + niter + buffer)
        ey = min(img_h, y + height + niter + buffer)

        component_mask = labels[sy:ey, sx:ex] == label_idx
        selected_linemap = linemap[sy:ey, sx:ex][component_mask]
        if selected_linemap.size == 0:
            continue

        line_max = float(np.max(selected_linemap))
        if line_max < text_threshold:
            continue

        segmap = component_mask.astype(np.uint8)
        ksize = max(1, buffer + niter)
        kernel = cv2.getStructuringElement(cv2.MORPH_RECT, (ksize, ksize))
        selected_segmap = cv2.dilate(segmap, kernel)
        binary_mask[sy:ey, sx:ex][selected_segmap > 0] = 255

        y_inds, x_inds = np.nonzero(selected_segmap)
        x_inds += sx
        y_inds += sy
        if x_inds.size == 0 or y_inds.size == 0:
            continue
        np_contours = np.column_stack((x_inds, y_inds))
        rectangle = cv2.minAreaRect(np_contours)
        box = cv2.boxPoints(rectangle)

        edge_w = np.linalg.norm(box[0] - box[1])
        edge_h = np.linalg.norm(box[1] - box[2])
        box_ratio = max(edge_w, edge_h) / (min(edge_w, edge_h) + 1e-5)
        if abs(1 - box_ratio) <= 0.1:
            left = np_contours[:, 0].min()
            right = np_contours[:, 0].max()
            top = np_contours[:, 1].min()
            bottom = np_contours[:, 1].max()
            box = np.array(
                [[left, top], [right, top], [right, bottom], [left, bottom]],
                dtype=np.float32,
            )

        startidx = box.sum(axis=1).argmin()
        box = np.roll(box, 4 - startidx, 0)
        det.append(box)
        confidences.append(line_max)
        max_confidence = max(max_confidence, line_max)

    if max_confidence > 0:
        confidences = [confidence / max_confidence for confidence in confidences]

    return det, confidences, binary_mask, {
        "label_count": int(max(0, label_count - 1)),
        "text_threshold": float(text_threshold),
        "low_text": float(low_text),
        "max_confidence": float(max_confidence),
        "proc_mask_nonzero": int(np.count_nonzero(binary_mask)),
    }


def _encode_mask_png_base64(cv2, mask: np.ndarray) -> str:
    ok, encoded = cv2.imencode(".png", mask)
    if not ok:
        raise RuntimeError("Не удалось закодировать mask PNG.")
    return base64.b64encode(encoded.tobytes()).decode("ascii")


def _resolve_checkpoint_local_dir(checkpoint: str) -> str:
    normalized = str(checkpoint or "").strip()
    if not normalized.startswith("s3://"):
        return normalized
    try:
        from surya.common.s3 import S3DownloaderMixin  # type: ignore
    except Exception:
        return ""
    return str(S3DownloaderMixin.get_local_path(normalized))


def _check_checkpoint_ready(local_dir: str) -> bool:
    normalized = str(local_dir or "").strip()
    if not normalized:
        return False
    try:
        from surya.common.s3 import check_manifest  # type: ignore
    except Exception:
        return False
    return bool(check_manifest(normalized))


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
    normalized = str(device or "").strip().lower()
    if normalized.startswith("cuda"):
        try:
            import torch  # type: ignore
        except Exception:
            return None
        # Surya detection on some CUDA setups emits NaN heatmaps in float16.
        # Prefer float32 here to preserve correctness.
        return torch.float32
    return None
