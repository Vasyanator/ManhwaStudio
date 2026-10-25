"""
File: modules/ai_backend/inpaint/sdxl.py

Purpose:
SDXL inpainting service adapter for the Python AI backend (endpoint `/inpaint/sdxl`).

Main responsibilities:
- lazily build and cache a `StableDiffusionXLInpaintPipeline`, strictly offline
  (`local_files_only=True`), from one of three model sources:
  - a local ckpt/safetensors file, with the pinned diffusers config repo Rust
    downloaded (`sdxl_config_dir`, chosen by Rust per channel mode) as its
    `config=`;
  - a local diffusers folder;
  - the one curated repo id `CURATED_INPAINT_REPO_ID`, loaded (fp16 variant)
    from the directory Rust downloaded it to (`sdxl_model_dir`);
  any other repo id is rejected, never fetched;
- support two channel modes:
  - `nine_channel`: dedicated 9-channel inpaint UNet (clean masked-image channel,
    full denoise);
  - `four_channel`: ordinary 4-channel SDXL checkpoint, where the hole is first
    prefilled with LaMa (so the text is gone from the context) and then refined
    with a moderate denoise via latent-blending inpaint;
- normalize generation parameters and map sampler names to diffusers schedulers;
- dilate/blur the mask, run the pipeline off the GUI thread, and composite the
  result back over the original outside the mask;
- expose health/unload hooks and reuse the shared resident-model manager.

Notes:
The Python `diffusers`/`transformers` packages are imported lazily. Missing
packages, weights, or an SDXL/mode channel mismatch surface as explicit errors.
The host->device move of the built pipeline is wrapped in
`runtime.rocm_mmap_transfer.patched_module_to`, which is a strict no-op off
ROCm; see `_build_sdxl_inpaint_pipeline` for why.

`_encode_png_bytes_rgb` is part of this module's contract beyond the service
class: `ipc/handlers/sdxl.py` imports it directly to encode the streamed latent
previews. Do not rename or move it.
"""

from __future__ import annotations

import functools
import io
import logging
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    import numpy as np

from ..runtime.health_snapshot import HealthSnapshot
from ..runtime.model_manager import SmallModelManager
from ..runtime.residency.footprint import hint_from_files
from ..runtime.rocm_mmap_transfer import patched_module_to
from ..runtime.torch_device import clear_torch_cache, resolve_selected_backend_device
from .lama import LamaInpaintService

MODEL_SUFFIXES = (".safetensors", ".ckpt")
#: Owner name of this service's leases in the residency arbiter (owner rule; the
#: nested LaMa prefill lease is LaMa's own owner).
_LEASE_OWNER = "inpaint.sdxl"

log = logging.getLogger(__name__)

# The only Hugging Face repo id `model_path` may name. Rust downloads its pinned
# fp16 files (`ms_sysprobe` external catalog) and sends that directory as the
# `sdxl_model_dir` request field; the backend never resolves a repo id itself.
CURATED_INPAINT_REPO_ID = "diffusers/stable-diffusion-xl-1.0-inpainting-0.1"
# Weight variant of the curated download: its catalog pins only `*.fp16.*` files.
CURATED_INPAINT_VARIANT = "fp16"

# Sampler name -> (diffusers scheduler class name, from_config kwargs).
# Keep names in sync with `SDXL_SAMPLERS` in
# crates/ms-tab-cleaning/src/tools/ai_editor/engines/sdxl/mod.rs.
SAMPLER_CONFIGS: dict[str, tuple[str, dict[str, Any]]] = {
    "Euler": ("EulerDiscreteScheduler", {}),
    "Euler a": ("EulerAncestralDiscreteScheduler", {}),
    "DPM++ 2M": ("DPMSolverMultistepScheduler", {"algorithm_type": "dpmsolver++"}),
    "DPM++ 2M Karras": (
        "DPMSolverMultistepScheduler",
        {"algorithm_type": "dpmsolver++", "use_karras_sigmas": True},
    ),
    "DPM++ SDE Karras": (
        "DPMSolverMultistepScheduler",
        {"algorithm_type": "sde-dpmsolver++", "use_karras_sigmas": True},
    ),
    "DDIM": ("DDIMScheduler", {}),
    "UniPC": ("UniPCMultistepScheduler", {}),
    "Heun": ("HeunDiscreteScheduler", {}),
}

VALID_MODES = ("nine_channel", "four_channel")

