"""
File: modules/ai_backend/inpaint/flux3_klein/large_model.py

Purpose:
FLUX.3 klein as the large-model manager sees it: the `LargeModelSpec`s
(components, residency blocks, phase table, weight formats, residency key) that
`runtime.large_models.manager.LargeModelManager` plans, loads and drives a run
with, plus the request-shape arithmetic the forecast needs.

Main responsibilities:
- `Flux3ModelOptions` - what every spec callable receives (the user's
  `Flux3Options`, the run device, the concrete fp8 path);
- `model_name` / `model_specs` / `build_spec` - one registered model per
  (encoder mode, AE dtype), all of family `FAMILY`;
- `residency_key` - one manager handle per loaded byte set;
- `formats` - bf16 / fp8r / nvfp4 with availability, DiT size, compute path,
  quality rank (the user's choice; the manager never changes it);
- `request_shape`, `caption_token_bound`, `reference_pixel_bound` - the
  `RequestShape` of a request and the per-phase `PhaseWork` derived from it.

Components (names `COMPONENT_*`):
- `dit` (role DIT, run device only): blocks in EXECUTION order — per depth `d`
  `content_mode_blocks.image.d`, `content_mode_blocks.image_cond.d`,
  `txt_mode_blocks.d`, then `single_blocks.0..n` (the residency hooks prefetch
  the next block of that order); embedders, modulations and the final layer
  are non-block weights. Floats become bf16, quantized payloads stay as stored
  (`loading.convert_dit_tensor`).
- `encoder` (role ENCODER, run device or CPU): the truncated Qwen3-VL (33 of
  36 layers), blocks = its decoder layers (`layers.N` in TEXT,
  `language_model.layers.N` in VL_REFS; the vision tower is a non-block
  weight), bf16.
- `ae` (role AE, run device only): monolithic, in the model's AE dtype
  (BatchNorm statistics stay float32).

Phases, in run order: `encode` (encoder), `ref_encode` (ae: the reference AE
encode), `denoise` (dit), `decode` (ae).

Notes:
- Why four models and not one: the manager caches a component inventory per
  (model, format, files), and `ComponentSpec.convert` / `device_itemsize` see
  no options. The encoder mode changes the encoder's state keys (a different
  module) and the AE dtype its device bytes, so each (mode, AE dtype) pair is
  its own registered model; the residency key keeps their handles apart too.
- No model-specific degrade rung (`fallbacks=()`): `reference_max_pixels` and
  the AE dtype are the user's quality choices, the port has no tiled AE, and
  "encoder on the CPU" is already a step placement (`tiers.template_for`).
- Forecast inputs are UPPER bounds where the exact figure needs the tokenizer
  (`caption_token_bound`) or the reference preparation (`reference_pixel_bound`):
  a forecast may be high, never low because of them.
- Torch-free at import: the loader callables import `loading` /
  `autoencoder` / `text_encoder` on call and look them up as
  `<module>.<name>` (the tests patch them there).
"""

from __future__ import annotations

import logging
import math
import re
from collections.abc import Callable, Iterator, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, assert_never

from ...runtime.encoder_truncation import keep_layers_for_hidden_states
from ...runtime.large_models.spec import ComponentSpec, ExecDevice, FormatInfo, LargeModelSpec, PhaseSpec, PhaseWork, RequestShape, Role
from .config import (
    TEXT_MAX_TOKENS,
    TEXT_OUTPUT_LAYERS,
    TEXT_PAD_MULTIPLE,
    SamplerConfig,
    WeightFormat,
    package_ae_file,
    package_text_encoder_dir,
    read_package_config,
    resolve_model_files,
    text_encoder_shards,
    text_mode_key_map,
    variant_availability,
    vl_mode_key_map,
)
from .params import MAX_SIDE, MIN_SIDE, SIDE_MULTIPLE, AeDtype, EncoderMode, Flux3Options, Fp8Compute

log = logging.getLogger(__name__)

#: Calibration family of every FLUX.3 klein model (`calibration/*.json` keys).
FAMILY = "flux3_klein"

COMPONENT_DIT = "dit"
COMPONENT_ENCODER = "encoder"
COMPONENT_AE = "ae"

PHASE_ENCODE = "encode"
PHASE_REF_ENCODE = "ref_encode"
PHASE_DENOISE = "denoise"
PHASE_DECODE = "decode"

#: Compute paths (`FormatInfo.compute_path`, calibration keys).
PATH_NATIVE = "native"
PATH_SCALED_MM = "scaled_mm"
PATH_DEQUANT = "dequant"

