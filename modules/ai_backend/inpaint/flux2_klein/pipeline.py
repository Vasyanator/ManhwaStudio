"""
File: modules/ai_backend/inpaint/flux2_klein/pipeline.py

Purpose:
What a FLUX.2 klein run does WITH the modules the large-model manager placed:
the prompt encode, the warm-up decode, the denoise call's device check and the
VAE decode with its tiling options, plus the per-component residency
vocabulary and the action matrix the client renders. Nothing here decides
where weights live, loads a checkpoint or moves a module: that is the manager
(`runtime.large_models`), driven by `service.py` through `large_model.py`'s
specs. Nothing here owns the service lock either.

Main responsibilities:
- residency and actions (`RESIDENCY_*`, `residency_of`, `_component_actions`,
  the name validators, the busy refusal);
- `_encode_prompt_phase` (the prompt through the pipeline's own
  `encode_prompt`, with nothing but the encoder);
- `_warmup_vae_decode` (one 64x64 decode after a fresh load: primes the
  allocator and, on ROCm, compiles the MIOpen kernels the real decode reuses);
- `_require_execution_device` (the pipeline must compute where the manager
  placed the transformer);
- `decode_region_latents` - ONE decode attempt with the tiling the options
  ask for: the request's tiling flag where it engages, and the lowered tile
  of the ladder's last FLUX.2 fallback (`_lowered_vae_tile_thresholds`, whose
  two thresholds move together), plus the gates that say whether tiling can
  engage at all (`_vae_tiling_engages`, `_emergency_tile_latent_size`).

Notes:
- An out-of-memory decode is NOT recovered here: the exception reaches the
  manager's `Session.run_phase`, whose ladder re-plans (spec fallbacks
  `large_model.FALLBACK_VAE_TILING` / `FALLBACK_VAE_TILE_LOWERED`, then the next
  «Видеопамять» step, which takes the transformer off the device) and calls
  the decode again from the same host copy of the latents.
- torch / diffusers are imported lazily inside the functions that need them.
"""

from __future__ import annotations

import contextlib
import logging
from typing import Any, Iterator

from .components import TEXT_ENCODER_OUT_LAYER_INDICES

log = logging.getLogger(__name__)

#: Spatial size of the warm-up latent handed to the VAE, in latent cells: a real
#: forward (it primes the allocator and the MIOpen kernels) that costs a 64x64
#: image.
WARMUP_LATENT_CELLS = 8

#: Latent tile side of the lowered-threshold fallback (diffusers' own
#: "overlapping 64x64 tiles"; a 512 px tile on the klein VAE, a quarter of the
#: shipped 1024 px threshold's area). Used per decode only and restored
#: afterwards; see `_lowered_vae_tile_thresholds`.
EMERGENCY_TILE_LATENT_MIN_SIZE = 64


# ---------------------------------------------------------------------------
# Per-component residency and actions (dev-docs/flux2_component_residency.md)
# ---------------------------------------------------------------------------
#: The component holds no bytes anywhere (dropped, never loaded).
RESIDENCY_NOT_LOADED = "not_loaded"
#: Every byte the component holds is in host memory.
RESIDENCY_RAM = "ram"
#: Every byte the component holds is on the compute device.
RESIDENCY_GPU = "gpu"
#: Kept for the wire vocabulary: an accelerate offload hook owned the module.
#: The manager never installs one, so `residency_of` never answers it.
RESIDENCY_OFFLOADED = "offloaded"
#: Split between device and host (a juggled component parked in part).
#: REPORTED, never rounded to a neighbouring state.
RESIDENCY_MIXED = "mixed"

#: The three weight-bearing components a client can ask about or act on.
ACTIONABLE_COMPONENTS = ("text_encoder", "transformer", "vae")

#: Every action name the wire accepts, in the order `actions` lists them.
COMPONENT_ACTIONS = ("load", "unload", "to_ram", "to_gpu", "warmup")


def residency_of(device_bytes: int, host_bytes: int) -> str:
    """The `RESIDENCY_*` literal of a component holding `device_bytes` / `host_bytes` (the manager's ledger parts)."""
    if device_bytes > 0 and host_bytes > 0:
        return RESIDENCY_MIXED
    if device_bytes > 0:
        return RESIDENCY_GPU
    if host_bytes > 0:
        return RESIDENCY_RAM
    return RESIDENCY_NOT_LOADED