# Linear SDXL latent -> RGB approximation for fast per-step previews (no VAE
# decode). Values are the widely used SDXL preview factors; they only need to
# produce a recognizable thumbnail, not a color-accurate image.
LATENT_RGB_FACTORS = (
    (0.3651, 0.4232, 0.4341),
    (-0.2533, -0.0042, 0.1068),
    (0.1076, 0.1111, -0.0362),
    (-0.3165, -0.2492, -0.2188),
)
LATENT_RGB_BIAS = (0.1084, -0.0175, -0.0011)


def _latent_preview_rgb(latents: Any) -> np.ndarray:
    """Maps a `[B, 4, h, w]` SDXL latent tensor to a small `h x w x 3` uint8 RGB
    preview using a cheap linear approximation (no VAE decode)."""
    np = _np()
    torch = _torch()
    weight = torch.tensor(LATENT_RGB_FACTORS, dtype=latents.dtype, device=latents.device)
    bias = torch.tensor(LATENT_RGB_BIAS, dtype=latents.dtype, device=latents.device)
    lat = latents[0]  # [4, h, w]
    img = torch.einsum("chw,cr->hwr", lat, weight) + bias
    img = ((img + 1.0) / 2.0).clamp(0.0, 1.0)
    arr = (img.float().cpu().numpy() * 255.0).astype(np.uint8)
    return np.ascontiguousarray(arr)


def resolve_scheduler_config(sampler: str) -> tuple[str, dict[str, Any]]:
    """Returns the diffusers scheduler class name and kwargs for `sampler`.

    Raises ValueError if the sampler is not supported.
    """
    key = str(sampler or "").strip()
    if key not in SAMPLER_CONFIGS:
        raise ValueError(f"Неизвестный сэмплер SDXL: {sampler!r}")
    class_name, kwargs = SAMPLER_CONFIGS[key]
    return class_name, dict(kwargs)


def normalize_sdxl_params(params: dict[str, Any] | None) -> dict[str, Any]:
    """Validates and clamps SDXL generation parameters.

    Raises ValueError for an invalid mode, sampler, or an empty model path.
    """
    merged: dict[str, Any] = {}
    if isinstance(params, dict):
        merged.update(params)

    mode = str(merged.get("mode", "nine_channel") or "").strip()
    if mode not in VALID_MODES:
        raise ValueError(f"Неизвестный режим SDXL: {mode!r}")

    model_path = str(merged.get("model_path", "") or "").strip()
    if not model_path:
        raise ValueError("Не указан путь к весам SDXL (model_path).")

    sampler = str(merged.get("sampler", "DPM++ 2M Karras") or "").strip()
    # Validate eagerly so a bad sampler fails before model loading.
    resolve_scheduler_config(sampler)

    steps = _clamp_int(merged.get("steps"), default=30, low=1, high=150)
    cfg_scale = _clamp_float(merged.get("cfg_scale"), default=7.0, low=0.0, high=30.0)
    denoise = _clamp_float(merged.get("denoise_strength"), default=1.0, low=0.0, high=1.0)
    mask_blur = _clamp_int(merged.get("mask_blur"), default=4, low=0, high=64)
    mask_dilation = _clamp_int(merged.get("mask_dilation"), default=6, low=0, high=64)
    seed = _to_int(merged.get("seed"), -1)
    lama_model = str(merged.get("lama_model", "") or "").strip()

    if mode == "four_channel" and denoise >= 0.999:
        # Latent blending at strength 1.0 ignores the LaMa prefill (the hole is
        # re-noised to pure noise). Keep the prefill meaningful.
        denoise = 0.99

    return {
        "mode": mode,
        "model_path": model_path,
        "positive_prompt": str(merged.get("positive_prompt", "") or ""),
        "negative_prompt": str(merged.get("negative_prompt", "") or ""),
        "steps": steps,
        "cfg_scale": cfg_scale,
        "denoise_strength": denoise,
        "seed": seed,
        "sampler": sampler,
        "mask_blur": mask_blur,
        "mask_dilation": mask_dilation,
        "lama_model": lama_model,
    }


