"""
File: modules/ai_backend/flux_fill_inpaint_service.py

Purpose:
FLUX.1-Fill-dev inpainting service for the Python AI backend (method
`inpaint.flux_fill`, streaming). Powers the "AI редактирование области" cleaning
tool: regenerates / removes content under a painted mask.

Model layout (everything under `ManhwaStudio_AI_Models/side_models/`, NOT the HF
cache):
- transformer: a GGUF quant chosen by the user, downloaded on demand from
  `YarvixPA/FLUX.1-Fill-dev-GGUF` into `FLUX_FILL_DIR`;
- VAE / CLIP-L / T5-XXL / scheduler / tokenizers: diffusers components downloaded
  from the open (non-gated) `ostris/Flex.1-alpha` repo (architecturally identical
  to FLUX.1) into `FLUX_FILL_COMPONENTS_DIR`.

Main responsibilities:
- list/quant management + on-demand download with byte-level progress;
- lazy pipeline build (GGUF transformer + local components), pinned to the
  DISCRETE GPU (the integrated Ryzen iGPU is explicitly excluded);
- MIOpen immediate mode on ROCm (set before torch import);
- generation with mask dilation, seamless (Poisson) tone matching, and compositing
  back over the original outside the mask;
- progress streamed as `progress_callback(phase, step, total, label)` where phase
  is "download" or "generate";
- health / unload hooks; reuse of the shared resident-model manager.

Notes:
The heavy packages (torch/diffusers/transformers) are imported lazily. Missing
packages, weights, or download failures surface as explicit errors.
"""

from __future__ import annotations

import io
import os
import threading
import time
from typing import TYPE_CHECKING, Any, Callable

if TYPE_CHECKING:
    import numpy as np

try:
    import config as _config
except Exception:  # pragma: no cover - config is always importable in-app
    _config = None

from .model_manager import LoadedModelManager

# --- Repos / quants -------------------------------------------------------
GGUF_REPO = "YarvixPA/FLUX.1-Fill-dev-GGUF"
COMPONENTS_REPO = "ostris/Flex.1-alpha"
# Component subfolders to pull (the transformer folder is intentionally skipped —
# we use the local GGUF transformer instead).
COMPONENT_PREFIXES = (
    "model_index.json",
    "scheduler/",
    "vae/",
    "text_encoder/",
    "text_encoder_2/",
    "tokenizer/",
    "tokenizer_2/",
)

# Available GGUF quants in GGUF_REPO (filename = flux1-fill-dev-{QUANT}.gguf),
# ordered small -> large. Q8_0 is the default (best quality, ~12.6 GB).
AVAILABLE_QUANTS = (
    "Q3_K_S",
    "Q4_0",
    "Q4_1",
    "Q4_K_S",
    "Q5_0",
    "Q5_1",
    "Q5_K_S",
    "Q6_K",
    "Q8_0",
)
DEFAULT_QUANT = "Q8_0"

OBJECT_REMOVAL_PROMPT = (
    "clean background, seamless continuation of the surrounding texture, "
    "consistent lighting and color"
)
VALID_MODES = ("inpaint", "object_removal")

# Progress callback: (phase, step, total, label). phase in {"download","generate"}.
ProgressCb = Callable[[str, int, int, str], None]


def gguf_filename(quant: str) -> str:
    return f"flux1-fill-dev-{quant}.gguf"


def gguf_path(quant: str) -> str:
    return os.path.join(_flux_dir(), gguf_filename(quant))


def _flux_dir() -> str:
    if _config is not None and hasattr(_config, "FLUX_FILL_DIR"):
        return _config.FLUX_FILL_DIR
    # Fallback relative to repo root (…/modules/ai_backend/this_file).
    root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    return os.path.join(root, "ManhwaStudio_AI_Models", "side_models", "FLUX.1-Fill-dev-GGUF")


def _components_dir() -> str:
    if _config is not None and hasattr(_config, "FLUX_FILL_COMPONENTS_DIR"):
        return _config.FLUX_FILL_COMPONENTS_DIR
    return os.path.join(_flux_dir(), "components")


def normalize_quant(value: Any) -> str:
    q = str(value or "").strip()
    return q if q in AVAILABLE_QUANTS else DEFAULT_QUANT


