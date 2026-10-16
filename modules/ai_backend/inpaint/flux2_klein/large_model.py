"""
File: modules/ai_backend/inpaint/flux2_klein/large_model.py

Purpose:
FLUX.2 klein as the large-model manager sees it: the `LargeModelSpec`s the
service registers with `runtime.large_models.manager.LargeModelManager`, the
options objects a run hands to the manager, and the request shape. One spec
family serves BOTH variants (klein 9B and 4B): sizes, block counts and layer
counts are read from the user's files, never assumed.

Main responsibilities:
- two kinds of handle, registered under separate model names:
  - the PIPELINE (`transformer` with its 8 + 24 (9B) double / single stream
    blocks, `vae` monolithic; phases `denoise`, `decode`), one spec per compute
    dtype (`pipeline_model_name`);
  - the TEXT ENCODER (Qwen3 truncated to `components.ENCODER_KEEP_LAYERS`
    decoder layers, one block per layer; phase `encode`), one spec per encoder
    precision (bf16, or weight-only fp8 done by the conversion) and per
    execution rule (the run device or the CPU, or the CPU only:
    `encoder_model_name`);
- `PipelineOptions` / `EncoderOptions` (the manager's `options`), built from a
  normalized request by `pipeline_options` / `encoder_options`, and
  `request_shape`;
- the decode fallbacks of the degrade ladder (`FALLBACK_VAE_TILING`,
  `FALLBACK_VAE_TILE_LOWERED`): tiled VAE decode where tiling engages for the
  region, then the VAE's tile lowered to `pipeline.EMERGENCY_TILE_LATENT_MIN_SIZE`
  latent cells;
- `require_loadable` - the refusals a run must give BEFORE the manager reads a
  header (fp8_scaled checkpoint, missing transformer config, a VAE file without
  its folder, a too-shallow encoder), with the wording the service always used;
- `register_specs` - registration of every FLUX.2 spec on a manager.

Notes:
- Why the encoder is its own handle: a prompt-cache build and the encoder's
  «load» action must touch the encoder only, and a run whose prompt is cached
  must not need an encoder installed at all. The pipeline is admitted first,
  the encoder second (it then plans on what the pipeline's reservation left:
  on the GPU when both fit, else on the CPU from RAM, `tiers.template_for`).
- Why one spec per dtype / per encoder precision: `ComponentSpec.convert` sees
  only `(file_key, tensor)`, so the stored dtype is a property of the spec.
- Why a CPU-only encoder spec: where a component may compute is the spec's
  static `exec_devices_allowed`, never an option. A legacy request (the stage-1
  `placement` / `profile` wire, `memory.encoder_cpu_pinned`) means "the encoder
  runs on the host", so it is planned under the CPU-only spec
  (`EncoderOptions.cpu_only`); its key differs from the other spec's, so the
  two never share a handle.
- Conversions are device-agnostic torch code (the DISK tier applies them to
  device views on every pass); torch / diffusers / transformers are imported
  lazily inside the functions.
- The calibration tables measure the `flux2_klein` phases per device class
  (`runtime/large_models/calibration/`, fitted by `tools/large_model_calibration/`);
  a phase whose entry is unmeasured (or a borrowed table of an unknown device
  class) is `calibrated=False`: the planner chooses plans with the activation
  estimates but may admit it on its exact weight bytes (`planner` module
  contract, judged per phase).
"""

from __future__ import annotations

import functools
import logging
import re
from collections.abc import Iterator, Sequence
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any

from ...runtime.large_models.manager import LargeModelManager
from ...runtime.large_models.spec import (
    ComponentSpec,
    ExecDevice,
    Fallback,
    FormatInfo,
    LargeModelSpec,
    PhaseSpec,
    PhaseWork,
    RequestShape,
    Role,
)
from . import attention, components, conversion
from .params import REGION_SIZE_MULTIPLE, effective_steps, text_encoder_dtype_name
from .pipeline import EMERGENCY_TILE_LATENT_MIN_SIZE

