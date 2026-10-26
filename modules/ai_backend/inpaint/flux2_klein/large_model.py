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
    precision (bf16, or weight-only fp8 done by the conversion:
    `encoder_model_name`), computing on the run device or the CPU as the plan
    decides; its file is a safetensors folder or a llama.cpp GGUF (names
    bridged by `encoder_gguf`, kept quantized like a GGUF transformer);
- `PipelineOptions` / `EncoderOptions` (the manager's `options`), built from a
  normalized request by `pipeline_options` / `encoder_options`, and
  `request_shape`;
- the decode fallbacks of the degrade ladder (`FALLBACK_VAE_TILING`,
  `FALLBACK_VAE_TILE_LOWERED`): tiled VAE decode where tiling engages for the
  region, then the VAE's tile lowered to `pipeline.EMERGENCY_TILE_LATENT_MIN_SIZE`
  latent cells;
- the WEIGHT FORMATS of both handles (protocol 21): one `FormatInfo` per
  candidate file of the request (`_pipeline_formats`, `_make_encoder_formats`),
  the one resolver of "which format is this run" (`weight_format_of`) and of
  "which file is that format" (`transformer_path_for`, `encoder_path_for`); a
  GGUF transformer or encoder is built with `runtime.gguf_layers` modules where
  its file is block-quantized (`_build_transformer_meta`,
  `_make_build_encoder_meta`) and dequantized per forward; a GGUF encoder's
  encode work is variant `ENCODE_VARIANT_GGUF` (never a borrowed bf16 cost);
  weight-only fp8 and a GGUF encoder exclude each other (`encoder_options`);
- `require_loadable` - the refusals a run must give BEFORE the manager reads a
  header (fp8_scaled checkpoint, missing transformer config, a VAE file without
  its folder, a too-shallow encoder, a GGUF quantization the layers cannot
  dequantize, a GGUF encoder that is not Qwen3 / lacks a consistent config /
  comes with fp8), with the wording the service always used;
- `register_specs` - registration of every FLUX.2 spec on a manager.

Notes:
- Why the encoder is its own handle: a prompt-cache build and the encoder's
  «load» action must touch the encoder only, and a run whose prompt is cached
  must not need an encoder installed at all. The pipeline is admitted first,
  the encoder second (it then plans on what the pipeline's reservation left:
  on the GPU when both fit, else on the CPU from RAM, `tiers.template_for`).
- Why one spec per dtype / per encoder precision: `ComponentSpec.convert` sees
  only `(file_key, tensor)`, so the stored dtype is a property of the spec.
- Weight formats are NOT separate specs: the manager hands every
  `files` / `build_meta` / `key_of` call the format it plans, and the format
  id selects the candidate file (`transformer_path_for`). The ids are opaque
  (`params.WEIGHT_FORMAT_ID_PATTERN`); what a file holds is read from its
  container (`runtime.tensor_container`), never inferred from the id. A
  block-quantized GGUF tensor stays `uint8` rows end to end: the BFL
  conversion only renames, splits rows (`torch.chunk(dim=0)` of a fused qkv)
  or swaps row halves (`swap_scale_shift`), all valid on whole rows, and the
  device size of a `U8` span is its file size (`_two_byte_float`).
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
from ...runtime.tensor_container import container_kind, read_container_layout
from . import attention, components, conversion, encoder_gguf
from .params import REGION_SIZE_MULTIPLE, SAFETENSORS_FORMAT, effective_steps, text_encoder_dtype_name
from .pipeline import EMERGENCY_TILE_LATENT_MIN_SIZE

log = logging.getLogger(__name__)

#: The calibration-table family of every FLUX.2 spec.
FAMILY = "flux2_klein"

#: The text encoder's weight format when a request names none and its path is
#: a safetensors folder (fp8 is a spec of its own, not a format). The selected
#: format of a request is `EncoderOptions.text_encoder_format`
#: (`weight_format_of`); a GGUF file without a named format is
#: `params.GGUF_FORMAT`.
ENCODER_WEIGHT_FORMAT = SAFETENSORS_FORMAT

#: `PhaseWork.variant` of the encode phase: a safetensors encoder, or a GGUF
#: encoder dequantized per forward (D7: its cost is never borrowed from the
#: measured bf16 `encode text` entries, so its forecast is an estimate).
ENCODE_VARIANT = "text"
ENCODE_VARIANT_GGUF = "text_gguf"

#: Calibration compute paths of a transformer format (`FormatInfo.compute_path`):
#: plain matmuls over a safetensors checkpoint, or a GGUF file dequantized per
#: forward. Deliberately not FLUX.3 nvfp4's `"dequant"`, whose measured cost
#: entries must never be borrowed.
COMPUTE_NATIVE = "native"
COMPUTE_GGUF = "gguf"

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
#: `fp8 -> model name`.
_ENCODER_MODELS = {
    False: "flux2_klein_text_encoder",
    True: "flux2_klein_text_encoder.fp8",
}


def pipeline_model_name(dtype: str) -> str:
    """The manager model name of the pipeline spec computing in `dtype` (`params.VALID_DTYPES`)."""
    return _PIPELINE_MODELS[dtype]


def encoder_model_name(fp8: bool) -> str:
    """The manager model name of the text-encoder spec (`fp8` = weight-only fp8 linears)."""
    return _ENCODER_MODELS[bool(fp8)]


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
    `transformer_format` is the selected weight format (its file is
    `transformer_path`) and `transformer_alternatives` the request's candidate
    `(id, path)` pairs, best first (`()`: the selected format alone).
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
    transformer_format: str = SAFETENSORS_FORMAT
    transformer_alternatives: tuple[tuple[str, str], ...] = ()


@dataclass(frozen=True)
class EncoderOptions:
    """The manager's `options` of one text-encoder use.

    `on_cpu` is «Энкодер на CPU»: the encoder spec pins its one component to
    host execution (`LargeModelSpec.host_pin_of`), so the RAM step alone places
    it and the residency key carries the pin. `text_encoder_format` is the
    selected weight format (its file is `text_encoder_path`) and
    `text_encoder_alternatives` the request's candidate `(id, path)` pairs,
    best first (`()`: the selected format alone).
    """

    text_encoder_path: str
    fp8: bool
    max_sequence_length: int
    on_cpu: bool = False
    text_encoder_format: str = ENCODER_WEIGHT_FORMAT
    text_encoder_alternatives: tuple[tuple[str, str], ...] = ()


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
        transformer_format=normalized["transformer_format"],
        transformer_alternatives=tuple(normalized["transformer_formats"]),
    )


