"""
File: modules/ai_backend/server.py

Purpose:
HTTP server runtime for local AI services used by the Rust application.

Main responsibilities:
- route OCR, text detector, inpaint, translation, device, and diagnostics endpoints;
- maintain a non-blocking health snapshot for Rust probes;
- publish backend version metadata for Rust-side compatibility checks.
"""

from __future__ import annotations

import base64
import errno
import json
import sys
import threading
import time
import traceback
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import urlparse

from .device_service import AiDeviceService
from .ctd_text_detector_service import CtdTextDetectorService
from .paddle_text_detector_service import PaddleTextDetectorService
from .easy_ocr_service import EasyOcrService
from .aot_inpaint_service import AotInpaintService
from .lama_mpe_inpaint_service import LamaMpeInpaintService
from .lama_inpaint_service import LamaInpaintService
from .manga_ocr_service import MangaOcrService
from .machine_translation_service import MachineTranslationService
from .model_manager import LoadedModelManager
from .paddle_ocr_service import PaddleOcrService
from .paddle_onnx_runtime import RuntimeFactory
from .surya_ocr_service import SuryaOcrService
from .surya_text_detector_service import SuryaTextDetectorService
from .torch_support import is_torch_available, torch_unavailable_error

MAX_REQUEST_BYTES = 24 * 1024 * 1024
HEALTH_SNAPSHOT_REFRESH_SECS = 1.0

# ============================================================================
# AI BACKEND HTTP SERVER
# ----------------------------------------------------------------------------
# Что в файле:
# - HTTP маршрутизация для OCR/MT/textdetector/inpaint эндпоинтов.
# - HTTP маршрутизация для управления AI-устройством (`modules/ai_device.py`).
# - Валидация JSON-полей и унифицированные JSON-ответы.
# - AppState: shared сервисы OCR/MT/Inpaint.
# - `InterruptibleOcrRunner`: выделенный OCR-worker на endpoint с семантикой
#   "latest wins" (новый запрос прерывает ожидание предыдущего).
# - Сетевой слой: ThreadingHTTPServer + keep-alive (HTTP/1.1), чтобы локальный
#   клиент мог отправлять серии OCR-запросов без reconnect.
# - `GET /health` не вычисляет состояние сервисов в request-thread: endpoint
#   отдаёт заранее подготовленный snapshot из фонового health-worker потока.
#   Это сохраняет отзывчивость health-check даже при долгой загрузке моделей.
# - health snapshot также публикует `is_torch_available`, а Torch-зависимые
#   endpoint'ы возвращают 503, если Torch недоступен или отключён debug-флагом.
# - Шумоподавление сети:
#   - health ping (`GET /health`) не пишется в access-log,
#   - `ConnectionResetError`/`BrokenPipeError` на уровне server thread
#     не печатают stacktrace.
# ============================================================================

PADDLE_ONNX_MODEL_TO_LANG: dict[str, str] = {
    "korean_v5": "korean",
    "chinese_v5": "ch",
    "chinese_cht_v5": "chinese_cht",
    "english_v5": "en",
    "japan_v5": "japan",
    "latin_v5": "latin",
    "eslav_v5": "eslav",
    "thai_v5": "thai",
    "greek_v5": "greek",
    "arabic_v3": "arabic",
    "cyrillic_v3": "cyrillic",
    "devanagari_v3": "devanagari",
    "telugu_v3": "telugu",
    "tamil_v3": "tamil",
}


@dataclass
class AppState:
    app_version: str
    model_manager: LoadedModelManager
    easy_ocr: EasyOcrService
    manga_ocr: MangaOcrService
    paddle_ocr: PaddleOcrService
    surya_ocr: SuryaOcrService
    text_detector_ctd: CtdTextDetectorService
    text_detector_paddle: PaddleTextDetectorService
    text_detector_surya: SuryaTextDetectorService
    lama_inpaint: LamaInpaintService
    lama_mpe_inpaint: LamaMpeInpaintService
    aot_inpaint: AotInpaintService
    machine_translation: MachineTranslationService
    ai_device: AiDeviceService
    health_snapshot: dict[str, Any] = field(default_factory=dict)
    health_snapshot_lock: threading.Lock = field(default_factory=threading.Lock, repr=False)


def _json_dumps(payload: dict[str, Any]) -> bytes:
    return json.dumps(payload, ensure_ascii=False).encode("utf-8")


def _ocr_interrupted_payload() -> dict[str, Any]:
    return {
        "ok": False,
        "interrupted": True,
        "error": "OCR request interrupted by a newer selection.",
    }


@dataclass
class _OcrRunnerJob:
    payload: dict[str, Any]
    done: threading.Event = field(default_factory=threading.Event)
    canceled: threading.Event = field(default_factory=threading.Event)
    response_status: int = 500
    response_payload: dict[str, Any] = field(
        default_factory=lambda: {"ok": False, "error": "OCR runner internal error."}
    )


class InterruptibleOcrRunner:
    def __init__(self, worker_fn):
        self._worker_fn = worker_fn
        self._lock = threading.Lock()
        self._condition = threading.Condition(self._lock)
        self._pending_job: _OcrRunnerJob | None = None
        self._running_job: _OcrRunnerJob | None = None
        self._worker = threading.Thread(target=self._worker_loop, daemon=True)
        self._worker.start()

    def run_latest(self, payload: dict[str, Any]) -> tuple[int, dict[str, Any]]:
        job = _OcrRunnerJob(payload=payload)
        with self._condition:
            if self._running_job is not None:
                self._running_job.canceled.set()
            if self._pending_job is not None:
                self._pending_job.canceled.set()
                self._pending_job.response_status = 409
                self._pending_job.response_payload = _ocr_interrupted_payload()
                self._pending_job.done.set()
            self._pending_job = job
            self._condition.notify()

        while True:
            if job.done.wait(0.05):
                return job.response_status, job.response_payload
            if job.canceled.is_set():
                return 409, _ocr_interrupted_payload()

    def _worker_loop(self) -> None:
        while True:
            with self._condition:
                while self._pending_job is None:
                    self._condition.wait()
                job = self._pending_job
                self._pending_job = None
                self._running_job = job

            if job.canceled.is_set():
                job.response_status = 409
                job.response_payload = _ocr_interrupted_payload()
                job.done.set()
                with self._condition:
                    if self._running_job is job:
                        self._running_job = None
                continue

            try:
                response_payload = self._worker_fn(job.payload, job.canceled)
                if job.canceled.is_set():
                    job.response_status = 409
                    job.response_payload = _ocr_interrupted_payload()
                else:
                    job.response_status = 200
                    job.response_payload = response_payload
            except Exception as exc:
                if job.canceled.is_set():
                    job.response_status = 409
                    job.response_payload = _ocr_interrupted_payload()
                else:
                    traceback.print_exc()
                    job.response_status = 500
                    job.response_payload = {"ok": False, "error": str(exc)}
            finally:
                job.done.set()
                with self._condition:
                    if self._running_job is job:
                        self._running_job = None