def _component_actions(component: str, residency: str, *, pipeline_loaded: bool) -> list[str]:
    """The actions that are genuinely possible for `component` right now.

    This list is the AUTHORITY the client renders: the rule lives here and
    nowhere else. Ordered by `COMPONENT_ACTIONS`.

    - The text encoder is its own manager handle: `load` when it holds nothing,
      `unload` otherwise. It never offers `to_gpu`: where it runs is the
      steps' decision (on the GPU when the device holds it beside the
      pipeline, else on the CPU), never a button.
    - `load` / `unload` appear on BOTH the transformer and the VAE, always
      together: the pipeline is one manager handle.
    - The transformer offers `to_ram` while it is on the device and `to_gpu`
      while it is in RAM; both are between-runs states of the manager
      (`service._component_action_locked`), never a hand-made move. A `mixed`
      transformer (juggled, parked in part) offers neither.
    - The VAE offers `warmup` while it is on the device; it has no
      `to_ram`/`to_gpu` (the decode always runs it on the device).
    """
    if component == "text_encoder":
        return ["load"] if residency == RESIDENCY_NOT_LOADED else ["unload"]

    if not pipeline_loaded:
        return ["load"]

    actions = ["unload"]
    if component == "transformer":
        if residency == RESIDENCY_GPU:
            actions.append("to_ram")
        elif residency == RESIDENCY_RAM:
            actions.append("to_gpu")
    elif component == "vae" and residency == RESIDENCY_GPU:
        actions.append("warmup")
    return sorted(actions, key=COMPONENT_ACTIONS.index)


def _require_component_name(value: Any) -> str:
    """Validate a wire `component` name against `ACTIONABLE_COMPONENTS`.

    # Raises
    `ValueError` naming the accepted values (the tokenizer and the scheduler
    carry no weights, so no residency action means anything for them).
    """
    name = str(value or "").strip()
    if name in ACTIONABLE_COMPONENTS:
        return name
    accepted = ", ".join(f"«{item}»" for item in ACTIONABLE_COMPONENTS)
    raise ValueError(
        f"FLUX.2 klein: неизвестный компонент «{name}». Допустимые значения: {accepted}."
    )


def _require_action_name(value: Any) -> str:
    """Validate a wire `action` name against `COMPONENT_ACTIONS`.

    # Raises
    `ValueError` naming the accepted values. Whether the action is possible
    right now is answered under the service lock
    (`Flux2KleinInpaintService._require_action_available_locked`).
    """
    name = str(value or "").strip()
    if name in COMPONENT_ACTIONS:
        return name
    accepted = ", ".join(f"«{item}»" for item in COMPONENT_ACTIONS)
    raise ValueError(
        f"FLUX.2 klein: неизвестное действие «{name}». Допустимые значения: {accepted}."
    )


def _component_busy_message(component: str, action: str) -> str:
    """The refusal text for an action requested while the service is busy."""
    return (
        f"FLUX.2 klein занят: действие «{action}» над компонентом «{component}» сейчас "
        "невозможно. Генерация или другая операция удерживают модель до своего "
        "завершения — дождитесь её окончания и повторите."
    )


# =====================================================================
#  Prompt encode, warm-up, device check
# =====================================================================
def _warmup_vae_decode(vae: Any, device: Any) -> bool:
    """Run one tiny decode on the freshly placed VAE; returns whether it ran.

    The latent is `WARMUP_LATENT_CELLS` cells square (a 64x64 image): a real
    forward that initializes the caching allocator and, on ROCm, compiles the
    MIOpen convolution kernels the real decode reuses. Without
    `config.latent_channels` no valid input can be synthesized: only the device
    is synchronized and that is logged (a klein VAE always carries the field).
    `torch.no_grad()` is explicit: this call is not inside `pipeline.__call__`.
    """
    import torch

    channels = getattr(getattr(vae, "config", None), "latent_channels", None)
    if not isinstance(channels, int) or isinstance(channels, bool) or channels <= 0:
        log.warning(
            "FLUX.2 klein: у VAE нет config.latent_channels — прогревочный проход пропущен, "
            "выполнена только синхронизация устройства."
        )
        _synchronize_device(device)
        return False

    with torch.no_grad():
        latents = torch.zeros(
            (1, int(channels), WARMUP_LATENT_CELLS, WARMUP_LATENT_CELLS),
            dtype=vae.dtype,
            device=vae.device,
        )
        vae.decode(latents, return_dict=False)
    _synchronize_device(device)
    return True


