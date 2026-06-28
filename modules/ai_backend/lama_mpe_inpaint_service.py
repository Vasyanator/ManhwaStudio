"""
File: modules/ai_backend/lama_mpe_inpaint_service.py

Purpose:
LaMa MPE inpainting service adapter for the Python AI backend.

Main responsibilities:
- load the LaMa MPE runtime lazily;
- synchronize model device with backend AI device settings;
- run inpainting requests and unload idle models through the model manager.
"""

from __future__ import annotations

import gc
import hashlib
import io
import threading
import urllib.request
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    import numpy as np
    import torch as torch_mod

try:
    from ai_device import AIDevice
except Exception:
    from modules.ai_device import AIDevice

try:
    from config import LAMA_MPE_DIR
except Exception:
    LAMA_MPE_DIR = str(
        Path(__file__).resolve().parents[2]
        / "ManhwaStudio_AI_Models"
        / "Torch"
        / "LaMa_MPE"
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
# LAMA MPE INPAINT SERVICE
# ----------------------------------------------------------------------------
# Что в файле:
# - `LamaMpeInpaintService`: lazy-load обёртка LaMa MPE для endpoint
#   `/inpaint/lama_mpe`.
# - Порт инференса из legacy `ui_new/tools/region_edit_lama_mpe.py`
#   (без Qt-слоя): resize->pad->MPE forward->композит по маске.
# - Автоподготовка веса `inpainting_lama_mpe.ckpt`:
#   SHA256-проверка, при отсутствии/порче скачивание в
#   `ManhwaStudio_AI_Models/Torch/LaMa_MPE`.
# - Декодирование PNG-изображений (RGB image + mask) и кодирование raw PNG.
# - Нормализация параметров endpoint (`inpaint_size`).
# - Синхронизация устройства с backend-настройкой `General.ai_device`
#   через `AIDevice`.
# ============================================================================

_LAMA_MPE_URL = (
    "https://github.com/zyddnys/manga-image-translator/releases/download/beta-0.3/"
    "inpainting_lama_mpe.ckpt"
)
_LAMA_MPE_SHA256 = "d625aa1b3e0d0408acfd6928aa84f005867aa8dbb9162480346a4e20660786cc"
_LAMA_MPE_CKPT_NAME = "inpainting_lama_mpe.ckpt"


def _maybe_torch():
    try:
        import torch  # type: ignore

        return torch
    except Exception:
        return None


def _torch() -> torch_mod:
    try:
        import torch  # type: ignore

        return torch
    except Exception as exc:
        raise RuntimeError(
            "Для /inpaint/lama_mpe требуется пакет torch. Установите зависимости backend."
        ) from exc


def _np():
    try:
        import numpy as np  # type: ignore

        return np
    except Exception as exc:
        raise RuntimeError(
            "Для /inpaint/lama_mpe требуется пакет numpy. Установите зависимости backend."
        ) from exc


def _maybe_cv2():
    try:
        import cv2  # type: ignore

        return cv2
    except Exception:
        return None


def _cv2_required():
    cv2 = _maybe_cv2()
    if cv2 is None:
        raise RuntimeError("Для /inpaint/lama_mpe требуется пакет opencv-python (cv2).")
    return cv2


def _load_lama_mpe_loader():
    try:
        from lama_mpe import load_lama_mpe  # type: ignore

        return load_lama_mpe
    except Exception:
        try:
            from modules.lama_mpe import load_lama_mpe  # type: ignore

            return load_lama_mpe
        except Exception as exc:
            raise RuntimeError(
                "Не удалось импортировать load_lama_mpe из modules/lama_mpe.py"
            ) from exc


def _to_int(value: Any, default: int) -> int:
    try:
        return int(value)
    except Exception:
        return default


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _download_file(url: str, dst: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    tmp = dst.with_suffix(dst.suffix + ".part")
    with urllib.request.urlopen(url) as response, tmp.open("wb") as out:
        while True:
            chunk = response.read(8192)
            if not chunk:
                break
            out.write(chunk)
    tmp.replace(dst)


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


def _clear_torch_cache() -> None:
    torch = _maybe_torch()
    if torch is None:
        return
    gc.collect()
    try:
        if hasattr(torch, "cuda") and torch.cuda.is_available():
            torch.cuda.empty_cache()
            if hasattr(torch.cuda, "ipc_collect"):
                torch.cuda.ipc_collect()
    except Exception:
        pass
    try:
        if hasattr(torch, "mps") and hasattr(torch.mps, "empty_cache"):
            torch.mps.empty_cache()
    except Exception:
        pass


def _resize_keepasp(
    im,
    new_shape: int | tuple[int, int] | None = 640,
    *,
    scaleup: bool = True,
    interpolation=None,
    stride: int | None = None,
):
    cv2 = _cv2_required()
    shape = im.shape[:2]  # (h, w)

    if new_shape is None:
        target_shape = shape
    elif isinstance(new_shape, tuple):
        target_shape = new_shape
    else:
        target_shape = (new_shape, new_shape)

    r = min(target_shape[0] / shape[0], target_shape[1] / shape[1])
    if not scaleup:
        r = min(r, 1.0)

    new_unpad = (int(round(shape[1] * r)), int(round(shape[0] * r)))  # (w, h)
    if stride is not None:
        w, h = new_unpad
        new_w = w + (stride - (w % stride)) if w % stride != 0 else w
        new_h = h + (stride - (h % stride)) if h % stride != 0 else h
        new_unpad = (new_w, new_h)

    if shape[::-1] != new_unpad:
        if interpolation is None:
            interpolation = cv2.INTER_LINEAR
        im = cv2.resize(im, new_unpad, interpolation=interpolation)
    return im


class LamaMpeInpaintService:
    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._model: Any = None
        self._active_device = "cpu"
        self._active_model_key: str | None = None
        self._last_error: str | None = None
        self._model_dir = Path(str(LAMA_MPE_DIR))
        self._model_path = self._model_dir / _LAMA_MPE_CKPT_NAME

    def health(self) -> dict[str, Any]:
        with self._lock:
            return {
                "ready": self._model is not None,
                "model": "lama_mpe",
                "device": self._active_device,
                "model_path": str(self._model_path),
                "model_exists": self._model_path.is_file(),
                "last_error": self._last_error,
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
        model_key = self._model_key_for(device)
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
        )

        with self._lock:
            try:
                model = self._ensure_model_locked(device)
                out_rgb = self._inpaint_locked(
                    model,
                    image_rgb=image_rgb,
                    mask_u8=mask_u8,
                    inpaint_size=normalized["inpaint_size"],
                    device=device,
                )
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
            "inpaint_size": int(normalized["inpaint_size"]),
        }

    def unload(self) -> bool:
        with self._lock:
            if self._model is None:
                return False
            current_key = self._active_model_key
            try:
                to_fn = getattr(self._model, "to", None)
                if callable(to_fn):
                    to_fn("cpu")
            except Exception:
                pass
            self._model = None
            self._active_model_key = None
            _clear_torch_cache()
            if current_key is not None:
                self._model_manager.mark_unloaded(current_key)
            return True

    def _ensure_model_locked(self, device: str):
        requested_key = self._model_key_for(device)
        if (
            self._model is not None
            and self._active_device == device
            and self._active_model_key == requested_key
        ):
            return self._model

        previous_key = self._active_model_key
        self._model = None
        self._active_model_key = None
        _clear_torch_cache()
        if previous_key is not None:
            self._model_manager.mark_unloaded(previous_key)

        self._validate_runtime_layout_locked()
        ckpt_path = self._ensure_checkpoint_locked()
        load_lama_mpe = _load_lama_mpe_loader()
        self._model = load_lama_mpe(str(ckpt_path), device, use_mpe=True)
        self._active_device = device
        self._active_model_key = requested_key
        return self._model

    def _unload_key(self, model_key: str) -> bool:
        with self._lock:
            if self._model is None or self._active_model_key != model_key:
                return False
            return self.unload()

    @staticmethod
    def _model_key_for(device: str) -> str:
        return f"lama_mpe:{device}"

    def _validate_runtime_layout_locked(self) -> None:
        runtime_repo = Path(str(_PROGRAM_DIR)) / "lama_modernised"
        if not runtime_repo.is_dir():
            raise FileNotFoundError(
                "Папка lama_modernised не найдена. Установите репозиторий в корень программы."
            )

    def _ensure_checkpoint_locked(self) -> Path:
        if self._model_path.is_file():
            try:
                if _sha256(self._model_path) == _LAMA_MPE_SHA256:
                    return self._model_path
            except Exception:
                pass

        _download_file(_LAMA_MPE_URL, self._model_path)
        if _sha256(self._model_path) != _LAMA_MPE_SHA256:
            raise RuntimeError("Повреждена скачанная lama_mpe.ckpt (SHA256 mismatch)")
        return self._model_path

    def _normalize_params(self, params: dict[str, Any] | None) -> dict[str, Any]:
        merged: dict[str, Any] = {}
        if isinstance(params, dict):
            merged.update(params)

        inpaint_size = _to_int(merged.get("inpaint_size"), 2048)
        inpaint_size = max(512, min(4096, inpaint_size))
        return {"inpaint_size": inpaint_size}

    def _inpaint_locked(
        self,
        model,
        *,
        image_rgb: np.ndarray,
        mask_u8: np.ndarray,
        inpaint_size: int,
        device: str,
    ) -> np.ndarray:
        np = _np()
        torch = _torch()
        cv2 = _cv2_required()

        if image_rgb.ndim != 3 or image_rgb.shape[2] != 3:
            raise ValueError("Ожидается RGB изображение (H, W, 3)")
        if mask_u8.ndim != 2:
            raise ValueError("Ожидается маска (H, W)")
        if tuple(mask_u8.shape[:2]) != tuple(image_rgb.shape[:2]):
            raise ValueError("Размер маски не совпадает с изображением")
        if not np.any(mask_u8 > 0):
            return image_rgb.copy()

        img_orig = np.ascontiguousarray(image_rgb.astype(np.uint8))
        mask_orig = np.where(mask_u8 >= 127, 1, 0).astype(np.uint8)[..., None]

        new_shape = inpaint_size if max(img_orig.shape[:2]) > inpaint_size else None
        img = _resize_keepasp(img_orig, new_shape, stride=64)
        mask = _resize_keepasp(
            np.ascontiguousarray(mask_u8.astype(np.uint8)),
            new_shape,
            interpolation=cv2.INTER_NEAREST,
            stride=64,
        )

        h, w = mask.shape[:2]
        longer = max(h, w)
        pad_bottom = max(0, longer - h)
        pad_right = max(0, longer - w)
        if pad_bottom > 0 or pad_right > 0:
            mask = cv2.copyMakeBorder(mask, 0, pad_bottom, 0, pad_right, cv2.BORDER_REFLECT)
            img = cv2.copyMakeBorder(img, 0, pad_bottom, 0, pad_right, cv2.BORDER_REFLECT)

        img_t = torch.from_numpy(np.ascontiguousarray(img)).permute(2, 0, 1).unsqueeze(0).float()
        img_t = img_t / 255.0
        mask_t = (
            torch.from_numpy(np.ascontiguousarray(mask))
            .unsqueeze(0)
            .unsqueeze(0)
            .float()
            / 255.0
        )
        mask_t = (mask_t >= 0.5).float()

        rel_pos, _, direct = model.load_masked_position_encoding(mask_t[0, 0].cpu().numpy())
        rel_pos_t = torch.LongTensor(rel_pos).unsqueeze(0)
        direct_t = torch.LongTensor(direct).unsqueeze(0)

        if device != "cpu":
            img_t = img_t.to(device)
            mask_t = mask_t.to(device)
            rel_pos_t = rel_pos_t.to(device)
            direct_t = direct_t.to(device)

        img_t = img_t * (1 - mask_t)

        with torch.no_grad():
            if device != "cpu":
                device_type = "cuda" if device.startswith("cuda") else device
                try:
                    with torch.autocast(device_type=device_type, dtype=torch.float16):
                        out_t = model(img_t, mask_t, rel_pos_t, direct_t)
                except Exception:
                    out_t = model(img_t, mask_t, rel_pos_t, direct_t)
            else:
                out_t = model(img_t, mask_t, rel_pos_t, direct_t)

        out = out_t.to(device="cpu", dtype=torch.float32).squeeze(0).permute(1, 2, 0).numpy() * 255
        out = np.clip(np.round(out), 0, 255).astype(np.uint8)

        if pad_bottom > 0:
            out = out[:-pad_bottom, :, :]
        if pad_right > 0:
            out = out[:, :-pad_right, :]

        im_h0, im_w0 = img_orig.shape[:2]
        if out.shape[0] != im_h0 or out.shape[1] != im_w0:
            out = cv2.resize(out, (im_w0, im_h0), interpolation=cv2.INTER_LINEAR)

        composed = out * mask_orig + img_orig * (1 - mask_orig)
        return np.ascontiguousarray(np.clip(composed, 0, 255).astype(np.uint8))

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