def weight_format_of(options: PipelineOptions | EncoderOptions) -> str:
    """THE weight format a handle of `options` is planned, keyed and loaded in.

    The pipeline: the request's `transformer_format`; the text encoder: the
    request's `text_encoder_format`. Every `acquire` / `profiles` / forecast
    of this package asks here, never spells a format itself.

    # Raises
    `TypeError` for any other options type.
    """
    if isinstance(options, PipelineOptions):
        return options.transformer_format
    if isinstance(options, EncoderOptions):
        return options.text_encoder_format
    raise TypeError(f"FLUX.2 klein: нет формата весов для опций {type(options).__name__}")


def transformer_path_for(options: PipelineOptions, weight_format: str) -> str:
    """The transformer file (or diffusers folder) of `weight_format` in `options`.

    The candidate list first; the selected format is always its own
    `transformer_path` (the request's contract, `params._weight_formats`).

    # Raises
    `ValueError` for a format the request did not offer.
    """
    for format_id, path in options.transformer_alternatives:
        if format_id == weight_format:
            return path
    if weight_format == options.transformer_format:
        return options.transformer_path
    offered = [format_id for format_id, _ in options.transformer_alternatives] or [options.transformer_format]
    raise ValueError(f"FLUX.2 klein: формата весов трансформера {weight_format!r} нет в запросе (есть: {', '.join(offered)})")


