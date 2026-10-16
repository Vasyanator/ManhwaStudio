"""
File: modules/ai_backend/inpaint/lama_mpe.py

Purpose:
LaMa MPE inpainting service adapter for the Python AI backend.

Main responsibilities:
- load the LaMa MPE runtime lazily;
- check the `inpainting_lama_mpe.ckpt` checkpoint against its pinned SHA-256
  before every load;
- synchronize model device with backend AI device settings;
- run inpainting requests and unload idle models through the model manager.

Notes:
The checkpoint is NOT downloaded here. Its one owner is the Rust side
(`ms_sysprobe::ai_models::ensure_lama_mpe`, called by the cleaning tab before
every `inpaint.lama_mpe` request); a missing file is a `FileNotFoundError`
naming that owner, a file with another digest a `RuntimeError`.
"""

from __future__ import annotations

import functools
import hashlib
import io
import logging
import threading
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    import numpy as np
    import torch as torch_mod

from ..runtime.health_snapshot import HealthSnapshot
from ..runtime.model_manager import SmallModelManager
from ..runtime.paths import program_root
from ..runtime.residency.footprint import hint_from_files
from ..runtime.torch_device import clear_torch_cache, resolve_selected_backend_device

try:
    from config import LAMA_MPE_DIR
except Exception:
    LAMA_MPE_DIR = str(
        program_root() / "ManhwaStudio_AI_Models" / "Torch" / "LaMa_MPE"
    )

try:
    from config import program_dir as _PROGRAM_DIR
except Exception:
    _PROGRAM_DIR = program_root()

log = logging.getLogger(__name__)


# ============================================================================
# LAMA MPE INPAINT SERVICE
# ----------------------------------------------------------------------------
# Что в файле:
# - `LamaMpeInpaintService`: lazy-load обёртка LaMa MPE для endpoint
#   `/inpaint/lama_mpe`.
# - Порт инференса из legacy `ui_new/tools/region_edit_lama_mpe.py`
#   (без Qt-слоя): resize->pad->MPE forward->композит по маске.
# - Checkpoint `inpainting_lama_mpe.ckpt` in `ManhwaStudio_AI_Models/Torch/LaMa_MPE`:
#   SHA-256 check before every load. Downloaded by Rust only
#   (`ms_sysprobe::ai_models::ensure_lama_mpe`); never fetched here.
# - Декодирование PNG-изображений (RGB image + mask) и кодирование raw PNG.
# - Нормализация параметров endpoint (`inpaint_size`).
# - Device sync with the backend setting `General.ai_device` through
#   `runtime.torch_device`.
# ============================================================================

#: Digest of the published checkpoint; the copy the Rust catalog serves
#: (`Torch/LaMa_MPE/inpainting_lama_mpe.ckpt`, 108 580 583 bytes) has these bytes.
_LAMA_MPE_SHA256 = "d625aa1b3e0d0408acfd6928aa84f005867aa8dbb9162480346a4e20660786cc"
_LAMA_MPE_CKPT_NAME = "inpainting_lama_mpe.ckpt"
#: Owner name of this service's leases in the residency arbiter (owner rule).
_LEASE_OWNER = "inpaint.lama_mpe"


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


#: `General.ai_device` resolution with this service's device policy (owner:
#: `runtime.torch_device`); patched by name in the test suite.
_resolve_selected_backend_device = functools.partial(resolve_selected_backend_device, mps_last_resort=True)

