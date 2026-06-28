"""
File: modules/ai_backend/aot_inpaint_service.py

Purpose:
AOT inpainting service adapter for the Python AI backend.

Main responsibilities:
- load AOT inpainting runtime lazily;
- synchronize runtime device with backend AI device settings;
- run inpainting requests and expose health/unload hooks.
"""

from __future__ import annotations

import gc
import io
import threading
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
    from config import AOT_DIR
except Exception:
    AOT_DIR = str(
        Path(__file__).resolve().parents[2] / "ManhwaStudio_AI_Models" / "Torch" / "AOT"
    )

try:
    from config import UserConfig
except Exception:
    UserConfig = None

from .model_manager import LoadedModelManager


# ============================================================================
# AOT INPAINT SERVICE
# ----------------------------------------------------------------------------
# Что в файле:
# - `AotInpaintService`: lazy-load обёртка AOT-модели для endpoint `/inpaint/aot`.
# - Порт инференса из legacy `ui_new/tools/aot_inpaint_tool.py` (без Qt-слоя).
# - Декодирование PNG-изображений (RGB image + mask) и кодирование raw PNG.
# - Нормализация параметров AOT (`inpaint_size`).
# - Синхронизация устройства с backend-настройкой `General.ai_device`
#   через `AIDevice`.
# ============================================================================


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
            "Для /inpaint/aot требуется пакет torch. Установите зависимости backend."
        ) from exc


def _np():
    try:
        import numpy as np  # type: ignore

        return np
    except Exception as exc:
        raise RuntimeError(
            "Для /inpaint/aot требуется пакет numpy. Установите зависимости backend."
        ) from exc


_TORCH_RUNTIME = _maybe_torch()

if _TORCH_RUNTIME is not None:
    _ModuleBase = _TORCH_RUNTIME.nn.Module
    _Conv2dBase = _TORCH_RUNTIME.nn.Conv2d
    _ConvTranspose2dBase = _TORCH_RUNTIME.nn.ConvTranspose2d
else:
    class _ModuleBase:
        def __init__(self, *args, **kwargs):  # noqa: ANN002, ANN003
            pass

    class _Conv2dBase(_ModuleBase):
        def __init__(self, *args, **kwargs):  # noqa: ANN002, ANN003
            super().__init__()

    class _ConvTranspose2dBase(_ModuleBase):
        def __init__(self, *args, **kwargs):  # noqa: ANN002, ANN003
            super().__init__()


def relu_nf(x):
    torch = _torch()
    return torch.nn.functional.relu(x) * 1.7139588594436646


class LambdaLayer(_ModuleBase):
    def __init__(self, f):
        super().__init__()
        self.f = f

    def forward(self, x):
        return self.f(x)


class ScaledWSConv2d(_Conv2dBase):
    def __init__(
        self,
        in_channels,
        out_channels,
        kernel_size,
        stride=1,
        padding=0,
        dilation=1,
        groups=1,
        bias=True,
        gain=True,
        eps=1e-4,
    ):
        torch = _torch()
        super().__init__(
            in_channels,
            out_channels,
            kernel_size,
            stride,
            padding,
            dilation,
            groups,
            bias,
        )
        self.gain = torch.nn.Parameter(torch.ones(out_channels, 1, 1, 1)) if gain else None
        self.eps = eps

    def get_weight(self):
        np = _np()
        torch = _torch()
        fan_in = int(np.prod(self.weight.shape[1:]))
        var, mean = torch.var_mean(self.weight, dim=(1, 2, 3), keepdim=True)
        eps_t = torch.tensor(self.eps, device=var.device, dtype=var.dtype)
        scale = torch.rsqrt(torch.max(var * fan_in, eps_t))
        if self.gain is not None:
            scale = scale * self.gain.to(var.device).view_as(var)
        shift = mean * scale
        return self.weight * scale - shift

    def forward(self, x):
        torch = _torch()
        return torch.nn.functional.conv2d(
            x,
            self.get_weight(),
            self.bias,
            self.stride,
            self.padding,
            self.dilation,
            self.groups,
        )


class ScaledWSTransposeConv2d(_ConvTranspose2dBase):
    def __init__(
        self,
        in_channels,
        out_channels,
        kernel_size,
        stride=1,
        padding=0,
        output_padding=0,
        groups=1,
        bias=True,
        dilation=1,
        gain=True,
        eps=1e-4,
    ):
        torch = _torch()
        super().__init__(
            in_channels,
            out_channels,
            kernel_size,
            stride=stride,
            padding=padding,
            output_padding=output_padding,
            groups=groups,
            bias=bias,
            dilation=dilation,
            padding_mode="zeros",
        )
        self.gain = torch.nn.Parameter(torch.ones(in_channels, 1, 1, 1)) if gain else None
        self.eps = eps

    def get_weight(self):
        np = _np()
        torch = _torch()
        fan_in = int(np.prod(self.weight.shape[1:]))
        var, mean = torch.var_mean(self.weight, dim=(1, 2, 3), keepdim=True)
        eps_t = torch.tensor(self.eps, device=var.device, dtype=var.dtype)
        scale = torch.rsqrt(torch.max(var * fan_in, eps_t))
        if self.gain is not None:
            scale = scale * self.gain.to(var.device).view_as(var)
        shift = mean * scale
        return self.weight * scale - shift

    def forward(self, x, output_size=None):
        torch = _torch()
        output_padding = self._output_padding(
            x,
            output_size,
            self.stride,
            self.padding,
            self.kernel_size,
            self.dilation,
        )
        return torch.nn.functional.conv_transpose2d(
            x,
            self.get_weight(),
            self.bias,
            self.stride,
            self.padding,
            output_padding,
            self.groups,
            self.dilation,
        )