#: Output quality order of the formats, higher = better.
QUALITY_RANK: Mapping[WeightFormat, int] = {WeightFormat.BF16: 3, WeightFormat.FP8R: 2, WeightFormat.NVFP4: 1}

#: Decoder layers the encoder keeps (the rule: `runtime.encoder_truncation`).
ENCODER_KEEP_LAYERS = keep_layers_for_hidden_states(TEXT_OUTPUT_LAYERS)

#: Chat-template tokens around a caption, upper bound (`<|im_start|>user\n` ...
#: `<|im_end|>\n<|im_start|>assistant\n` is about ten).
_TEMPLATE_TOKEN_BOUND = 32

#: Prompt length the forecast assumes when the caller does not know the caption.
DEFAULT_PROMPT_TOKENS = 512

#: VL mode: pixels per LLM token of a reference and the wrapper tokens per image
#: (`text_encoder.VL_PIXELS_PER_TOKEN`, `VL_WRAPPER_TOKENS_PER_REFERENCE`).
_VL_PIXELS_PER_TOKEN = 32 * 32
_VL_WRAPPER_TOKENS = 2

#: Single-reference canvas cap and per-reference cap of a multi-reference edit
#: (`packing.MATCH_MAX_PIXELS`, `packing.MULTI_REFERENCE_MAX_PIXELS`).
_SINGLE_REFERENCE_MAX_PIXELS = 4 * 1024 * 1024
_MULTI_REFERENCE_MAX_PIXELS = 1024 * 1024

#: Header dtype tokens of 8-bit payloads, and the scale-buffer leaves of the
#: quantized layers (`quant.QUANT_SUFFIXES`): both stay as stored on the device.
_EIGHT_BIT_TOKENS = frozenset({"F8_E4M3", "F8_E5M2", "U8"})
_QUANT_LEAVES = frozenset({"weight_scale", "weight_scale_2", "input_scale"})
_FLOAT_TOKENS = frozenset({"F16", "BF16", "F32", "F64"})

#: State keys of the AE's BatchNorm statistics: never cast (`autoencoder._BN_PREFIX`).
_AE_BN_PREFIX = "bn."

_DIT_CONTENT_MODES = ("image", "image_cond")
_DIT_BLOCK = re.compile(r"^(content_mode_blocks\.(image_cond|image)\.(\d+)|txt_mode_blocks\.(\d+)|single_blocks\.(\d+))\.")
_TEXT_LAYER = re.compile(r"^(layers\.(\d+))\.")
_VL_LAYER = re.compile(r"^(language_model\.layers\.(\d+))\.")


@dataclass(frozen=True)
class Flux3ModelOptions:
    """What every FLUX.3 spec callable receives.

    `options` are the user's options; `device` the normalized run device;
    `fp8_compute` the CONCRETE fp8 path of the fp8r format on that device
    (`SCALED_MM` or `DEQUANT`, resolved by the caller before admission: it
    decides the fp8r compute path, its calibration entry and its fp8 layers).

    # Raises
    `ValueError` (construction) for `fp8_compute` `AUTO`.
    """

    options: Flux3Options
    device: str
    fp8_compute: Fp8Compute

    def __post_init__(self) -> None:
        if self.fp8_compute is Fp8Compute.AUTO:
            raise ValueError("Flux3ModelOptions: fp8_compute должен быть выбранным путём (scaled_mm или dequant), а не auto")


def model_name(encoder_mode: EncoderMode, ae_dtype: AeDtype) -> str:
    """The registered model name of (`encoder_mode`, `ae_dtype`): `flux3_klein.<mode>.ae_<dtype>`."""
    return f"{FAMILY}.{encoder_mode.value}.ae_{ae_dtype.value}"


def residency_key(options: Flux3Options, weight_format: str, device: str) -> str:
    """The manager handle key of `options` loaded in `weight_format` on `device`.

    Changes whenever the loaded bytes or modules would: format, device, the
    requested fp8 path (fp8 layers carry it), AE dtype, encoder mode, package
    root. `reference_max_pixels`, the resource steps and the request do not change the
    weights and are not in it.
    """
    return (
        f"{FAMILY}:{weight_format}|{device}|{options.fp8_compute.value}"
        f"|{options.ae_dtype.value}|{options.encoder_mode.value}|{options.model_root.absolute()}"
    )