# =====================================================================
#  Parameter normalization
# =====================================================================
def normalize_flux_fill_params(params: dict[str, Any] | None) -> dict[str, Any]:
    merged: dict[str, Any] = {}
    if isinstance(params, dict):
        merged.update(params)

    mode = str(merged.get("mode", "object_removal") or "").strip()
    if mode not in VALID_MODES:
        raise ValueError(f"Неизвестный режим Flux Fill: {mode!r}")

    quant = normalize_quant(merged.get("quant"))

    prompt = str(merged.get("prompt", "") or "").strip()
    if mode == "object_removal" and not prompt:
        prompt = OBJECT_REMOVAL_PROMPT

    return {
        "mode": mode,
        "quant": quant,
        "prompt": prompt,
        "steps": _clamp_int(merged.get("steps"), default=28, low=1, high=100),
        "guidance": _clamp_float(merged.get("guidance"), default=30.0, low=0.0, high=100.0),
        "seed": _to_int(merged.get("seed"), -1),
        "max_seq": _clamp_int(merged.get("max_seq"), default=512, low=64, high=512),
        "max_side": _clamp_int(merged.get("max_side"), default=1536, low=0, high=4096),
        "dilate": _clamp_int(merged.get("dilate"), default=4, low=0, high=100),
        "feather": _clamp_int(merged.get("feather"), default=3, low=0, high=100),
        "seamless": _to_bool(merged.get("seamless"), True),
        "vae_tiling": _to_bool(merged.get("vae_tiling"), True),
        "cpu_offload": _to_bool(merged.get("cpu_offload"), False),
        "miopen_fast": _to_bool(merged.get("miopen_fast"), True),
        "dtype": "fp16" if str(merged.get("dtype", "bf16")).strip() == "fp16" else "bf16",
    }


