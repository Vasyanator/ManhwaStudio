"""
File: modules/ai_backend/lama_inpaint_service.py

Purpose:
HTTP-friendly LaMa V2 inpaint service for the Python AI backend.

Main responsibilities:
- lazy-load `InpainterV2` and keep one active checkpoint/device pair in memory;
- decode input image + mask from PNG bytes and return raw PNG bytes result;
- normalize refine parameters and requested checkpoint name from the HTTP payload;
- expose health information about available and currently active LaMa checkpoints.

Key structures:
- `LamaInpaintService`

Key functions:
- `health()`
- `inpaint_image_bytes()`
- `unload()`

Notes:
- Checkpoints are discovered from `ManhwaStudio_AI_Models/Torch/LaMa/models`
  (`.ckpt` and `.pt`).
- Switching checkpoint or device triggers inpainter reload under the service lock.
"""

from __future__ import annotations

import importlib.util
import io
import sys
import threading
from pathlib import Path
from types import ModuleType
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    import numpy as np

try:
    from ai_device import AIDevice
except Exception:
    from modules.ai_device import AIDevice

try:
    from config import LAMA_DIR
except Exception:
    LAMA_DIR = str(
        Path(__file__).resolve().parents[2] / "ManhwaStudio_AI_Models" / "Torch" / "LaMa"
    )

try:
    from config import UserConfig
except Exception:
    UserConfig = None

try:
    from config import program_dir as _PROGRAM_DIR
except Exception:
    _PROGRAM_DIR = Path(__file__).resolve().parents[2]

from .model_manager import LoadedModelManager


# ============================================================================
# LAMA V2 INPAINT SERVICE
# ----------------------------------------------------------------------------
# Что в файле:
# - `LamaInpaintService`: lazy-load обёртка для `InpainterV2` (модель как в
#   `ui_new/tools/region_edit_ai.py`) с HTTP-friendly API.
# - Декодирование PNG-изображений (RGB image + mask) и кодирование raw PNG.
# - Нормализация параметров `refine` (`n_iters`, `max_scales`, `px_budget`).
# - Синхронизация устройства с backend-настройкой `General.ai_device`
#   через `AIDevice`.
# - Динамический импорт локального runtime-модуля
#   `modules/ai_backend/lama_v2_runtime_inpainter.py` через путь к файлу
#   (без зависимости от `lama_files/inpainter_v2.py` и `lama_modernised/*`).
# ============================================================================

_INPAINTER_MODULE_NAME = "mf_lama_inpainter_v2_runtime"


def _to_int(value: Any, default: int) -> int:
    try:
        return int(value)
    except Exception:
        return default


def _to_bool(value: Any, default: bool) -> bool:
    if isinstance(value, bool):
        return value
    if value is None:
        return default
    if isinstance(value, (int, float)):
        return bool(value)
    text = str(value).strip().lower()
    if text in {"1", "true", "yes", "y", "on"}:
        return True
    if text in {"0", "false", "no", "n", "off"}:
        return False
    return default