def compute_path(weight_format: WeightFormat, fp8_compute: Fp8Compute) -> str:
    """How `weight_format`'s matmuls run: bf16 native, nvfp4 dequant (no FP4 kernel), fp8r per `fp8_compute`."""
    match weight_format:
        case WeightFormat.BF16:
            return PATH_NATIVE
        case WeightFormat.NVFP4:
            return PATH_DEQUANT
        case WeightFormat.FP8R:
            return PATH_SCALED_MM if fp8_compute is Fp8Compute.SCALED_MM else PATH_DEQUANT
        case _:
            assert_never(weight_format)


# =====================================================================
#  Request shape -> work
# =====================================================================
def _padded(tokens: int) -> int:
    """The encoder's padded length: up to a multiple of 80, at most 4096 (`text_encoder.padded_length`)."""
    return min(max(TEXT_PAD_MULTIPLE, math.ceil(tokens / TEXT_PAD_MULTIPLE) * TEXT_PAD_MULTIPLE), TEXT_MAX_TOKENS)


def caption_token_bound(caption: str) -> int:
    """An upper bound of the text-only template's token count of `caption` (no tokenizer needed).

    BPE never makes more tokens than UTF-8 bytes, plus the template's own
    tokens; capped at `TEXT_MAX_TOKENS` (the encoder truncates there).
    """
    return min(TEXT_MAX_TOKENS, len(caption.encode("utf-8")) + _TEMPLATE_TOKEN_BOUND)


