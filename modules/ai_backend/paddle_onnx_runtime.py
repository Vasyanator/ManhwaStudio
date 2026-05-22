"""
FILE OVERVIEW: modules/ai_backend/paddle_onnx_runtime.py
Shared ONNX Runtime helpers for PaddleOCR recognition and text detection.

Main responsibilities:
- Resolve PaddleOCR ONNX model files from `ManhwaStudio_AI_Models/ONNX/PaddleOCR`.
- Build ONNX Runtime sessions for the selected Execution Provider and device id.
- Run PP-OCR detection and recognition pipelines without Paddle dependencies.
- Reuse runtime sessions across backend requests.
- Configure ONNX Runtime cache directories used by MiGraphX where supported.

Key structures:
- `ProviderSettings`
- `ResolvedModelPaths`
- `RuntimeFactory`
- `PaddleOnnxRuntime`

Notes:
- No model auto-download is implemented here.
- The selected provider is always used directly; initialization errors are surfaced.
"""

from __future__ import annotations

import json
import logging
import math
import os
import re
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional, Sequence

import cv2
import numpy as np
import pyclipper

try:
    import onnxruntime as ort  # type: ignore
except Exception as exc:  # pragma: no cover - environment specific
    ort = None
    ORT_IMPORT_ERROR: Exception | None = exc
else:
    ORT_IMPORT_ERROR = None

from .model_manager import LoadedModelManager, ModelUsageLease


log = logging.getLogger(__name__)

DEFAULT_DET_RESIZE_LONG = 960
DEFAULT_DET_STRIDE = 32
DEFAULT_REC_MAX_DYNAMIC_WIDTH = 3200
DEFAULT_REC_BUCKET_WIDTHS: tuple[int, ...] = (320, 640, 960, 1280, 1600, 1920, 2560, 3200)
DEFAULT_REC_BATCH_SIZE = 8

MODELS_DIR_NAME = "ManhwaStudio_AI_Models"
ONNX_DIR_NAME = "ONNX"
PADDLE_DIR_NAME = "PaddleOCR"
CACHE_DIR_NAME = ".cache"
DEFAULT_DET_KEY = "det_v5"
DEFAULT_REC_MODEL_KEY = "korean_v5"

REC_MODEL_DIRS: dict[str, str] = {
    "chinese_v5": "chinese",
    "english_v5": "english",
    "eslav_v5": "eslav",
    "greek_v5": "greek",
    "korean_v5": "korean",
    "latin_v5": "latin",
    "thai_v5": "thai",
    "arabic_v3": "arabic",
    "hindi_v3": "hindi",
    "telugu_v3": "telugu",
    "tamil_v3": "tamil",
}


@dataclass(frozen=True)
class ProviderSettings:
    provider: str
    device_id: str = "0"

    def cache_key(self) -> str:
        return f"{self.provider}:{self.device_id}"

    def is_migraphx(self) -> bool:
        return self.provider == "MIGraphXExecutionProvider"


@dataclass(frozen=True)
class ResolvedModelPaths:
    det_model_path: Path
    det_config_path: Path
    rec_model_path: Path
    rec_dict_path: Path


@dataclass(frozen=True)
class DetConfig:
    resize_long: int
    max_stride: int
    mean: tuple[float, float, float]
    std: tuple[float, float, float]
    scale: float
    thresh: float
    box_thresh: float
    unclip_ratio: float
    max_candidates: int


@dataclass(frozen=True)
class RecConfig:
    image_shape: tuple[int, int, int]
    character_dict: list[str]
    dynamic_width: bool
    max_dynamic_width: int


@dataclass(frozen=True)
class RecognitionCandidate:
    sort_idx: int
    det_idx: int
    box: np.ndarray
    det_score: float
    crop: np.ndarray