def encoder_path_for(options: EncoderOptions, weight_format: str) -> str:
    """The text-encoder folder or GGUF file of `weight_format` in `options` (as `transformer_path_for`).

    # Raises
    `ValueError` for a format the request did not offer.
    """
    for format_id, path in options.text_encoder_alternatives:
        if format_id == weight_format:
            return path
    if weight_format == options.text_encoder_format:
        return options.text_encoder_path
    offered = [format_id for format_id, _ in options.text_encoder_alternatives] or [options.text_encoder_format]
    raise ValueError(f"FLUX.2 klein: формата весов текстового энкодера {weight_format!r} нет в запросе (есть: {', '.join(offered)})")


def encoder_options(normalized: dict[str, Any]) -> EncoderOptions:
    """`EncoderOptions` of a normalized request.

    Every use of the encoder (run, profiles, forecast, actions) builds its
    options here, so this is where the D8 rule holds: weight-only fp8 and a
    GGUF encoder exclude each other (the GGUF weights are already quantized).

    # Raises
    `ValueError` naming `text_encoder_fp8` and `text_encoder_path` when
    both ask for it — never a silent fallback to one of them.
    """
    options = EncoderOptions(
        text_encoder_path=normalized["text_encoder_path"],
        fp8=bool(normalized["text_encoder_fp8"]),
        max_sequence_length=int(normalized["max_sequence_length"]),
        on_cpu=bool(normalized["encoder_on_cpu"]),
        text_encoder_format=normalized["text_encoder_format"],
        text_encoder_alternatives=tuple(normalized["text_encoder_formats"]),
    )
    if options.fp8 and components.is_gguf_file(Path(options.text_encoder_path)):
        raise ValueError(_fp8_gguf_message(options.text_encoder_path))
    return options