class SdxlInpaintService:
    """Lazy-loading wrapper around an SDXL inpaint pipeline for `/inpaint/sdxl`."""

    def __init__(
        self,
        model_manager: SmallModelManager,
        lama_service: LamaInpaintService,
    ) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._lama_service = lama_service
        self._pipe: Any = None
        self._active_device = "cpu"
        self._active_model_key: str | None = None
        self._last_error: str | None = None
        # Published copy of the state-derived health fields; `health()` never
        # takes `_lock`, which a load and inference hold (S-014).
        self._health = HealthSnapshot(ready=False, device="cpu", active_model_key=None, last_error=None)

    def health(self) -> dict[str, Any]:
        """Return `{"ready", "model", "device", "active_model_key", "last_error"}` for health.

        Reads only the published snapshot, so it answers while a load or an
        inference holds the instance lock.
        """
        return {"model": "sdxl_inpaint", **self._health.read()}

    def _publish_health_locked(self) -> None:
        """Republish the state-derived health fields; caller holds `self._lock`."""
        self._health.publish(
            ready=self._pipe is not None,
            device=self._active_device,
            active_model_key=self._active_model_key,
            last_error=self._last_error,
        )

    def inpaint_image_bytes(
        self,
        image_bytes: bytes,
        mask_bytes: bytes,
        *,
        config_dir: str,
        curated_model_dir: str | None = None,
        params: dict[str, Any] | None = None,
        progress_callback: Any = None,
    ) -> dict[str, Any]:
        """Runs SDXL inpainting and returns the composited region as raw PNG bytes.

        `config_dir` is the pinned diffusers config repo Rust downloaded for the
        requested mode; it is read only for a single-file checkpoint.
        `curated_model_dir` is Rust's download of `CURATED_INPAINT_REPO_ID`,
        required when `params["model_path"]` names that repo id.

        `progress_callback`, if given, is called once per diffusion step as
        `progress_callback(step, total, preview_rgb)` where `preview_rgb` is a
        small `H x W x 3` uint8 latent preview (or `None`). It runs on the
        worker thread inside the generation lock.

        Raises `ValueError` for invalid parameters, a repo id other than the
        curated one, or a curated id without `curated_model_dir`;
        `FileNotFoundError` for a missing model path or an absent ("not
        downloaded") config / curated directory; `RuntimeError` for a missing
        package.
        """
        normalized = normalize_sdxl_params(params)
        source = resolve_model_source(normalized["model_path"], config_dir=config_dir, curated_model_dir=curated_model_dir)
        image_rgb = self._decode_image_rgb(image_bytes)
        mask_u8 = self._decode_mask(mask_bytes, expected_hw=image_rgb.shape[:2])

        device = _resolve_selected_backend_device(self._active_device)
        model_key = self._model_key_for(source, normalized["mode"], device)
        # Pre-load size: the checkpoint file, or every file of the diffusers
        # folder (the curated download holds only its pinned fp16 files). The
        # single-file config repo holds no weights. File bytes, not the loaded
        # dtype: an fp32 checkpoint loaded in fp16 stays over-counted (a clean
        # load records max(hint, measured)).
        hint = hint_from_files([source.path], device)
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
            owner=_LEASE_OWNER,
            hint=hint,
        )

        with self._lock:
            try:
                # Load scope: only a failure in here is a failed LOAD, and only
                # then may the manager drop its entry for `model_key`.
                try:
                    pipe = self._ensure_pipeline_locked(
                        source=source,
                        mode=normalized["mode"],
                        device=device,
                        model_key=model_key,
                    )
                except Exception:
                    if lease.needs_load:
                        lease.mark_load_failed()
                    raise
                # The pipeline is resident from here on, so it is registered
                # before generation: a generation failure must leave it counted
                # and evictable, not silently occupying VRAM off the books. In
                # `four_channel` mode the generation below takes a SECOND lease
                # (the shared LaMa prefill), which is exactly when the manager
                # must see this pipeline as resident and in use.
                lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
                out_rgb = self._inpaint_locked(
                    pipe,
                    image_rgb=image_rgb,
                    mask_u8=mask_u8,
                    normalized=normalized,
                    device=device,
                    progress_callback=progress_callback,
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
            "mode": normalized["mode"],
        }

    def unload(self) -> bool:
        with self._lock:
            if self._pipe is None:
                return False
            current_key = self._active_model_key
            self._pipe = None
            self._active_model_key = None
            self._publish_health_locked()
            _clear_torch_cache()
            if current_key is not None:
                self._model_manager.mark_unloaded(current_key)
            return True

    def _ensure_pipeline_locked(
        self,
        *,
        source: SdxlModelSource,
        mode: str,
        device: str,
        model_key: str,
    ) -> Any:
        if (
            self._pipe is not None
            and self._active_device == device
            and self._active_model_key == model_key
        ):
            return self._pipe

        previous_key = self._active_model_key
        self._pipe = None
        self._active_model_key = None
        self._publish_health_locked()
        _clear_torch_cache()
        if previous_key is not None:
            self._model_manager.mark_unloaded(previous_key)

        pipe = _build_sdxl_inpaint_pipeline(source, device)
        in_channels = int(pipe.unet.config.in_channels)
        _validate_mode_channels(mode, in_channels)

        self._pipe = pipe
        self._active_device = device
        self._active_model_key = model_key
        self._publish_health_locked()
        return pipe

    def _unload_key(self, model_key: str) -> bool:
        with self._lock:
            if self._pipe is None or self._active_model_key != model_key:
                return False
            return self.unload()

    @staticmethod
    def _model_key_for(source: SdxlModelSource, mode: str, device: str) -> str:
        """Lease key: the resolved source (path, config, variant), mode and device are the identity."""
        return f"sdxl:{mode}:{device}:{source.kind}:{source.path}:{source.config_dir or ''}:{source.variant or ''}"

    def _inpaint_locked(
        self,
        pipe: Any,
        *,
        image_rgb: np.ndarray,
        mask_u8: np.ndarray,
        normalized: dict[str, Any],
        device: str,
        progress_callback: Any = None,
    ) -> np.ndarray:
        np = _np()
        torch = _torch()
        from PIL import Image

        height0, width0 = image_rgb.shape[:2]
        cond_mask_u8, alpha = _process_mask(
            mask_u8,
            dilation=normalized["mask_dilation"],
            blur=normalized["mask_blur"],
        )

        # 4-channel mode: prefill the hole with LaMa so the text is removed from
        # the context before the diffusion refine pass.
        init_rgb = image_rgb
        if normalized["mode"] == "four_channel":
            init_rgb = self._lama_prefill(
                image_rgb=image_rgb,
                cond_mask_u8=cond_mask_u8,
                lama_model=normalized["lama_model"],
            )

        # SDXL VAE requires multiples of 8; round the working size up.
        work_w = _round_up(width0, 8)
        work_h = _round_up(height0, 8)

        init_img = _to_pil_resized(Image.fromarray(init_rgb, mode="RGB"), work_w, work_h)
        mask_img = _to_pil_resized(
            Image.fromarray(cond_mask_u8, mode="L"), work_w, work_h, nearest=True
        )

        scheduler_class, scheduler_kwargs = resolve_scheduler_config(normalized["sampler"])
        _apply_scheduler(pipe, scheduler_class, scheduler_kwargs)

        seed = int(normalized["seed"])
        generator = None
        if seed >= 0:
            generator = torch.Generator(device="cpu").manual_seed(seed)

        requested_steps = int(normalized["steps"])
        pipe_kwargs: dict[str, Any] = {
            "prompt": normalized["positive_prompt"],
            "negative_prompt": normalized["negative_prompt"],
            "image": init_img,
            "mask_image": mask_img,
            "num_inference_steps": requested_steps,
            "guidance_scale": float(normalized["cfg_scale"]),
            "strength": float(normalized["denoise_strength"]),
            "width": work_w,
            "height": work_h,
            "generator": generator,
        }

        # Stream a cheap per-step latent preview back to the caller. The exact
        # step total is read from the pipeline (strength < 1.0 runs fewer steps).
        if progress_callback is not None:

            def _on_step_end(pipe_inner: Any, step: int, _timestep: Any, cb_kwargs: dict[str, Any]):
                total = int(getattr(pipe_inner, "_num_timesteps", requested_steps) or requested_steps)
                preview = None
                try:
                    latents = cb_kwargs.get("latents")
                    if latents is not None:
                        preview = _latent_preview_rgb(latents)
                except Exception:
                    # A preview is cosmetic: the step still reports progress
                    # without one, and the run must not fail over it.
                    log.warning("SDXL latent preview failed at step %s/%s; reporting progress without a preview", int(step) + 1, total, exc_info=True)
                    preview = None
                try:
                    progress_callback(int(step) + 1, total, preview)
                except Exception:
                    # Progress delivery (an IPC event) must not abort the
                    # denoising run; the result is still returned to the caller.
                    log.warning("SDXL progress callback failed at step %s/%s; continuing the run", int(step) + 1, total, exc_info=True)
                return cb_kwargs

            pipe_kwargs["callback_on_step_end"] = _on_step_end
            pipe_kwargs["callback_on_step_end_tensor_inputs"] = ["latents"]

        result = pipe(**pipe_kwargs)
        generated = result.images[0].convert("RGB")
        if generated.size != (width0, height0):
            generated = generated.resize((width0, height0), Image.LANCZOS)
        generated_rgb = np.asarray(generated, dtype=np.float32)

        # VAE roundtrip color correction. Outside the mask the pipeline output is
        # just the original passed through the VAE encode/decode, so the
        # brightness/color shift there is exactly the VAE error. Neutralize that
        # systematic shift on the generated region so the inpainted patch matches
        # the surroundings instead of coming out darker.
        generated_rgb = _match_vae_roundtrip(generated_rgb, image_rgb, alpha)

        # Composite: keep everything outside the (blurred) mask exactly as the
        # original; only the masked region comes from the diffusion output.
        alpha3 = alpha[..., None]
        composed = generated_rgb * alpha3 + image_rgb.astype(np.float32) * (1.0 - alpha3)
        return np.ascontiguousarray(np.clip(np.round(composed), 0, 255).astype(np.uint8))

    def _lama_prefill(
        self,
        *,
        image_rgb: np.ndarray,
        cond_mask_u8: np.ndarray,
        lama_model: str,
    ) -> np.ndarray:
        np = _np()
        image_bytes = _encode_png_bytes_rgb(image_rgb)
        mask_bytes = _encode_png_bytes_gray(cond_mask_u8)
        params: dict[str, Any] = {}
        if lama_model:
            params["model_name"] = lama_model
        result = self._lama_service.inpaint_image_bytes(
            image_bytes, mask_bytes, params=params
        )
        prefilled_png = result.get("image_png", b"")
        if not prefilled_png:
            raise RuntimeError("LaMa-префилл не вернул изображение.")
        prefilled = _decode_rgb_from_png_bytes(prefilled_png)
        if prefilled.shape[:2] != image_rgb.shape[:2]:
            from PIL import Image

            prefilled = np.asarray(
                Image.fromarray(prefilled, mode="RGB").resize(
                    (image_rgb.shape[1], image_rgb.shape[0]), Image.LANCZOS
                ),
                dtype=np.uint8,
            )
        return np.ascontiguousarray(prefilled)

    def _decode_image_rgb(self, image_bytes: bytes) -> np.ndarray:
        np = _np()
        from PIL import Image

        with Image.open(io.BytesIO(image_bytes)) as img:
            return np.ascontiguousarray(np.array(img.convert("RGB"), dtype=np.uint8))

    def _decode_mask(self, mask_bytes: bytes, *, expected_hw: tuple[int, int]) -> np.ndarray:
        np = _np()
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