class GatedWSConvPadded(_ModuleBase):
    def __init__(self, in_ch, out_ch, ks, stride=1, dilation=1):
        torch = _torch()
        super().__init__()
        self.padding = torch.nn.ReflectionPad2d(((ks - 1) * dilation) // 2)
        self.conv = ScaledWSConv2d(in_ch, out_ch, kernel_size=ks, stride=stride, dilation=dilation)
        self.conv_gate = ScaledWSConv2d(
            in_ch,
            out_ch,
            kernel_size=ks,
            stride=stride,
            dilation=dilation,
        )

    def forward(self, x):
        torch = _torch()
        x = self.padding(x)
        signal = self.conv(x)
        gate = torch.sigmoid(self.conv_gate(x))
        return signal * gate * 1.8


class GatedWSTransposeConvPadded(_ModuleBase):
    def __init__(self, in_ch, out_ch, ks, stride=1):
        torch = _torch()
        super().__init__()
        self.conv = ScaledWSTransposeConv2d(
            in_ch,
            out_ch,
            kernel_size=ks,
            stride=stride,
            padding=(ks - 1) // 2,
        )
        self.conv_gate = ScaledWSTransposeConv2d(
            in_ch,
            out_ch,
            kernel_size=ks,
            stride=stride,
            padding=(ks - 1) // 2,
        )

    def forward(self, x):
        torch = _torch()
        signal = self.conv(x)
        gate = torch.sigmoid(self.conv_gate(x))
        return signal * gate * 1.8


def _my_layer_norm(feat):
    mean = feat.mean((2, 3), keepdim=True)
    std = feat.std((2, 3), keepdim=True) + 1e-9
    feat = 2 * (feat - mean) / std - 1
    feat = 5 * feat
    return feat


class AOTBlock(_ModuleBase):
    def __init__(self, dim, rates=(2, 4, 8, 16)):
        torch = _torch()
        super().__init__()
        self.rates = list(rates)
        for i, rate in enumerate(self.rates):
            setattr(
                self,
                f"block{str(i).zfill(2)}",
                torch.nn.Sequential(
                    torch.nn.ReflectionPad2d(rate),
                    torch.nn.Conv2d(dim, dim // 4, 3, padding=0, dilation=rate),
                    torch.nn.ReLU(True),
                ),
            )
        self.fuse = torch.nn.Sequential(
            torch.nn.ReflectionPad2d(1),
            torch.nn.Conv2d(dim, dim, 3, padding=0, dilation=1),
        )
        self.gate = torch.nn.Sequential(
            torch.nn.ReflectionPad2d(1),
            torch.nn.Conv2d(dim, dim, 3, padding=0, dilation=1),
        )

    def forward(self, x):
        torch = _torch()
        out = [getattr(self, f"block{str(i).zfill(2)}")(x) for i in range(len(self.rates))]
        out = torch.cat(out, 1)
        out = self.fuse(out)
        mask = torch.sigmoid(_my_layer_norm(self.gate(x)))
        return x * (1 - mask) + out * mask


class AOTGenerator(_ModuleBase):
    def __init__(self, in_ch=4, out_ch=3, ch=32):
        torch = _torch()
        super().__init__()
        self.head = torch.nn.Sequential(
            GatedWSConvPadded(in_ch, ch, 3, stride=1),
            LambdaLayer(relu_nf),
            GatedWSConvPadded(ch, ch * 2, 4, stride=2),
            LambdaLayer(relu_nf),
            GatedWSConvPadded(ch * 2, ch * 4, 4, stride=2),
        )
        self.body_conv = torch.nn.Sequential(*[AOTBlock(ch * 4) for _ in range(10)])
        self.tail = torch.nn.Sequential(
            GatedWSConvPadded(ch * 4, ch * 4, 3, 1),
            LambdaLayer(relu_nf),
            GatedWSConvPadded(ch * 4, ch * 4, 3, 1),
            LambdaLayer(relu_nf),
            GatedWSTransposeConvPadded(ch * 4, ch * 2, 4, 2),
            LambdaLayer(relu_nf),
            GatedWSTransposeConvPadded(ch * 2, ch, 4, 2),
            LambdaLayer(relu_nf),
            GatedWSConvPadded(ch, out_ch, 3, stride=1),
        )

    def forward(self, img, mask):
        torch = _torch()
        x = torch.cat([mask, img], dim=1)
        x = self.head(x)
        x = self.body_conv(x)
        x = self.tail(x)
        if self.training:
            return x
        return torch.clip(x, -1, 1)


def resize_keepasp(im, new_shape=640, scaleup=True, interpolation=None, stride=None):
    cv2 = _cv2_required()
    shape = im.shape[:2]  # (h, w)
    if new_shape is not None:
        if not isinstance(new_shape, tuple):
            new_shape = (new_shape, new_shape)
    else:
        new_shape = shape

    r = min(new_shape[0] / shape[0], new_shape[1] / shape[1])
    if not scaleup:
        r = min(r, 1.0)

    new_unpad = int(round(shape[1] * r)), int(round(shape[0] * r))  # (w, h)

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


def load_aot_model(model_path: str, device: str) -> AOTGenerator:
    torch = _torch()
    model = AOTGenerator(in_ch=4, out_ch=3, ch=32)
    sd = torch.load(model_path, map_location="cpu")
    if isinstance(sd, dict) and "model" in sd:
        state_dict = sd["model"]
    else:
        state_dict = sd
    model.load_state_dict(state_dict)
    model.eval().to(device)
    return model


class AotInpaintService:
    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._model: AOTGenerator | None = None
        self._active_device = "cpu"
        self._active_model_key: str | None = None
        self._last_error: str | None = None
        self._model_path = Path(str(AOT_DIR)) / "inpainting.ckpt"

    def health(self) -> dict[str, Any]:
        with self._lock:
            return {
                "ready": self._model is not None,
                "model": "aot",
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
            self._model = None
            self._active_model_key = None
            _clear_torch_cache()
            if current_key is not None:
                self._model_manager.mark_unloaded(current_key)
            return True

    def _ensure_model_locked(self, device: str) -> AOTGenerator:
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

        if not self._model_path.is_file():
            raise FileNotFoundError(f"Не найден checkpoint AOT: {self._model_path}")

        self._model = load_aot_model(str(self._model_path), device)
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
        return f"aot:{device}"

    def _normalize_params(self, params: dict[str, Any] | None) -> dict[str, Any]:
        merged: dict[str, Any] = {}
        if isinstance(params, dict):
            merged.update(params)

        inpaint_size = _to_int(merged.get("inpaint_size"), 2048)
        inpaint_size = max(256, min(4096, inpaint_size))
        return {"inpaint_size": inpaint_size}

    def _inpaint_locked(
        self,
        model: AOTGenerator,
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
        im_h0, im_w0 = img_orig.shape[:2]

        new_shape = inpaint_size if max(im_h0, im_w0) > inpaint_size else None
        img = resize_keepasp(img_orig, new_shape, stride=None)
        mask = resize_keepasp(
            np.ascontiguousarray(mask_u8.astype(np.uint8)),
            new_shape,
            interpolation=cv2.INTER_NEAREST,
            stride=None,
        )

        im_h, im_w = img.shape[:2]
        pad_bottom = max(0, 128 - im_h)
        pad_right = max(0, 128 - im_w)
        if pad_bottom > 0 or pad_right > 0:
            img = cv2.copyMakeBorder(img, 0, pad_bottom, 0, pad_right, cv2.BORDER_REFLECT)
            mask = cv2.copyMakeBorder(mask, 0, pad_bottom, 0, pad_right, cv2.BORDER_REFLECT)

        img_t = torch.from_numpy(np.ascontiguousarray(img)).permute(2, 0, 1).unsqueeze(0).float()
        img_t = img_t / 127.5 - 1.0
        mask_t = (
            torch.from_numpy(np.ascontiguousarray(mask))
            .unsqueeze(0)
            .unsqueeze(0)
            .float()
            / 255.0
        )
        mask_t = (mask_t >= 0.5).float()

        if device != "cpu":
            img_t = img_t.to(device)
            mask_t = mask_t.to(device)

        img_t = img_t * (1 - mask_t)

        with torch.no_grad():
            out_t = model(img_t, mask_t)

        out = (out_t.detach().cpu().squeeze(0).permute(1, 2, 0).numpy() + 1.0) * 127.5
        out = np.clip(np.round(out), 0, 255).astype(np.uint8)

        if pad_bottom > 0:
            out = out[:-pad_bottom, :, :]
        if pad_right > 0:
            out = out[:, :-pad_right, :]

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


def _maybe_cv2():
    try:
        import cv2  # type: ignore

        return cv2
    except Exception:
        return None


def _cv2_required():
    cv2 = _maybe_cv2()
    if cv2 is None:
        raise RuntimeError("Для /inpaint/aot требуется пакет opencv-python (cv2).")
    return cv2


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


def _to_int(value: Any, default: int) -> int:
    try:
        return int(value)
    except Exception:
        return default


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