class DBPostProcess:
    def __init__(
        self,
        thresh: float = 0.3,
        box_thresh: float = 0.6,
        max_candidates: int = 1000,
        unclip_ratio: float = 1.5,
    ) -> None:
        self.thresh = float(thresh)
        self.box_thresh = float(box_thresh)
        self.max_candidates = int(max_candidates)
        self.unclip_ratio = float(unclip_ratio)
        self.min_size = 3

    def process_single(
        self,
        pred_map: np.ndarray,
        src_h: int,
        src_w: int,
    ) -> tuple[list[np.ndarray], list[float]]:
        pred = pred_map[0] if pred_map.ndim == 3 else pred_map
        segmentation = pred > self.thresh
        return self._boxes_from_bitmap(
            pred,
            segmentation.astype(np.uint8),
            src_w,
            src_h,
        )

    def _boxes_from_bitmap(
        self,
        pred: np.ndarray,
        bitmap: np.ndarray,
        dest_width: int,
        dest_height: int,
    ) -> tuple[list[np.ndarray], list[float]]:
        height, width = bitmap.shape
        width_scale = float(dest_width) / max(width, 1)
        height_scale = float(dest_height) / max(height, 1)

        contours, _ = cv2.findContours(
            (bitmap * 255).astype(np.uint8),
            cv2.RETR_LIST,
            cv2.CHAIN_APPROX_SIMPLE,
        )

        boxes: list[np.ndarray] = []
        scores: list[float] = []
        for contour in contours[: self.max_candidates]:
            points, short_side = self._get_mini_boxes(contour)
            if short_side < self.min_size:
                continue

            points_np = np.array(points, dtype=np.float32)
            score = self._box_score_fast(pred, points_np)
            if score < self.box_thresh:
                continue

            unclipped = self._unclip(points_np)
            if unclipped.size == 0:
                continue

            box, short_side = self._get_mini_boxes(unclipped.reshape(-1, 1, 2))
            if short_side < self.min_size + 2:
                continue

            box_np = np.array(box, dtype=np.float32)
            box_np[:, 0] = np.clip(np.round(box_np[:, 0] * width_scale), 0, dest_width)
            box_np[:, 1] = np.clip(np.round(box_np[:, 1] * height_scale), 0, dest_height)
            boxes.append(box_np.astype(np.float32))
            scores.append(float(score))

        return boxes, scores

    def _unclip(self, box: np.ndarray) -> np.ndarray:
        area = float(cv2.contourArea(box))
        length = float(cv2.arcLength(box, True))
        if length < 1e-6:
            return np.array([], dtype=np.float32)
        distance = area * self.unclip_ratio / length
        offset = pyclipper.PyclipperOffset()
        offset.AddPath(box.tolist(), pyclipper.JT_ROUND, pyclipper.ET_CLOSEDPOLYGON)
        expanded = offset.Execute(distance)
        if not expanded:
            return np.array([], dtype=np.float32)
        return np.array(expanded[0], dtype=np.float32)

    @staticmethod
    def _get_mini_boxes(contour: np.ndarray) -> tuple[list[np.ndarray], float]:
        bounding_box = cv2.minAreaRect(contour)
        points = sorted(list(cv2.boxPoints(bounding_box)), key=lambda item: item[0])
        if points[1][1] > points[0][1]:
            index_1, index_4 = 0, 1
        else:
            index_1, index_4 = 1, 0
        if points[3][1] > points[2][1]:
            index_2, index_3 = 2, 3
        else:
            index_2, index_3 = 3, 2
        box = [points[index_1], points[index_2], points[index_3], points[index_4]]
        return box, float(min(bounding_box[1]))

    @staticmethod
    def _box_score_fast(bitmap: np.ndarray, box: np.ndarray) -> float:
        h, w = bitmap.shape[:2]
        xmin = max(0, min(math.floor(float(box[:, 0].min())), w - 1))
        xmax = max(0, min(math.ceil(float(box[:, 0].max())), w - 1))
        ymin = max(0, min(math.floor(float(box[:, 1].min())), h - 1))
        ymax = max(0, min(math.ceil(float(box[:, 1].max())), h - 1))
        if xmax < xmin or ymax < ymin:
            return 0.0

        mask = np.zeros((ymax - ymin + 1, xmax - xmin + 1), dtype=np.uint8)
        box_local = box.copy()
        box_local[:, 0] -= xmin
        box_local[:, 1] -= ymin
        cv2.fillPoly(mask, box_local.reshape(1, -1, 2).astype(np.int32), 1)
        return float(cv2.mean(bitmap[ymin : ymax + 1, xmin : xmax + 1], mask)[0])


class CTCLabelDecoder:
    def __init__(self, character_dict: Sequence[str]) -> None:
        characters = [str(ch) for ch in character_dict]
        if " " not in characters:
            characters.append(" ")
        self.character = ["blank"] + characters

    def decode_batch(self, pred: np.ndarray) -> list[tuple[str, float]]:
        logits = np.asarray(pred[0] if isinstance(pred, (tuple, list)) else pred)
        if logits.ndim == 2:
            logits = np.expand_dims(logits, axis=0)
        if logits.ndim != 3:
            raise RuntimeError(f"Unexpected recognizer output shape: {logits.shape}")
        if float(np.max(logits)) > 1.0 or float(np.min(logits)) < 0.0:
            logits = _softmax(logits, axis=-1)
        return [self._decode_logits(sample) for sample in logits]

    def _decode_logits(self, logits: np.ndarray) -> tuple[str, float]:
        indices = np.argmax(logits, axis=-1)
        probs = np.max(logits, axis=-1)
        result_chars: list[str] = []
        confs: list[float] = []
        prev_idx = -1
        for idx, prob in zip(indices, probs):
            idx_i = int(idx)
            if idx_i == 0 or idx_i == prev_idx:
                prev_idx = idx_i
                continue
            if idx_i < len(self.character):
                result_chars.append(self.character[idx_i])
                confs.append(float(prob))
            prev_idx = idx_i
        text = "".join(result_chars)
        score = float(np.mean(confs)) if confs else 0.0
        return text, score


def _shape_str(array: np.ndarray) -> str:
    return "x".join(str(dim) for dim in array.shape)


def _array_stats_str(array: np.ndarray) -> str:
    arr = np.asarray(array)
    if arr.size == 0:
        return "empty"
    return (
        f"shape={_shape_str(arr)} dtype={arr.dtype} "
        f"min={float(np.min(arr)):.4f} max={float(np.max(arr)):.4f} "
        f"mean={float(np.mean(arr)):.4f}"
    )


def _preview_text(text: str, max_len: int = 80) -> str:
    sanitized = text.replace("\n", "\\n")
    if len(sanitized) <= max_len:
        return sanitized
    return sanitized[:max_len] + "..."


def _log_topk_for_logits(logits: np.ndarray, decoder: CTCLabelDecoder, prefix: str) -> None:
    if logits.ndim != 2 or logits.shape[0] == 0:
        return
    time_steps = min(3, logits.shape[0])
    class_count = logits.shape[1]
    top_k = min(5, class_count)
    for time_idx in range(time_steps):
        row = logits[time_idx]
        top_indices = np.argsort(row)[-top_k:][::-1]
        top_items = []
        for idx in top_indices:
            idx_i = int(idx)
            token = decoder.character[idx_i] if idx_i < len(decoder.character) else f"<out:{idx_i}>"
            top_items.append(f"{idx_i}:{repr(token)}={float(row[idx_i]):.4f}")
        log.info("%s timestep=%s topk=%s", prefix, time_idx, ", ".join(top_items))