log = logging.getLogger(__name__)

#: The calibration-table family of every FLUX.2 spec.
FAMILY = "flux2_klein"

#: The one weight format of a FLUX.2 checkpoint the manager plans (bf16 files;
#: fp8_scaled checkpoints are refused by `require_loadable`).
WEIGHT_FORMAT = "bf16"

#: Component names (the manager's, the residency report's and the actions').
TRANSFORMER = "transformer"
VAE = "vae"
TEXT_ENCODER = "text_encoder"

#: Phase names.
PHASE_ENCODE = "encode"
PHASE_DENOISE = "denoise"
PHASE_DECODE = "decode"

#: Names of the decode fallbacks, in ladder order.
FALLBACK_VAE_TILING = "vae_tiling"
FALLBACK_VAE_TILE_LOWERED = "vae_tile_lowered"

_PIPELINE_MODELS = {"bfloat16": "flux2_klein", "float16": "flux2_klein.float16"}
#: `(fp8, cpu_only) -> model name`.
_ENCODER_MODELS = {
    (False, False): "flux2_klein_text_encoder",
    (True, False): "flux2_klein_text_encoder.fp8",
    (False, True): "flux2_klein_text_encoder.cpu",
    (True, True): "flux2_klein_text_encoder.fp8.cpu",
}


def pipeline_model_name(dtype: str) -> str:
    """The manager model name of the pipeline spec computing in `dtype` (`params.VALID_DTYPES`)."""
    return _PIPELINE_MODELS[dtype]


def encoder_model_name(fp8: bool, cpu_only: bool = False) -> str:
    """The manager model name of the text-encoder spec (`fp8` = weight-only fp8 linears; `cpu_only` = it computes on the CPU only)."""
    return _ENCODER_MODELS[(bool(fp8), bool(cpu_only))]


def pipeline_model_names() -> tuple[str, ...]:
    """Every pipeline model name, for `unload` and the residency report."""
    return tuple(_PIPELINE_MODELS.values())


def encoder_model_names() -> tuple[str, ...]:
    """Every text-encoder model name, for `unload` and the residency report."""
    return tuple(_ENCODER_MODELS.values())


# =====================================================================
#  Options and request shape
# =====================================================================
@dataclass(frozen=True)
class PipelineOptions:
    """The manager's `options` of one pipeline run (everything the phases' work and the weights depend on).

    `steps` are the EFFECTIVE denoise steps (`params.effective_steps`) and
    `guidance` whether classifier-free guidance runs (it doubles the transformer
    passes); `vae_tiling` is the request's tiling flag, switched on by the
    tiling fallback; `vae_tile_lowered` is set by the lowered-threshold
    fallback only; `vae_tile_px` / `vae_scale` are the VAE's tiling threshold
    (output pixels) and pixels per latent cell from its `config.json`.
    """

    transformer_path: str
    vae_path: str
    dtype: str
    max_sequence_length: int
    steps: int
    guidance: bool
    text_attention_in_mask: bool
    vae_tiling: bool
    vae_tile_px: int
    vae_scale: int | None
    vae_tile_lowered: bool = False


@dataclass(frozen=True)
class EncoderOptions:
    """The manager's `options` of one text-encoder use.

    `cpu_only` selects the CPU-only spec (`encoder_model_name`): set for a
    legacy request (`memory.encoder_cpu_pinned`), it changes the residency key.
    """

    text_encoder_path: str
    fp8: bool
    max_sequence_length: int
    cpu_only: bool = False


def vae_latent_scale(vae_path: str) -> int | None:
    """Pixels per latent cell of the VAE at `vae_path` (`2 ** (len(block_out_channels) - 1)`), `None` when unreadable."""
    folder = components.component_dir_for_path(Path(vae_path))
    data = components._read_json_object(folder / components._MODEL_CONFIG_MARKER) if folder.is_dir() else None
    blocks = (data or {}).get("block_out_channels")
    if not isinstance(blocks, list) or not blocks:
        return None
    return 2 ** (len(blocks) - 1)


