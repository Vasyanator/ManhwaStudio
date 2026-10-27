"""
File: modules/ai_backend/inpaint/flux3_klein/large_model.py

Purpose:
FLUX.3 klein as the large-model manager sees it: the two `LargeModelSpec`s
of one run — the PIPELINE handle (DiT + AE) and the TEXT-ENCODER handle —
(components, residency blocks, phase tables, weight formats, residency keys)
that `runtime.large_models.manager.LargeModelManager` plans, loads and drives a
run with, the two-handle phase names (`FLUX3_PAIR`, for
`runtime.large_models.paired`), plus the request-shape arithmetic the forecast
needs.

Main responsibilities:
- `Flux3ModelOptions` - what every spec callable of both handles receives (the
  user's `Flux3Options`, the run device, the concrete fp8 path);
- `pipeline_model_name` / `pipeline_spec` - one registered pipeline model per
  AE dtype (`flux3_klein.ae_<dtype>`); `encoder_model_name` / `encoder_spec` -
  one registered encoder model per encoder mode
  (`flux3_klein_text_encoder.<text|vl_refs>`); `model_specs` - all four, every
  one of family `FAMILY`;
- `pipeline_key` / `encoder_key` - one manager handle per loaded byte set of
  each handle (the manager adds the encoder's host pin, `LargeModelSpec.key`);
- `formats` - one row per offered DiT format (`Flux3Options.dit_formats`:
  natives and GGUF files) with availability, DiT size, compute path, quality
  rank = offer order (the user's choice; the manager never changes it);
  `encoder_formats` - one row per offered encoder format
  (`Flux3Options.encoder_formats`: bf16 and GGUF files), a GGUF row
  unavailable in VL_REFS mode (D6);
- `dit_source` / `encoder_source` - the offered source a format id names;
- `request_shape`, `caption_token_bound`, `reference_pixel_bound` - the
  `RequestShape` of a request and the per-phase `PhaseWork` derived from it.

Components (names `COMPONENT_*`):
- pipeline `dit` (role DIT, run device only): blocks in EXECUTION order — per
  depth `d` `content_mode_blocks.image.d`, `content_mode_blocks.image_cond.d`,
  `txt_mode_blocks.d`, then `single_blocks.0..n` (the residency hooks prefetch
  the next block of that order); embedders, modulations and the final layer
  are non-block weights. Floats become bf16, quantized payloads stay as stored
  (`loading.convert_dit_tensor`): fp8r / nvfp4 payloads, and a GGUF DiT's
  block-quantized `uint8` rows (`runtime.gguf_layers` modules, dequantized per
  forward; compute path `gguf`).
- pipeline `ae` (role AE, run device only): monolithic, in the model's AE
  dtype (BatchNorm statistics stay float32).
- encoder `encoder` (role ENCODER, run device or CPU): the truncated Qwen3-VL
  (33 of 36 layers), blocks = its decoder layers (`layers.N` in TEXT,
  `language_model.layers.N` in VL_REFS; the vision tower is a non-block
  weight), bf16. Its files are the planned FORMAT's (`encoder_source`): the
  bf16 shards or a GGUF file; GGUF names (`blk.N.*`, `token_embd`, ...) are
  bridged by `runtime.qwen3_gguf` onto the `Qwen3VLTextModel` keys (TEXT mode
  only), quantized tensors stay `uint8` rows (`gguf_layers`), and the encode
  work is variant `ENCODE_VARIANT_GGUF` (never a borrowed bf16 measurement).

Phases, in run order: encoder `encode`; pipeline `ref_encode` (ae: the
reference AE encode), `denoise` (dit), `decode` (ae).

Notes:
- Why two handles: a run whose prompt needs no encode (the prompt cache, a
  later package) must not acquire, read or forecast the encoder at all; and
  the encoder handle is the second admission of the two-handle memory rule
  (`runtime.large_models.paired`, phases `FLUX3_PAIR`: the encoder runs beside
  the pipeline's `ref_encode`-state weights).
- Why one registered model per AE dtype / encoder mode: the manager caches a
  component inventory per (model, format, files), and `ComponentSpec.convert`
  / `device_itemsize` see no options. The encoder mode changes the encoder's
  state keys (a different module), the AE dtype its device bytes; the
  residency keys keep their handles apart too.
- No model-specific degrade rung (`fallbacks=()`): `reference_max_pixels` and
  the AE dtype are the user's quality choices, the port has no tiled AE, and
  "encoder on the CPU" is a step placement (`tiers.template_for`) or, under
  `Flux3Options.encoder_on_cpu`, the encoder spec's host pin (`_host_pin`).
- Forecast inputs are UPPER bounds where the exact figure needs the tokenizer
  (`caption_token_bound`) or the reference preparation (`reference_pixel_bound`):
  a forecast may be high, never low because of them. The encode work's
  variant follows the SELECTED encoder (`options.encoder`; `PhaseWork`
  callables see no format), so another encoder format's `formats` row is
  planned with the selected one's activation cost (display only).
- Torch-free at import: the loader callables import `loading` /
  `autoencoder` / `text_encoder` on call and look them up as
  `<module>.<name>` (the tests patch them there); `runtime.qwen3_gguf` builds
  its name tables (transformers) only when a GGUF name is seen.
"""