class OnnxSessionRunner:
    def __init__(
        self,
        onnx_path: Path,
        settings: ProviderSettings,
        session_model_path: Path | None = None,
    ) -> None:
        if ort is None:
            raise RuntimeError(f"onnxruntime import failed: {ORT_IMPORT_ERROR}")

        session_options = ort.SessionOptions()
        session_options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        resolved_session_path = session_model_path or onnx_path
        attempts = provider_attempts(settings)
        errors: list[str] = []
        self._session = None

        for providers in attempts:
            try:
                self._session = ort.InferenceSession(
                    str(resolved_session_path),
                    sess_options=session_options,
                    providers=providers,
                )
                break
            except Exception as exc:
                errors.append(f"{providers}: {exc}")

        if self._session is None:
            details = "\n".join(errors) if errors else "No providers attempted."
            raise RuntimeError(
                "Failed to initialize ONNX Runtime session.\n"
                f"Model: {onnx_path}\n"
                f"Requested provider: {settings.provider}\n"
                f"Attempts:\n{details}"
            )

        self._lock = threading.Lock()
        input_meta = self._session.get_inputs()[0]
        self._input_name = input_meta.name
        self._input_shape = tuple(input_meta.shape)
        self._output_name = self._session.get_outputs()[0].name
        providers = self._session.get_providers()
        self.selected_provider = providers[0] if providers else "unknown"
        log.info(
            "ONNX Runtime session ready: model=%s requested_provider=%s selected_provider=%s input=%s output=%s input_shape=%s",
            resolved_session_path,
            settings.provider,
            self.selected_provider,
            self._input_name,
            self._output_name,
            self._input_shape,
        )

    @property
    def input_shape(self) -> tuple[Any, ...]:
        return self._input_shape

    def run(self, x: np.ndarray) -> np.ndarray:
        log.info(
            "Running inference: provider=%s input_name=%s %s",
            self.selected_provider,
            self._input_name,
            _array_stats_str(x),
        )
        with self._lock:
            output = self._session.run([self._output_name], {self._input_name: x})[0]
        output_np = np.asarray(output)
        log.info(
            "Inference output: provider=%s output_name=%s %s",
            self.selected_provider,
            self._output_name,
            _array_stats_str(output_np),
        )
        return output_np


@dataclass
class ManagedOnnxSession:
    runner: OnnxSessionRunner
    lease: ModelUsageLease

    def release(self) -> None:
        self.lease.release()


class RuntimeFactory:
    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.Lock()
        self._model_manager = model_manager
        self._cache: dict[tuple[str, str], OnnxSessionRunner] = {}
        self._configured_cache_key: str | None = None

    def acquire_runner(self, model_path: Path, settings: ProviderSettings) -> ManagedOnnxSession:
        key = (str(model_path.resolve()), settings.cache_key())
        model_key = self._manager_key_for(key)
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_runner_by_key(key),
        )
        try:
            with self._lock:
                cached = self._cache.get(key)
                if cached is not None:
                    log.info(
                        "Reusing ONNX Runtime session: model=%s provider=%s device=%s",
                        model_path,
                        settings.provider,
                        settings.device_id,
                    )
                    return ManagedOnnxSession(cached, lease)
                if self._configured_cache_key is None:
                    _configure_onnx_cache_environment(resolve_compiled_cache_root(), settings)
                    self._configured_cache_key = settings.cache_key()
                elif self._configured_cache_key != settings.cache_key():
                    log.warning(
                        "MiGraphX cache environment already configured for %s; "
                        "keeping it unchanged for new session model=%s requested=%s",
                        self._configured_cache_key,
                        model_path,
                        settings.cache_key(),
                    )
                runner = OnnxSessionRunner(
                    model_path,
                    settings,
                )
                self._cache[key] = runner
                lease.mark_loaded(unload_callback=lambda: self._unload_runner_by_key(key))
                log.info(
                    "Created ONNX Runtime session: model=%s provider=%s device=%s cache_key=%s",
                    model_path,
                    settings.provider,
                    settings.device_id,
                    settings.cache_key(),
                )
                return ManagedOnnxSession(runner, lease)
        except Exception:
            if lease.needs_load:
                lease.mark_load_failed()
            lease.release()
            raise

    def _unload_runner_by_key(self, cache_key: tuple[str, str]) -> bool:
        with self._lock:
            runner = self._cache.pop(cache_key, None)
        if runner is None:
            return False
        session = getattr(runner, "_session", None)
        if session is not None:
            runner._session = None
            del session
        self._model_manager.mark_unloaded(self._manager_key_for(cache_key))
        return True

    @staticmethod
    def _manager_key_for(cache_key: tuple[str, str]) -> str:
        model_path, provider_key = cache_key
        return f"onnx:{model_path}:{provider_key}"