def _synchronize_device(device: Any) -> None:
    """Block until every queued kernel and copy on `device` has retired; a no-op off CUDA/HIP."""
    kind = getattr(device, "type", None) or str(device or "").split(":")[0]
    if kind != "cuda":
        return
    import torch

    if torch.cuda.is_available():
        torch.cuda.synchronize()


def _encode_prompt_phase(
    pipeline_cls: Any,
    text_encoder: Any,
    tokenizer: Any,
    prompt: str,
    max_sequence_length: int,
    device: Any,
) -> Any:
    """Encode one prompt with nothing but the encoder; returns CPU embeddings.

    A pipeline instance holding ONLY the encoder and its tokenizer is built
    (every other component `None`, which `Flux2KleinInpaintPipeline.__init__`
    tolerates) so the pipeline's own `encode_prompt` runs: the Qwen3 chat
    template, the attention mask and the stacking of hidden states live there.
    `device` is where the encoder computes (the run device, or `cpu` when the
    plan runs it from RAM). `text_encoder_out_layers` is passed explicitly,
    never inherited from diffusers' default: `components.ENCODER_KEEP_LAYERS`
    truncates the encoder to exactly those layers. The result is moved to the
    host so the prompt cache never pins device memory.
    """
    import torch

    holder = pipeline_cls(
        scheduler=None, vae=None, text_encoder=text_encoder, tokenizer=tokenizer, transformer=None
    )
    with torch.no_grad():
        prompt_embeds, _text_ids = holder.encode_prompt(
            prompt=prompt or "",
            device=device,
            max_sequence_length=max_sequence_length,
            text_encoder_out_layers=TEXT_ENCODER_OUT_LAYER_INDICES,
        )
    return prompt_embeds.detach().to("cpu")


def _require_execution_device(pipe: Any, device: Any) -> None:
    """Check the pipeline will build its tensors on `device`, or raise saying why.

    `_execution_device` places the region, the noise and the mask latents and is
    derived from the components; a mismatch would surface as a device error
    several frames inside the VAE. A pipeline without the property (a test
    double) is not probed.

    # Raises
    `RuntimeError` naming every component's device when the probe disagrees.
    """
    probed = getattr(pipe, "_execution_device", None)
    if probed is None or str(probed) == str(device):
        return
    devices = ", ".join(
        f"{name}={getattr(getattr(pipe, name, None), 'device', 'нет')}"
        for name in ("transformer", "vae")
    )
    raise RuntimeError(
        f"FLUX.2 klein: пайплайн собирается считать на «{probed}», хотя трансформер размещён на "
        f"«{device}». Размещение компонентов: {devices}. Запуск остановлен до обращения к модели — "
        "иначе ошибка всплыла бы внутри VAE как несовпадение устройств."
    )


# =====================================================================
#  VAE decode
# =====================================================================
def _latent_spatial_sides(latents: Any) -> tuple[int, int] | None:
    """`(height, width)` of a latent tensor in latent cells, or `None` without a usable 2-D shape."""
    shape = getattr(latents, "shape", None)
    if shape is None:
        return None
    try:
        dims = tuple(int(dim) for dim in shape)
    except (TypeError, ValueError):
        return None
    if len(dims) < 2 or dims[-1] <= 0 or dims[-2] <= 0:
        return None
    return dims[-2], dims[-1]


def _vae_tile_latent_min_size(vae: Any) -> int | None:
    """The VAE instance's current latent tiling threshold, or `None` when it has none."""
    value = getattr(vae, "tile_latent_min_size", None)
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        return None
    return value


def _vae_latent_scale_factor(vae: Any) -> int | None:
    """Pixels per latent cell (`2 ** (len(block_out_channels) - 1)`), `None` when the config cannot give it."""
    blocks = getattr(getattr(vae, "config", None), "block_out_channels", None)
    if not isinstance(blocks, (list, tuple)) or not blocks:
        return None
    return 2 ** (len(blocks) - 1)