def pipeline_options(normalized: dict[str, Any], *, guidance: bool) -> PipelineOptions:
    """`PipelineOptions` of a normalized request (`guidance` = the effective CFG decision of the service)."""
    return PipelineOptions(
        transformer_path=normalized["transformer_path"],
        vae_path=normalized["vae_path"],
        dtype=normalized["dtype"],
        max_sequence_length=int(normalized["max_sequence_length"]),
        steps=effective_steps(int(normalized["steps"]), float(normalized["strength"])),
        guidance=bool(guidance),
        text_attention_in_mask=bool(normalized["text_attention_in_mask"]),
        vae_tiling=bool(normalized["vae_tiling"]),
        vae_tile_px=components.vae_tile_threshold_pixels(normalized["vae_path"]),
        vae_scale=vae_latent_scale(normalized["vae_path"]),
    )


def encoder_options(normalized: dict[str, Any], *, cpu_only: bool = False) -> EncoderOptions:
    """`EncoderOptions` of a normalized request (`cpu_only`: the service passes `memory.encoder_cpu_pinned`)."""
    return EncoderOptions(
        text_encoder_path=normalized["text_encoder_path"],
        fp8=bool(normalized["text_encoder_fp8"]),
        max_sequence_length=int(normalized["max_sequence_length"]),
        cpu_only=bool(cpu_only),
    )


def request_shape(width: int, height: int, *, with_reference: bool, max_sequence_length: int, steps: int) -> RequestShape:
    """The `RequestShape` of one region edit (a marks reference has the region's size)."""
    references = ((int(width), int(height)),) if with_reference else ()
    return RequestShape(width=int(width), height=int(height), references=references, prompt_tokens=int(max_sequence_length), steps=int(steps))


def tiling_engages(options: PipelineOptions, width: int, height: int) -> bool:
    """Whether a tiled decode of this region really enters the tiled path with the VAE's own threshold."""
    return max(int(width), int(height)) > int(options.vae_tile_px)


def lowered_tiling_engages(options: PipelineOptions, width: int, height: int) -> bool:
    """Whether the lowered-threshold rung can engage: the VAE's scale is known and the region exceeds the emergency tile."""
    if options.vae_scale is None:
        return False
    side = EMERGENCY_TILE_LATENT_MIN_SIZE * options.vae_scale
    return side < int(options.vae_tile_px) and max(int(width), int(height)) > side


