"""
File: modules/ai_backend/ocr/easy.py

Purpose:
Lazy EasyOCR backend service used by the Rust translation tab.

Main responsibilities:
- load EasyOCR readers on demand from the weight directory Rust downloaded
  (`easyocr_model_dir`), strictly offline (`download_enabled=False`);
- synchronize the reader device with backend AI device settings;
- run OCR requests and return normalized text payloads.

Notes:
Rust owns the EasyOCR weights (pinned GitHub release zips, one shared
directory holding the `craft_mlt_25k` detector once and every downloaded
recognizer) and the language-code canonicalization; the language list arrives
here in EasyOCR's own codes. easyocr 1.7.2 checks each file's md5 on load and,
with downloads disabled, raises `FileNotFoundError` for a missing or corrupt
file instead of fetching it.
"""

from __future__ import annotations

import functools
import io
import logging
import threading
from pathlib import Path
from typing import Any

from ..runtime.health_snapshot import HealthSnapshot
from ..runtime.model_manager import LoadedModelManager
from ..runtime.torch_device import clear_torch_cache, resolve_selected_backend_device

log = logging.getLogger(__name__)

# ============================================================================
# EASY OCR SERVICE
# ----------------------------------------------------------------------------
# Что в файле:
# - lazy init и health для `easyocr.Reader`.
# - OCR распознавание из image bytes.
# - Синхронизация устройства с backend-настройкой `General.ai_device`.
# - Runtime-оптимизации:
#   1) декод изображения выполняется вне service-lock,
#   2) по возможности используется `cv2.imdecode` (быстрый путь), fallback на PIL.
# ============================================================================


#: `General.ai_device` resolution with this service's device policy (owner:
#: `runtime.torch_device`); patched by name in the test suite.
_resolve_selected_backend_device = functools.partial(resolve_selected_backend_device, accept_mps=False)