from __future__ import annotations

import logging
import math
import re
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, assert_never

from ...runtime import qwen3_gguf
from ...runtime.encoder_truncation import keep_layers_for_hidden_states
from ...runtime.large_models.paired import PairPhases
from ...runtime.large_models.spec import ComponentSpec, ExecDevice, FormatInfo, LargeModelSpec, PhaseSpec, PhaseWork, RequestShape, Role
from .config import (
    TEXT_MAX_TOKENS,
    TEXT_OUTPUT_LAYERS,
    TEXT_PAD_MULTIPLE,
    DitSource,
    EncoderSource,
    GgufDit,
    SamplerConfig,
    WeightFormat,
    package_ae_file,
    package_text_encoder_dir,
    read_package_config,
    resolve_model_files,
    text_encoder_availability,
    text_encoder_files,
    text_mode_key_map,
    vl_mode_key_map,
)
from .params import MAX_SIDE, MIN_SIDE, SIDE_MULTIPLE, AeDtype, EncoderMode, Flux3Options, Fp8Compute

log = logging.getLogger(__name__)

#: Calibration family of every FLUX.3 klein model of both handles
#: (`calibration/*.json` keys), and the pipeline's model-name / key prefix.
FAMILY = "flux3_klein"

#: Model-name / residency-key prefix of the text-encoder handle.
ENCODER_MODEL = "flux3_klein_text_encoder"

COMPONENT_DIT = "dit"
COMPONENT_ENCODER = "encoder"
COMPONENT_AE = "ae"

PHASE_ENCODE = "encode"
PHASE_REF_ENCODE = "ref_encode"
PHASE_DENOISE = "denoise"
PHASE_DECODE = "decode"

#: The two-handle rule's phase names (`runtime.large_models.paired`): the
#: pipeline's phases bound the run; the encoder runs beside the pipeline's
#: `ref_encode`-state weights (a run enters `ref_encode` placement before it
#: encodes).
FLUX3_PAIR = PairPhases(pipeline=(PHASE_REF_ENCODE, PHASE_DENOISE, PHASE_DECODE), rest=PHASE_REF_ENCODE, encode=PHASE_ENCODE)

#: Compute paths (`FormatInfo.compute_path`, calibration keys).
PATH_NATIVE = "native"
PATH_SCALED_MM = "scaled_mm"
PATH_DEQUANT = "dequant"
PATH_GGUF = "gguf"

#: `PhaseWork.variant` of the encode phase: the bf16 encoder in each mode, and
#: a GGUF encoder (TEXT only), whose cost is its own calibration entry.
ENCODE_VARIANT_TEXT = "text"
ENCODE_VARIANT_VL = "vl"
ENCODE_VARIANT_GGUF = "text_gguf"

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


def pipeline_model_name(ae_dtype: AeDtype) -> str:
    """The registered pipeline model of `ae_dtype`: `flux3_klein.ae_<dtype>`."""
    return f"{FAMILY}.ae_{ae_dtype.value}"