def _validate_mode_channels(mode: str, in_channels: int) -> None:
    """Enforces that the loaded UNet channel count matches the requested mode."""
    if mode == "nine_channel" and in_channels != 9:
        raise ValueError(
            "Выбран 9-канальный режим, но у модели "
            f"{in_channels}-канальный UNet. Используйте inpaint-модель SDXL "
            "(stable-diffusion-xl-1.0-inpainting-0.1) или переключитесь на 4-канальный режим."
        )
    if mode == "four_channel" and in_channels != 4:
        raise ValueError(
            "Выбран 4-канальный режим, но у модели "
            f"{in_channels}-канальный UNet. Переключитесь на 9-канальный режим "
            "для выделенной inpaint-модели."
        )


@dataclass(frozen=True)
class SdxlModelSource:
    """Where an SDXL pipeline is loaded from, resolved before any load.

    `kind` is `"single_file"` (`path` is a ckpt/safetensors file, `config_dir`
    its diffusers config repo), `"folder"` (`path` is a diffusers folder) or
    `"curated"` (`path` is Rust's download of `CURATED_INPAINT_REPO_ID`,
    loaded with weight `variant`). Every path exists when this is built.
    """

    kind: str
    path: str
    config_dir: str | None = None
    variant: str | None = None


def _require_diffusers_dir(raw: str | None, *, field: str, what: str) -> Path:
    """Return `raw` as an absolute directory holding `model_index.json`.

    `field` names the request field in errors, `what` the model. Raises
    `ValueError` for an empty or relative path and `FileNotFoundError` ("not
    downloaded") when the directory or its `model_index.json` is missing.
    """
    normalized = str(raw or "").strip()
    if not normalized:
        raise ValueError(f"SDXL: поле {field!r} обязательно ({what}).")
    path = Path(normalized)
    if not path.is_absolute():
        raise ValueError(f"SDXL: поле {field!r} должно быть абсолютным путём: {normalized}")
    if not (path / "model_index.json").is_file():
        raise FileNotFoundError(f"SDXL: {what} не скачана (нет {path / 'model_index.json'}).")
    return path