# =====================================================================
#  Service
# =====================================================================
class FluxFillInpaintService:
    """Lazy-loading FLUX.1-Fill-dev inpaint pipeline for `inpaint.flux_fill`."""

    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._pipe: Any = None
        self._active_key: str | None = None
        self._device: Any = None
        self._last_error: str | None = None

    # ---- status / health ----
    def status(self) -> dict[str, Any]:
        """Quant catalog + which quants and components are already on disk."""
        downloaded = [q for q in AVAILABLE_QUANTS if _is_nonempty_file(gguf_path(q))]
        return {
            "quants": list(AVAILABLE_QUANTS),
            "default_quant": DEFAULT_QUANT,
            "downloaded_quants": downloaded,
            "components_ready": _components_present(),
            "gguf_repo": GGUF_REPO,
            "components_repo": COMPONENTS_REPO,
        }

    def health(self) -> dict[str, Any]:
        with self._lock:
            return {
                "ready": self._pipe is not None,
                "model": "flux_fill",
                "device": str(self._device) if self._device is not None else "cpu",
                "active_key": self._active_key,
                "downloaded_quants": [q for q in AVAILABLE_QUANTS if _is_nonempty_file(gguf_path(q))],
                "components_ready": _components_present(),
                "last_error": self._last_error,
            }

    def unload(self) -> bool:
        with self._lock:
            if self._pipe is None:
                return False
            key = self._active_key
            self._pipe = None
            self._active_key = None
            _clear_torch_cache()
            if key is not None:
                self._model_manager.mark_unloaded(key)
            return True

    # ---- main entry ----
    def inpaint_image_bytes(
        self,
        image_bytes: bytes,
        mask_bytes: bytes,
        *,
        params: dict[str, Any] | None = None,
        progress_callback: ProgressCb | None = None,
    ) -> dict[str, Any]:
        normalized = normalize_flux_fill_params(params)
        image_rgb = _decode_image_rgb(image_bytes)
        mask_u8 = _decode_mask(mask_bytes, expected_hw=image_rgb.shape[:2])

        # 1) Make sure the weights are on disk (streams download progress).
        self.ensure_model(normalized["quant"], progress_callback)

        quant = normalized["quant"]
        model_key = f"flux_fill:{quant}"
        lease = self._model_manager.begin_model_use(
            model_key, unload_callback=lambda: self._unload_key(model_key)
        )
        with self._lock:
            try:
                pipe = self._ensure_pipeline_locked(normalized, model_key)
                out_rgb = self._generate_locked(
                    pipe, image_rgb, mask_u8, normalized, progress_callback
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
            "device": str(self._device) if self._device is not None else "cpu",
            "mode": normalized["mode"],
            "quant": quant,
        }

    # ---- model download ----
    def ensure_model(self, quant: str, progress_callback: ProgressCb | None) -> None:
        """Download the selected GGUF quant + diffusers components into side_models.

        Files already present (non-empty) are skipped. Progress is reported in
        bytes via `progress_callback("download", done, total, label)`.
        """
        quant = normalize_quant(quant)
        os.makedirs(_flux_dir(), exist_ok=True)
        os.makedirs(_components_dir(), exist_ok=True)

        plan = _build_download_plan(quant)
        # Filter to missing files only.
        missing = [item for item in plan if not _is_nonempty_file(item["dest"])]
        if not missing:
            return

        total_bytes = sum(int(item.get("size") or 0) for item in missing)
        done_bytes = 0
        cb = progress_callback

        def report(label: str) -> None:
            if cb is not None:
                try:
                    cb("download", int(done_bytes), int(max(total_bytes, 1)), label)
                except Exception:
                    pass

        report("Подготовка загрузки модели…")
        for item in missing:
            name = os.path.basename(item["dest"])
            base = done_bytes

            def on_chunk(n: int) -> None:
                nonlocal done_bytes
                done_bytes = base + n
                report(f"Скачивание {name}")

            _download_file_streaming(item["url"], item["dest"], on_chunk)
            done_bytes = base + int(item.get("size") or 0)
            report(f"Скачано {name}")

    # ---- pipeline ----
    def _ensure_pipeline_locked(self, normalized: dict[str, Any], model_key: str) -> Any:
        if self._pipe is not None and self._active_key == model_key:
            return self._pipe

        prev = self._active_key
        self._pipe = None
        self._active_key = None
        _clear_torch_cache()
        if prev is not None:
            self._model_manager.mark_unloaded(prev)

        if normalized["miopen_fast"]:
            _apply_miopen_fast()

        import torch  # noqa: F401  (after MIOpen env is set)

        try:
            torch.backends.cudnn.benchmark = False
        except Exception:
            pass

        from diffusers import (
            FluxFillPipeline,
            FluxTransformer2DModel,
            GGUFQuantizationConfig,
        )

        quant = normalized["quant"]
        gguf = gguf_path(quant)
        if not _is_nonempty_file(gguf):
            raise FileNotFoundError(f"GGUF не найден: {gguf}")
        if not _components_present():
            raise FileNotFoundError(
                f"Компоненты Flux Fill не загружены: {_components_dir()}"
            )

        dtype = torch.bfloat16 if normalized["dtype"] == "bf16" else torch.float16
        device = _select_discrete_device()

        transformer = FluxTransformer2DModel.from_single_file(
            gguf,
            quantization_config=GGUFQuantizationConfig(compute_dtype=dtype),
            torch_dtype=dtype,
        )
        pipe = FluxFillPipeline.from_pretrained(
            _components_dir(),
            transformer=transformer,
            torch_dtype=dtype,
        )
        pipe.set_progress_bar_config(disable=True)
        if normalized["vae_tiling"]:
            try:
                pipe.vae.enable_tiling()
                pipe.vae.enable_slicing()
            except Exception:
                pass
        if normalized["cpu_offload"] and getattr(device, "type", "cpu") != "cpu":
            pipe.enable_model_cpu_offload(device=str(device))
        else:
            pipe.to(device)

        self._pipe = pipe
        self._device = device
        self._active_key = model_key
        return pipe

    def _unload_key(self, model_key: str) -> bool:
        with self._lock:
            if self._pipe is None or self._active_key != model_key:
                return False
            return self.unload()

    # ---- generation ----
    def _generate_locked(
        self,
        pipe: Any,
        image_rgb: np.ndarray,
        mask_u8: np.ndarray,
        normalized: dict[str, Any],
        progress_callback: ProgressCb | None,
    ) -> np.ndarray:
        import numpy as np
        import torch
        from PIL import Image

        H0, W0 = image_rgb.shape[:2]
        mask = _dilate_mask(mask_u8, normalized["dilate"])

        work_rgb, work_mask, scaled = image_rgb, mask, False
        max_side = normalized["max_side"]
        if max_side and max(H0, W0) > max_side:
            scale = max_side / max(H0, W0)
            nw, nh = max(1, int(W0 * scale)), max(1, int(H0 * scale))
            work_rgb = np.asarray(Image.fromarray(image_rgb, "RGB").resize((nw, nh), Image.LANCZOS))
            work_mask = np.asarray(Image.fromarray(mask, "L").resize((nw, nh), Image.NEAREST))
            scaled = True

        rgb_pad, _ = _pad_to_multiple(work_rgb, 16)
        mask_pad, _ = _pad_to_multiple(work_mask, 16)
        Hh, Ww = rgb_pad.shape[:2]

        seed = int(normalized["seed"])
        generator = torch.Generator("cpu")
        if seed >= 0:
            generator = generator.manual_seed(seed)
        else:
            generator = generator.manual_seed(int.from_bytes(os.urandom(4), "little"))

        steps = int(normalized["steps"])
        cb = progress_callback

        def _on_step(_pipe: Any, step: int, _t: Any, kwargs: dict[str, Any]):
            if cb is not None:
                try:
                    cb("generate", int(step) + 1, steps, "Генерация")
                except Exception:
                    pass
            return kwargs

        if cb is not None:
            cb("generate", 0, steps, "Генерация")

        result = pipe(
            prompt=normalized["prompt"] or "",
            image=Image.fromarray(rgb_pad, "RGB"),
            mask_image=Image.fromarray(mask_pad, "L"),
            height=Hh,
            width=Ww,
            num_inference_steps=steps,
            guidance_scale=float(normalized["guidance"]),
            max_sequence_length=int(normalized["max_seq"]),
            generator=generator,
            callback_on_step_end=_on_step,
            callback_on_step_end_tensor_inputs=["latents"],
        ).images[0]

        out = np.asarray(result.convert("RGB"))[: work_rgb.shape[0], : work_rgb.shape[1]]
        if scaled:
            out = np.asarray(Image.fromarray(out, "RGB").resize((W0, H0), Image.LANCZOS))

        if normalized["seamless"] and mask.any():
            try:
                return _seamless_composite(image_rgb, out, mask)
            except Exception:
                pass

        m = mask.astype(np.float32) / 255.0
        if normalized["feather"] > 0:
            from PIL import ImageFilter

            m = np.asarray(
                Image.fromarray((m * 255).astype(np.uint8), "L").filter(
                    ImageFilter.GaussianBlur(normalized["feather"])
                )
            ).astype(np.float32) / 255.0
        m = m[..., None]
        composed = image_rgb.astype(np.float32) * (1 - m) + out.astype(np.float32) * m
        return np.clip(composed, 0, 255).astype(np.uint8)