def encoder_model_name(encoder_mode: EncoderMode) -> str:
    """The registered text-encoder model of `encoder_mode`: `flux3_klein_text_encoder.<mode>`."""
    return f"{ENCODER_MODEL}.{encoder_mode.value}"


def dit_source(options: Flux3Options, weight_format: str) -> DitSource:
    """The offered DiT format (`options.dit_formats`) whose id is `weight_format`.

    # Raises
    `ValueError` naming the offered ids when `weight_format` is not one of them.
    """
    for source in options.dit_formats:
        if source.value == weight_format:
            return source
    raise ValueError(f"FLUX.3 klein: формат DiT «{weight_format}» не входит в предложенные {[source.value for source in options.dit_formats]}")


def encoder_source(options: Flux3Options, weight_format: str) -> EncoderSource:
    """The offered encoder format (`options.encoder_formats`) whose id is `weight_format`.

    # Raises
    `ValueError` naming the offered ids when `weight_format` is not one of them.
    """
    for source in options.encoder_formats:
        if source.id == weight_format:
            return source
    raise ValueError(f"FLUX.3 klein: формат текстового энкодера «{weight_format}» не входит в предложенные {[source.id for source in options.encoder_formats]}")


def pipeline_key(options: Flux3Options, weight_format: str, device: str) -> str:
    """The pipeline handle key of `options` loaded in DiT format `weight_format` on `device`.

    `flux3_klein:{format}|{device}|{fp8 requested}|{ae dtype}|{root}`, then
    `|dit=<path>` for a GGUF DiT (two files may share an id): everything that
    changes the DiT's or the AE's loaded bytes or modules. The encoder (its
    mode, format, pin) is the other handle's; `reference_max_pixels`, the
    resource steps and the request do not change the weights.

    # Raises
    `ValueError` when `weight_format` is not offered (`dit_source`).
    """
    key = f"{FAMILY}:{weight_format}|{device}|{options.fp8_compute.value}|{options.ae_dtype.value}|{options.model_root.absolute()}"
    source = dit_source(options, weight_format)
    if isinstance(source, GgufDit):
        key += f"|dit={source.path}"
    return key


def encoder_key(options: Flux3Options, weight_format: str, device: str) -> str:
    """The text-encoder handle key of `options` loaded in encoder format `weight_format` on `device`.

    `flux3_klein_text_encoder:{format}|{device}|{encoder mode}|{root}`, then
    `|enc=<path>` for a GGUF encoder (two files may share an id); the manager
    appends `|host_pin=encoder` under «Энкодер на CPU» (`LargeModelSpec.key`).

    # Raises
    `ValueError` when `weight_format` is not offered (`encoder_source`).
    """
    source = encoder_source(options, weight_format)
    key = f"{ENCODER_MODEL}:{source.id}|{device}|{options.encoder_mode.value}|{options.model_root.absolute()}"
    if source.path is not None:
        key += f"|enc={source.path}"
    return key


def compute_path(source: DitSource, fp8_compute: Fp8Compute) -> str:
    """How `source`'s matmuls run: bf16 native, nvfp4 dequant (no FP4 kernel), fp8r per `fp8_compute`, GGUF `gguf` (per-forward dequant)."""
    if isinstance(source, GgufDit):
        return PATH_GGUF
    match source:
        case WeightFormat.BF16:
            return PATH_NATIVE
        case WeightFormat.NVFP4:
            return PATH_DEQUANT
        case WeightFormat.FP8R:
            return PATH_SCALED_MM if fp8_compute is Fp8Compute.SCALED_MM else PATH_DEQUANT
        case _:
            assert_never(source)


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
    options = model_options.options
    if options.encoder.is_gguf:
        variant = ENCODE_VARIANT_GGUF
    elif options.encoder_mode is EncoderMode.VL_REFS:
        variant = ENCODE_VARIANT_VL
    else:
        variant = ENCODE_VARIANT_TEXT
    return PhaseWork(tokens=_text_tokens(shape, options), variant=variant)


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
    options = model_options.options
    return [resolve_model_files(options.model_root, dit_source(options, weight_format)).dit_file]