def resolve_model_source(model_path: str, *, config_dir: str, curated_model_dir: str | None) -> SdxlModelSource:
    """Resolve the user's `model_path` into a loadable `SdxlModelSource`.

    A local file must be a ckpt/safetensors checkpoint and takes `config_dir`
    (checked here: `from_single_file` would otherwise fetch the configs from
    the Hub even with `local_files_only=True`). A local folder must be a
    diffusers repo. `CURATED_INPAINT_REPO_ID` resolves to `curated_model_dir`.
    Anything else is refused: no repo id is ever downloaded by the backend.

    Raises `ValueError` / `FileNotFoundError` as documented on
    `SdxlInpaintService.inpaint_image_bytes`.
    """
    if model_path == CURATED_INPAINT_REPO_ID:
        curated = _require_diffusers_dir(
            curated_model_dir, field="sdxl_model_dir", what=f"модель {CURATED_INPAINT_REPO_ID}"
        )
        return SdxlModelSource(kind="curated", path=str(curated), variant=CURATED_INPAINT_VARIANT)
    candidate = Path(model_path).expanduser()
    if candidate.is_file():
        if candidate.suffix.lower() not in MODEL_SUFFIXES:
            raise FileNotFoundError(
                f"Путь к весам SDXL не является ckpt/safetensors файлом: {candidate}"
            )
        config = _require_diffusers_dir(config_dir, field="sdxl_config_dir", what="конфигурация SDXL")
        return SdxlModelSource(kind="single_file", path=str(candidate), config_dir=str(config))
    if candidate.is_dir():
        # Local diffusers-format repo (folder with model_index.json, unet/, vae/, ...).
        if not (candidate / "model_index.json").is_file():
            raise FileNotFoundError(
                "Папка SDXL не похожа на diffusers-репозиторий "
                f"(нет model_index.json): {candidate}"
            )
        return SdxlModelSource(kind="folder", path=str(candidate))
    if candidate.is_absolute():
        raise FileNotFoundError(f"Путь к весам SDXL не найден: {candidate}")
    raise ValueError(
        f"SDXL: {model_path!r} не является локальным файлом или папкой. Идентификаторы "
        f"Hugging Face не загружаются; поддерживается только {CURATED_INPAINT_REPO_ID}, "
        "скачиваемая приложением."
    )