class LamaInpaintService:
    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._inpainter: Any = None
        self._inpainter_cls: type | None = None
        self._active_device = "cpu"
        self._active_checkpoint_name: str | None = None
        self._active_model_key: str | None = None
        self._last_error: str | None = None
        self._module_source_path: Path | None = None
        self._model_dir = Path(str(LAMA_DIR))

    def health(self) -> dict[str, Any]:
        with self._lock:
            ckpts = self._collect_checkpoints()
            checkpoint_names = [path.name for path in ckpts]
            default_checkpoint = self._pick_default_checkpoint_name(ckpts)
            return {
                "ready": self._inpainter is not None,
                "model": "lama_v2",
                "device": self._active_device,
                "model_dir": str(self._model_dir),
                "config_exists": (self._model_dir / "config.yaml").is_file(),
                "checkpoint_count": len(ckpts),
                "available_models": checkpoint_names,
                "selected_model": self._active_checkpoint_name or default_checkpoint,
                "loaded_model": self._active_checkpoint_name,
                "module_source_path": str(self._module_source_path) if self._module_source_path else None,
                "last_error": self._last_error,
                "memory": self._safe_memory_stats_locked(),
            }

    def inpaint_image_bytes(
        self,
        image_bytes: bytes,
        mask_bytes: bytes,
        *,
        params: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        image_rgb = self._decode_image_rgb(image_bytes)
        mask_u8 = self._decode_mask(mask_bytes, expected_hw=image_rgb.shape[:2])
        normalized = self._normalize_params(params)
        device = _resolve_selected_backend_device(self._active_device)
        checkpoint_name = self._resolve_checkpoint_name(normalized.get("model_name"))
        model_key = self._model_key_for(device, checkpoint_name)
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
        )

        with self._lock:
            try:
                inpainter = self._ensure_inpainter_locked(device, checkpoint_name)
                inpainter.set_refine(
                    normalized["refine"],
                    n_iters=normalized["n_iters"],
                    max_scales=normalized["max_scales"],
                    px_budget=normalized["px_budget"],
                )
                out_rgb = inpainter(image_rgb, mask_u8)
                if lease.needs_load:
                    lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
                self._last_error = None
            except Exception as exc:
                if lease.needs_load:
                    lease.mark_load_failed()
                self._last_error = str(exc)
                raise
            finally:
                lease.release()

        return {
            "image_png": _encode_png_bytes_rgb(out_rgb),
            "source_size": [int(image_rgb.shape[1]), int(image_rgb.shape[0])],
            "device": self._active_device,
            "refine": bool(normalized["refine"]),
            "model_name": checkpoint_name,
        }

    def unload(self) -> bool:
        with self._lock:
            if self._inpainter is None:
                return False
            current_key = self._active_model_key
            try:
                unload = getattr(self._inpainter, "unload", None)
                if callable(unload):
                    unload(clear_cache=True)
            finally:
                self._inpainter = None
                self._active_checkpoint_name = None
                self._active_model_key = None
                if current_key is not None:
                    self._model_manager.mark_unloaded(current_key)
            return True

    def _ensure_inpainter_locked(self, device: str, checkpoint_name: str):
        requested_key = self._model_key_for(device, checkpoint_name)
        if (
            self._inpainter is not None
            and self._active_device == device
            and self._active_checkpoint_name == checkpoint_name
            and self._active_model_key == requested_key
        ):
            return self._inpainter

        previous_key = self._active_model_key
        if self._inpainter is not None:
            try:
                unload = getattr(self._inpainter, "unload", None)
                if callable(unload):
                    unload(clear_cache=True)
            except Exception:
                pass
            self._inpainter = None
            if previous_key is not None:
                self._model_manager.mark_unloaded(previous_key)

        inpainter_cls = self._load_inpainter_class_locked()
        self._inpainter = inpainter_cls(
            checkpoint_dir=str(self._model_dir),
            checkpoint_name=checkpoint_name,
            device=device,
            refine=False,
            verbose=False,
        )
        self._active_device = device
        self._active_checkpoint_name = checkpoint_name
        self._active_model_key = requested_key
        return self._inpainter

    def _unload_key(self, model_key: str) -> bool:
        with self._lock:
            if self._inpainter is None or self._active_model_key != model_key:
                return False
            return self.unload()

    @staticmethod
    def _model_key_for(device: str, checkpoint_name: str) -> str:
        return f"lama_v2:{device}:{checkpoint_name}"

    def _load_inpainter_class_locked(self) -> type:
        if self._inpainter_cls is not None:
            return self._inpainter_cls

        module_path = self._resolve_inpainter_source_path()
        self._prepare_runtime_paths(module_path)

        module = sys.modules.get(_INPAINTER_MODULE_NAME)
        if not isinstance(module, ModuleType):
            spec = importlib.util.spec_from_file_location(_INPAINTER_MODULE_NAME, module_path)
            if spec is None or spec.loader is None:
                raise RuntimeError(f"Не удалось подготовить импорт модуля: {module_path}")
            module = importlib.util.module_from_spec(spec)
            sys.modules[_INPAINTER_MODULE_NAME] = module
            spec.loader.exec_module(module)

        cls = getattr(module, "InpainterV2", None)
        if cls is None:
            raise RuntimeError(f"В модуле '{module_path}' не найден класс InpainterV2")

        self._inpainter_cls = cls
        self._module_source_path = module_path
        return cls

    def _resolve_inpainter_source_path(self) -> Path:
        root = Path(str(_PROGRAM_DIR))
        source = root / "modules" / "ai_backend" / "lama_v2_runtime_inpainter.py"
        if source.is_file():
            return source
        raise FileNotFoundError(
            "Локальный runtime-модуль LaMa V2 не найден. "
            f"Ожидался путь: {source}"
        )

    def _prepare_runtime_paths(self, module_path: Path) -> None:
        # Важно: добавляем локальные runtime-пути для зависимостей локального
        # модуля `lama_v2_runtime_inpainter.py`. `saicinpainting` берём из
        # backend-runtime бандла, чтобы endpoint не зависел от внешних репозиториев.
        root = Path(str(_PROGRAM_DIR))
        runtime_bundle = root / "modules" / "ai_backend" / "lama_runtime_bundle"
        runtime_paths = [
            runtime_bundle,
            module_path.parent,
            root,
        ]
        for path in runtime_paths:
            path_str = str(path)
            if path.is_dir() and path_str not in sys.path:
                sys.path.insert(0, path_str)

    def _resolve_checkpoint_name(self, requested_name: str | None = None) -> str:
        config_path = self._model_dir / "config.yaml"
        if not config_path.is_file():
            raise FileNotFoundError(f"Не найден файл конфигурации LaMa: {config_path}")
        checkpoints = self._collect_checkpoints()
        if not checkpoints:
            raise FileNotFoundError(
                f"В папке '{self._model_dir / 'models'}' не найдено .ckpt/.pt файлов"
            )
        if requested_name is not None:
            normalized_name = requested_name.strip()
            if normalized_name:
                checkpoint_names = {path.name for path in checkpoints}
                if normalized_name not in checkpoint_names:
                    raise FileNotFoundError(
                        f"Модель LaMa '{normalized_name}' не найдена в '{self._model_dir / 'models'}'"
                    )
                return normalized_name
        default_name = self._pick_default_checkpoint_name(checkpoints)
        if default_name is None:
            raise FileNotFoundError(
                f"В папке '{self._model_dir / 'models'}' не найдено .ckpt/.pt файлов"
            )
        return default_name

    def _collect_checkpoints(self) -> list[Path]:
        models_dir = self._model_dir / "models"
        if not models_dir.is_dir():
            return []
        return sorted(
            path
            for path in models_dir.iterdir()
            if path.is_file() and path.suffix.lower() in {".ckpt", ".pt"}
        )

    @staticmethod
    def _pick_default_checkpoint_name(checkpoints: list[Path]) -> str | None:
        for path in checkpoints:
            if path.name == "best.ckpt":
                return path.name
        for path in checkpoints:
            if path.name == "best.pt":
                return path.name
        for path in checkpoints:
            if path.name == "lama_large_512px.ckpt.ckpt":
                return path.name
        for path in checkpoints:
            if path.name == "lama_large_512px.ckpt":
                return path.name
        for path in checkpoints:
            if path.name == "lama_large_512px.pt":
                return path.name
        if checkpoints:
            return checkpoints[0].name
        return None

    def _normalize_params(self, params: dict[str, Any] | None) -> dict[str, Any]:
        merged: dict[str, Any] = {}
        if isinstance(params, dict):
            merged.update(params)

        out: dict[str, Any] = {}
        out["refine"] = _to_bool(merged.get("refine"), False)
        out["n_iters"] = max(5, min(50, _to_int(merged.get("n_iters"), 15)))
        out["max_scales"] = max(1, min(5, _to_int(merged.get("max_scales"), 3)))
        out["px_budget"] = max(
            500_000,
            min(4_000_000, _to_int(merged.get("px_budget"), 1_000_000)),
        )
        out["model_name"] = _normalize_optional_model_name(merged.get("model_name"))
        return out

    def _decode_image_rgb(self, image_bytes: bytes) -> np.ndarray:
        np = _np()
        cv2 = _maybe_cv2()
        if cv2 is not None:
            try:
                encoded = np.frombuffer(image_bytes, dtype=np.uint8)
                decoded = cv2.imdecode(encoded, cv2.IMREAD_UNCHANGED)
                if decoded is not None:
                    if decoded.ndim == 2:
                        return np.ascontiguousarray(cv2.cvtColor(decoded, cv2.COLOR_GRAY2RGB))
                    if decoded.ndim == 3 and decoded.shape[2] == 3:
                        return np.ascontiguousarray(cv2.cvtColor(decoded, cv2.COLOR_BGR2RGB))
                    if decoded.ndim == 3 and decoded.shape[2] == 4:
                        return np.ascontiguousarray(cv2.cvtColor(decoded, cv2.COLOR_BGRA2RGB))
            except Exception:
                pass

        from PIL import Image

        with Image.open(io.BytesIO(image_bytes)) as img:
            rgb = np.array(img.convert("RGB"), dtype=np.uint8)
            return np.ascontiguousarray(rgb)

    def _decode_mask(self, mask_bytes: bytes, *, expected_hw: tuple[int, int]) -> np.ndarray:
        np = _np()
        arr = _decode_image_any(mask_bytes)
        if arr.ndim == 3:
            if arr.shape[2] >= 4:
                mask = arr[..., 3]
            else:
                mask = np.max(arr[..., :3], axis=2)
        elif arr.ndim == 2:
            mask = arr
        else:
            raise ValueError("Некорректная маска: ожидается 2D/3D массив")

        mask = np.ascontiguousarray(mask.astype(np.uint8))
        if tuple(mask.shape[:2]) != tuple(expected_hw):
            raise ValueError(
                f"Размер маски {mask.shape[1]}x{mask.shape[0]} не совпадает с изображением "
                f"{expected_hw[1]}x{expected_hw[0]}"
            )
        return np.where(mask > 0, 255, 0).astype(np.uint8)

    def _safe_memory_stats_locked(self) -> dict[str, Any]:
        if self._inpainter is None:
            return {"model_loaded": False}
        get_stats = getattr(self._inpainter, "get_memory_stats", None)
        if callable(get_stats):
            try:
                raw = get_stats()
                if isinstance(raw, dict):
                    return raw
            except Exception:
                pass
        return {"model_loaded": True, "device": self._active_device}


