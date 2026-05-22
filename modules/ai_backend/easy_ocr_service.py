"""
File: modules/ai_backend/easy_ocr_service.py

Purpose:
Lazy EasyOCR backend service used by the Rust translation tab.

Main responsibilities:
- load EasyOCR readers on demand;
- synchronize the reader device with backend AI device settings;
- run OCR requests and return normalized text payloads.
"""

from __future__ import annotations

import gc
import io
import os
import ssl
import sys
import threading
import traceback
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

# ============================================================================
# EASY OCR SERVICE
# ----------------------------------------------------------------------------
# Что в файле:
# - lazy init и health для `easyocr.Reader`.
# - OCR распознавание из image bytes.
# - Нормализация языковых кодов.
# - Синхронизация устройства с backend-настройкой `General.ai_device`.
# - SSL fallback для standalone Python на Windows:
#   при `CERTIFICATE_VERIFY_FAILED` переключаем `urllib` на CA-bundle из `certifi`
#   и повторяем инициализацию EasyOCR (без отключения проверки сертификатов).
# - Если проверка TLS всё ещё не проходит (часто из-за корпоративного MITM-сертификата,
#   которого нет в certifi), можно включить fallback на unverified HTTPS через
#   `MF_EASYOCR_INSECURE_SSL_FALLBACK=1` (по умолчанию включено).
# - На Windows-консолях с `cp1251` предотвращаем падение EasyOCR progress bar
#   (`UnicodeEncodeError` на символах блока), переводя stdout/stderr в `errors=replace`.
# - Runtime-оптимизации:
#   1) декод изображения выполняется вне service-lock,
#   2) по возможности используется `cv2.imdecode` (быстрый путь), fallback на PIL.
# ============================================================================


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