def _build_dit(model_options: Flux3ModelOptions, weight_format: str) -> Any:
    from . import loading

    options = model_options.options
    variant = dit_source(options, weight_format)
    files = resolve_model_files(options.model_root, variant)
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


def _is_gguf_name(file_key: str) -> bool:
    """Whether an encoder file key is a llama.cpp GGUF name. The bf16 shards' keys all start with
    `model.`, checked first, so a bf16 encoder never builds `qwen3_gguf`'s name tables."""
    return not file_key.startswith("model.") and qwen3_gguf.is_gguf_encoder_name(file_key)


def _encoder_component(mode: EncoderMode) -> ComponentSpec:
    key_map, layer = _encoder_layout(mode)

    def state_key(file_key: str) -> str | None:
        """The state key of one encoder file key (bf16 shard or GGUF name), `None` = never read."""
        if _is_gguf_name(file_key):
            if mode is not EncoderMode.TEXT:
                # Unreachable through `Flux3Options` (it refuses VL_REFS with a
                # GGUF encoder): a GGUF has no vision tower to fill.
                raise ValueError(f"FLUX.3 klein: тензор GGUF «{file_key}» в режиме энкодера «{mode.value}» (GGUF — только текстовый режим)")
            return qwen3_gguf.encoder_state_key_of_gguf(file_key, ENCODER_KEEP_LAYERS)
        return key_map(file_key)

    def files(model_options: Flux3ModelOptions, weight_format: str) -> list[Path]:
        options = model_options.options
        return text_encoder_files(options.model_root, encoder_source(options, weight_format))

    def state_keys_of(file_key: str) -> tuple[str, ...]:
        found = state_key(file_key)
        return () if found is None else (found,)

    def block_of(state_key: str) -> str | None:
        match = layer.match(state_key)
        return match.group(1) if match else None

    def device_itemsize(file_key: str, dtype_token: str) -> int | None:
        return 2 if dtype_token in _FLOAT_TOKENS else None

    def build_meta(model_options: Flux3ModelOptions, weight_format: str) -> Any:
        from . import text_encoder

        options = model_options.options
        directory = package_text_encoder_dir(options.model_root)
        source = encoder_source(options, weight_format)
        if source.path is not None:
            return text_encoder.build_gguf_text_encoder_meta(directory, source.path, format_id=source.id, keep_layers=ENCODER_KEEP_LAYERS)
        return text_encoder.build_text_encoder_meta(directory, mode=mode, keep_layers=ENCODER_KEEP_LAYERS)

    def convert(file_key: str, tensor: Any) -> Iterator[tuple[str, Any]]:
        import torch

        # Floats (the shards' bf16, a GGUF's F32 norms) become bf16; a GGUF's
        # block-quantized `uint8` rows stay as stored for `gguf_layers`.
        found = state_key(file_key)
        if found is not None:
            yield found, tensor.to(torch.bfloat16) if tensor.is_floating_point() else tensor

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
    """One row per offered DiT format (`Flux3Options.dit_formats`, in offer order; file checks only).

    `available` = the format's DiT file, its `config.json` and the AE exist
    (`resolve_model_files`; `reason` names what is missing; the encoder is the
    other handle's and never makes a DiT row unavailable); `dit_bytes` is the DiT file's size (quantized payloads stay
    quantized on the device, other tensors are stored bf16), 0 when absent;
    `quality_rank` = count - index (the client offers best first: bf16 >
    fp8r > q8_0 > ... in the catalog's order).
    """
    options = model_options.options
    count = len(options.dit_formats)
    result: dict[str, FormatInfo] = {}
    for index, source in enumerate(options.dit_formats):
        reason: str | None = None
        size = 0
        try:
            dit_file = resolve_model_files(options.model_root, source).dit_file
        except FileNotFoundError as exc:
            reason = str(exc)
        else:
            try:
                size = dit_file.stat().st_size
            except OSError as exc:
                log.warning("FLUX.3 klein: размер %s не прочитан (%s)", dit_file, exc)
        result[source.value] = FormatInfo(available=reason is None, reason=reason, dit_bytes=size, compute_path=compute_path(source, model_options.fp8_compute), quality_rank=count - index)
    return result