def _build_health_snapshot(state: AppState) -> dict[str, Any]:
    now_s = time.time()
    return {
        "ok": True,
        "service": "mf_ai_backend",
        "backend_version": state.app_version,
        "snapshot_unix_s": now_s,
        "is_torch_available": is_torch_available(),
        "ocr": {
            "easyocr": state.easy_ocr.health(),
            "mangaocr": state.manga_ocr.health(),
            "paddleocr": state.paddle_ocr.health(),
            "suryaocr": state.surya_ocr.health(),
        },
        "text_detector": {
            "ctd": state.text_detector_ctd.health(),
            "paddle": state.text_detector_paddle.health(),
            "surya": state.text_detector_surya.health(),
        },
        "inpaint": {
            "lama_v2": state.lama_inpaint.health(),
            "lama_mpe": state.lama_mpe_inpaint.health(),
            "aot": state.aot_inpaint.health(),
        },
        "machine_translation": state.machine_translation.health(),
        "model_manager": state.model_manager.health(),
    }


def _set_health_snapshot(state: AppState, payload: dict[str, Any]) -> None:
    with state.health_snapshot_lock:
        state.health_snapshot = payload


def _get_health_snapshot(state: AppState) -> dict[str, Any]:
    with state.health_snapshot_lock:
        if state.health_snapshot:
            return dict(state.health_snapshot)
    return {
        "ok": True,
        "service": "mf_ai_backend",
        "backend_version": state.app_version,
        "snapshot_unix_s": time.time(),
        "snapshot_state": "warming_up",
        "is_torch_available": is_torch_available(),
    }