def _grid(value: int) -> int:
    return -(-value // SIDE_MULTIPLE) * SIDE_MULTIPLE


def reference_pixel_bound(width: int, height: int, *, count: int, max_pixels: int | None) -> int:
    """An upper bound of the pixels one `width` x `height` reference has after the preparation rule.

    `count` is how many references the request carries (one: the 4 MP canvas
    rule with sides clamped to 64..4096; several: 1 MP each), `max_pixels`
    `Flux3Options.reference_max_pixels`. Idempotent on an already prepared
    size (sides on the 16 grid, area under the cap), so the service may pass
    prepared sizes.
    """
    cap = _SINGLE_REFERENCE_MAX_PIXELS if count == 1 else _MULTI_REFERENCE_MAX_PIXELS
    if max_pixels is not None:
        cap = min(cap, max_pixels)
    if count == 1:
        sides = min(MAX_SIDE, max(MIN_SIDE, _grid(width))) * min(MAX_SIDE, max(MIN_SIDE, _grid(height)))
    else:
        sides = _grid(width) * _grid(height)
    return min(cap, sides)


def request_shape(width: int, height: int, references: Sequence[tuple[int, int]], *, caption: str | None, steps: int | None) -> RequestShape:
    """The `RequestShape` of one request: output size, reference sizes, caption token bound, steps."""
    return RequestShape(width=width, height=height, references=tuple(references), prompt_tokens=None if caption is None else caption_token_bound(caption), steps=steps)


def _reference_pixels(shape: RequestShape, options: Flux3Options) -> list[int]:
    count = len(shape.references)
    return [reference_pixel_bound(w, h, count=count, max_pixels=options.reference_max_pixels) for w, h in shape.references]


def _text_tokens(shape: RequestShape, options: Flux3Options) -> int:
    """Padded encoder sequence length (= the DiT's text tokens) of the request, upper bound."""
    prompt = DEFAULT_PROMPT_TOKENS if shape.prompt_tokens is None else shape.prompt_tokens
    if options.encoder_mode is EncoderMode.VL_REFS:
        images = sum(min(options.vl_ref_max_pixels, pixels) // _VL_PIXELS_PER_TOKEN + _VL_WRAPPER_TOKENS for pixels in _reference_pixels(shape, options))
        prompt += images
    return _padded(prompt)


def _encode_work(shape: RequestShape, model_options: Flux3ModelOptions) -> PhaseWork:
    variant = "vl" if model_options.options.encoder_mode is EncoderMode.VL_REFS else "text"
    return PhaseWork(tokens=_text_tokens(shape, model_options.options), variant=variant)


def _ref_encode_work(shape: RequestShape, model_options: Flux3ModelOptions) -> PhaseWork:
    pixels = _reference_pixels(shape, model_options.options)
    return PhaseWork(pixels=max(pixels, default=0), variant=model_options.options.ae_dtype.value)


def _denoise_work(shape: RequestShape, model_options: Flux3ModelOptions) -> PhaseWork:
    options = model_options.options
    per_token = SIDE_MULTIPLE * SIDE_MULTIPLE
    image = (shape.width // SIDE_MULTIPLE) * (shape.height // SIDE_MULTIPLE)
    refs = sum(pixels // per_token for pixels in _reference_pixels(shape, options))
    steps = shape.steps if shape.steps is not None else SamplerConfig().steps
    return PhaseWork(tokens=image + refs + _text_tokens(shape, options), passes=steps)


def _decode_work(shape: RequestShape, model_options: Flux3ModelOptions) -> PhaseWork:
    return PhaseWork(pixels=shape.width * shape.height, variant=model_options.options.ae_dtype.value)


# =====================================================================
#  Components
# =====================================================================
def dit_block_of(state_key: str) -> str | None:
    """The residency block of a DiT state key (`None` = embedders, modulations, final layer)."""
    match = _DIT_BLOCK.match(state_key)
    return match.group(1) if match else None


def dit_block_order(names: Sequence[str]) -> list[str]:
    """DiT blocks in execution order (`Flux3Dit.forward`): per depth the content modes then text, then the single blocks."""

    def rank(name: str) -> tuple[int, int, int]:
        match = _DIT_BLOCK.match(name + ".")
        if match is None:
            raise ValueError(f"FLUX.3 klein: неизвестный блок DiT «{name}»")
        if match.group(2) is not None:
            return 0, int(match.group(3)), _DIT_CONTENT_MODES.index(match.group(2))
        if match.group(4) is not None:
            return 0, int(match.group(4)), len(_DIT_CONTENT_MODES)
        return 1, int(match.group(5)), 0

    return sorted(names, key=rank)


def dit_device_itemsize(file_key: str, dtype_token: str) -> int | None:
    """Device bytes per element of a DiT tensor: quantized payloads as stored, other floats bf16 (2)."""
    if file_key.rpartition(".")[2] in _QUANT_LEAVES or dtype_token in _EIGHT_BIT_TOKENS:
        return None
    return 2 if dtype_token in _FLOAT_TOKENS else None


def _dit_files(model_options: Flux3ModelOptions, weight_format: str) -> list[Path]:
    return [resolve_model_files(model_options.options.model_root, WeightFormat(weight_format)).dit_file]


def _build_dit(model_options: Flux3ModelOptions, weight_format: str) -> Any:
    from . import loading

    variant = WeightFormat(weight_format)
    files = resolve_model_files(model_options.options.model_root, variant)
    fp8 = model_options.fp8_compute if variant is WeightFormat.FP8R else None
    return loading.build_dit_meta(files, read_package_config(files.dit_config), variant=variant, fp8_compute_applied=fp8)


def _convert_dit(file_key: str, tensor: Any) -> Iterator[tuple[str, Any]]:
    from . import loading

    return loading.convert_dit_tensor(file_key, tensor)


def _encoder_layout(mode: EncoderMode) -> tuple[Callable[[str], str | None], re.Pattern[str]]:
    match mode:
        case EncoderMode.TEXT:
            return text_mode_key_map(ENCODER_KEEP_LAYERS), _TEXT_LAYER
        case EncoderMode.VL_REFS:
            return vl_mode_key_map(ENCODER_KEEP_LAYERS), _VL_LAYER
        case _:
            assert_never(mode)


def _encoder_component(mode: EncoderMode) -> ComponentSpec:
    key_map, layer = _encoder_layout(mode)

    def files(model_options: Flux3ModelOptions, weight_format: str) -> list[Path]:
        return text_encoder_shards(package_text_encoder_dir(model_options.options.model_root))

    def state_keys_of(file_key: str) -> tuple[str, ...]:
        state_key = key_map(file_key)
        return () if state_key is None else (state_key,)

    def block_of(state_key: str) -> str | None:
        match = layer.match(state_key)
        return match.group(1) if match else None

    def device_itemsize(file_key: str, dtype_token: str) -> int | None:
        return 2 if dtype_token in _FLOAT_TOKENS else None

    def build_meta(model_options: Flux3ModelOptions, weight_format: str) -> Any:
        from . import text_encoder

        return text_encoder.build_text_encoder_meta(package_text_encoder_dir(model_options.options.model_root), mode=mode, keep_layers=ENCODER_KEEP_LAYERS)

    def convert(file_key: str, tensor: Any) -> Iterator[tuple[str, Any]]:
        import torch

        state_key = key_map(file_key)
        if state_key is not None:
            yield state_key, tensor.to(torch.bfloat16) if tensor.is_floating_point() else tensor

    return ComponentSpec(
        COMPONENT_ENCODER,
        Role.ENCODER,
        files,
        block_of,
        state_keys_of=state_keys_of,
        device_itemsize=device_itemsize,
        exec_devices_allowed=frozenset({ExecDevice.RUN_DEVICE, ExecDevice.CPU}),
        build_meta=build_meta,
        convert=convert,
    )


def _ae_itemsize(ae_dtype: AeDtype) -> int:
    match ae_dtype:
        case AeDtype.FLOAT32:
            return 4
        case AeDtype.BFLOAT16:
            return 2
        case _:
            assert_never(ae_dtype)


def _ae_component(ae_dtype: AeDtype) -> ComponentSpec:
    itemsize = _ae_itemsize(ae_dtype)

    def files(model_options: Flux3ModelOptions, weight_format: str) -> list[Path]:
        return [package_ae_file(model_options.options.model_root)]

    def device_itemsize(file_key: str, dtype_token: str) -> int | None:
        if file_key.startswith(_AE_BN_PREFIX) or dtype_token not in _FLOAT_TOKENS:
            return None
        return itemsize

    def build_meta(model_options: Flux3ModelOptions, weight_format: str) -> Any:
        from . import autoencoder

        return autoencoder.build_autoencoder_meta()

    def convert(file_key: str, tensor: Any) -> Iterator[tuple[str, Any]]:
        import torch

        from . import autoencoder

        dtype = torch.float32 if ae_dtype is AeDtype.FLOAT32 else torch.bfloat16
        yield file_key, autoencoder.ae_tensor_cast(dtype)(file_key, tensor)

    return ComponentSpec(COMPONENT_AE, Role.AE, files, lambda state_key: None, device_itemsize=device_itemsize, build_meta=build_meta, convert=convert)


# =====================================================================
#  Formats and the spec
# =====================================================================
def formats(model_options: Flux3ModelOptions) -> dict[str, FormatInfo]:
    """Every weight format of the package under `model_options.options.model_root` (file checks only).

    `dit_bytes` is the DiT checkpoint's file size (quantized payloads stay
    quantized on the device, other tensors are stored bf16), 0 when absent.
    """
    root = model_options.options.model_root
    availability = variant_availability(root)
    result: dict[str, FormatInfo] = {}
    for variant in WeightFormat:
        reason = availability[variant]
        size = 0
        if reason is None:
            dit_file = resolve_model_files(root, variant).dit_file
            try:
                size = dit_file.stat().st_size
            except OSError as exc:
                log.warning("FLUX.3 klein: размер %s не прочитан (%s)", dit_file, exc)
        result[variant.value] = FormatInfo(available=reason is None, reason=reason, dit_bytes=size, compute_path=compute_path(variant, model_options.fp8_compute), quality_rank=QUALITY_RANK[variant])
    return result


def _key_of(model_options: Flux3ModelOptions, weight_format: str, device: str) -> str:
    return residency_key(model_options.options, weight_format, device)


def build_spec(encoder_mode: EncoderMode, ae_dtype: AeDtype) -> LargeModelSpec:
    """The `LargeModelSpec` of FLUX.3 klein with the `encoder_mode` encoder and an `ae_dtype` AE.

    Its callables take `Flux3ModelOptions` (the manager passes the options it
    was given through unchanged).
    """
    dit = ComponentSpec(
        COMPONENT_DIT,
        Role.DIT,
        _dit_files,
        dit_block_of,
        device_itemsize=dit_device_itemsize,
        block_order=dit_block_order,
        build_meta=_build_dit,
        convert=_convert_dit,
    )
    return LargeModelSpec(
        family=FAMILY,
        components=(dit, _encoder_component(encoder_mode), _ae_component(ae_dtype)),
        phases=(
            PhaseSpec(PHASE_ENCODE, frozenset({COMPONENT_ENCODER}), COMPONENT_ENCODER, _encode_work),
            PhaseSpec(PHASE_REF_ENCODE, frozenset({COMPONENT_AE}), COMPONENT_AE, _ref_encode_work),
            PhaseSpec(PHASE_DENOISE, frozenset({COMPONENT_DIT}), COMPONENT_DIT, _denoise_work),
            PhaseSpec(PHASE_DECODE, frozenset({COMPONENT_AE}), COMPONENT_AE, _decode_work),
        ),
        formats=formats,
        key_of=_key_of,
        fallbacks=(),
    )


def model_specs() -> dict[str, LargeModelSpec]:
    """Every FLUX.3 klein model to register with the manager, by `model_name`."""
    return {model_name(mode, ae_dtype): build_spec(mode, ae_dtype) for mode in EncoderMode for ae_dtype in AeDtype}