def _fp8_gguf_message(text_encoder_path: str) -> str:
    """The D8 refusal: `text_encoder_fp8` together with a GGUF encoder file."""
    return (
        f"Параметр «fp8 для текстового энкодера» (text_encoder_fp8) нельзя сочетать с GGUF-энкодером "
        f"(text_encoder_path: {Path(text_encoder_path).name}): веса GGUF уже квантованы. Отключите fp8 "
        "или выберите текстовый энкодер bf16."
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
    """One prompt at `max_sequence_length` tokens; variant `ENCODE_VARIANT_GGUF` for a GGUF encoder (D7).

    The work sees the options, not the planned format, so the variant follows
    the SELECTED encoder file: an alternative format's row is planned with it.
    """
    gguf = components.is_gguf_file(Path(options.text_encoder_path))
    return PhaseWork(tokens=options.max_sequence_length, variant=ENCODE_VARIANT_GGUF if gguf else ENCODE_VARIANT)


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
    """The transformer's checkpoint files: the single file (safetensors or GGUF), or every shard of a diffusers folder."""
    source = Path(path)
    return _shards(source) if source.is_dir() else [source]


def vae_files(path: str) -> list[Path]:
    """The VAE's safetensors files (a weights file is normalized to its folder, `components.component_dir_for_path`)."""
    return _shards(components.component_dir_for_path(Path(path)))


def encoder_files(path: str) -> list[Path]:
    """The text encoder's checkpoint files: a GGUF file itself, else the folder's safetensors (index weight map first, then the glob)."""
    source = Path(path)
    if components.is_gguf_file(source):
        return [source]
    return _shards(components.component_dir_for_path(source))


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
    prefixed file refused, a single file needs its config; a GGUF file:
    the same prefix and config rules plus `_require_dequantizable_gguf`), the
    VAE (a folder with `config.json`; diffusers 0.39 has no
    single-file VAE loader) and, when `encoder`, that the encoder folder is a
    transformers folder deep enough for `ENCODER_KEEP_LAYERS`.

    # Raises
    `ValueError` / `FileNotFoundError` naming the file and the way out.
    """
    source = Path(normalized["transformer_path"])
    if source.is_dir():
        components._reject_fp8_scaled_directory(source)
    elif container_kind(source) == "gguf":
        layout = read_container_layout(source)
        conversion.require_known_layout([span.name for span in layout.spans], source=source)
        _require_dequantizable_gguf(layout.spans, source=source)
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


def _require_dequantizable_gguf(spans: Sequence[Any], *, source: Path, component: str = "Трансформер") -> None:
    """Refuse a GGUF file holding a block-quantized type `runtime.gguf_layers` cannot dequantize.

    `spans` are the file's `TensorSpan`s, `component` the capitalized
    component name the message starts with. Every unsupported type is named
    once, with its first tensor, so the user can pick another file. Imports
    torch (through `gguf_layers`), so it runs only on a GGUF file.

    # Raises
    `ValueError` naming the file, the types, a tensor of each and the
    supported types.
    """
    from ...runtime.gguf_layers import dequantizable_types, quant_type_name

    supported = dequantizable_types()
    refused: dict[int, str] = {}
    for span in spans:
        if span.quant_type is not None and span.quant_type not in supported:
            refused.setdefault(span.quant_type, span.name)
    if not refused:
        return
    named = ", ".join(f"{quant_type_name(quant_type)} (тензор {tensor})" for quant_type, tensor in refused.items())
    allowed = ", ".join(sorted(quant_type_name(quant_type) for quant_type in supported))
    log.error("FLUX.2 klein: GGUF %s (%s) содержит неподдерживаемые типы квантования: %s; поддерживаются: %s", source, component, named, allowed)
    raise ValueError(
        f"{component} {source.name}: тип квантования GGUF {named} не поддерживается — такие веса "
        f"нечем распаковать. Поддерживаются: {allowed}. Возьмите файл с одним из этих типов."
    )


def require_loadable_encoder(normalized: dict[str, Any]) -> None:
    """Refuse an encoder path that is not a transformers folder or a usable GGUF file, or one shallower than `ENCODER_KEEP_LAYERS`.

    A GGUF file (`_require_loadable_gguf_encoder`) must not come with
    `text_encoder_fp8` (D8), must be a `qwen3` GGUF deep enough, hold only
    dequantizable types and Qwen3 tensor names, and have a consistent
    `config.json` (beside it or in the installed 4B package).

    # Raises
    `ValueError` / `FileNotFoundError` naming the folder or file (and, for a
    shallow encoder, the counts).
    """
    source = Path(normalized["text_encoder_path"])
    if components.is_gguf_file(source):
        _require_loadable_gguf_encoder(source, fp8=bool(normalized["text_encoder_fp8"]))
        return
    encoder_dir = components.component_dir_for_path(source)
    if not encoder_dir.is_dir():
        raise ValueError(f"Путь текстового энкодера должен быть каталогом в формате transformers: {encoder_dir}")
    components.text_encoder_truncation_kwargs(encoder_dir, components.ENCODER_KEEP_LAYERS)


def _require_loadable_gguf_encoder(source: Path, *, fp8: bool) -> None:
    """The GGUF branch of `require_loadable_encoder` (reads the header only).

    # Raises
    `ValueError` for fp8 together with GGUF (D8, naming both fields), a file
    that is not a `qwen3` GGUF, one with fewer blocks than
    `ENCODER_KEEP_LAYERS`, an undequantizable type, a foreign or missing
    tensor; `FileNotFoundError` when no consistent `config.json` is found.
    """
    if fp8:
        raise ValueError(_fp8_gguf_message(str(source)))
    import gguf

    layout = components.gguf_layout(source)
    metadata = layout.gguf.metadata if layout.gguf is not None else {}
    architecture = metadata.get(gguf.Keys.General.ARCHITECTURE)
    if architecture != components.GGUF_ENCODER_ARCHITECTURE:
        raise ValueError(
            f"Текстовый энкодер {source.name}: это GGUF модели «{architecture}», а нужен Qwen3 "
            f"(general.architecture = «{components.GGUF_ENCODER_ARCHITECTURE}») — возьмите GGUF текстового энкодера FLUX.2 klein."
        )
    keep = components.ENCODER_KEEP_LAYERS
    blocks = components._config_int(metadata, gguf.Keys.LLM.BLOCK_COUNT.format(arch=components.GGUF_ENCODER_ARCHITECTURE))
    if blocks is not None and blocks < keep:
        raise ValueError(
            f"Текстовый энкодер {source.name}: в файле {blocks} слоёв, а пайплайн читает скрытые состояния "
            f"слоёв {', '.join(str(index) for index in components.TEXT_ENCODER_OUT_LAYER_INDICES)} и требует минимум {keep}."
        )
    _require_dequantizable_gguf(layout.spans, source=source, component="Текстовый энкодер")
    encoder_gguf.require_known_encoder_names([span.name for span in layout.spans], keep, source=source)
    components.text_encoder_truncation_kwargs(components.find_text_encoder_config_dir(source), keep)


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


def gguf_quant_types(path: str) -> dict[str, int]:
    """`{diffusers state key: ggml type}` of every block-quantized tensor of a GGUF transformer (header only).

    Each file tensor is mapped through `transformer_state_keys`, so a BFL
    fused qkv gives its three row slices the type of the fused tensor; plain
    (F32 / F16 / BF16) tensors are absent and stay ordinary weights.

    # Raises
    `ValueError` for an unreadable or malformed file.
    """
    return {
        state_key: int(span.quant_type)
        for span in read_container_layout(path).spans
        if span.quant_type is not None
        for state_key in transformer_state_keys(span.name)
    }


def _build_transformer_meta(options: PipelineOptions, weight_format: str) -> Any:
    """`Flux2Transformer2DModel` from the checkpoint's own config, on `meta` (`guidance_embeds=False`: klein has none).

    For a GGUF file of `weight_format`, every module whose weight the file
    stores block-quantized is swapped for a `runtime.gguf_layers` module
    (`uint8` rows, dequantized per forward into `options.dtype`) BEFORE the
    loader fills it, so the stored layout derived on `meta` is the file's.

    # Raises
    `ValueError` / `FileNotFoundError` from the config probe, and `ValueError`
    when a quantized tensor names no linear / embedding of the model
    (`gguf_layers.swap_gguf_modules`).
    """
    from accelerate import init_empty_weights
    from diffusers import Flux2Transformer2DModel

    path = transformer_path_for(options, weight_format)
    config = dict(Flux2Transformer2DModel.load_config(str(transformer_config_dir(path)), local_files_only=True))
    config["guidance_embeds"] = False
    with init_empty_weights():
        model = Flux2Transformer2DModel.from_config(config)
    if not Path(path).is_dir() and container_kind(path) == "gguf":
        from ...runtime.gguf_layers import swap_gguf_modules

        swapped = swap_gguf_modules(model, gguf_quant_types(path), compute_dtype=_torch_dtype(options.dtype))
        log.info("FLUX.2 klein: трансформер %s (формат %s, конфиг %s) — %d слоёв GGUF, распаковка в %s при каждом проходе.", Path(path).name, weight_format, transformer_config_dir(path), swapped, options.dtype)
    return model


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
    """The `Qwen3Model` state key of a checkpoint key, `None` for weights the truncated model lacks.

    Two disjoint name spaces: an HF key (`model.` stripped; `lm_head.*`
    dropped) and a llama.cpp GGUF name (`encoder_gguf.encoder_state_key_of_gguf`).
    The prefixed HF names are recognized first, so a safetensors encoder never
    builds the GGUF name tables.

    # Raises
    `ValueError` for a GGUF name that is not a Qwen3 tensor.
    """
    if file_key.startswith(components._ENCODER_LM_HEAD_PREFIX):
        return None
    if not file_key.startswith(_ENCODER_PREFIX) and encoder_gguf.is_gguf_encoder_name(file_key):
        return encoder_gguf.encoder_state_key_of_gguf(file_key, components.ENCODER_KEEP_LAYERS)
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
            # D8: `encoder_options` refuses fp8 with a GGUF file before any spec
            # call; a GGUF linear reaching here would be quantized twice.
            if not file_key.startswith(_ENCODER_PREFIX) and encoder_gguf.is_gguf_encoder_name(file_key):
                raise ValueError(f"FLUX.2 klein: fp8 для текстового энкодера не применим к тензору GGUF «{file_key}»")
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

        For a GGUF file of `weight_format` the config is the one found for it
        (`components.text_encoder_config_dir`) and every module the file stores
        block-quantized (the linears and `embed_tokens`) is swapped for a
        `runtime.gguf_layers` module (`uint8` rows, dequantized per forward
        into `text_encoder_dtype_name()`) before the loader fills it.

        # Raises
        `ValueError` for an encoder shallower than the pipeline reads, or a
        config that does not say how deep it is (a whole-model load would
        not match the truncated key set), a torch without float8, fp8 asked
        for a GGUF file (D8), or a quantized tensor naming no linear /
        embedding; `FileNotFoundError` for a GGUF file without a config.
        """
        import torch
        from accelerate import init_empty_weights
        from transformers import Qwen3Config, Qwen3Model

        path = encoder_path_for(options, weight_format)
        gguf = components.is_gguf_file(Path(path))
        if fp8 and gguf:
            raise ValueError(_fp8_gguf_message(path))
        folder = components.text_encoder_config_dir(path)
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
        if gguf:
            from ...runtime.gguf_layers import swap_gguf_modules

            quant_types = encoder_gguf.gguf_encoder_quant_types(components.gguf_layout(Path(path)).spans, keep)
            swapped = swap_gguf_modules(model, quant_types, compute_dtype=dtype)
            log.info("FLUX.2 klein: текстовый энкодер %s (формат %s, конфиг %s) — %d слоёв GGUF, распаковка в %s при каждом проходе.", Path(path).name, weight_format, folder, swapped, text_encoder_dtype_name())
        return model

    return build