def _build_handler(
    state: AppState,
    manga_runner: InterruptibleOcrRunner,
    easy_runner: InterruptibleOcrRunner,
    paddle_runner: InterruptibleOcrRunner,
    surya_runner: InterruptibleOcrRunner,
    paddle_onnx_runner: InterruptibleOcrRunner,
):
    class Handler(BaseHTTPRequestHandler):
        server_version = "MF-AI-Backend/0.1"
        protocol_version = "HTTP/1.1"

        def _torch_required(self) -> bool:
            if is_torch_available():
                return True
            self._send_json(503, {"ok": False, "error": torch_unavailable_error()})
            return False

        def _manga_torch_requested(self, payload: dict[str, Any]) -> bool:
            requested_model = MangaOcrService._normalize_model_name(payload.get("manga_model"))
            return requested_model == "base_torch"

        def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
            path = urlparse(self.path).path
            if path != "/health":
                print(
                    f"[AI Backend][http] GET path={path!r} raw_path={self.path!r}",
                    flush=True,
                )
            if path == "/health":
                self._send_json(200, _get_health_snapshot(state))
                return
            if path == "/device":
                self._handle_ai_device_get()
                return
            if path == "/device/cuda_diagnostics":
                self._handle_ai_cuda_diagnostics()
                return
            self._send_json(404, {"ok": False, "error": f"Unknown path: {path}"})

        def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
            path = urlparse(self.path).path
            if path == "/ocr/manga":
                self._handle_ocr_manga()
                return
            if path == "/ocr/easy":
                self._handle_ocr_easy()
                return
            if path == "/ocr/paddle":
                self._handle_ocr_paddle()
                return
            if path == "/ocr/surya":
                self._handle_ocr_surya()
                return
            if path == "/ocr/paddle_onnx":
                self._handle_ocr_paddle_onnx()
                return
            if path == "/translate/deep":
                self._handle_translate_deep()
                return
            if path == "/inpaint/lama_v2":
                self._handle_inpaint_lama_v2()
                return
            if path == "/inpaint/lama_v2/unload":
                self._handle_inpaint_lama_v2_unload()
                return
            if path == "/inpaint/lama_mpe":
                self._handle_inpaint_lama_mpe()
                return
            if path == "/inpaint/lama_mpe/unload":
                self._handle_inpaint_lama_mpe_unload()
                return
            if path == "/inpaint/aot":
                self._handle_inpaint_aot()
                return
            if path == "/inpaint/aot/unload":
                self._handle_inpaint_aot_unload()
                return
            if path == "/textdetector/ctd/detect":
                self._handle_textdetector_ctd_detect()
                return
            if path == "/textdetector/paddle/detect":
                self._handle_textdetector_paddle_detect()
                return
            if path == "/textdetector/surya/detect":
                self._handle_textdetector_surya_detect()
                return
            if path == "/device/set":
                self._handle_ai_device_set()
                return
            self._send_json(404, {"ok": False, "error": f"Unknown path: {path}"})

        def log_message(self, fmt: str, *args: Any) -> None:
            if urlparse(self.path).path == "/health":
                return
            print(f"[AI Backend] {self.address_string()} - {fmt % args}")

        def _handle_ocr_manga(self) -> None:
            payload = self._read_json_body()
            if isinstance(payload, dict) and payload.get("__invalid_json__"):
                self._send_json(400, {"ok": False, "error": payload["error"]})
                return
            if not isinstance(payload, dict):
                self._send_json(400, {"ok": False, "error": "JSON object is required."})
                return
            if self._manga_torch_requested(payload) and not self._torch_required():
                return

            request_data = self._decode_ocr_payload(payload)
            if request_data is None:
                return
            image_bytes, join_newlines, reflect_strings = request_data

            status, response = manga_runner.run_latest(
                {
                    "image_bytes": image_bytes,
                    "join_newlines": join_newlines,
                    "reflect_strings": reflect_strings,
                    "manga_model": payload.get("manga_model"),
                }
            )
            self._send_json(status, response)

        def _handle_ocr_easy(self) -> None:
            payload = self._read_json_body()
            if isinstance(payload, dict) and payload.get("__invalid_json__"):
                self._send_json(400, {"ok": False, "error": payload["error"]})
                return
            if not isinstance(payload, dict):
                self._send_json(400, {"ok": False, "error": "JSON object is required."})
                return
            if not self._torch_required():
                return

            request_data = self._decode_ocr_payload(payload)
            if request_data is None:
                return
            image_bytes, join_newlines, reflect_strings = request_data

            easy_langs_raw = payload.get("easy_langs", "ko")
            if easy_langs_raw is None:
                easy_langs_raw = "ko"
            if not isinstance(easy_langs_raw, str):
                self._send_json(400, {"ok": False, "error": "Field 'easy_langs' must be a string."})
                return
            easy_langs = easy_langs_raw.strip() or "ko"

            status, response = easy_runner.run_latest(
                {
                    "image_bytes": image_bytes,
                    "join_newlines": join_newlines,
                    "reflect_strings": reflect_strings,
                    "easy_langs": easy_langs,
                }
            )
            self._send_json(status, response)

        def _handle_ocr_paddle(self) -> None:
            payload = self._read_json_body()
            if isinstance(payload, dict) and payload.get("__invalid_json__"):
                self._send_json(400, {"ok": False, "error": payload["error"]})
                return
            if not isinstance(payload, dict):
                self._send_json(400, {"ok": False, "error": "JSON object is required."})
                return

            request_data = self._decode_ocr_payload(payload)
            if request_data is None:
                return
            image_bytes, join_newlines, reflect_strings = request_data

            paddle_lang_raw = payload.get("paddle_lang", "korean_v5")
            if paddle_lang_raw is None:
                paddle_lang_raw = "korean_v5"
            if not isinstance(paddle_lang_raw, str):
                self._send_json(400, {"ok": False, "error": "Field 'paddle_lang' must be a string."})
                return
            paddle_lang = paddle_lang_raw.strip() or "korean_v5"

            status, response = paddle_runner.run_latest(
                {
                    "image_bytes": image_bytes,
                    "join_newlines": join_newlines,
                    "reflect_strings": reflect_strings,
                    "paddle_lang": paddle_lang,
                }
            )
            self._send_json(status, response)

        def _handle_ocr_paddle_onnx(self) -> None:
            payload = self._read_json_body()
            if isinstance(payload, dict) and payload.get("__invalid_json__"):
                self._send_json(400, {"ok": False, "error": payload["error"]})
                return
            if not isinstance(payload, dict):
                self._send_json(400, {"ok": False, "error": "JSON object is required."})
                return

            request_data = self._decode_ocr_payload(payload)
            if request_data is None:
                return
            image_bytes, join_newlines, reflect_strings = request_data

            model_raw = payload.get("paddle_onnx_model", "korean_v5")
            if model_raw is None:
                model_raw = "korean_v5"
            if not isinstance(model_raw, str):
                self._send_json(
                    400,
                    {"ok": False, "error": "Field 'paddle_onnx_model' must be a string."},
                )
                return
            model_key = model_raw.strip().lower() or "korean_v5"
            device_raw = payload.get("paddle_onnx_device")
            if device_raw is not None and not isinstance(device_raw, str):
                self._send_json(
                    400,
                    {"ok": False, "error": "Field 'paddle_onnx_device' must be a string."},
                )
                return
            device = str(device_raw or "").strip().lower() or "cpu"

            status, response = paddle_onnx_runner.run_latest(
                {
                    "image_bytes": image_bytes,
                    "join_newlines": join_newlines,
                    "reflect_strings": reflect_strings,
                    "paddle_lang": model_key,
                    "paddle_onnx_model": model_key,
                    "paddle_onnx_device": device,
                }
            )
            self._send_json(status, response)

        def _handle_ocr_surya(self) -> None:
            payload = self._read_json_body()
            if isinstance(payload, dict) and payload.get("__invalid_json__"):
                self._send_json(400, {"ok": False, "error": payload["error"]})
                return
            if not isinstance(payload, dict):
                self._send_json(400, {"ok": False, "error": "JSON object is required."})
                return
            if not self._torch_required():
                return

            request_data = self._decode_ocr_payload(payload)
            if request_data is None:
                return
            image_bytes, join_newlines, reflect_strings = request_data

            task_name_raw = payload.get("surya_task_name", "ocr_without_boxes")
            if task_name_raw is None:
                task_name_raw = "ocr_without_boxes"
            if not isinstance(task_name_raw, str):
                self._send_json(
                    400,
                    {"ok": False, "error": "Field 'surya_task_name' must be a string."},
                )
                return

            max_sliding_window = self._decode_optional_positive_int(
                payload, "surya_max_sliding_window"
            )
            if payload.get("__decode_optional_int_error__"):
                return
            max_tokens = self._decode_optional_positive_int(payload, "surya_max_tokens")
            if payload.get("__decode_optional_int_error__"):
                return

            status, response = surya_runner.run_latest(
                {
                    "image_bytes": image_bytes,
                    "join_newlines": join_newlines,
                    "reflect_strings": reflect_strings,
                    "surya_task_name": task_name_raw.strip().lower() or "ocr_without_boxes",
                    "surya_recognize_math": bool(payload.get("surya_recognize_math", False)),
                    "surya_sort_lines": bool(payload.get("surya_sort_lines", False)),
                    "surya_drop_repeated_text": bool(
                        payload.get("surya_drop_repeated_text", False)
                    ),
                    "surya_max_sliding_window": max_sliding_window,
                    "surya_max_tokens": max_tokens,
                }
            )
            self._send_json(status, response)

        def _handle_translate_deep(self) -> None:
            payload = self._read_json_body()
            if isinstance(payload, dict) and payload.get("__invalid_json__"):
                self._send_json(400, {"ok": False, "error": payload["error"]})
                return
            if not isinstance(payload, dict):
                self._send_json(400, {"ok": False, "error": "JSON object is required."})
                return

            service_raw = payload.get("service", "google")
            if service_raw is None:
                service_raw = "google"
            if not isinstance(service_raw, str):
                self._send_json(400, {"ok": False, "error": "Field 'service' must be a string."})
                return

            source_raw = payload.get("source", "auto")
            if source_raw is None:
                source_raw = "auto"
            if not isinstance(source_raw, str):
                self._send_json(400, {"ok": False, "error": "Field 'source' must be a string."})
                return

            target_raw = payload.get("target", "ru")
            if target_raw is None:
                target_raw = "ru"
            if not isinstance(target_raw, str):
                self._send_json(400, {"ok": False, "error": "Field 'target' must be a string."})
                return

            params_raw = payload.get("params", {})
            if params_raw is None:
                params_raw = {}
            if not isinstance(params_raw, dict):
                self._send_json(400, {"ok": False, "error": "Field 'params' must be an object."})
                return

            texts_raw = payload.get("texts")
            if not isinstance(texts_raw, list):
                self._send_json(400, {"ok": False, "error": "Field 'texts' must be an array."})
                return
            if not texts_raw:
                self._send_json(400, {"ok": False, "error": "Field 'texts' must not be empty."})
                return
            texts = [str(text or "") for text in texts_raw]

            try:
                results = state.machine_translation.translate_batch(
                    service=service_raw,
                    source=source_raw,
                    target=target_raw,
                    params=params_raw,
                    texts=texts,
                )
            except ValueError as exc:
                self._send_json(400, {"ok": False, "error": str(exc)})
                return
            except Exception as exc:
                traceback.print_exc()
                self._send_json(500, {"ok": False, "error": str(exc)})
                return

            translated = sum(1 for item in results if bool(item.get("ok")))
            errors = len(results) - translated
            self._send_json(
                200,
                {
                    "ok": True,
                    "service": str(service_raw).strip().lower() or "google",
                    "translated": translated,
                    "errors": errors,
                    "results": results,
                },
            )

        def _handle_inpaint_lama_v2(self) -> None:
            payload = self._read_json_body()
            if isinstance(payload, dict) and payload.get("__invalid_json__"):
                self._send_json(400, {"ok": False, "error": payload["error"]})
                return
            if not isinstance(payload, dict):
                self._send_json(400, {"ok": False, "error": "JSON object is required."})
                return
            if not self._torch_required():
                return

            image_bytes = self._decode_base64_field(
                payload=payload,
                field_name="image_base64",
                aliases=("image_b64",),
            )
            if image_bytes is None:
                return

            mask_bytes = self._decode_base64_field(
                payload=payload,
                field_name="mask_base64",
                aliases=("mask_b64",),
            )
            if mask_bytes is None:
                return

            params_raw = payload.get("params", {})
            if params_raw is None:
                params_raw = {}
            if not isinstance(params_raw, dict):
                self._send_json(400, {"ok": False, "error": "Field 'params' must be an object."})
                return

            try:
                result = state.lama_inpaint.inpaint_image_bytes(
                    image_bytes,
                    mask_bytes,
                    params=params_raw,
                )
            except ValueError as exc:
                self._send_json(400, {"ok": False, "error": str(exc)})
                return
            except FileNotFoundError as exc:
                self._send_json(400, {"ok": False, "error": str(exc)})
                return
            except Exception as exc:
                traceback.print_exc()
                self._send_json(500, {"ok": False, "error": str(exc)})
                return

            self._send_json(
                200,
                {
                    "ok": True,
                    "engine": "lama_v2",
                    "image_png_base64": result.get("image_png_base64", ""),
                    "source_size": result.get("source_size", [0, 0]),
                    "device": result.get("device", "cpu"),
                    "refine": bool(result.get("refine", False)),
                    "model_name": result.get("model_name"),
                },
            )

        def _handle_inpaint_lama_v2_unload(self) -> None:
            try:
                unloaded = bool(state.lama_inpaint.unload())
            except Exception as exc:
                traceback.print_exc()
                self._send_json(500, {"ok": False, "error": str(exc)})
                return
            self._send_json(200, {"ok": True, "unloaded": unloaded})

        def _handle_inpaint_lama_mpe(self) -> None:
            payload = self._read_json_body()
            if isinstance(payload, dict) and payload.get("__invalid_json__"):
                self._send_json(400, {"ok": False, "error": payload["error"]})
                return
            if not isinstance(payload, dict):
                self._send_json(400, {"ok": False, "error": "JSON object is required."})
                return
            if not self._torch_required():
                return

            image_bytes = self._decode_base64_field(
                payload=payload,
                field_name="image_base64",
                aliases=("image_b64",),
            )
            if image_bytes is None:
                return

            mask_bytes = self._decode_base64_field(
                payload=payload,
                field_name="mask_base64",
                aliases=("mask_b64",),
            )
            if mask_bytes is None:
                return

            params_raw = payload.get("params", {})
            if params_raw is None:
                params_raw = {}
            if not isinstance(params_raw, dict):
                self._send_json(400, {"ok": False, "error": "Field 'params' must be an object."})
                return

            try:
                result = state.lama_mpe_inpaint.inpaint_image_bytes(
                    image_bytes,
                    mask_bytes,
                    params=params_raw,
                )
            except ValueError as exc:
                self._send_json(400, {"ok": False, "error": str(exc)})
                return
            except FileNotFoundError as exc:
                self._send_json(400, {"ok": False, "error": str(exc)})
                return
            except Exception as exc:
                traceback.print_exc()
                self._send_json(500, {"ok": False, "error": str(exc)})
                return

            self._send_json(
                200,
                {
                    "ok": True,
                    "engine": "lama_mpe",
                    "image_png_base64": result.get("image_png_base64", ""),
                    "source_size": result.get("source_size", [0, 0]),
                    "device": result.get("device", "cpu"),
                    "inpaint_size": int(result.get("inpaint_size", 2048)),
                },
            )

        def _handle_inpaint_lama_mpe_unload(self) -> None:
            try:
                unloaded = bool(state.lama_mpe_inpaint.unload())
            except Exception as exc:
                traceback.print_exc()
                self._send_json(500, {"ok": False, "error": str(exc)})
                return
            self._send_json(200, {"ok": True, "unloaded": unloaded})

        def _handle_inpaint_aot(self) -> None:
            payload = self._read_json_body()
            if isinstance(payload, dict) and payload.get("__invalid_json__"):
                self._send_json(400, {"ok": False, "error": payload["error"]})
                return
            if not isinstance(payload, dict):
                self._send_json(400, {"ok": False, "error": "JSON object is required."})
                return
            if not self._torch_required():
                return

            image_bytes = self._decode_base64_field(
                payload=payload,
                field_name="image_base64",
                aliases=("image_b64",),
            )
            if image_bytes is None:
                return

            mask_bytes = self._decode_base64_field(
                payload=payload,
                field_name="mask_base64",
                aliases=("mask_b64",),
            )
            if mask_bytes is None:
                return

            params_raw = payload.get("params", {})
            if params_raw is None:
                params_raw = {}
            if not isinstance(params_raw, dict):
                self._send_json(400, {"ok": False, "error": "Field 'params' must be an object."})
                return

            try:
                result = state.aot_inpaint.inpaint_image_bytes(
                    image_bytes,
                    mask_bytes,
                    params=params_raw,
                )
            except ValueError as exc:
                self._send_json(400, {"ok": False, "error": str(exc)})
                return
            except FileNotFoundError as exc:
                self._send_json(400, {"ok": False, "error": str(exc)})
                return
            except Exception as exc:
                traceback.print_exc()
                self._send_json(500, {"ok": False, "error": str(exc)})
                return

            self._send_json(
                200,
                {
                    "ok": True,
                    "engine": "aot",
                    "image_png_base64": result.get("image_png_base64", ""),
                    "source_size": result.get("source_size", [0, 0]),
                    "device": result.get("device", "cpu"),
                    "inpaint_size": int(result.get("inpaint_size", 2048)),
                },
            )

        def _handle_inpaint_aot_unload(self) -> None:
            try:
                unloaded = bool(state.aot_inpaint.unload())
            except Exception as exc:
                traceback.print_exc()
                self._send_json(500, {"ok": False, "error": str(exc)})
                return
            self._send_json(200, {"ok": True, "unloaded": unloaded})

        def _handle_textdetector_ctd_detect(self) -> None:
            payload = self._read_json_body()
            if isinstance(payload, dict) and payload.get("__invalid_json__"):
                self._send_json(400, {"ok": False, "error": payload["error"]})
                return
            if not isinstance(payload, dict):
                self._send_json(400, {"ok": False, "error": "JSON object is required."})
                return
            if not self._torch_required():
                return

            params_raw = payload.get("params", {})
            if params_raw is None:
                params_raw = {}
            if not isinstance(params_raw, dict):
                self._send_json(400, {"ok": False, "error": "Field 'params' must be an object."})
                return

            try:
                page_path_raw = payload.get("page_path")
                if (not isinstance(page_path_raw, str) or not page_path_raw.strip()) and isinstance(
                    payload.get("path"), str
                ):
                    page_path_raw = payload.get("path")

                if isinstance(page_path_raw, str) and page_path_raw.strip():
                    result = state.text_detector_ctd.detect_page(
                        page_path_raw.strip(), params=params_raw
                    )
                else:
                    has_inline_image = any(
                        isinstance(payload.get(key), str) and payload.get(key).strip()
                        for key in ("image_base64", "image_b64", "image_png_base64")
                    )
                    if not has_inline_image:
                        self._send_json(
                            400,
                            {
                                "ok": False,
                                "error": "Either 'page_path'/'path' or 'image_base64'/'image_b64'/'image_png_base64' must be provided.",
                            },
                        )
                        return
                    image_bytes = self._decode_base64_field(
                        payload=payload,
                        field_name="image_base64",
                        aliases=("image_b64", "image_png_base64"),
                    )
                    if image_bytes is None:
                        return
                    result = state.text_detector_ctd.detect_image_bytes(
                        image_bytes, params=params_raw
                    )
            except FileNotFoundError as exc:
                self._send_json(400, {"ok": False, "error": str(exc)})
                return
            except Exception as exc:
                traceback.print_exc()
                self._send_json(500, {"ok": False, "error": str(exc)})
                return

            self._send_json(
                200,
                {
                    "ok": True,
                    "engine": "ctd",
                    "source_size": result.get("source_size", [0, 0]),
                    "blocks": result.get("blocks", []),
                    "mask_png_base64": result.get("mask_png_base64", ""),
                },
            )

        def _handle_textdetector_paddle_detect(self) -> None:
            payload = self._read_json_body()
            if isinstance(payload, dict) and payload.get("__invalid_json__"):
                self._send_json(400, {"ok": False, "error": payload["error"]})
                return
            if not isinstance(payload, dict):
                self._send_json(400, {"ok": False, "error": "JSON object is required."})
                return

            try:
                page_path_raw = payload.get("page_path")
                if (
                    not isinstance(page_path_raw, str) or not page_path_raw.strip()
                ) and isinstance(payload.get("path"), str):
                    page_path_raw = payload.get("path")

                if isinstance(page_path_raw, str) and page_path_raw.strip():
                    result = state.text_detector_paddle.detect_page(
                        page_path_raw.strip()
                    )
                else:
                    has_inline_image = any(
                        isinstance(payload.get(key), str) and payload.get(key).strip()
                        for key in ("image_base64", "image_b64", "image_png_base64")
                    )
                    if not has_inline_image:
                        self._send_json(
                            400,
                            {
                                "ok": False,
                                "error": (
                                    "Either 'page_path'/'path' or "
                                    "'image_base64'/'image_b64'/'image_png_base64' "
                                    "must be provided."
                                ),
                            },
                        )
                        return
                    image_bytes = self._decode_base64_field(
                        payload=payload,
                        field_name="image_base64",
                        aliases=("image_b64", "image_png_base64"),
                    )
                    if image_bytes is None:
                        return
                    result = state.text_detector_paddle.detect_image_bytes(image_bytes)
            except FileNotFoundError as exc:
                self._send_json(400, {"ok": False, "error": str(exc)})
                return
            except Exception as exc:
                traceback.print_exc()
                self._send_json(500, {"ok": False, "error": str(exc)})
                return

            self._send_json(
                200,
                {
                    "ok": True,
                    "engine": "paddle",
                    "source_size": result.get("source_size", [0, 0]),
                    "blocks": result.get("blocks", []),
                    "polys": result.get("polys", []),
                    "mask_png_base64": result.get("mask_png_base64", ""),
                },
            )

        def _handle_textdetector_surya_detect(self) -> None:
            payload = self._read_json_body()
            if isinstance(payload, dict) and payload.get("__invalid_json__"):
                self._send_json(400, {"ok": False, "error": payload["error"]})
                return
            if not isinstance(payload, dict):
                self._send_json(400, {"ok": False, "error": "JSON object is required."})
                return
            if not self._torch_required():
                return

            try:
                page_path_raw = payload.get("page_path")
                if (
                    not isinstance(page_path_raw, str) or not page_path_raw.strip()
                ) and isinstance(payload.get("path"), str):
                    page_path_raw = payload.get("path")

                if isinstance(page_path_raw, str) and page_path_raw.strip():
                    result = state.text_detector_surya.detect_page(page_path_raw.strip())
                else:
                    has_inline_image = any(
                        isinstance(payload.get(key), str) and payload.get(key).strip()
                        for key in ("image_base64", "image_b64", "image_png_base64")
                    )
                    if not has_inline_image:
                        self._send_json(
                            400,
                            {
                                "ok": False,
                                "error": (
                                    "Either 'page_path'/'path' or "
                                    "'image_base64'/'image_b64'/'image_png_base64' "
                                    "must be provided."
                                ),
                            },
                        )
                        return
                    image_bytes = self._decode_base64_field(
                        payload=payload,
                        field_name="image_base64",
                        aliases=("image_b64", "image_png_base64"),
                    )
                    if image_bytes is None:
                        return
                    result = state.text_detector_surya.detect_image_bytes(image_bytes)
            except FileNotFoundError as exc:
                self._send_json(400, {"ok": False, "error": str(exc)})
                return
            except Exception as exc:
                traceback.print_exc()
                self._send_json(500, {"ok": False, "error": str(exc)})
                return

            self._send_json(
                200,
                {
                    "ok": True,
                    "engine": "surya",
                    "source_size": result.get("source_size", [0, 0]),
                    "blocks": result.get("blocks", []),
                    "lines": result.get("lines", []),
                    "mask_png_base64": result.get("mask_png_base64", ""),
                },
            )

        def _handle_ai_device_get(self) -> None:
            started_at = time.perf_counter()
            print("[AI Backend][device_http] GET /device start", flush=True)
            try:
                state_payload = state.ai_device.get_state()
            except Exception as exc:
                traceback.print_exc()
                print(
                    "[AI Backend][device_http] GET /device failed "
                    f"elapsed_ms={int((time.perf_counter() - started_at) * 1000)} "
                    f"error={type(exc).__name__}: {exc}",
                    flush=True,
                )
                self._send_json(500, {"ok": False, "error": str(exc)})
                return

            response_payload = {
                "ok": True,
                "selected_device": state_payload.get("selected_device", "cpu"),
                "available_devices": state_payload.get("available_devices", ["cpu"]),
                "available_device_options": state_payload.get("available_device_options", []),
                "torch_device_needs_selection": bool(
                    state_payload.get("torch_device_needs_selection", False)
                ),
                "max_loaded_models": state_payload.get("max_loaded_models", 3),
                "selected_onnx_provider": state_payload.get(
                    "selected_onnx_provider",
                    "CPUExecutionProvider",
                ),
                "available_onnx_providers": state_payload.get(
                    "available_onnx_providers",
                    ["CPUExecutionProvider"],
                ),
                "selected_onnx_device_id": state_payload.get(
                    "selected_onnx_device_id",
                    "0",
                ),
                "available_onnx_device_options": state_payload.get(
                    "available_onnx_device_options",
                    [{"id": "0", "label": "0"}],
                ),
                "available_onnx_devices_by_provider": state_payload.get(
                    "available_onnx_devices_by_provider",
                    {"CPUExecutionProvider": [{"id": "0", "label": "0"}]},
                ),
                "onnx_device_needs_selection": bool(
                    state_payload.get("onnx_device_needs_selection", False)
                ),
            }
            print(
                "[AI Backend][device_http] GET /device ok "
                f"elapsed_ms={int((time.perf_counter() - started_at) * 1000)} "
                f"torch_selected={response_payload['selected_device']!r} "
                f"torch_devices={len(response_payload['available_devices'])} "
                f"torch_options={len(response_payload['available_device_options'])} "
                f"torch_needs_selection={response_payload['torch_device_needs_selection']} "
                f"onnx_provider={response_payload['selected_onnx_provider']!r} "
                f"onnx_providers={len(response_payload['available_onnx_providers'])} "
                f"onnx_device={response_payload['selected_onnx_device_id']!r} "
                f"onnx_options={len(response_payload['available_onnx_device_options'])} "
                f"onnx_needs_selection={response_payload['onnx_device_needs_selection']}",
                flush=True,
            )
            self._send_json(200, response_payload)

        def _handle_ai_device_set(self) -> None:
            started_at = time.perf_counter()
            payload = self._read_json_body()
            if isinstance(payload, dict) and payload.get("__invalid_json__"):
                print(
                    "[AI Backend][device_http] POST /device/set invalid_json "
                    f"elapsed_ms={int((time.perf_counter() - started_at) * 1000)} "
                    f"error={payload['error']}",
                    flush=True,
                )
                self._send_json(400, {"ok": False, "error": payload["error"]})
                return
            if not isinstance(payload, dict):
                print(
                    "[AI Backend][device_http] POST /device/set non_object_body "
                    f"elapsed_ms={int((time.perf_counter() - started_at) * 1000)}",
                    flush=True,
                )
                self._send_json(400, {"ok": False, "error": "JSON object is required."})
                return

            print(
                "[AI Backend][device_http] POST /device/set start "
                f"has_device={'device' in payload} "
                f"has_onnx_provider={'onnx_provider' in payload} "
                f"has_onnx_device_id={'onnx_device_id' in payload} "
                f"has_max_loaded_models={'max_loaded_models' in payload}",
                flush=True,
            )
            try:
                state_payload = state.ai_device.set_device(
                    payload.get("device"),
                    payload.get("onnx_provider"),
                    payload.get("onnx_device_id"),
                    payload.get("max_loaded_models"),
                )
            except ValueError as exc:
                print(
                    "[AI Backend][device_http] POST /device/set validation_failed "
                    f"elapsed_ms={int((time.perf_counter() - started_at) * 1000)} "
                    f"error={exc}",
                    flush=True,
                )
                self._send_json(400, {"ok": False, "error": str(exc)})
                return
            except Exception as exc:
                traceback.print_exc()
                print(
                    "[AI Backend][device_http] POST /device/set failed "
                    f"elapsed_ms={int((time.perf_counter() - started_at) * 1000)} "
                    f"error={type(exc).__name__}: {exc}",
                    flush=True,
                )
                self._send_json(500, {"ok": False, "error": str(exc)})
                return

            response_payload = {
                "ok": True,
                "selected_device": state_payload.get("selected_device", "cpu"),
                "available_devices": state_payload.get("available_devices", ["cpu"]),
                "available_device_options": state_payload.get("available_device_options", []),
                "torch_device_needs_selection": bool(
                    state_payload.get("torch_device_needs_selection", False)
                ),
                "max_loaded_models": state_payload.get("max_loaded_models", 3),
                "selected_onnx_provider": state_payload.get(
                    "selected_onnx_provider",
                    "CPUExecutionProvider",
                ),
                "available_onnx_providers": state_payload.get(
                    "available_onnx_providers",
                    ["CPUExecutionProvider"],
                ),
                "selected_onnx_device_id": state_payload.get(
                    "selected_onnx_device_id",
                    "0",
                ),
                "available_onnx_device_options": state_payload.get(
                    "available_onnx_device_options",
                    [{"id": "0", "label": "0"}],
                ),
                "available_onnx_devices_by_provider": state_payload.get(
                    "available_onnx_devices_by_provider",
                    {"CPUExecutionProvider": [{"id": "0", "label": "0"}]},
                ),
                "onnx_device_needs_selection": bool(
                    state_payload.get("onnx_device_needs_selection", False)
                ),
            }
            print(
                "[AI Backend][device_http] POST /device/set ok "
                f"elapsed_ms={int((time.perf_counter() - started_at) * 1000)} "
                f"torch_selected={response_payload['selected_device']!r} "
                f"torch_needs_selection={response_payload['torch_device_needs_selection']} "
                f"onnx_provider={response_payload['selected_onnx_provider']!r} "
                f"onnx_device={response_payload['selected_onnx_device_id']!r} "
                f"onnx_needs_selection={response_payload['onnx_device_needs_selection']}",
                flush=True,
            )
            self._send_json(200, response_payload)

        def _handle_ai_cuda_diagnostics(self) -> None:
            try:
                diagnostics = state.ai_device.diagnose_cuda_rocm()
            except Exception as exc:
                traceback.print_exc()
                self._send_json(500, {"ok": False, "error": str(exc)})
                return

            self._send_json(200, {"ok": True, "diagnostics": diagnostics})

        def _decode_ocr_payload(
            self, payload: dict[str, Any]
        ) -> tuple[bytes, bool, bool] | None:
            image_b64 = payload.get("image_base64") or payload.get("image_b64")
            if not isinstance(image_b64, str) or not image_b64.strip():
                self._send_json(
                    400,
                    {"ok": False, "error": "Field 'image_base64' is required."},
                )
                return None

            image_b64 = image_b64.strip()
            if image_b64.startswith("data:") and "," in image_b64:
                image_b64 = image_b64.split(",", 1)[1]
            try:
                image_bytes = base64.b64decode(image_b64, validate=True)
            except Exception:
                self._send_json(400, {"ok": False, "error": "Invalid base64 image payload."})
                return None

            join_newlines = bool(payload.get("join_newlines", True))
            reflect_strings = bool(payload.get("reflect_strings", False))
            return image_bytes, join_newlines, reflect_strings

        def _decode_base64_field(
            self,
            *,
            payload: dict[str, Any],
            field_name: str,
            aliases: tuple[str, ...] = (),
        ) -> bytes | None:
            value = payload.get(field_name)
            if not isinstance(value, str) or not value.strip():
                for alias in aliases:
                    alias_value = payload.get(alias)
                    if isinstance(alias_value, str) and alias_value.strip():
                        value = alias_value
                        break

            if not isinstance(value, str) or not value.strip():
                self._send_json(
                    400,
                    {"ok": False, "error": f"Field '{field_name}' is required."},
                )
                return None

            value = value.strip()
            if value.startswith("data:") and "," in value:
                value = value.split(",", 1)[1]
            try:
                return base64.b64decode(value, validate=True)
            except Exception:
                self._send_json(
                    400,
                    {"ok": False, "error": f"Field '{field_name}' contains invalid base64."},
                )
                return None

        def _decode_optional_positive_int(
            self,
            payload: dict[str, Any],
            field_name: str,
        ) -> int | None:
            payload.pop("__decode_optional_int_error__", None)
            value = payload.get(field_name)
            if value is None or value == "":
                return None
            if isinstance(value, bool):
                self._send_json(
                    400,
                    {"ok": False, "error": f"Field '{field_name}' must be an integer."},
                )
                payload["__decode_optional_int_error__"] = True
                return None
            try:
                normalized = int(value)
            except Exception:
                self._send_json(
                    400,
                    {"ok": False, "error": f"Field '{field_name}' must be an integer."},
                )
                payload["__decode_optional_int_error__"] = True
                return None
            if normalized <= 0:
                return None
            return normalized

        def _read_json_body(self) -> dict[str, Any] | None:
            length_raw = self.headers.get("Content-Length")
            if length_raw is None:
                return {"__invalid_json__": True, "error": "Missing Content-Length header."}
            try:
                length = int(length_raw)
            except ValueError:
                return {"__invalid_json__": True, "error": "Invalid Content-Length header."}
            if length < 0 or length > MAX_REQUEST_BYTES:
                return {
                    "__invalid_json__": True,
                    "error": f"Request body is too large (max {MAX_REQUEST_BYTES} bytes).",
                }
            try:
                raw_body = self.rfile.read(length)
            except Exception:
                return {"__invalid_json__": True, "error": "Failed to read request body."}
            try:
                return json.loads(raw_body.decode("utf-8"))
            except Exception:
                return {"__invalid_json__": True, "error": "Request body must be valid JSON."}

        def _send_json(self, status: int, payload: dict[str, Any]) -> None:
            body = _json_dumps(payload)
            try:
                self.send_response(status)
                self.send_header("Content-Type", "application/json; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Access-Control-Allow-Origin", "*")
                self.send_header("Connection", "keep-alive")
                self.end_headers()
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                # Клиент мог закрыть сокет до отправки ответа (например, по таймауту).
                return

    return Handler