class PaddleOnnxRuntime:
    def __init__(self, factory: RuntimeFactory) -> None:
        self._factory = factory
        self._lock = threading.Lock()

    @staticmethod
    def _det_provider_settings(settings: ProviderSettings) -> ProviderSettings:
        if settings.is_migraphx():
            return ProviderSettings(provider="CPUExecutionProvider", device_id="0")
        return settings

    @staticmethod
    def _rec_bucket_width(
        requested_width: int,
        cfg: RecConfig,
        selected_provider: str,
    ) -> int:
        if selected_provider == "MIGraphXExecutionProvider":
            return choose_rec_bucket_width(requested_width, cfg)
        return requested_width

    def detect(
        self,
        image_bgr: np.ndarray,
        settings: ProviderSettings,
    ) -> dict[str, Any]:
        det_model_path = resolve_det_model_path()
        det_cfg = parse_det_config(det_model_path.with_name("config.json"))
        det_settings = self._det_provider_settings(settings)
        managed_runner = self._factory.acquire_runner(det_model_path, det_settings)
        try:
            det_runner = managed_runner.runner
            det_input, src_h, src_w = preprocess_det_image(image_bgr, det_cfg)
            log.info(
                "Paddle detect started: image=%s det_input=%s src_h=%s src_w=%s requested_provider=%s det_provider=%s",
                _array_stats_str(image_bgr),
                _array_stats_str(det_input),
                src_h,
                src_w,
                settings.provider,
                det_runner.selected_provider,
            )
            det_pred = det_runner.run(det_input)
            if det_pred.ndim != 4:
                raise RuntimeError(f"Unexpected detector output shape: {det_pred.shape}")

            det_post = DBPostProcess(
                thresh=det_cfg.thresh,
                box_thresh=det_cfg.box_thresh,
                max_candidates=det_cfg.max_candidates,
                unclip_ratio=det_cfg.unclip_ratio,
            )
            boxes, scores = det_post.process_single(det_pred[0], src_h, src_w)
            det_map = np.asarray(det_pred[0, 0])
            above_thresh = int(np.count_nonzero(det_map > det_cfg.thresh))
            log.info(
                "Paddle detect finished: boxes=%s scores=%s det_map=%s thresh=%.3f above_thresh=%s/%s",
                len(boxes),
                [round(float(score), 4) for score in scores[:10]],
                _array_stats_str(det_map),
                det_cfg.thresh,
                above_thresh,
                int(det_map.size),
            )
            return {
                "pred_map": np.asarray(det_pred[0, 0]),
                "boxes": boxes,
                "scores": scores,
            }
        finally:
            managed_runner.release()

    def recognize(
        self,
        image_bgr: np.ndarray,
        model_key: str,
        settings: ProviderSettings,
    ) -> dict[str, Any]:
        model_paths = resolve_model_paths(model_key)
        det_cfg = parse_det_config(model_paths.det_config_path)
        rec_cfg = parse_rec_config(model_paths.rec_dict_path)
        det_settings = self._det_provider_settings(settings)
        managed_det_runner = self._factory.acquire_runner(model_paths.det_model_path, det_settings)
        managed_rec_runner = self._factory.acquire_runner(model_paths.rec_model_path, settings)
        try:
            det_runner = managed_det_runner.runner
            rec_runner = managed_rec_runner.runner
            rec_cfg = adapt_rec_config_to_model_input(rec_cfg, rec_runner.input_shape)
            rec_batch_size, rec_dynamic_batch = resolve_batch_shape(
                rec_runner.input_shape,
                DEFAULT_REC_BATCH_SIZE,
            )
            rec_decoder = CTCLabelDecoder(rec_cfg.character_dict)
            det_post = DBPostProcess(
                thresh=det_cfg.thresh,
                box_thresh=det_cfg.box_thresh,
                max_candidates=det_cfg.max_candidates,
                unclip_ratio=det_cfg.unclip_ratio,
            )

            det_input, src_h, src_w = preprocess_det_image(image_bgr, det_cfg)
            log.info(
                "Paddle recognize started: image=%s requested_provider=%s det_provider=%s rec_provider=%s det_input=%s rec_input_shape=%s batch_size=%s dynamic_batch=%s",
                _array_stats_str(image_bgr),
                settings.provider,
                det_runner.selected_provider,
                rec_runner.selected_provider,
                _array_stats_str(det_input),
                rec_runner.input_shape,
                rec_batch_size,
                rec_dynamic_batch,
            )
            det_pred = det_runner.run(det_input)
            if det_pred.ndim != 4:
                raise RuntimeError(f"Unexpected detector output shape: {det_pred.shape}")
            boxes, det_scores = det_post.process_single(det_pred[0], src_h, src_w)
            sorted_indices = sort_quad_indices(boxes)
            det_map = np.asarray(det_pred[0, 0])
            log.info(
                "Recognizer detector result: boxes=%s sorted_indices=%s det_map=%s thresh=%.3f above_thresh=%s/%s",
                len(boxes),
                sorted_indices[:10],
                _array_stats_str(det_map),
                det_cfg.thresh,
                int(np.count_nonzero(det_map > det_cfg.thresh)),
                int(det_map.size),
            )

            candidates: list[RecognitionCandidate] = []
            for sort_idx, det_idx in enumerate(sorted_indices):
                box = boxes[det_idx]
                crop = get_rotate_crop_image(image_bgr, box)
                if crop.size == 0 or crop.shape[0] < 2 or crop.shape[1] < 2:
                    log.warning(
                        "Skipping invalid crop: det_idx=%s sort_idx=%s crop_shape=%s box=%s",
                        det_idx,
                        sort_idx,
                        crop.shape,
                        box.astype(float).tolist(),
                    )
                    continue
                candidates.append(
                    RecognitionCandidate(
                        sort_idx=sort_idx,
                        det_idx=det_idx,
                        box=box,
                        det_score=float(det_scores[det_idx]) if det_idx < len(det_scores) else 0.0,
                        crop=crop,
                    )
                )
                if len(candidates) <= 5:
                    log.info(
                        "Prepared candidate: det_idx=%s sort_idx=%s det_score=%.4f crop=%s box=%s",
                        det_idx,
                        sort_idx,
                        float(det_scores[det_idx]) if det_idx < len(det_scores) else 0.0,
                        _array_stats_str(crop),
                        box.astype(float).tolist(),
                    )

            lines_by_order: dict[int, dict[str, Any]] = {}
            groups: dict[int, list[RecognitionCandidate]] = {}
            for candidate in candidates:
                requested_width = plan_rec_input_width(candidate.crop, rec_cfg)
                bucket_width = self._rec_bucket_width(
                    requested_width,
                    rec_cfg,
                    rec_runner.selected_provider,
                )
                groups.setdefault(bucket_width, []).append(candidate)

            use_fixed_batch_capacity = (
                rec_dynamic_batch
                and rec_runner.selected_provider == "MIGraphXExecutionProvider"
            )
            log.info(
                "Recognition grouping: candidates=%s groups=%s fixed_batch_capacity=%s migraphx_buckets=%s",
                len(candidates),
                {width: len(items) for width, items in groups.items()},
                use_fixed_batch_capacity,
                rec_runner.selected_provider == "MIGraphXExecutionProvider",
            )
            for bucket_width in sorted(groups):
                chunk = groups[bucket_width]
                batch_capacity = rec_batch_size if use_fixed_batch_capacity else len(chunk)
                rec_input = build_rec_batch_input(chunk, rec_cfg, bucket_width, batch_capacity)
                log.info(
                    "Recognition batch prepared: bucket_width=%s actual_batch=%s batch_capacity=%s input=%s",
                    bucket_width,
                    len(chunk),
                    batch_capacity,
                    _array_stats_str(rec_input),
                )
                rec_pred = rec_runner.run(rec_input)
                decoded_batch = rec_decoder.decode_batch(rec_pred)
                log.info(
                    "Recognition batch decoded: bucket_width=%s actual_batch=%s decoded=%s",
                    bucket_width,
                    len(chunk),
                    len(decoded_batch),
                )
                for local_idx, (candidate, (text, rec_score)) in enumerate(zip(chunk, decoded_batch)):
                    _log_topk_for_logits(
                        rec_pred[local_idx],
                        rec_decoder,
                        f"rec-batch det_idx={candidate.det_idx} sort_idx={candidate.sort_idx}",
                    )
                    cleaned = text.strip()
                    log.info(
                        "Decoded candidate: det_idx=%s sort_idx=%s det_score=%.4f rec_score=%.4f raw=%s cleaned=%s",
                        candidate.det_idx,
                        candidate.sort_idx,
                        float(candidate.det_score),
                        float(rec_score),
                        repr(text),
                        repr(cleaned),
                    )
                    if not cleaned:
                        continue
                    lines_by_order[candidate.sort_idx] = {
                        "text": cleaned,
                        "rec_score": float(rec_score),
                        "det_score": float(candidate.det_score),
                        "det_idx": int(candidate.det_idx),
                        "box": candidate.box.astype(float).tolist(),
                    }

            if not lines_by_order and candidates:
                log.warning(
                    "Batched recognition returned no text for %s candidates; retrying sequentially.",
                    len(candidates),
                )
                for candidate in candidates:
                    requested_width = plan_rec_input_width(candidate.crop, rec_cfg)
                    rec_input = np.ascontiguousarray(
                        np.expand_dims(
                            preprocess_rec_image_to_width(candidate.crop, rec_cfg, requested_width),
                            axis=0,
                        )
                    )
                    log.info(
                        "Sequential recognition input: det_idx=%s sort_idx=%s width=%s input=%s",
                        candidate.det_idx,
                        candidate.sort_idx,
                        requested_width,
                        _array_stats_str(rec_input),
                    )
                    rec_pred = rec_runner.run(rec_input)
                    decoded_batch = rec_decoder.decode_batch(rec_pred)
                    if not decoded_batch:
                        log.warning(
                            "Sequential recognition produced empty decoded batch: det_idx=%s sort_idx=%s",
                            candidate.det_idx,
                            candidate.sort_idx,
                        )
                        continue
                    text, rec_score = decoded_batch[0]
                    _log_topk_for_logits(
                        rec_pred[0],
                        rec_decoder,
                        f"rec-seq det_idx={candidate.det_idx} sort_idx={candidate.sort_idx}",
                    )
                    cleaned = text.strip()
                    log.info(
                        "Sequential decoded candidate: det_idx=%s sort_idx=%s det_score=%.4f rec_score=%.4f raw=%s cleaned=%s",
                        candidate.det_idx,
                        candidate.sort_idx,
                        float(candidate.det_score),
                        float(rec_score),
                        repr(text),
                        repr(cleaned),
                    )
                    if not cleaned:
                        continue
                    lines_by_order[candidate.sort_idx] = {
                        "text": cleaned,
                        "rec_score": float(rec_score),
                        "det_score": float(candidate.det_score),
                        "det_idx": int(candidate.det_idx),
                        "box": candidate.box.astype(float).tolist(),
                    }

            lines = [lines_by_order[idx] for idx in sorted(lines_by_order)]
            log.info(
                "Paddle recognize finished: det_boxes=%s candidates=%s lines=%s text_preview=%s",
                len(boxes),
                len(candidates),
                len(lines),
                _preview_text("\n".join(item["text"] for item in lines)),
            )
            return {
                "text": "\n".join(item["text"] for item in lines),
                "lines": lines,
                "det_boxes": len(boxes),
            }
        finally:
            managed_rec_runner.release()
            managed_det_runner.release()