def _diffusers_import_error_text(exc: BaseException) -> str:
    """Return the user-facing text for a failed `diffusers` import.

    Only a `ModuleNotFoundError` for `diffusers` itself means "not installed";
    anything else (an import error raised INSIDE diffusers, e.g. by an
    incompatible transformers) is reported as a broken installation with the
    original `Type: message`, so the user and the log see the real cause instead
    of a misleading "install diffusers".
    """
    if isinstance(exc, ModuleNotFoundError) and exc.name == "diffusers":
        return (
            "Для /inpaint/sdxl требуется пакет diffusers (и transformers). "
            "Установите зависимости backend."
        )
    return (
        "Не удалось импортировать diffusers для /inpaint/sdxl: "
        f"{type(exc).__name__}: {exc}. "
        "Восстановите зависимости backend (проверка окружения установщика)."
    )


def _build_sdxl_inpaint_pipeline(source: SdxlModelSource, device: str) -> Any:
    """Build a `StableDiffusionXLInpaintPipeline` from `source` and move it onto `device`.

    Every load passes `local_files_only=True`: `source` was resolved to
    existing local paths, so nothing reaches the network. Weights are loaded
    in fp16 for a `cuda` device and fp32 otherwise. The returned pipeline has
    progress bars disabled, attention slicing enabled and, in fp16, the VAE
    `force_upcast` flag set.

    The host->device move runs inside `patched_module_to()` (ROCm mmap staging);
    it is a strict no-op on every other build.

    # Raises
    `RuntimeError` if `diffusers` cannot be imported: absent, or present but
    failing to import (an incompatible `transformers` / `huggingface_hub`, a
    missing transitive package); the message carries the real import error, which
    the IPC dispatcher passes through `sanitize_torch_error` on its way to the
    user. Loader errors (a missing component file in an incomplete folder, for
    example) propagate.
    """
    torch = _torch()
    try:
        from diffusers import StableDiffusionXLInpaintPipeline
    except Exception as exc:
        raise RuntimeError(_diffusers_import_error_text(exc)) from exc

    dtype = torch.float16 if str(device).startswith("cuda") else torch.float32
    if source.kind == "single_file":
        pipe = StableDiffusionXLInpaintPipeline.from_single_file(
            source.path, config=source.config_dir, local_files_only=True, torch_dtype=dtype
        )
    elif source.kind == "folder":
        pipe = StableDiffusionXLInpaintPipeline.from_pretrained(
            source.path, local_files_only=True, torch_dtype=dtype
        )
    elif source.kind == "curated":
        pipe = StableDiffusionXLInpaintPipeline.from_pretrained(
            source.path, variant=source.variant, local_files_only=True, torch_dtype=dtype
        )
    else:
        raise ValueError(f"SDXL: неизвестный источник модели {source.kind!r}")

    # On a ROCm build a host->device copy whose source is the writable private
    # file mapping that safetensors hands out stalls ~1-2 s per tensor of >=1 MiB
    # inside amdkfd, and a typical SDXL checkpoint is ~2500 tensors / ~6.9 GB.
    # The stall only exists when the checkpoint dtype already matches `dtype`
    # (an fp16 checkpoint on CUDA here); an fp32 checkpoint is cast on the host
    # first, which allocates anonymous memory and sidesteps it. That condition is
    # detected per tensor by the helper, so this wrapper is unconditional.
    # The patch wraps the stock `DiffusionPipeline.to` instead of replacing it:
    # `to` first does its own bnb / offload / device-map bookkeeping and only
    # then calls `module.to(device, dtype)` per component, and that inner call
    # reaches the patch because `ModelMixin.to` / `PreTrainedModel.to` both end
    # in `super().to(...)`, which resolves the patched `nn.Module.to` class
    # attribute at call time. Staging is per tensor with immediate release, so
    # peak host RSS grows by one tensor, not by the model. Deliberately NOT the
    # diffusers `disable_mmap=True` flag: that reads the whole checkpoint into
    # `bytes` (~2x file size, ~14 GB for SDXL).
    with patched_module_to():
        pipe = pipe.to(device)
    pipe.set_progress_bar_config(disable=True)
    try:
        pipe.enable_attention_slicing()
    except Exception:
        # Optional memory saving: the pipeline still runs without it, only with
        # a higher attention peak.
        log.warning("SDXL: enable_attention_slicing failed; running without attention slicing (device=%s source=%s)", device, source.kind, exc_info=True)
    # SDXL's VAE overflows in fp16 and produces darkened/NaN decodes. Enable the
    # built-in force_upcast so diffusers upcasts the VAE to fp32 only around the
    # decode step (and restores fp16 after); do NOT call upcast_vae() here, which
    # would leave the VAE in fp32 and break the fp16 masked-image encode.
    if dtype == torch.float16:
        try:
            pipe.vae.config.force_upcast = True
        except Exception:
            log.warning("SDXL: could not enable VAE force_upcast; fp16 decodes may come out darkened (device=%s source=%s)", device, source.kind, exc_info=True)
    return pipe