# =====================================================================
#  Formats and keys
# =====================================================================
def _transformer_format_info(path: str, vae_present: bool, quality_rank: int) -> FormatInfo:
    """One transformer format row: available when its file and the VAE exist; `dit_bytes` its file size; compute path by container.

    A missing file's container is unknown: it reads as `COMPUTE_NATIVE` and is
    never planned (the manager inventories available formats only).
    """
    source = Path(path)
    present = vae_present and source.exists()
    if not present:
        return FormatInfo(False, "files_missing", 0, COMPUTE_NATIVE, quality_rank)
    size = sum(file.stat().st_size for file in transformer_files(path) if file.is_file())
    gguf = not source.is_dir() and container_kind(source) == "gguf"
    return FormatInfo(True, None, size, COMPUTE_GGUF if gguf else COMPUTE_NATIVE, quality_rank)


def _pipeline_formats(options: PipelineOptions) -> dict[str, FormatInfo]:
    """One row per candidate format of the request (`quality_rank` = count - index: best first), or the selected one alone."""
    vae_present = Path(options.vae_path).exists()
    candidates = options.transformer_alternatives or ((options.transformer_format, options.transformer_path),)
    return {format_id: _transformer_format_info(path, vae_present, len(candidates) - index) for index, (format_id, path) in enumerate(candidates)}