def normalize_model_key(raw: str) -> str:
    normalized = str(raw or "").strip().lower()
    aliases = {
        "japan_v5": "chinese_v5",
        "chinese_cht_v5": "chinese_v5",
        "cyrillic_v3": "eslav_v5",
        "devanagari_v3": "hindi_v3",
        "en": "english_v5",
        "ko": "korean_v5",
        "korean": "korean_v5",
        "ch": "chinese_v5",
        "japan": "chinese_v5",
        "latin": "latin_v5",
        "eslav": "eslav_v5",
        "thai": "thai_v5",
        "greek": "greek_v5",
        "arabic": "arabic_v3",
        "hindi": "hindi_v3",
        "telugu": "telugu_v3",
        "tamil": "tamil_v3",
    }
    normalized = aliases.get(normalized, normalized)
    if normalized in REC_MODEL_DIRS:
        return normalized
    return DEFAULT_REC_MODEL_KEY


def resolve_provider_settings(
    user_config: Any,
    legacy_device_override: Optional[str] = None,
) -> ProviderSettings:
    provider = _read_config_string(user_config, "ai_onnx_provider") or "CPUExecutionProvider"
    device_id = _read_config_string(user_config, "ai_onnx_device_id") or "0"

    if legacy_device_override:
        override = str(legacy_device_override).strip().lower()
        if override == "cpu":
            provider = "CPUExecutionProvider"
            device_id = "0"
        elif override == "cuda":
            provider = "CUDAExecutionProvider"
            device_id = "0"
        elif override.startswith("cuda:"):
            provider = "CUDAExecutionProvider"
            device_id = override.split(":", 1)[1].strip() or "0"

    return ProviderSettings(provider=provider, device_id=device_id or "0")