class QuietThreadingHttpServer(ThreadingHTTPServer):
    def handle_error(self, request, client_address):  # noqa: ANN001 - stdlib signature
        exc = sys.exc_info()[1]
        print(
            "[AI Backend][http_error] "
            f"client={client_address!r} error={type(exc).__name__}: {exc}",
            flush=True,
        )
        traceback.print_exc()
        if isinstance(exc, (ConnectionResetError, ConnectionAbortedError, BrokenPipeError)):
            return
        if isinstance(exc, OSError) and exc.errno in (
            errno.ECONNABORTED,
            errno.ECONNRESET,
            errno.EPIPE,
        ):
            return
        super().handle_error(request, client_address)


def run_server(
    *,
    host: str = "127.0.0.1",
    port: int = 8765,
    warmup_mangaocr: bool = False,
    app_version: str,
) -> None:
    model_manager = LoadedModelManager()
    onnx_runtime_factory = RuntimeFactory(model_manager)
    ai_device_service = AiDeviceService(model_manager)
    state = AppState(
        app_version=app_version,
        model_manager=model_manager,
        easy_ocr=EasyOcrService(model_manager),
        manga_ocr=MangaOcrService(model_manager, ai_device_service),
        paddle_ocr=PaddleOcrService(onnx_runtime_factory),
        surya_ocr=SuryaOcrService(model_manager),
        text_detector_ctd=CtdTextDetectorService(model_manager),
        text_detector_paddle=PaddleTextDetectorService(onnx_runtime_factory),
        text_detector_surya=SuryaTextDetectorService(model_manager),
        lama_inpaint=LamaInpaintService(model_manager),
        lama_mpe_inpaint=LamaMpeInpaintService(model_manager),
        aot_inpaint=AotInpaintService(model_manager),
        machine_translation=MachineTranslationService(),
        ai_device=ai_device_service,
    )
    _set_health_snapshot(
        state,
        {
            "ok": True,
            "service": "mf_ai_backend",
            "backend_version": state.app_version,
            "snapshot_unix_s": time.time(),
            "snapshot_state": "warming_up",
            "is_torch_available": is_torch_available(),
        },
    )
    stop_event = threading.Event()
    health_thread = threading.Thread(
        target=_health_snapshot_worker,
        args=(state, stop_event),
        daemon=True,
    )
    health_thread.start()
    if warmup_mangaocr:
        threading.Thread(target=_warmup_safe, args=(state,), daemon=True).start()

    def manga_worker(payload: dict[str, Any], canceled: threading.Event) -> dict[str, Any]:
        if canceled.is_set():
            return _ocr_interrupted_payload()
        result = state.manga_ocr.recognize_image_bytes(
            payload["image_bytes"],
            join_newlines=bool(payload.get("join_newlines", True)),
            reflect_strings=bool(payload.get("reflect_strings", False)),
            manga_model=payload.get("manga_model"),
        )
        return {
            "ok": True,
            "engine": "mangaocr",
            "lines": result["lines"],
            "text": result["text"],
        }

    def easy_worker(payload: dict[str, Any], canceled: threading.Event) -> dict[str, Any]:
        if canceled.is_set():
            return _ocr_interrupted_payload()
        result = state.easy_ocr.recognize_image_bytes(
            payload["image_bytes"],
            join_newlines=bool(payload.get("join_newlines", True)),
            reflect_strings=bool(payload.get("reflect_strings", False)),
            langs=str(payload.get("easy_langs", "ko")),
        )
        return {
            "ok": True,
            "engine": "easyocr",
            "lines": result["lines"],
            "text": result["text"],
        }

    def paddle_worker(payload: dict[str, Any], canceled: threading.Event) -> dict[str, Any]:
        if canceled.is_set():
            return _ocr_interrupted_payload()
        result = state.paddle_ocr.recognize_image_bytes(
            payload["image_bytes"],
            join_newlines=bool(payload.get("join_newlines", True)),
            reflect_strings=bool(payload.get("reflect_strings", False)),
            lang=str(payload.get("paddle_lang", "korean_v5")),
        )
        return {
            "ok": True,
            "engine": "paddleocr",
            "lines": result["lines"],
            "text": result["text"],
        }

    def paddle_onnx_worker(payload: dict[str, Any], canceled: threading.Event) -> dict[str, Any]:
        if canceled.is_set():
            return _ocr_interrupted_payload()
        result = state.paddle_ocr.recognize_image_bytes(
            payload["image_bytes"],
            join_newlines=bool(payload.get("join_newlines", True)),
            reflect_strings=bool(payload.get("reflect_strings", False)),
            lang=str(payload.get("paddle_lang", "korean_v5")),
            device=str(payload.get("paddle_onnx_device", "cpu")),
        )
        return {
            "ok": True,
            "engine": "paddleocr_onnx",
            "model": str(payload.get("paddle_onnx_model", "korean_v5")),
            "device": str(payload.get("paddle_onnx_device", "cpu")),
            "lines": result["lines"],
            "text": result["text"],
        }

    def surya_worker(payload: dict[str, Any], canceled: threading.Event) -> dict[str, Any]:
        if canceled.is_set():
            return _ocr_interrupted_payload()
        result = state.surya_ocr.recognize_image_bytes(
            payload["image_bytes"],
            join_newlines=bool(payload.get("join_newlines", True)),
            reflect_strings=bool(payload.get("reflect_strings", False)),
            task_name=str(payload.get("surya_task_name", "ocr_without_boxes")),
            recognize_math=bool(payload.get("surya_recognize_math", False)),
            sort_lines=bool(payload.get("surya_sort_lines", False)),
            drop_repeated_text=bool(payload.get("surya_drop_repeated_text", False)),
            max_sliding_window=payload.get("surya_max_sliding_window"),
            max_tokens=payload.get("surya_max_tokens"),
        )
        return {
            "ok": True,
            "engine": "suryaocr",
            "task_name": str(payload.get("surya_task_name", "ocr_without_boxes")),
            "lines": result["lines"],
            "text": result["text"],
        }

    handler_cls = _build_handler(
        state,
        InterruptibleOcrRunner(manga_worker),
        InterruptibleOcrRunner(easy_worker),
        InterruptibleOcrRunner(paddle_worker),
        InterruptibleOcrRunner(surya_worker),
        InterruptibleOcrRunner(paddle_onnx_worker),
    )
    server = QuietThreadingHttpServer((host, port), handler_cls)
    print(f"[AI Backend] Running on http://{host}:{port}")
    print(
        "[AI Backend] Endpoints: GET /health, POST /ocr/manga, POST /ocr/easy, "
        "POST /ocr/paddle, POST /ocr/surya, POST /ocr/paddle_onnx, POST /translate/deep, POST /inpaint/lama_v2, "
        "POST /inpaint/lama_v2/unload, POST /inpaint/lama_mpe, POST /inpaint/lama_mpe/unload, "
        "POST /inpaint/aot, POST /inpaint/aot/unload, "
        "POST /textdetector/ctd/detect, POST /textdetector/paddle/detect, POST /textdetector/surya/detect, GET /device, "
        "POST /device/set, GET /device/cuda_diagnostics"
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[AI Backend] Stopping...")
    finally:
        stop_event.set()
        server.server_close()


def _warmup_safe(state: AppState) -> None:
    try:
        state.manga_ocr.warmup()
    except Exception:
        traceback.print_exc()


def _health_snapshot_worker(state: AppState, stop_event: threading.Event) -> None:
    while not stop_event.is_set():
        try:
            _set_health_snapshot(state, _build_health_snapshot(state))
        except Exception:
            traceback.print_exc()
        stop_event.wait(HEALTH_SNAPSHOT_REFRESH_SECS)