class EasyOcrService:
    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.Lock()
        self._model_manager = model_manager
        self._reader = None
        self._reader_model_key: str | None = None
        self._langs: tuple[str, ...] | None = None
        self._last_error: str | None = None
        self._detail = 1
        self._paragraph = False
        self._device = _resolve_selected_backend_device("cpu")
        self._gpu: Any = self._easyocr_gpu_value(self._device)

    def health(self) -> dict[str, Any]:
        with self._lock:
            return {
                "ready": self._reader is not None,
                "langs": list(self._langs or ()),
                "device": self._device,
                "last_error": self._last_error,
            }

    def warmup(self, *, langs: str = "ko") -> None:
        selected_device = _resolve_selected_backend_device(self._device)
        model_key = self._model_key_for(langs, selected_device)
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
        )
        try:
            with self._lock:
                reader = self._ensure_loaded_locked(langs)
            if lease.needs_load:
                lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
            import numpy as np  # type: ignore

            dummy = np.zeros((8, 8, 3), dtype="uint8")
            reader.readtext(dummy, detail=0, paragraph=False)
        except Exception:
            if lease.needs_load:
                lease.mark_load_failed()
            raise
        finally:
            lease.release()

    def recognize_image_bytes(
        self,
        image_bytes: bytes,
        *,
        join_newlines: bool = True,
        reflect_strings: bool = False,
        langs: str = "ko",
    ) -> dict[str, Any]:
        rgb_arr = self._decode_image_rgb(image_bytes)
        selected_device = _resolve_selected_backend_device(self._device)
        model_key = self._model_key_for(langs, selected_device)
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
        )
        try:
            with self._lock:
                reader = self._ensure_loaded_locked(langs)
                result = reader.readtext(
                    rgb_arr,
                    detail=self._detail,
                    paragraph=self._paragraph,
                )
            if lease.needs_load:
                lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
        except Exception:
            if lease.needs_load:
                lease.mark_load_failed()
            raise
        finally:
            lease.release()

        lines = self._extract_lines(result)
        if reflect_strings:
            lines.reverse()

        output_text = "\n".join(lines) if join_newlines else " ".join(lines)
        return {
            "lines": lines,
            "text": output_text.strip(),
        }

    def _ensure_loaded_locked(self, langs_raw: str):
        langs = self._parse_langs(langs_raw)
        selected_device = _resolve_selected_backend_device(self._device)
        selected_gpu = self._easyocr_gpu_value(selected_device)
        requested_key = self._model_key_for(langs, selected_device)
        if (
            self._reader is not None
            and self._langs == tuple(langs)
            and self._device == selected_device
            and self._gpu == selected_gpu
            and self._reader_model_key == requested_key
        ):
            return self._reader

        previous_key = self._reader_model_key
        if self._reader is not None and previous_key != requested_key:
            self._reader = None
            self._langs = None
            self._reader_model_key = None
            _clear_torch_cache()
            if previous_key is not None:
                self._model_manager.mark_unloaded(previous_key)

        try:
            import easyocr  # type: ignore
        except Exception as exc:  # pragma: no cover - runtime dependency
            self._last_error = f"EasyOCR package is not available: {exc}"
            raise RuntimeError(self._last_error) from exc

        _ensure_stdio_unicode_safe()
        model_dir = self._resolve_model_dir()
        reader = None
        init_exception: Exception | None = None
        tried_ssl_certifi_fallback = False
        tried_ssl_insecure_fallback = False
        try:
            load_attempts: list[tuple[Any, str]] = [(selected_gpu, selected_device)]
            if selected_gpu is not False:
                load_attempts.append((False, "cpu"))

            for gpu_value, device_value in load_attempts:
                try:
                    reader = self._create_reader(
                        easyocr_module=easyocr,
                        langs=langs,
                        model_dir=model_dir,
                        gpu=gpu_value,
                    )
                    selected_device = device_value
                    selected_gpu = gpu_value
                    break
                except Exception as exc:
                    init_exception = exc
                    traceback.print_exc()
                    if not _is_ssl_cert_verification_error(exc):
                        continue

                    fallback_ok = False
                    fallback_note: str | None = None
                    if not tried_ssl_certifi_fallback:
                        tried_ssl_certifi_fallback = True
                        fallback_ok, fallback_note = _configure_certifi_ssl_for_urllib()
                        if fallback_ok:
                            print("[EasyOCR] SSL CA fallback via certifi activated.")
                    elif (
                        not tried_ssl_insecure_fallback
                        and _allow_insecure_ssl_fallback()
                    ):
                        tried_ssl_insecure_fallback = True
                        fallback_ok, fallback_note = _configure_insecure_ssl_for_urllib()
                        if fallback_ok:
                            print(
                                "[EasyOCR] WARNING: SSL verification disabled for model download fallback."
                            )

                    if fallback_ok:
                        try:
                            reader = self._create_reader(
                                easyocr_module=easyocr,
                                langs=langs,
                                model_dir=model_dir,
                                gpu=gpu_value,
                            )
                            selected_device = device_value
                            selected_gpu = gpu_value
                            break
                        except Exception as retry_exc:
                            init_exception = retry_exc
                            traceback.print_exc()
                    elif fallback_note is not None:
                        print(f"[EasyOCR] SSL fallback unavailable: {fallback_note}")

            if reader is None and init_exception is not None:
                raise init_exception
        except Exception as exc:  # pragma: no cover - runtime dependency
            self._reader = None
            self._langs = None
            self._last_error = f"EasyOCR init failed: {exc}"
            raise RuntimeError(self._last_error) from exc

        self._reader = reader
        self._langs = tuple(langs)
        self._device = selected_device
        self._gpu = selected_gpu
        self._reader_model_key = requested_key
        self._last_error = None
        return reader

    def _unload_key(self, model_key: str) -> bool:
        with self._lock:
            if self._reader is None or self._reader_model_key != model_key:
                return False
            self._reader = None
            self._langs = None
            self._reader_model_key = None
            _clear_torch_cache()
            self._model_manager.mark_unloaded(model_key)
            return True

    @staticmethod
    def _model_key_for(langs_raw: str | list[str], device: str) -> str:
        if isinstance(langs_raw, list):
            normalized_langs = langs_raw
        else:
            normalized_langs = EasyOcrService._parse_langs(langs_raw)
        return f"easyocr:{','.join(normalized_langs)}:{device}"

    @staticmethod
    def _easyocr_gpu_value(device: str) -> Any:
        val = str(device).strip().lower()
        if not val or val == "cpu":
            return False
        if val == "cuda" or val.startswith("cuda:"):
            return val
        # EasyOCR не гарантирует поддержку MPS/XPU в текущем пайплайне.
        return False

    @staticmethod
    def _extract_lines(result: Any) -> list[str]:
        lines: list[str] = []
        if not isinstance(result, (list, tuple)):
            return lines
        for item in result:
            if isinstance(item, str):
                cleaned = item.strip()
                if cleaned:
                    lines.append(cleaned)
                continue
            if not isinstance(item, (list, tuple)) or len(item) < 2:
                continue
            text = item[1]
            if isinstance(text, str):
                cleaned = text.strip()
                if cleaned:
                    lines.append(cleaned)
        return lines

    @staticmethod
    def _resolve_model_dir() -> str | None:
        return None

    @staticmethod
    def _parse_langs(raw: str) -> list[str]:
        pieces = [part.strip().lower() for part in str(raw or "").split(",")]
        langs = [EasyOcrService._normalize_lang_code(code) for code in pieces if code]
        if not langs:
            return ["ko"]
        unique: list[str] = []
        seen: set[str] = set()
        for code in langs:
            if code in seen:
                continue
            seen.add(code)
            unique.append(code)
        return unique

    @staticmethod
    def _normalize_lang_code(code: str) -> str:
        if code in ("ko", "kor", "korean"):
            return "ko"
        if code in ("ja", "jpn", "jp", "japan", "japanese"):
            return "ja"
        if code in ("zh", "ch", "chinese", "zh-cn", "zh-hans"):
            return "ch_sim"
        if code in ("zh-tw", "zh-hant", "chinese_cht", "ch_tra"):
            return "ch_tra"
        return code

    @staticmethod
    def _decode_image_rgb(image_bytes: bytes):
        import numpy as np  # type: ignore

        # Быстрый путь: OpenCV-decode в BGR и конвертация в RGB для EasyOCR.
        try:
            import cv2  # type: ignore

            encoded = np.frombuffer(image_bytes, dtype=np.uint8)
            bgr = cv2.imdecode(encoded, cv2.IMREAD_COLOR)
            if bgr is None:
                raise RuntimeError("cv2.imdecode returned None")
            return cv2.cvtColor(bgr, cv2.COLOR_BGR2RGB)
        except Exception:
            pass

        # Надёжный fallback для окружений без OpenCV.
        from PIL import Image

        with Image.open(io.BytesIO(image_bytes)) as img:
            rgb = img.convert("RGB")
            return np.array(rgb)

    @staticmethod
    def _create_reader(
        *,
        easyocr_module: Any,
        langs: list[str],
        model_dir: str | None,
        gpu: Any,
    ):
        return easyocr_module.Reader(
            langs,
            model_storage_directory=model_dir,
            download_enabled=True,
            gpu=gpu,
        )