def resolve_model_paths(model_key: str) -> ResolvedModelPaths:
    models_root = resolve_models_root()
    normalized_key = normalize_model_key(model_key)
    rec_dir = models_root / "languages" / REC_MODEL_DIRS[normalized_key]
    det_dir = models_root / "detection" / "v5"

    det_model_path = det_dir / "det.onnx"
    det_config_path = det_dir / "config.json"
    rec_model_path = rec_dir / "rec.onnx"
    rec_dict_path = rec_dir / "dict.txt"

    missing = [
        str(path)
        for path in (det_model_path, det_config_path, rec_model_path, rec_dict_path)
        if not path.is_file()
    ]
    if missing:
        missing_list = "\n".join(missing)
        raise FileNotFoundError(
            "PaddleOCR ONNX model files are missing.\n"
            f"Models root: {models_root}\n"
            f"Model key: {normalized_key}\n"
            f"Missing:\n{missing_list}"
        )

    return ResolvedModelPaths(
        det_model_path=det_model_path,
        det_config_path=det_config_path,
        rec_model_path=rec_model_path,
        rec_dict_path=rec_dict_path,
    )


def resolve_det_model_path() -> Path:
    models_root = resolve_models_root()
    det_model_path = models_root / "detection" / "v5" / "det.onnx"
    if not det_model_path.is_file():
        raise FileNotFoundError(
            "PaddleOCR detection model is missing.\n"
            f"Expected: {det_model_path}"
        )
    return det_model_path


def resolve_models_root() -> Path:
    current = Path.cwd().resolve()
    module_root = Path(__file__).resolve().parents[2]
    candidates = [
        current / MODELS_DIR_NAME / ONNX_DIR_NAME / PADDLE_DIR_NAME,
        module_root / MODELS_DIR_NAME / ONNX_DIR_NAME / PADDLE_DIR_NAME,
        current.parent / MODELS_DIR_NAME / ONNX_DIR_NAME / PADDLE_DIR_NAME,
        module_root.parent / MODELS_DIR_NAME / ONNX_DIR_NAME / PADDLE_DIR_NAME,
    ]
    for candidate in candidates:
        if candidate.exists():
            return candidate
    return candidates[0]


def resolve_models_root_parent() -> Path:
    return resolve_models_root().parents[1]


def resolve_compiled_cache_root() -> Path:
    return resolve_models_root_parent() / CACHE_DIR_NAME


def provider_attempts(settings: ProviderSettings) -> list[list[Any]]:
    if ort is None:
        return []

    available = set(ort.get_available_providers())
    attempts: list[list[Any]] = []

    def append_attempt(provider_names: list[str]) -> None:
        filtered = [
            provider_spec(provider_name, settings)
            for provider_name in provider_names
            if provider_name in available
        ]
        if filtered and filtered not in attempts:
            attempts.append(filtered)

    append_attempt([settings.provider])
    return attempts


def provider_spec(provider_name: str, settings: ProviderSettings) -> Any:
    if provider_name in {
        "CUDAExecutionProvider",
        "DmlExecutionProvider",
        "MIGraphXExecutionProvider",
    }:
        return (provider_name, {"device_id": settings.device_id})
    return provider_name


def _sanitize_cache_component(value: str) -> str:
    sanitized = re.sub(r"[^A-Za-z0-9._-]+", "_", value.strip())
    return sanitized or "default"


def _build_provider_cache_namespace(settings: ProviderSettings) -> Path:
    provider_part = _sanitize_cache_component(settings.provider)
    device_part = _sanitize_cache_component(settings.device_id)
    return Path(provider_part) / f"device_{device_part}"


def _configure_onnx_cache_environment(
    cache_dir: Path,
    settings: ProviderSettings,
) -> None:
    namespace = _build_provider_cache_namespace(settings)
    weights_cache_dir = cache_dir / "weights" / namespace
    model_cache_dir = cache_dir / "models" / namespace
    weights_cache_dir.mkdir(parents=True, exist_ok=True)
    model_cache_dir.mkdir(parents=True, exist_ok=True)
    os.environ["ORT_MIGRAPHX_CACHE_PATH"] = str(weights_cache_dir)
    os.environ["ORT_MIGRAPHX_MODEL_CACHE_PATH"] = str(model_cache_dir)
    log.info(
        "Configured MiGraphX cache paths: provider=%s device=%s weights=%s models=%s",
        settings.provider,
        settings.device_id,
        weights_cache_dir,
        model_cache_dir,
    )