def _maybe_cv2():
    try:
        import cv2  # type: ignore

        return cv2
    except Exception:
        return None


def _decode_image_any(image_bytes: bytes) -> np.ndarray:
    np = _np()
    cv2 = _maybe_cv2()
    if cv2 is not None:
        try:
            encoded = np.frombuffer(image_bytes, dtype=np.uint8)
            decoded = cv2.imdecode(encoded, cv2.IMREAD_UNCHANGED)
            if decoded is not None:
                return decoded
        except Exception:
            pass

    from PIL import Image

    with Image.open(io.BytesIO(image_bytes)) as img:
        if img.mode in {"RGBA", "RGB", "L"}:
            return np.array(img)
        return np.array(img.convert("RGBA"))


def _encode_png_bytes_rgb(image_rgb: np.ndarray) -> bytes:
    np = _np()
    if image_rgb.ndim != 3 or image_rgb.shape[2] != 3:
        raise ValueError("Ожидается RGB изображение (H, W, 3)")

    image_rgb = np.ascontiguousarray(image_rgb.astype(np.uint8))
    cv2 = _maybe_cv2()
    if cv2 is not None:
        bgr = cv2.cvtColor(image_rgb, cv2.COLOR_RGB2BGR)
        ok, encoded = cv2.imencode(".png", bgr)
        if not ok:
            raise RuntimeError("cv2.imencode('.png') вернул ошибку")
        return encoded.tobytes()

    from PIL import Image

    with io.BytesIO() as buffer:
        Image.fromarray(image_rgb, mode="RGB").save(buffer, format="PNG")
        return buffer.getvalue()


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
    if "mps" in available:
        return "mps"
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
    value = str(raw or "").strip().lower()
    if value in {"cpu", "mps", "cuda"}:
        return value
    if value.startswith("cuda:"):
        return value
    return str(fallback or "cpu").strip().lower() or "cpu"


def _np():
    try:
        import numpy as np  # type: ignore

        return np
    except Exception as exc:
        raise RuntimeError(
            "Для /inpaint/lama_v2 требуется пакет numpy. Установите зависимости backend."
        ) from exc


def _normalize_optional_model_name(value: Any) -> str | None:
    if not isinstance(value, str):
        return None
    normalized = Path(value.strip()).name.strip()
    return normalized or None