# =====================================================================
#  Phase work
# =====================================================================
def _latent_tokens(width: int, height: int) -> int:
    return (int(width) // REGION_SIZE_MULTIPLE) * (int(height) // REGION_SIZE_MULTIPLE)


def _denoise_work(shape: RequestShape, options: PipelineOptions) -> PhaseWork:
    """The joint sequence `[text L | noisy N | region clean copy N | reference R]`, every step (twice under CFG)."""
    noisy = _latent_tokens(shape.width, shape.height)
    reference = sum(attention.condition_tokens(width, height) for width, height in shape.references)
    text = options.max_sequence_length
    mask_bytes = attention.text_attention_mask_bytes(text, noisy, reference, options.dtype) if options.text_attention_in_mask else 0
    passes = max(1, options.steps) * (2 if options.guidance else 1)
    return PhaseWork(tokens=text + 2 * noisy + reference, passes=passes, variant="", extra_device_bytes=mask_bytes)


def _decode_work(shape: RequestShape, options: PipelineOptions) -> PhaseWork:
    """Decoded pixels of the largest tile alive at once (the whole region when no tiling engages)."""
    pixels = shape.width * shape.height
    if options.vae_tile_lowered and lowered_tiling_engages(options, shape.width, shape.height):
        assert options.vae_scale is not None  # checked by lowered_tiling_engages
        side = EMERGENCY_TILE_LATENT_MIN_SIZE * options.vae_scale
        pixels = min(shape.width, side) * min(shape.height, side)
    elif options.vae_tiling and tiling_engages(options, shape.width, shape.height):
        side = options.vae_tile_px
        pixels = min(shape.width, side) * min(shape.height, side)
    return PhaseWork(pixels=pixels, variant=options.dtype)


def _encode_work(shape: RequestShape, options: EncoderOptions) -> PhaseWork:
    """One prompt at `max_sequence_length` tokens."""
    return PhaseWork(tokens=options.max_sequence_length, variant="text")


# =====================================================================
#  Fallbacks of the decode
# =====================================================================
def _apply_vae_tiling(options: PipelineOptions) -> PipelineOptions:
    """Rung "tiled decode": the request's tiling switched on.

    On a region below the VAE's threshold tiling cannot engage and the retry
    changes nothing but costs one of the ladder's retries; the next rung (the
    lowered tile) is what helps there.
    """
    return replace(options, vae_tiling=True)


def _apply_vae_tile_lowered(options: PipelineOptions) -> PipelineOptions:
    """Rung "lowered VAE tile": both tile thresholds lowered together for this decode (`pipeline.decode_region_latents`)."""
    return replace(options, vae_tiling=True, vae_tile_lowered=True)


_DECODE_FALLBACKS = (
    Fallback(FALLBACK_VAE_TILING, frozenset({PHASE_DECODE}), _apply_vae_tiling),
    Fallback(FALLBACK_VAE_TILE_LOWERED, frozenset({PHASE_DECODE}), _apply_vae_tile_lowered),
)


# =====================================================================
#  Files and refusals
# =====================================================================
def _shards(folder: Path) -> list[Path]:
    return components.component_safetensors_shards(folder)


def transformer_files(path: str) -> list[Path]:
    """The transformer's safetensors files: the single file, or every shard of a diffusers folder."""
    source = Path(path)
    return _shards(source) if source.is_dir() else [source]


def vae_files(path: str) -> list[Path]:
    """The VAE's safetensors files (a weights file is normalized to its folder, `components.component_dir_for_path`)."""
    return _shards(components.component_dir_for_path(Path(path)))


def encoder_files(path: str) -> list[Path]:
    """The text encoder's safetensors files (index weight map first, then the folder glob)."""
    return _shards(components.component_dir_for_path(Path(path)))


def transformer_config_dir(path: str) -> Path:
    """The directory of the transformer's `config.json` (the folder itself, or the one found next to a single file).

    # Raises
    `FileNotFoundError` with the established wording when a single file has no
    config next to it; `ValueError` when the config names another class.
    """
    source = Path(path)
    config_dir = source if source.is_dir() else components.find_transformer_config_dir(source)
    if config_dir is None:
        raise FileNotFoundError(components._missing_transformer_config_message(source))
    if not source.is_dir():
        components.validate_transformer_config_dir(config_dir)
    return config_dir


def require_loadable(normalized: dict[str, Any], *, encoder: bool) -> None:
    """Refuse a request the manager could not load, BEFORE any header is inventoried, with the service's own wording.

    Checks the transformer (fp8_scaled refused, a `model.diffusion_model.`
    prefixed file refused, a single file needs its config), the VAE (a folder with `config.json`; diffusers 0.39 has no
    single-file VAE loader) and, when `encoder`, that the encoder folder is a
    transformers folder deep enough for `ENCODER_KEEP_LAYERS`.

    # Raises
    `ValueError` / `FileNotFoundError` naming the file and the way out.
    """
    source = Path(normalized["transformer_path"])
    if source.is_dir():
        components._reject_fp8_scaled_directory(source)
    else:
        header = components.read_safetensors_header(source)
        if components.is_fp8_scaled_checkpoint(header):
            raise ValueError(components._fp8_scaled_message(source))
        conversion.require_known_layout(header, source=source)
    transformer_config_dir(normalized["transformer_path"])
    vae_dir = components.component_dir_for_path(Path(normalized["vae_path"]))
    if not vae_dir.is_dir():
        raise ValueError(
            f"Не удалось загрузить VAE из файла {vae_dir.name}: рядом нет config.json. Укажите каталог VAE в "
            "формате diffusers (config.json + diffusion_pytorch_model.safetensors)."
        )
    if encoder:
        require_loadable_encoder(normalized)


def require_loadable_encoder(normalized: dict[str, Any]) -> None:
    """Refuse an encoder path that is not a transformers folder, or one shallower than `ENCODER_KEEP_LAYERS`.

    # Raises
    `ValueError` naming the folder (and, for a shallow encoder, the counts).
    """
    encoder_dir = components.component_dir_for_path(Path(normalized["text_encoder_path"]))
    if not encoder_dir.is_dir():
        raise ValueError(f"Путь текстового энкодера должен быть каталогом в формате transformers: {encoder_dir}")
    components.text_encoder_truncation_kwargs(encoder_dir, components.ENCODER_KEEP_LAYERS)


# =====================================================================
#  Transformer: BFL or diffusers keys -> diffusers state keys
# =====================================================================
_BLOCK_RE = re.compile(r"^(transformer_blocks|single_transformer_blocks)\.(\d+)\.")


@functools.lru_cache(maxsize=4096)
def transformer_state_keys(file_key: str) -> tuple[str, ...]:
    """The diffusers state keys one transformer file tensor becomes (several for a fused qkv).

    A BFL key goes through diffusers' own per-key converter
    (`conversion.convert_bfl_tensor`), here on a `meta` placeholder: the
    produced NAMES do not depend on the tensor's values (the 6x6 shape only
    has to survive a split into 3 or 2). Any other key is already a diffusers key.
    """
    if not conversion.is_bfl_key(file_key):
        return (file_key,)
    import torch

    return tuple(conversion.convert_bfl_tensor(file_key, torch.empty(6, 6, device="meta")))


def transformer_block_of(state_key: str) -> str | None:
    """`transformer_blocks.<i>` / `single_transformer_blocks.<i>`; `None` for the embedders, modulations and final layer."""
    match = _BLOCK_RE.match(state_key)
    return f"{match.group(1)}.{match.group(2)}" if match else None


def transformer_block_order(names: Sequence[str]) -> list[str]:
    """Forward order: every double-stream block by index, then every single-stream block."""
    return sorted(names, key=lambda name: (0 if name.startswith("transformer_blocks.") else 1, int(name.rsplit(".", 1)[1])))


def _torch_dtype(name: str) -> Any:
    import torch

    return getattr(torch, name)


def _cast_floating(tensor: Any, dtype: Any) -> Any:
    return tensor.to(dtype) if tensor.is_floating_point() and tensor.dtype != dtype else tensor


def _two_byte_float(file_key: str, dtype_token: str) -> int | None:
    """Device bytes per element of a float tensor cast to a 2-byte compute dtype; others keep theirs."""
    return 2 if dtype_token in ("BF16", "F16", "F32") else None


def _make_transformer_convert(dtype_name: str) -> Any:
    def convert(file_key: str, tensor: Any) -> Iterator[tuple[str, Any]]:
        dtype = _torch_dtype(dtype_name)
        if not conversion.is_bfl_key(file_key):
            yield file_key, _cast_floating(tensor, dtype)
            return
        for key, value in conversion.convert_bfl_tensor(file_key, tensor).items():
            yield key, _cast_floating(value, dtype)

    return convert


def _build_transformer_meta(options: PipelineOptions, weight_format: str) -> Any:
    """`Flux2Transformer2DModel` from the checkpoint's own config, on `meta` (`guidance_embeds=False`: klein has none)."""
    from accelerate import init_empty_weights
    from diffusers import Flux2Transformer2DModel

    config = dict(Flux2Transformer2DModel.load_config(str(transformer_config_dir(options.transformer_path)), local_files_only=True))
    config["guidance_embeds"] = False
    with init_empty_weights():
        return Flux2Transformer2DModel.from_config(config)


# =====================================================================
#  VAE
# =====================================================================
def _make_vae_convert(dtype_name: str) -> Any:
    def convert(file_key: str, tensor: Any) -> Iterator[tuple[str, Any]]:
        yield file_key, _cast_floating(tensor, _torch_dtype(dtype_name))

    return convert


def _build_vae_meta(options: PipelineOptions, weight_format: str) -> Any:
    """`AutoencoderKLFlux2` from its folder's config, on `meta`."""
    from accelerate import init_empty_weights
    from diffusers import AutoencoderKLFlux2

    folder = components.component_dir_for_path(Path(options.vae_path))
    config = AutoencoderKLFlux2.load_config(str(folder), local_files_only=True)
    with init_empty_weights():
        return AutoencoderKLFlux2.from_config(config)


# =====================================================================
#  Text encoder (Qwen3, truncated; optional weight-only fp8)
# =====================================================================
_ENCODER_PREFIX = "model."
_ENCODER_LINEARS = ("self_attn.q_proj", "self_attn.k_proj", "self_attn.v_proj", "self_attn.o_proj", "mlp.gate_proj", "mlp.up_proj", "mlp.down_proj")
_LAYER_RE = re.compile(r"^layers\.(\d+)\.(.+)$")
#: Largest finite magnitude of float8_e4m3fn; each output row is scaled to it.
_FP8_MAX = 448.0


def _encoder_state_key(file_key: str) -> str | None:
    """The `Qwen3Model` state key of a checkpoint key (`model.` stripped), `None` for weights the truncated model lacks."""
    if file_key.startswith(components._ENCODER_LM_HEAD_PREFIX):
        return None
    key = file_key[len(_ENCODER_PREFIX):] if file_key.startswith(_ENCODER_PREFIX) else file_key
    match = _LAYER_RE.match(key)
    if match and int(match.group(1)) >= components.ENCODER_KEEP_LAYERS:
        return None
    return key


def _is_fp8_linear_weight(state_key: str) -> bool:
    match = _LAYER_RE.match(state_key)
    return bool(match) and any(match.group(2) == f"{name}.weight" for name in _ENCODER_LINEARS)


def _make_encoder_state_keys(fp8: bool) -> Any:
    def state_keys_of(file_key: str) -> tuple[str, ...]:
        key = _encoder_state_key(file_key)
        if key is None:
            return ()
        if fp8 and _is_fp8_linear_weight(key):
            stem = key[: -len(".weight")]
            return (f"{stem}.weight_fp8", f"{stem}.weight_scale")
        return (key,)

    return state_keys_of


def encoder_block_of(state_key: str) -> str | None:
    """`layers.<i>` for a decoder layer's weights; `None` for the embedding and the final norm."""
    match = _LAYER_RE.match(state_key)
    return f"layers.{match.group(1)}" if match else None


def _make_encoder_itemsize(fp8: bool) -> Any:
    state_keys_of = _make_encoder_state_keys(fp8)

    def itemsize(file_key: str, dtype_token: str) -> int | None:
        keys = state_keys_of(file_key)
        if fp8 and len(keys) == 2:
            # One byte per element; the per-row scale is `rows` elements more
            # (0.02 % of a 4096-wide layer) and stays inside the forecast margin.
            return 1
        return _two_byte_float(file_key, dtype_token)

    return itemsize


def _make_encoder_convert(fp8: bool) -> Any:
    state_keys_of = _make_encoder_state_keys(fp8)

    def convert(file_key: str, tensor: Any) -> Iterator[tuple[str, Any]]:
        import torch

        keys = state_keys_of(file_key)
        dtype = _torch_dtype(text_encoder_dtype_name())
        if fp8 and len(keys) == 2:
            weight = tensor.to(dtype)
            scale = weight.abs().amax(dim=1, keepdim=True).clamp(min=1e-6) / _FP8_MAX
            yield keys[0], (weight / scale).to(torch.float8_e4m3fn)
            yield keys[1], scale.to(dtype)
            return
        for key in keys:
            yield key, _cast_floating(tensor, dtype)

    return convert


@functools.lru_cache(maxsize=1)
def _fp8_linear_class() -> Any:
    """`nn.Linear` stand-in whose weight is float8 with per-output-row scales (buffers `weight_fp8`, `weight_scale`).

    Built lazily (this module imports without torch). The weight is
    dequantized to the compute dtype in `forward`; measured relative max error
    of the round trip ~3.4 % per weight tensor (gfx1201, torch 2.12+rocm7.2).
    """
    import torch

    class Fp8Linear(torch.nn.Module):
        def __init__(self, in_features: int, out_features: int, bias: bool, compute_dtype: Any) -> None:
            super().__init__()
            self.in_features = in_features
            self.out_features = out_features
            self.compute_dtype = compute_dtype
            self.register_buffer("weight_fp8", torch.empty((out_features, in_features), dtype=torch.float8_e4m3fn, device="meta"))
            self.register_buffer("weight_scale", torch.empty((out_features, 1), dtype=compute_dtype, device="meta"))
            self.bias = torch.nn.Parameter(torch.empty(out_features, dtype=compute_dtype, device="meta")) if bias else None

        def forward(self, x: Any) -> Any:
            weight = self.weight_fp8.to(self.compute_dtype) * self.weight_scale
            return torch.nn.functional.linear(x, weight, self.bias)

    return Fp8Linear


def _make_build_encoder_meta(fp8: bool) -> Any:
    def build(options: EncoderOptions, weight_format: str) -> Any:
        """`Qwen3Model` truncated to `ENCODER_KEEP_LAYERS`, on `meta` (non-persistent rotary buffers real); fp8 linears when asked.

        # Raises
        `ValueError` for an encoder shallower than the pipeline reads, or a
        config that does not say how deep it is (a whole-model load would
        not match the truncated key set), or a torch without float8.
        """
        import torch
        from accelerate import init_empty_weights
        from transformers import Qwen3Config, Qwen3Model

        folder = components.component_dir_for_path(Path(options.text_encoder_path))
        keep = components.ENCODER_KEEP_LAYERS
        kwargs = components.text_encoder_truncation_kwargs(folder, keep)
        config = Qwen3Config.from_pretrained(str(folder), local_files_only=True, **kwargs)
        if int(config.num_hidden_layers) != keep:
            raise ValueError(f"Текстовый энкодер {folder}: в config.json {config.num_hidden_layers} слоёв, а модель грузится урезанной до {keep}; укажите энкодер FLUX.2 klein.")
        if fp8 and getattr(torch, "float8_e4m3fn", None) is None:
            raise RuntimeError(f"fp8 для текстового энкодера не поддерживается этой сборкой torch ({torch.__version__}): нет типа float8_e4m3fn. Отключите параметр «fp8 для текстового энкодера».")
        dtype = _torch_dtype(text_encoder_dtype_name())
        config._attn_implementation = "sdpa"
        with init_empty_weights():
            model = Qwen3Model(config)
        if fp8:
            fp8_linear = _fp8_linear_class()
            for layer in model.layers:
                for name in _ENCODER_LINEARS:
                    owner_path, _, leaf = name.rpartition(".")
                    owner = layer.get_submodule(owner_path)
                    source = getattr(owner, leaf)
                    setattr(owner, leaf, fp8_linear(source.in_features, source.out_features, source.bias is not None, dtype))
        return model

    return build


# =====================================================================
#  Formats and keys
# =====================================================================
def _pipeline_formats(options: PipelineOptions) -> dict[str, FormatInfo]:
    present = Path(options.transformer_path).exists() and Path(options.vae_path).exists()
    size = sum(path.stat().st_size for path in transformer_files(options.transformer_path) if path.is_file()) if present else 0
    return {WEIGHT_FORMAT: FormatInfo(present, None if present else "files_missing", size, "native", 1)}


def _encoder_formats(options: EncoderOptions) -> dict[str, FormatInfo]:
    present = bool(options.text_encoder_path) and Path(options.text_encoder_path).exists()
    return {WEIGHT_FORMAT: FormatInfo(present, None if present else "files_missing", 0, "native", 1)}


def _pipeline_key(options: PipelineOptions, weight_format: str, device: str) -> str:
    return f"flux2_klein:{options.dtype}|{options.transformer_path}|{options.vae_path}|{device}"


def _encoder_key(options: EncoderOptions, weight_format: str, device: str) -> str:
    """`flux2_klein_text_encoder:<bf16|fp8>[+cpu]|<path>|<device>`: the CPU-only spec's handles never share a key with the other's."""
    return f"flux2_klein_text_encoder:{'fp8' if options.fp8 else 'bf16'}{'+cpu' if options.cpu_only else ''}|{options.text_encoder_path}|{device}"


# =====================================================================
#  Specs
# =====================================================================
def pipeline_spec(dtype: str) -> LargeModelSpec:
    """The pipeline spec computing in `dtype` ("bfloat16" / "float16")."""
    return LargeModelSpec(
        family=FAMILY,
        components=(
            ComponentSpec(
                TRANSFORMER,
                Role.DIT,
                lambda options, weight_format: transformer_files(options.transformer_path),
                transformer_block_of,
                state_keys_of=transformer_state_keys,
                device_itemsize=_two_byte_float,
                block_order=transformer_block_order,
                build_meta=_build_transformer_meta,
                convert=_make_transformer_convert(dtype),
            ),
            ComponentSpec(
                VAE,
                Role.AE,
                lambda options, weight_format: vae_files(options.vae_path),
                lambda state_key: None,
                device_itemsize=_two_byte_float,
                build_meta=_build_vae_meta,
                convert=_make_vae_convert(dtype),
            ),
        ),
        phases=(
            # The pipeline call encodes the region and the reference with the
            # VAE before the denoise loop, so the VAE belongs to the denoise.
            PhaseSpec(PHASE_DENOISE, frozenset({TRANSFORMER, VAE}), TRANSFORMER, _denoise_work),
            PhaseSpec(PHASE_DECODE, frozenset({VAE}), VAE, _decode_work),
        ),
        formats=_pipeline_formats,
        key_of=_pipeline_key,
        fallbacks=_DECODE_FALLBACKS,
    )


def encoder_spec(fp8: bool, cpu_only: bool = False) -> LargeModelSpec:
    """The text-encoder spec (`fp8`: weight-only fp8 linears, quantized tensor by tensor while loading).

    `cpu_only`: the encoder may compute on the CPU only (the legacy wire's
    meaning, module notes); otherwise on the run device or the CPU, as the
    plan decides.
    """
    return LargeModelSpec(
        family=FAMILY,
        components=(
            ComponentSpec(
                TEXT_ENCODER,
                Role.ENCODER,
                lambda options, weight_format: encoder_files(options.text_encoder_path),
                encoder_block_of,
                state_keys_of=_make_encoder_state_keys(fp8),
                device_itemsize=_make_encoder_itemsize(fp8),
                exec_devices_allowed=frozenset({ExecDevice.CPU}) if cpu_only else frozenset({ExecDevice.RUN_DEVICE, ExecDevice.CPU}),
                build_meta=_make_build_encoder_meta(fp8),
                convert=_make_encoder_convert(fp8),
            ),
        ),
        phases=(PhaseSpec(PHASE_ENCODE, frozenset({TEXT_ENCODER}), TEXT_ENCODER, _encode_work),),
        formats=_encoder_formats,
        key_of=_encoder_key,
    )


def register_specs(manager: LargeModelManager) -> None:
    """Register every FLUX.2 spec on `manager` that is not registered yet (idempotent per name)."""
    known = set(manager.models())
    for dtype, name in _PIPELINE_MODELS.items():
        if name not in known:
            manager.register(pipeline_spec(dtype), model=name)
    for (fp8, cpu_only), name in _ENCODER_MODELS.items():
        if name not in known:
            manager.register(encoder_spec(fp8, cpu_only), model=name)