def _encoder_format_info(path: str, quality_rank: int) -> FormatInfo:
    """One text-encoder format row: available when its folder / file exists; `dit_bytes` its weight files' size; compute path by container."""
    source = Path(path)
    if not path or not source.exists():
        return FormatInfo(False, "files_missing", 0, COMPUTE_NATIVE, quality_rank)
    size = sum(file.stat().st_size for file in encoder_files(path) if file.is_file())
    gguf = components.is_gguf_file(source)
    return FormatInfo(True, None, size, COMPUTE_GGUF if gguf else COMPUTE_NATIVE, quality_rank)


def _make_encoder_formats(fp8: bool) -> Any:
    """The `formats` callable of `encoder_spec(fp8)`: one row per candidate of the request, best first.

    The fp8 spec offers only the candidates that are NOT GGUF files: fp8 is a
    conversion of bf16 weights (D8). A missing candidate's container is
    unknown and stays listed (unavailable).
    """

    def formats(options: EncoderOptions) -> dict[str, FormatInfo]:
        candidates = options.text_encoder_alternatives or ((options.text_encoder_format, options.text_encoder_path),)
        if fp8:
            candidates = tuple((format_id, path) for format_id, path in candidates if not components.is_gguf_file(Path(path)))
        return {format_id: _encoder_format_info(path, len(candidates) - index) for index, (format_id, path) in enumerate(candidates)}

    return formats