# =====================================================================
#  Download plumbing
# =====================================================================
def _build_download_plan(quant: str) -> list[dict[str, Any]]:
    """Returns [{url, dest, size}] for the GGUF quant + all component files."""
    from huggingface_hub import HfApi, hf_hub_url

    token = os.environ.get("HF_TOKEN") or os.environ.get("HUGGING_FACE_HUB_TOKEN")
    api = HfApi(token=token)
    plan: list[dict[str, Any]] = []

    # GGUF transformer (single file).
    gname = gguf_filename(quant)
    gsize = 0
    try:
        info = api.model_info(GGUF_REPO, files_metadata=True)
        for sib in info.siblings or []:
            if sib.rfilename == gname:
                gsize = int(sib.size or 0)
                break
    except Exception:
        gsize = 0
    plan.append({
        "url": hf_hub_url(GGUF_REPO, gname),
        "dest": gguf_path(quant),
        "size": gsize,
    })

    # diffusers components from the open Flex.1-alpha repo.
    try:
        comp_info = api.model_info(COMPONENTS_REPO, files_metadata=True)
        siblings = comp_info.siblings or []
    except Exception as exc:
        raise RuntimeError(f"Не удалось получить список файлов {COMPONENTS_REPO}: {exc}") from exc
    for sib in siblings:
        path = sib.rfilename
        if not any(path == p or path.startswith(p) for p in COMPONENT_PREFIXES):
            continue
        plan.append({
            "url": hf_hub_url(COMPONENTS_REPO, path),
            "dest": os.path.join(_components_dir(), *path.split("/")),
            "size": int(sib.size or 0),
        })
    return plan