def _apply_scheduler(pipe: Any, scheduler_class: str, scheduler_kwargs: dict[str, Any]) -> None:
    import diffusers

    cls = getattr(diffusers, scheduler_class, None)
    if cls is None:
        raise ValueError(f"diffusers не содержит планировщик {scheduler_class}")
    pipe.scheduler = cls.from_config(pipe.scheduler.config, **scheduler_kwargs)


def _process_mask(mask_u8: np.ndarray, *, dilation: int, blur: int) -> tuple[np.ndarray, np.ndarray]:
    """Returns (binary conditioning mask, soft alpha in [0,1]).

    The binary mask (optionally dilated) is fed to the pipeline; the blurred
    alpha is used only for the final pixel composite so the seam is smooth.
    """
    np = _np()
    cv2 = _maybe_cv2()
    binary = np.where(mask_u8 > 0, 255, 0).astype(np.uint8)
    if dilation > 0 and cv2 is not None:
        ksize = 2 * int(dilation) + 1
        kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (ksize, ksize))
        binary = cv2.dilate(binary, kernel, iterations=1)

    alpha_src = binary
    if blur > 0 and cv2 is not None:
        ksize = 2 * int(blur) + 1
        alpha_src = cv2.GaussianBlur(binary, (ksize, ksize), 0)
    alpha = alpha_src.astype(np.float32) / 255.0
    return np.ascontiguousarray(binary), np.ascontiguousarray(alpha)