def _pipeline_key(options: PipelineOptions, weight_format: str, device: str) -> str:
    """`flux2_klein:<dtype>|<the format's transformer file>|<vae>|<device>` (the selected format's file is `transformer_path`)."""
    return f"flux2_klein:{options.dtype}|{transformer_path_for(options, weight_format)}|{options.vae_path}|{device}"


def _encoder_host_pin(options: EncoderOptions) -> tuple[str, ...]:
    """«Энкодер на CPU»: the encoder component pinned to host execution when `options.on_cpu`."""
    return (TEXT_ENCODER,) if options.on_cpu else ()


def _encoder_key(options: EncoderOptions, weight_format: str, device: str) -> str:
    """`flux2_klein_text_encoder:<bf16|fp8>|<the format's folder or file>|<device>` (the selected format's is `text_encoder_path`, so the bf16 key is unchanged)."""
    return f"flux2_klein_text_encoder:{'fp8' if options.fp8 else 'bf16'}|{encoder_path_for(options, weight_format)}|{device}"


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
                lambda options, weight_format: transformer_files(transformer_path_for(options, weight_format)),
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


def encoder_spec(fp8: bool) -> LargeModelSpec:
    """The text-encoder spec (`fp8`: weight-only fp8 linears, quantized tensor by tensor while loading); it computes on the run device or the CPU, as the plan decides.

    Its formats are the request's candidates (a safetensors folder or a GGUF
    file each; the fp8 spec only the non-GGUF ones). A GGUF encoder stays
    quantized in memory: `uint8` rows stored as read (`convert` casts floats
    only), dequantized per forward.
    """
    return LargeModelSpec(
        family=FAMILY,
        components=(
            ComponentSpec(
                TEXT_ENCODER,
                Role.ENCODER,
                lambda options, weight_format: encoder_files(encoder_path_for(options, weight_format)),
                encoder_block_of,
                state_keys_of=_make_encoder_state_keys(fp8),
                device_itemsize=_make_encoder_itemsize(fp8),
                exec_devices_allowed=frozenset({ExecDevice.RUN_DEVICE, ExecDevice.CPU}),
                build_meta=_make_build_encoder_meta(fp8),
                convert=_make_encoder_convert(fp8),
            ),
        ),
        phases=(PhaseSpec(PHASE_ENCODE, frozenset({TEXT_ENCODER}), TEXT_ENCODER, _encode_work),),
        formats=_make_encoder_formats(fp8),
        key_of=_encoder_key,
        host_pin_of=_encoder_host_pin,
    )


def register_specs(manager: LargeModelManager) -> None:
    """Register every FLUX.2 spec on `manager` that is not registered yet (idempotent per name)."""
    known = set(manager.models())
    for dtype, name in _PIPELINE_MODELS.items():
        if name not in known:
            manager.register(pipeline_spec(dtype), model=name)
    for fp8, name in _ENCODER_MODELS.items():
        if name not in known:
            manager.register(encoder_spec(fp8), model=name)