def parse_det_config(config_path: Path | None) -> DetConfig:
    config = load_json_file(config_path)
    post_config = config.get("postprocess", {}) if isinstance(config.get("postprocess"), dict) else {}
    return DetConfig(
        resize_long=parse_int(config.get("resize_long"), DEFAULT_DET_RESIZE_LONG),
        max_stride=parse_int(config.get("max_stride"), DEFAULT_DET_STRIDE),
        mean=(0.485, 0.456, 0.406),
        std=(0.229, 0.224, 0.225),
        scale=1.0 / 255.0,
        thresh=parse_float(post_config.get("thresh", config.get("thresh")), 0.3),
        box_thresh=parse_float(post_config.get("box_thresh", config.get("box_thresh")), 0.6),
        unclip_ratio=parse_float(post_config.get("unclip_ratio", config.get("unclip_ratio")), 2.0),
        max_candidates=parse_int(post_config.get("max_candidates", config.get("max_candidates")), 1000),
    )


def parse_rec_config(dict_path: Path | None) -> RecConfig:
    return RecConfig(
        image_shape=(3, 48, 320),
        character_dict=load_character_dict(dict_path),
        dynamic_width=True,
        max_dynamic_width=DEFAULT_REC_MAX_DYNAMIC_WIDTH,
    )


def adapt_rec_config_to_model_input(cfg: RecConfig, input_shape: tuple[Any, ...]) -> RecConfig:
    if len(input_shape) < 4:
        return cfg
    channels = _resolve_positive_dim(input_shape[1], cfg.image_shape[0])
    height = _resolve_positive_dim(input_shape[2], cfg.image_shape[1])
    width = cfg.image_shape[2]
    width_value = input_shape[3]
    fixed_width = isinstance(width_value, (int, np.integer)) and int(width_value) > 0
    if fixed_width:
        width = int(width_value)
    return RecConfig(
        image_shape=(channels, height, width),
        character_dict=cfg.character_dict,
        dynamic_width=not fixed_width,
        max_dynamic_width=cfg.max_dynamic_width,
    )


def preprocess_det_image(img_bgr: np.ndarray, cfg: DetConfig) -> tuple[np.ndarray, int, int]:
    resized = resize_image_for_det(img_bgr, cfg.resize_long, cfg.max_stride)
    rgb = cv2.cvtColor(resized, cv2.COLOR_BGR2RGB)
    img = rgb.astype(np.float32) * cfg.scale
    mean = np.array(cfg.mean, dtype=np.float32).reshape(1, 1, 3)
    std = np.array(cfg.std, dtype=np.float32).reshape(1, 1, 3)
    img = (img - mean) / std
    chw = np.transpose(img, (2, 0, 1)).astype(np.float32)
    batch = np.ascontiguousarray(np.expand_dims(chw, axis=0))
    src_h, src_w = img_bgr.shape[:2]
    return batch, src_h, src_w


def resize_image_for_det(img: np.ndarray, resize_long: int, max_stride: int) -> np.ndarray:
    height, width = img.shape[:2]
    if height == 0 or width == 0:
        return img
    longest_side = max(height, width)
    ratio = float(resize_long) / float(longest_side) if longest_side > resize_long else 1.0
    resize_h = max(int(height * ratio), 1)
    resize_w = max(int(width * ratio), 1)
    stride = max(parse_int(max_stride, DEFAULT_DET_STRIDE), 1)
    resize_h = max(int(round(resize_h / stride) * stride), stride)
    resize_w = max(int(round(resize_w / stride) * stride), stride)
    if resize_h == height and resize_w == width:
        return img
    return cv2.resize(img, (resize_w, resize_h))


def plan_rec_input_width(crop_bgr: np.ndarray, cfg: RecConfig) -> int:
    _img_channels, img_height, img_width = cfg.image_shape
    height, width = crop_bgr.shape[:2]
    height = max(height, 1)
    width = max(width, 1)
    width_by_ratio = max(1, int(math.ceil(img_height * (width / float(height)))))
    if cfg.dynamic_width:
        return min(max(img_width, width_by_ratio), cfg.max_dynamic_width)
    return img_width


def choose_rec_bucket_width(requested_width: int, cfg: RecConfig) -> int:
    if not cfg.dynamic_width:
        return cfg.image_shape[2]
    minimum_width = max(cfg.image_shape[2], 1)
    clamped_width = min(max(requested_width, minimum_width), cfg.max_dynamic_width)
    for bucket_width in DEFAULT_REC_BUCKET_WIDTHS:
        if bucket_width < minimum_width:
            continue
        if bucket_width >= clamped_width:
            return min(bucket_width, cfg.max_dynamic_width)
    return cfg.max_dynamic_width


def preprocess_rec_image_to_width(crop_bgr: np.ndarray, cfg: RecConfig, target_width: int) -> np.ndarray:
    img_channels, img_height, img_width = cfg.image_shape
    if crop_bgr.ndim == 2:
        crop_bgr = cv2.cvtColor(crop_bgr, cv2.COLOR_GRAY2BGR)
    if crop_bgr.ndim != 3 or crop_bgr.shape[2] != 3:
        raise RuntimeError(f"Unexpected crop shape for recognizer: {crop_bgr.shape}")

    crop_rgb = cv2.cvtColor(crop_bgr, cv2.COLOR_BGR2RGB)
    height, width = crop_rgb.shape[:2]
    height = max(height, 1)
    width = max(width, 1)
    width_by_ratio = max(1, int(math.ceil(img_height * (width / float(height)))))
    target_width = max(
        min(int(target_width), cfg.max_dynamic_width),
        img_width,
    )

    resized_width = min(target_width, width_by_ratio)
    resized = cv2.resize(crop_rgb, (resized_width, img_height))
    if img_channels == 1:
        gray = cv2.cvtColor(resized, cv2.COLOR_RGB2GRAY).astype(np.float32) / 255.0
        normalized = np.expand_dims(gray, axis=0)
    else:
        normalized = resized.astype(np.float32).transpose((2, 0, 1)) / 255.0
    normalized = (normalized - 0.5) / 0.5
    padded = np.zeros((img_channels, img_height, target_width), dtype=np.float32)
    padded[:, :, :resized_width] = normalized
    return np.ascontiguousarray(padded)