#: Allocator-cache release with this service's policy (owner: `runtime.torch_device`);
#: patched by name in the test suite.
_clear_torch_cache = functools.partial(clear_torch_cache, owner="EasyOCR")


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
        # Published copy of the health fields; `health()` never takes `_lock`,
        # which a reader load (and inference) holds (S-014).
        self._health = HealthSnapshot(ready=False, langs=[], device=self._device, last_error=None)

    def health(self) -> dict[str, Any]:
        """Return `{"ready", "langs", "device", "last_error"}` for the health snapshot.

        Reads only the published snapshot, so it answers while a load or an
        inference holds the instance lock.
        """
        return self._health.read()

    def _publish_health_locked(self) -> None:
        """Republish the health fields from the guarded state; caller holds `self._lock`."""
        self._health.publish(
            ready=self._reader is not None,
            langs=list(self._langs or ()),
            device=self._device,
            last_error=self._last_error,
        )

    def warmup(self, *, model_dir: str, langs: str = "ko") -> None:
        """Load the reader for `langs` from `model_dir` and run it once on a blank image.

        Raises what `recognize_image_bytes` raises for the load.
        """
        selected_device = _resolve_selected_backend_device(self._device)
        model_key = self._model_key_for(self._parse_langs(langs), selected_device, model_dir)
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
        )
        try:
            with self._lock:
                reader = self._ensure_loaded_locked(langs, model_dir)
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
        model_dir: str,
        join_newlines: bool = True,
        reflect_strings: bool = False,
        langs: str = "ko",
    ) -> dict[str, Any]:
        """Recognize `image_bytes` and return `{"lines", "text"}`.

        `model_dir` is the absolute EasyOCR weight directory Rust downloaded;
        `langs` is a comma-separated list of canonical EasyOCR language codes.
        Raises `ValueError` for an empty language list or a relative
        `model_dir`, `FileNotFoundError` ("not downloaded") for a missing
        directory or a missing / md5-mismatched weight file, and `RuntimeError`
        for a missing package or any other failed load.
        """
        rgb_arr = self._decode_image_rgb(image_bytes)
        selected_device = _resolve_selected_backend_device(self._device)
        model_key = self._model_key_for(self._parse_langs(langs), selected_device, model_dir)
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
        )
        try:
            with self._lock:
                reader = self._ensure_loaded_locked(langs, model_dir)
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

    def _ensure_loaded_locked(self, langs_raw: str, model_dir: str):
        """Return the reader for `langs_raw` from `model_dir`, loading it if needed.

        Caller holds `self._lock`. A resident reader under another key is
        dropped (and reported unloaded) first. Never downloads.
        """
        langs = self._parse_langs(langs_raw)
        selected_device = _resolve_selected_backend_device(self._device)
        selected_gpu = self._easyocr_gpu_value(selected_device)
        requested_key = self._model_key_for(langs, selected_device, model_dir)
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
            self._publish_health_locked()
            _clear_torch_cache()
            if previous_key is not None:
                self._model_manager.mark_unloaded(previous_key)

        try:
            storage_dir = _require_model_dir(model_dir)
        except Exception as exc:
            self._last_error = str(exc)
            self._publish_health_locked()
            raise

        try:
            import easyocr  # type: ignore
        except Exception as exc:  # pragma: no cover - runtime dependency
            self._last_error = f"EasyOCR package is not available: {exc}"
            self._publish_health_locked()
            raise RuntimeError(self._last_error) from exc

        reader = None
        init_exception: Exception | None = None
        try:
            load_attempts: list[tuple[Any, str]] = [(selected_gpu, selected_device)]
            if selected_gpu is not False:
                load_attempts.append((False, "cpu"))

            for gpu_value, device_value in load_attempts:
                try:
                    reader = self._create_reader(
                        easyocr_module=easyocr,
                        langs=langs,
                        model_dir=storage_dir,
                        gpu=gpu_value,
                    )
                    selected_device = device_value
                    selected_gpu = gpu_value
                    break
                except FileNotFoundError:
                    # A missing or md5-mismatched weight file (downloads are
                    # disabled): another device cannot fix it.
                    raise
                except Exception as exc:
                    init_exception = exc
                    log.exception(
                        "EasyOCR reader init failed langs=%s device=%s model_dir=%s",
                        ",".join(langs),
                        device_value,
                        storage_dir,
                    )

            if reader is None and init_exception is not None:
                raise init_exception
        except FileNotFoundError as exc:
            self._reader = None
            self._langs = None
            self._last_error = f"EasyOCR model is not downloaded: {exc}"
            self._publish_health_locked()
            raise FileNotFoundError(self._last_error) from exc
        except Exception as exc:
            self._reader = None
            self._langs = None
            self._last_error = f"EasyOCR init failed: {exc}"
            self._publish_health_locked()
            raise RuntimeError(self._last_error) from exc

        self._reader = reader
        self._langs = tuple(langs)
        self._device = selected_device
        self._gpu = selected_gpu
        self._reader_model_key = requested_key
        self._last_error = None
        self._publish_health_locked()
        return reader

    def _unload_key(self, model_key: str) -> bool:
        with self._lock:
            if self._reader is None or self._reader_model_key != model_key:
                return False
            self._reader = None
            self._langs = None
            self._reader_model_key = None
            self._publish_health_locked()
            _clear_torch_cache()
            self._model_manager.mark_unloaded(model_key)
            return True

    @staticmethod
    def _model_key_for(langs: list[str], device: str, model_dir: str) -> str:
        """Lease key of a reader: languages, device and weight directory are its identity."""
        return f"easyocr:{','.join(langs)}:{device}:{model_dir}"

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
    def _parse_langs(raw: str) -> list[str]:
        """Split a comma-separated list of canonical EasyOCR codes, order kept, duplicates dropped.

        No aliasing: Rust (`ms_sysprobe::ai_models::easyocr`) is the one owner
        of the code canonicalization. Raises `ValueError` for an empty list.
        """
        unique: list[str] = []
        for code in (part.strip() for part in str(raw or "").split(",")):
            if code and code not in unique:
                unique.append(code)
        if not unique:
            raise ValueError("EasyOCR language list is empty.")
        return unique

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
        model_dir: str,
        gpu: Any,
    ):
        """Build an `easyocr.Reader` that loads from `model_dir` and never downloads."""
        return easyocr_module.Reader(
            langs,
            model_storage_directory=model_dir,
            download_enabled=False,
            gpu=gpu,
        )


def _require_model_dir(model_dir: str) -> str:
    """Return `model_dir` when it is an existing absolute directory.

    Checked BEFORE `easyocr.Reader`, which would otherwise create a missing
    directory itself and report a missing weight file instead. Raises
    `ValueError` for an empty or relative path and `FileNotFoundError` ("not
    downloaded") for a missing directory.
    """
    normalized = str(model_dir or "").strip()
    if not normalized:
        raise ValueError("EasyOCR model directory is required.")
    path = Path(normalized)
    if not path.is_absolute():
        raise ValueError(f"EasyOCR model directory must be absolute: {normalized}")
    if not path.is_dir():
        raise FileNotFoundError(f"EasyOCR model is not downloaded: directory does not exist: {normalized}")
    return normalized