def _vae_tiling_engages(vae: Any, latents_cpu: Any) -> bool:
    """Whether a tiled decode would ACTUALLY happen for these latents (diffusers' own gate); `False` when unknown."""
    sides = _latent_spatial_sides(latents_cpu)
    threshold = _vae_tile_latent_min_size(vae)
    if sides is None or threshold is None:
        return False
    return max(sides) > threshold


@contextlib.contextmanager
def _lowered_vae_tile_thresholds(vae: Any, latent_min_size: int, scale: int) -> Iterator[None]:
    """Temporarily shrink the VAE's tile, restoring both thresholds afterwards.

    **The two thresholds are ONE setting and must move together**:
    `tiled_decode` slices by `tile_latent_min_size` but blends and crops by
    `tile_sample_min_size` and assumes `sample == latent * scale` (lowering only
    one gave a sibling project a mosaic of nine shifted copies). The change is
    PER DECODE, restored in a `finally`.
    """
    missing = object()
    previous_latent = vae.tile_latent_min_size
    previous_sample = getattr(vae, "tile_sample_min_size", missing)
    vae.tile_latent_min_size = latent_min_size
    vae.tile_sample_min_size = latent_min_size * scale
    try:
        yield
    finally:
        vae.tile_latent_min_size = previous_latent
        if previous_sample is missing:
            delattr(vae, "tile_sample_min_size")
        else:
            vae.tile_sample_min_size = previous_sample


def _emergency_tile_latent_size(vae: Any, latents_cpu: Any) -> tuple[int, int] | None:
    """`(latent tile size, scale)` for the lowered tile, or `None` when it cannot engage for these latents."""
    threshold = _vae_tile_latent_min_size(vae)
    scale = _vae_latent_scale_factor(vae)
    sides = _latent_spatial_sides(latents_cpu)
    if threshold is None or scale is None or sides is None:
        return None
    target = EMERGENCY_TILE_LATENT_MIN_SIZE
    if threshold <= target or max(sides) <= target:
        return None
    return target, scale


def _set_vae_tiling(vae: Any, enabled: bool) -> None:
    """`enable_tiling()` / `disable_tiling()` when the VAE has them (a request: no threshold moves)."""
    method = getattr(vae, "enable_tiling" if enabled else "disable_tiling", None)
    if callable(method):
        method()


def _decode_once(vae: Any, image_processor: Any, latents_cpu: Any) -> Any:
    """Decode a CPU copy of the latents into one PIL image on the VAE's own device (under `no_grad`)."""
    import torch

    with torch.no_grad():
        latents = latents_cpu.to(device=vae.device, dtype=vae.dtype)
        image = vae.decode(latents, return_dict=False)[0]
        return image_processor.postprocess(image, output_type="pil")[0]


def decode_region_latents(vae: Any, image_processor: Any, latents_cpu: Any, *, tiling: bool, lowered: bool) -> tuple[Any, bool]:
    """One VAE decode of `latents_cpu`; returns `(image, tiled)`.

    `tiling` is the request's flag (or the tiling fallback's); `lowered` is the
    lowered-tile fallback, which lowers both thresholds for this decode where
    the region exceeds the emergency tile. `tiled` says whether the tiled path
    was really entered (it is what `applied["vae_tiling"]` may claim). An
    out-of-memory error propagates to the manager's ladder unchanged.
    """
    emergency = _emergency_tile_latent_size(vae, latents_cpu) if lowered else None
    if emergency is None:
        _set_vae_tiling(vae, tiling)
        return _decode_once(vae, image_processor, latents_cpu), tiling and _vae_tiling_engages(vae, latents_cpu)
    target, scale = emergency
    with _lowered_vae_tile_thresholds(vae, target, scale):
        _set_vae_tiling(vae, True)
        log.warning(
            "FLUX.2 klein: VAE decode with the tile lowered to %d latent cells / %d px; the tiles are "
            "blended, so the image is not bit-identical to an untiled decode.",
            target,
            target * scale,
        )
        return _decode_once(vae, image_processor, latents_cpu), True