def _match_vae_roundtrip(
    generated_rgb: np.ndarray, original_rgb: np.ndarray, alpha: np.ndarray
) -> np.ndarray:
    """Corrects the systematic VAE encode/decode brightness/color shift.

    `generated_rgb` is the full VAE-decoded region; outside the mask it equals the
    original run through the VAE. The per-channel offset measured on the unmasked
    pixels is added back so the masked patch matches the surroundings. The median
    is used for robustness, and the correction is skipped when too few unmasked
    pixels are available to estimate it reliably.
    """
    np = _np()
    unmasked = alpha < 0.5
    if int(np.count_nonzero(unmasked)) < 64:
        return generated_rgb
    orig_f = original_rgb.astype(np.float32)
    diff = orig_f[unmasked] - generated_rgb[unmasked]  # [N, 3]
    offset = np.median(diff, axis=0).astype(np.float32)  # per-channel
    return generated_rgb + offset[None, None, :]


def _round_up(value: int, multiple: int) -> int:
    if multiple <= 1:
        return int(value)
    return int(((int(value) + multiple - 1) // multiple) * multiple)


def _to_pil_resized(img: Any, width: int, height: int, *, nearest: bool = False) -> Any:
    from PIL import Image

    if img.size == (width, height):
        return img
    resample = Image.NEAREST if nearest else Image.LANCZOS
    return img.resize((width, height), resample)


def _encode_png_bytes_rgb(image_rgb: np.ndarray) -> bytes:
    from PIL import Image

    np = _np()
    arr = np.ascontiguousarray(image_rgb.astype(np.uint8))
    with io.BytesIO() as buffer:
        Image.fromarray(arr, mode="RGB").save(buffer, format="PNG")
        return buffer.getvalue()


def _encode_png_bytes_gray(mask_u8: np.ndarray) -> bytes:
    from PIL import Image

    np = _np()
    arr = np.ascontiguousarray(mask_u8.astype(np.uint8))
    with io.BytesIO() as buffer:
        Image.fromarray(arr, mode="L").save(buffer, format="PNG")
        return buffer.getvalue()


def _decode_rgb_from_png_bytes(data: bytes) -> np.ndarray:
    from PIL import Image

    np = _np()
    with Image.open(io.BytesIO(data)) as img:
        return np.ascontiguousarray(np.array(img.convert("RGB"), dtype=np.uint8))


def _np():
    try:
        import numpy as np  # type: ignore

        return np
    except Exception as exc:
        raise RuntimeError(
            "Для /inpaint/sdxl требуется пакет numpy. Установите зависимости backend."
        ) from exc


def _torch():
    try:
        import torch  # type: ignore

        return torch
    except Exception as exc:
        raise RuntimeError(
            "Для /inpaint/sdxl требуется пакет torch. Установите зависимости backend."
        ) from exc


def _maybe_cv2():
    try:
        import cv2  # type: ignore

        return cv2
    except Exception:
        return None


def _to_int(value: Any, default: int) -> int:
    try:
        if isinstance(value, bool):
            return default
        return int(value)
    except Exception:
        return default


def _clamp_int(value: Any, *, default: int, low: int, high: int) -> int:
    return max(low, min(high, _to_int(value, default)))


def _clamp_float(value: Any, *, default: float, low: float, high: float) -> float:
    try:
        if isinstance(value, bool):
            out = default
        else:
            out = float(value)
    except Exception:
        out = default
    return max(low, min(high, out))


#: `General.ai_device` resolution with this service's device policy (owner:
#: `runtime.torch_device`); patched by name in the test suite.
_resolve_selected_backend_device = functools.partial(resolve_selected_backend_device, mps_last_resort=True)

#: Allocator-cache release with this service's policy (owner: `runtime.torch_device`);
#: patched by name in the test suite.
_clear_torch_cache = functools.partial(clear_torch_cache, owner="SDXL")