def _download_file_streaming(url: str, dest: str, on_chunk: Callable[[int], None]) -> None:
    """Stream `url` to `dest` (atomic via .part), reporting cumulative bytes."""
    import requests

    os.makedirs(os.path.dirname(dest), exist_ok=True)
    tmp = dest + ".part"
    headers = {}
    token = os.environ.get("HF_TOKEN") or os.environ.get("HUGGING_FACE_HUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    done = 0
    with requests.get(url, stream=True, allow_redirects=True, headers=headers, timeout=60) as r:
        r.raise_for_status()
        with open(tmp, "wb") as f:
            for chunk in r.iter_content(chunk_size=1 << 20):
                if not chunk:
                    continue
                f.write(chunk)
                done += len(chunk)
                on_chunk(done)
    os.replace(tmp, dest)


def _components_present() -> bool:
    """Heuristic: model_index.json + the four big weight files exist non-empty."""
    cd = _components_dir()
    required = (
        os.path.join(cd, "model_index.json"),
        os.path.join(cd, "vae", "diffusion_pytorch_model.safetensors"),
        os.path.join(cd, "text_encoder", "model.safetensors"),
        os.path.join(cd, "text_encoder_2", "model-00001-of-00002.safetensors"),
        os.path.join(cd, "tokenizer_2", "spiece.model"),
    )
    return all(_is_nonempty_file(p) for p in required)


def _is_nonempty_file(path: str) -> bool:
    try:
        return os.path.isfile(path) and os.path.getsize(path) > 0
    except OSError:
        return False


# =====================================================================
#  Device / MIOpen (discrete GPU only — iGPU excluded)
# =====================================================================
def _select_discrete_device() -> Any:
    import torch

    if not torch.cuda.is_available():
        return torch.device("cpu")
    igpu_markers = ("ryzen", "cpu", "radeon graphics integrated")
    igpu_archs = ("gfx1036", "gfx1037", "gfx90c", "gfx902", "gfx1035", "gfx103")
    best = None
    for i in range(torch.cuda.device_count()):
        p = torch.cuda.get_device_properties(i)
        name = p.name.lower()
        arch = str(getattr(p, "gcnArchName", "")).lower()
        if any(m in name for m in igpu_markers) or any(arch.startswith(a) for a in igpu_archs):
            continue
        if best is None or p.total_memory > best[1].total_memory:
            best = (i, p)
    if best is None:
        # No discrete GPU detected; refuse iGPU and fall back to CPU.
        return torch.device("cpu")
    torch.cuda.set_device(best[0])
    return torch.device(f"cuda:{best[0]}")


def _apply_miopen_fast() -> None:
    """MIOpen immediate mode — set before torch import (mirrors rocm_runtime.py)."""
    os.environ.setdefault("MIOPEN_FIND_MODE", "2")
    cache_root = os.path.join(os.path.dirname(os.path.dirname(_flux_dir())), ".cache", "miopen")
    try:
        user_db = os.path.join(cache_root, "user_db")
        kernels = os.path.join(cache_root, "kernels")
        os.makedirs(user_db, exist_ok=True)
        os.makedirs(kernels, exist_ok=True)
        os.environ.setdefault("MIOPEN_USER_DB_PATH", user_db)
        os.environ.setdefault("MIOPEN_CUSTOM_CACHE_DIR", kernels)
    except OSError:
        pass


# =====================================================================
#  Image / mask helpers
# =====================================================================
def _decode_image_rgb(image_bytes: bytes) -> np.ndarray:
    import numpy as np
    from PIL import Image

    with Image.open(io.BytesIO(image_bytes)) as img:
        return np.ascontiguousarray(np.array(img.convert("RGB"), dtype=np.uint8))


def _decode_mask(mask_bytes: bytes, *, expected_hw: tuple[int, int]) -> np.ndarray:
    import numpy as np
    from PIL import Image

    with Image.open(io.BytesIO(mask_bytes)) as img:
        arr = np.array(img)
    if arr.ndim == 3:
        mask = arr[..., 3] if arr.shape[2] >= 4 else np.max(arr[..., :3], axis=2)
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


def _dilate_mask(mask: np.ndarray, dilate: int) -> np.ndarray:
    if dilate <= 0:
        return mask
    try:
        import cv2
        import numpy as np

        k = 2 * int(dilate) + 1
        kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (k, k))
        return cv2.dilate(mask, kernel, iterations=1)
    except Exception:
        from PIL import Image, ImageFilter
        import numpy as np

        m = Image.fromarray(mask, "L")
        extra = dilate
        while extra > 0:
            m = m.filter(ImageFilter.MaxFilter(min(2 * extra + 1, 31)))
            extra -= 15
        return np.asarray(m)