_CERTIFI_SSL_CONFIG_LOCK = threading.Lock()
_CERTIFI_SSL_CONFIG_DONE = False
_CERTIFI_SSL_CONFIG_ERROR: str | None = None
_INSECURE_SSL_CONFIG_DONE = False
_INSECURE_SSL_CONFIG_ERROR: str | None = None
_STDIO_UNICODE_SAFE_DONE = False


def _is_ssl_cert_verification_error(exc: BaseException) -> bool:
    checked: set[int] = set()
    cursor: BaseException | None = exc
    while cursor is not None and id(cursor) not in checked:
        checked.add(id(cursor))
        text = str(cursor)
        name = type(cursor).__name__
        if (
            "CERTIFICATE_VERIFY_FAILED" in text
            or "unable to get local issuer certificate" in text
            or name == "SSLCertVerificationError"
        ):
            return True
        next_exc = cursor.__cause__
        if next_exc is None and not getattr(cursor, "__suppress_context__", False):
            next_exc = cursor.__context__
        cursor = next_exc
    return False


def _configure_certifi_ssl_for_urllib() -> tuple[bool, str | None]:
    global _CERTIFI_SSL_CONFIG_DONE
    global _CERTIFI_SSL_CONFIG_ERROR

    with _CERTIFI_SSL_CONFIG_LOCK:
        if _CERTIFI_SSL_CONFIG_DONE:
            return True, None
        if _CERTIFI_SSL_CONFIG_ERROR is not None:
            return False, _CERTIFI_SSL_CONFIG_ERROR

        try:
            import certifi  # type: ignore
        except Exception as exc:
            _CERTIFI_SSL_CONFIG_ERROR = f"certifi import failed: {exc}"
            return False, _CERTIFI_SSL_CONFIG_ERROR

        cafile = certifi.where()
        if not cafile or not os.path.isfile(cafile):
            _CERTIFI_SSL_CONFIG_ERROR = f"certifi bundle path is invalid: {cafile!r}"
            return False, _CERTIFI_SSL_CONFIG_ERROR

        def _certifi_https_context(*args: Any, **kwargs: Any):
            kwargs.setdefault("cafile", cafile)
            return ssl.create_default_context(*args, **kwargs)

        try:
            ssl._create_default_https_context = _certifi_https_context  # type: ignore[attr-defined]
        except Exception as exc:
            _CERTIFI_SSL_CONFIG_ERROR = f"failed to patch ssl default context: {exc}"
            return False, _CERTIFI_SSL_CONFIG_ERROR

        os.environ.setdefault("SSL_CERT_FILE", cafile)
        os.environ.setdefault("REQUESTS_CA_BUNDLE", cafile)
        os.environ.setdefault("CURL_CA_BUNDLE", cafile)

        _CERTIFI_SSL_CONFIG_DONE = True
        _CERTIFI_SSL_CONFIG_ERROR = None
        return True, None