def encoder_formats(model_options: Flux3ModelOptions) -> dict[str, FormatInfo]:
    """One row per offered encoder format (`Flux3Options.encoder_formats`, in offer order; file checks only).

    `available` = the format's weights and the `text_encoder/` directory exist
    (`config.text_encoder_availability`; `reason` names what is missing) and,
    in `EncoderMode.VL_REFS`, the format is not a GGUF file (D6: a GGUF holds
    no vision tower); `dit_bytes` is the weight files' size (0 when absent);
    `compute_path` `gguf` for a GGUF file, else `native`; `quality_rank` =
    count - index (the client offers bf16 first).
    """
    options = model_options.options
    count = len(options.encoder_formats)
    result: dict[str, FormatInfo] = {}
    for index, source in enumerate(options.encoder_formats):
        reason, size = text_encoder_availability(options.model_root, source)
        if reason is None and source.is_gguf and options.encoder_mode is EncoderMode.VL_REFS:
            reason = f"FLUX.3 klein: текстовый энкодер «{source.id}» (GGUF) работает только в текстовом режиме: режим «{EncoderMode.VL_REFS.value}» показывает референсы зрительной части Qwen3-VL, которой в GGUF нет"
            size = 0
        result[source.id] = FormatInfo(available=reason is None, reason=reason, dit_bytes=size, compute_path=PATH_GGUF if source.is_gguf else PATH_NATIVE, quality_rank=count - index)
    return result


def _host_pin(model_options: Flux3ModelOptions) -> tuple[str, ...]:
    """«Энкодер на CPU»: the encoder pinned to host execution when `Flux3Options.encoder_on_cpu`."""
    return (COMPONENT_ENCODER,) if model_options.options.encoder_on_cpu else ()


def _pipeline_key_of(model_options: Flux3ModelOptions, weight_format: str, device: str) -> str:
    return pipeline_key(model_options.options, weight_format, device)


def _encoder_key_of(model_options: Flux3ModelOptions, weight_format: str, device: str) -> str:
    return encoder_key(model_options.options, weight_format, device)


def pipeline_spec(ae_dtype: AeDtype) -> LargeModelSpec:
    """The pipeline `LargeModelSpec` (DiT + `ae_dtype` AE; phases `ref_encode`, `denoise`, `decode`; formats = the offered DiT formats).

    Its callables take `Flux3ModelOptions` (the manager passes the options it
    was given through unchanged); it never pins a component to the host.
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
        components=(dit, _ae_component(ae_dtype)),
        phases=(
            PhaseSpec(PHASE_REF_ENCODE, frozenset({COMPONENT_AE}), COMPONENT_AE, _ref_encode_work),
            PhaseSpec(PHASE_DENOISE, frozenset({COMPONENT_DIT}), COMPONENT_DIT, _denoise_work),
            PhaseSpec(PHASE_DECODE, frozenset({COMPONENT_AE}), COMPONENT_AE, _decode_work),
        ),
        formats=formats,
        key_of=_pipeline_key_of,
        fallbacks=(),
    )


def encoder_spec(encoder_mode: EncoderMode) -> LargeModelSpec:
    """The text-encoder `LargeModelSpec` of `encoder_mode` (one component, phase `encode`; formats = the offered encoder formats).

    Its callables take `Flux3ModelOptions`; the format the manager plans picks
    the files (`encoder_source`). «Энкодер на CPU» is its host pin.
    """
    return LargeModelSpec(
        family=FAMILY,
        components=(_encoder_component(encoder_mode),),
        phases=(PhaseSpec(PHASE_ENCODE, frozenset({COMPONENT_ENCODER}), COMPONENT_ENCODER, _encode_work),),
        formats=encoder_formats,
        key_of=_encoder_key_of,
        fallbacks=(),
        host_pin_of=_host_pin,
    )


def model_specs() -> dict[str, LargeModelSpec]:
    """Every FLUX.3 klein model to register with the manager, by name: a pipeline per AE dtype, an encoder per encoder mode."""
    specs = {pipeline_model_name(ae_dtype): pipeline_spec(ae_dtype) for ae_dtype in AeDtype}
    specs.update({encoder_model_name(mode): encoder_spec(mode) for mode in EncoderMode})
    return specs