def _pad_to_multiple(arr: np.ndarray, mult: int = 16):
    import numpy as np

    h, w = arr.shape[:2]
    ph = (mult - h % mult) % mult
    pw = (mult - w % mult) % mult
    if ph == 0 and pw == 0:
        return arr, (h, w)
    if arr.ndim == 3:
        return np.pad(arr, ((0, ph), (0, pw), (0, 0)), mode="edge"), (h, w)
    return np.pad(arr, ((0, ph), (0, pw)), mode="constant"), (h, w)


def _seamless_composite(orig: np.ndarray, gen: np.ndarray, mask: np.ndarray) -> np.ndarray:
    """Poisson (cv2.seamlessClone) per mask component — matches the patch tone to
    the surroundings, removing the darker/lighter seam left by Flux Fill."""
    import cv2
    import numpy as np

    pad = 12
    o = cv2.copyMakeBorder(orig, pad, pad, pad, pad, cv2.BORDER_REPLICATE)
    g = cv2.copyMakeBorder(gen, pad, pad, pad, pad, cv2.BORDER_REPLICATE)
    m = cv2.copyMakeBorder(mask, pad, pad, pad, pad, cv2.BORDER_CONSTANT, value=0)
    n, labels = cv2.connectedComponents((m > 127).astype(np.uint8))
    result = o.copy()
    for i in range(1, n):
        comp = (labels == i).astype(np.uint8) * 255
        # OpenCV places the patch top-left at (center - boundingRect.size//2). To
        # land it exactly in place (no 1px shift on even-sized bboxes) the center
        # must be (x + w//2, y + h//2) — NOT the bbox midpoint.
        x, y, w, h = cv2.boundingRect(comp)
        if w < 2 or h < 2:
            continue
        center = (int(x + w // 2), int(y + h // 2))
        try:
            result = cv2.seamlessClone(g, result, comp, center, cv2.NORMAL_CLONE)
        except cv2.error:
            a = (comp.astype(np.float32) / 255.0)[..., None]
            result = (result.astype(np.float32) * (1 - a) + g.astype(np.float32) * a).astype(np.uint8)
    return np.ascontiguousarray(result[pad:-pad, pad:-pad])


def _encode_png_bytes_rgb(image_rgb: np.ndarray) -> bytes:
    import numpy as np
    from PIL import Image

    arr = np.ascontiguousarray(image_rgb.astype(np.uint8))
    with io.BytesIO() as buf:
        Image.fromarray(arr, "RGB").save(buf, format="PNG")
        return buf.getvalue()


def _clear_torch_cache() -> None:
    import gc

    gc.collect()
    try:
        import torch

        if torch.cuda.is_available():
            torch.cuda.empty_cache()
            if hasattr(torch.cuda, "ipc_collect"):
                torch.cuda.ipc_collect()
    except Exception:
        pass


# =====================================================================
#  Coercion helpers
# =====================================================================
def _to_int(value: Any, default: int) -> int:
    try:
        if isinstance(value, bool):
            return default
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
    s = str(value).strip().lower()
    if s in {"1", "true", "yes", "on"}:
        return True
    if s in {"0", "false", "no", "off"}:
        return False
    return default


def _clamp_int(value: Any, *, default: int, low: int, high: int) -> int:
    return max(low, min(high, _to_int(value, default)))


def _clamp_float(value: Any, *, default: float, low: float, high: float) -> float:
    try:
        out = default if isinstance(value, bool) else float(value)
    except Exception:
        out = default
    return max(low, min(high, out))