def _configure_insecure_ssl_for_urllib() -> tuple[bool, str | None]:
    global _INSECURE_SSL_CONFIG_DONE
    global _INSECURE_SSL_CONFIG_ERROR

    with _CERTIFI_SSL_CONFIG_LOCK:
        if _INSECURE_SSL_CONFIG_DONE:
            return True, None
        if _INSECURE_SSL_CONFIG_ERROR is not None:
            return False, _INSECURE_SSL_CONFIG_ERROR

        try:
            ssl._create_default_https_context = ssl._create_unverified_context  # type: ignore[attr-defined]
        except Exception as exc:
            _INSECURE_SSL_CONFIG_ERROR = f"failed to configure unverified ssl context: {exc}"
            return False, _INSECURE_SSL_CONFIG_ERROR

        os.environ.setdefault("PYTHONHTTPSVERIFY", "0")
        _INSECURE_SSL_CONFIG_DONE = True
        _INSECURE_SSL_CONFIG_ERROR = None
        return True, None


def _allow_insecure_ssl_fallback() -> bool:
    raw = str(os.environ.get("MF_EASYOCR_INSECURE_SSL_FALLBACK", "1")).strip().lower()
    return raw not in ("0", "false", "no", "off")


def _ensure_stdio_unicode_safe() -> None:
    global _STDIO_UNICODE_SAFE_DONE
    if _STDIO_UNICODE_SAFE_DONE:
        return
    _STDIO_UNICODE_SAFE_DONE = True

    for stream in (getattr(sys, "stdout", None), getattr(sys, "stderr", None)):
        if stream is None or not hasattr(stream, "reconfigure"):
            continue
        try:
            stream.reconfigure(errors="replace")
        except Exception:
            continue


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
    value = value.strip().lower()
    if value == "not-selected":
        return None
    return value or None


def _safe_available_devices() -> set[str]:
    try:
        return set(AIDevice.detect_available_devices())
    except Exception:
        return {"cpu"}


def _normalize_backend_device(raw: str, fallback: str) -> str:
    val = str(raw or "").strip().lower()
    if val == "cpu" or val == "cuda" or val.startswith("cuda:"):
        return val
    return str(fallback or "cpu").strip().lower() or "cpu"