#: Allocator-cache release with this service's policy (owner: `runtime.torch_device`);
#: patched by name in the test suite.
_clear_torch_cache = functools.partial(clear_torch_cache, owner="LaMa MPE")


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
    def __init__(self, model_manager: SmallModelManager) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._model: Any = None
        self._active_device = "cpu"
        self._active_model_key: str | None = None
        self._last_error: str | None = None
        # Published copy of the state-derived health fields; `health()` never
        # takes `_lock`, which a load and inference hold (S-014).
        self._health = HealthSnapshot(ready=False, device="cpu", last_error=None)
        self._model_dir = Path(str(LAMA_MPE_DIR))
        self._model_path = self._model_dir / _LAMA_MPE_CKPT_NAME

    def health(self) -> dict[str, Any]:
        """Return the lama_mpe health entry for the backend snapshot.

        State fields come from the published snapshot and `model_exists` is a
        filesystem probe taken here, outside any service lock, so this answers
        while a load or an inference holds the instance lock.
        """
        fields = self._health.read()
        return {
            "ready": fields["ready"],
            "model": "lama_mpe",
            "device": fields["device"],
            "model_path": str(self._model_path),
            "model_exists": self._model_path.is_file(),
            "last_error": fields["last_error"],
        }

    def _publish_health_locked(self) -> None:
        """Republish the state-derived health fields; caller holds `self._lock`."""
        self._health.publish(
            ready=self._model is not None,
            device=self._active_device,
            last_error=self._last_error,
        )

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
        # Pre-load size: the checkpoint file; before Rust fetched it the size
        # is unknown and the arbiter's unknown-size default applies.
        hint = hint_from_files([self._model_path], device)
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
            owner=_LEASE_OWNER,
            hint=hint,
        )

        with self._lock:
            try:
                # Load scope: only a failure in here (including the checkpoint
                # download it performs) is a failed LOAD, and only then may the
                # manager drop its entry for `model_key`.
                try:
                    model = self._ensure_model_locked(device)
                except Exception:
                    if lease.needs_load:
                        lease.mark_load_failed()
                    raise
                # The model is resident from here on, so it is registered before
                # inference: an inference failure must leave it counted and
                # evictable, not silently occupying VRAM off the books.
                lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
                out_rgb = self._inpaint_locked(
                    model,
                    image_rgb=image_rgb,
                    mask_u8=mask_u8,
                    inpaint_size=normalized["inpaint_size"],
                    device=device,
                )
                self._last_error = None
                self._publish_health_locked()
            except Exception as exc:
                self._last_error = str(exc)
                self._publish_health_locked()
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
                # The move to CPU only frees VRAM sooner; the reference is dropped
                # below either way, so the unload still completes.
                log.warning("LaMa MPE: moving the model to CPU before unload failed (key %s)", current_key, exc_info=True)
            self._model = None
            self._active_model_key = None
            self._publish_health_locked()
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
        self._publish_health_locked()
        _clear_torch_cache()
        if previous_key is not None:
            self._model_manager.mark_unloaded(previous_key)

        self._validate_runtime_layout_locked()
        ckpt_path = self._ensure_checkpoint_locked()
        load_lama_mpe = _load_lama_mpe_loader()
        self._model = load_lama_mpe(str(ckpt_path), device, use_mpe=True)
        self._active_device = device
        self._active_model_key = requested_key
        self._publish_health_locked()
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
        """Return the checkpoint path once it is present with the pinned SHA-256.

        Never downloads: the Rust side (`ms_sysprobe::ai_models::ensure_lama_mpe`)
        owns the fetch and runs before every `inpaint.lama_mpe` request.

        # Errors
        `FileNotFoundError` when the checkpoint is missing (naming the Rust owner);
        `RuntimeError` when it cannot be read or its digest differs.
        """
        path = self._model_path
        if not path.is_file():
            log.error("lama_mpe: checkpoint missing at %s", path)
            raise FileNotFoundError(
                f"Не найден checkpoint LaMa-MPE: {path}\n"
                "Его скачивает приложение (ms_sysprobe::ai_models::ensure_lama_mpe) перед "
                "запуском LaMa-MPE; запустите инструмент ещё раз, чтобы скачать файл."
            )
        try:
            actual = _sha256(path)
        except OSError as exc:
            log.error("lama_mpe: could not read checkpoint %s: %s", path, exc)
            raise RuntimeError(
                f"Не удалось прочитать checkpoint LaMa-MPE: {path}\nОшибка: {exc}"
            ) from exc
        if actual != _LAMA_MPE_SHA256:
            log.error(
                "lama_mpe: checkpoint %s has sha256 %s, expected %s",
                path,
                actual,
                _LAMA_MPE_SHA256,
            )
            raise RuntimeError(
                f"Checkpoint LaMa-MPE повреждён (не совпала контрольная сумма SHA-256): {path}\n"
                "Удалите файл — приложение скачает его заново при следующем запуске."
            )
        return path

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