def build_rec_batch_input(
    candidates: Sequence[RecognitionCandidate],
    cfg: RecConfig,
    target_width: int,
    batch_capacity: int,
) -> np.ndarray:
    img_channels, img_height, _img_width = cfg.image_shape
    batch = np.zeros((batch_capacity, img_channels, img_height, target_width), dtype=np.float32)
    for batch_idx, candidate in enumerate(candidates):
        batch[batch_idx] = preprocess_rec_image_to_width(candidate.crop, cfg, target_width)
    return np.ascontiguousarray(batch)


def sort_quad_indices(boxes: Sequence[np.ndarray]) -> list[int]:
    metrics = {
        idx: (
            float(np.min(box[:, 1]) + np.max(box[:, 1])) / 2.0,
            float(np.min(box[:, 0])),
            float(np.max(box[:, 1]) - np.min(box[:, 1])),
        )
        for idx, box in enumerate(boxes)
    }
    indexed = list(metrics.items())
    indexed.sort(key=lambda item: (item[1][0], item[1][1]))
    order = [idx for idx, _ in indexed]
    for idx in range(len(order)):
        for pos in range(idx, 0, -1):
            prev_y, prev_x, prev_h = metrics[order[pos - 1]]
            curr_y, curr_x, curr_h = metrics[order[pos]]
            same_line_tolerance = max(prev_h, curr_h, 10.0) * 0.5
            if abs(curr_y - prev_y) <= same_line_tolerance and curr_x < prev_x:
                order[pos - 1], order[pos] = order[pos], order[pos - 1]
            else:
                break
    return order


def get_rotate_crop_image(img: np.ndarray, points: np.ndarray) -> np.ndarray:
    points_np = np.array(points, dtype=np.float32)
    if points_np.shape != (4, 2):
        return np.zeros((0, 0, 3), dtype=np.uint8)
    crop_width = int(
        max(
            np.linalg.norm(points_np[0] - points_np[1]),
            np.linalg.norm(points_np[2] - points_np[3]),
        )
    )
    crop_height = int(
        max(
            np.linalg.norm(points_np[0] - points_np[3]),
            np.linalg.norm(points_np[1] - points_np[2]),
        )
    )
    if crop_width < 1 or crop_height < 1:
        return np.zeros((0, 0, 3), dtype=np.uint8)
    destination = np.float32(
        [[0, 0], [crop_width, 0], [crop_width, crop_height], [0, crop_height]]
    )
    matrix = cv2.getPerspectiveTransform(points_np, destination)
    output = cv2.warpPerspective(
        img,
        matrix,
        (crop_width, crop_height),
        borderMode=cv2.BORDER_REPLICATE,
        flags=cv2.INTER_CUBIC,
    )
    if crop_height / max(crop_width, 1) >= 1.5:
        output = np.rot90(output)
    return output


def load_json_file(path: Path | None) -> dict[str, Any]:
    if path is None or not path.exists():
        return {}
    try:
        with path.open("r", encoding="utf-8") as file:
            data = json.load(file)
    except (OSError, json.JSONDecodeError):
        return {}
    return data if isinstance(data, dict) else {}


def load_character_dict(dict_path: Path | None) -> list[str]:
    if dict_path is None or not dict_path.exists():
        raise RuntimeError(f"Character dictionary file not found: {dict_path}")
    tokens: list[str] = []
    with dict_path.open("r", encoding="utf-8") as file:
        for raw_line in file:
            token = raw_line.rstrip("\r\n")
            if token:
                tokens.append(token)
    if not tokens:
        raise RuntimeError(f"Character dictionary is empty: {dict_path}")
    return tokens


def parse_float(value: Any, default: float) -> float:
    if value is None:
        return float(default)
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        stripped = value.strip()
        if "/" in stripped:
            left, right = stripped.split("/", 1)
            return float(left) / float(right)
        return float(stripped)
    return float(default)


def parse_int(value: Any, default: int) -> int:
    if value is None or isinstance(value, bool):
        return int(default)
    try:
        parsed = int(value)
    except (TypeError, ValueError):
        return int(default)
    return parsed if parsed > 0 else int(default)


def _resolve_positive_dim(value: Any, fallback: int) -> int:
    try:
        parsed = int(value)
    except (TypeError, ValueError):
        return fallback
    return parsed if parsed > 0 else fallback


def resolve_batch_shape(input_shape: tuple[Any, ...], fallback: int) -> tuple[int, bool]:
    if not input_shape:
        return fallback, True

    batch_value = input_shape[0]
    fixed_batch = isinstance(batch_value, (int, np.integer)) and int(batch_value) > 0
    if fixed_batch:
        return int(batch_value), False
    return fallback, True


def _softmax(array: np.ndarray, axis: int) -> np.ndarray:
    shifted = array - np.max(array, axis=axis, keepdims=True)
    exp = np.exp(shifted)
    return exp / np.sum(exp, axis=axis, keepdims=True)


def _read_config_string(user_config: Any, key: str) -> Optional[str]:
    config_root = getattr(user_config, "config", None)
    if not isinstance(config_root, dict):
        return None
    general = config_root.get("General")
    if not isinstance(general, dict):
        return None
    value = general.get(key)
    if value is None:
        return None
    text = str(value).strip()
    if text.lower() == "not-selected":
        return None
    return text or None
