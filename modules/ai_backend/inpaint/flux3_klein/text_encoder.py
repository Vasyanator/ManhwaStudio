"""
File: modules/ai_backend/inpaint/flux3_klein/text_encoder.py

Purpose:
The FLUX.3 klein text encoder: Qwen3-VL-4B-Instruct truncated to the decoder
layers the DiT reads, loaded tensor by tensor onto the run device, and the two
ways a caption becomes DiT text tokens — text only (the reference) and the
experimental VL mode that also shows the reference images to the encoder.

Main responsibilities:
- building (`build_text_encoder_meta`, `encoder_key_map`): config truncation to
  `ENCODER_KEEP_LAYERS` and the model on `meta` — what the large-model spec
  (`large_model.py`) loads through the manager;
- the standalone loader (`load_text_encoder`, `load_text_encoder_model`,
  `load_text_processor`): the same model filled from the shards of
  `model.safetensors.index.json` (the parity script, tests);
- the text-only path (`tokenize_caption`, `encode_text`), byte-for-byte the
  reference's tokenization, padding and position ids;
- the VL experiment (`vl_pixel_budget`, `prepare_reference_batch`,
  `encode_with_references`);
- the shared forward (`run_text_encoder`), on whichever device the handle
  names (the run device or the CPU: the residency plan decides).

Key declarations:
- `TextEncoderHandle`, `TokenBatch`, `EncodedText`
- `ENCODER_KEEP_LAYERS`, `VL_PIXELS_PER_TOKEN`, `VL_MIN_TOKENS_PER_REFERENCE`
- `padded_length()`, `reference_position_ids()`

Notes:
- Ported from black-forest-labs/flux-3-image-klein `src/flux3/text_encoder.py`
  (commit e53f4ea), Copyright 2026 Black Forest Labs, Inc., licensed under the
  Apache License, Version 2.0 (see `LICENSE` in this directory). The ported
  parts are the chat template, the 4096-token truncation, the right padding to a
  multiple of 80, the position ids (cumsum - 1, pads = 1), the stacking of the
  hidden states 4..32 and the DiT ids `(0, 0, 0, position)`.
- Modified: the encoder keeps 33 of 36 decoder layers and never loads the vision
  tower in TEXT mode (`Qwen3VLTextModel` instead of
  `Qwen3VLForConditionalGeneration`, no `lm_head`); weights are streamed one
  tensor at a time (`runtime.safetensors_stream.fill_module_from_safetensors`)
  instead of `from_pretrained`; attention is pinned to SDPA; the padded length
  is computed without a second truncating tokenization; truncation is logged;
  the VL_REFS mode (images through Qwen3-VL's vision tower, mRoPE ids) is not
  in the reference at all — it is this port's experiment. Where the encoder
  lives and runs is the large-model manager's decision; on the CPU (bf16, 6
  torch threads, real weights) it measured ~1 s per text encode and ~2.3 s per
  VL encode with one reference.
- Why 33 layers and not 32: `runtime.encoder_truncation` (transformers replaces
  the LAST collected hidden state with the normed output).
- Why the VL mode does not reuse the reference ids: image tokens need Qwen3-VL's
  own 3-axis mRoPE ids, which `Qwen3VLModel.forward` computes through
  `get_rope_index` only when `position_ids is None`.
- Image-processor pixel cap (transformers 4.57.1,
  `Qwen2VLImageProcessorFast._further_process_kwargs`): the cap is
  `size={"shortest_edge": min, "longest_edge": max}` with BOTH keys; a lone
  `max_pixels` is silently ignored, and the processor pops the kwargs dicts it
  is given, so every call builds a fresh one (`test_real_processor.py` pins it).
- The shard list and the checkpoint key maps live in the torch-free
  `config.py` (the spec sizes the encoder from them without torch).
- torch is imported at module level; transformers, accelerate and PIL are
  imported inside the functions that need them.
"""

from __future__ import annotations

import logging
import math
import time
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any, assert_never

import torch

from ...runtime.encoder_truncation import keep_layers_for_hidden_states
from ...runtime.rocm_mmap_transfer import move_module_to
from ...runtime.safetensors_stream import FillReport, fill_module_from_safetensors
from .config import TEXT_MAX_TOKENS, TEXT_OUTPUT_LAYERS, TEXT_PAD_MULTIPLE, text_encoder_shards, text_mode_key_map, vl_mode_key_map
from .params import EncoderMode

if TYPE_CHECKING:
    from PIL.Image import Image

log = logging.getLogger(__name__)

#: Decoder layers the encoder keeps: `max(TEXT_OUTPUT_LAYERS) + 1` = 33. The
#: `+ 1` keeps `hidden_states[32]` raw (rule and measurement:
#: `runtime.encoder_truncation.keep_layers_for_hidden_states`).
ENCODER_KEEP_LAYERS = keep_layers_for_hidden_states(TEXT_OUTPUT_LAYERS)

#: Pixels per LLM token of one image in the VL mode: one token per
#: (patch 16 x merge 2)^2 = 32 x 32 px of the resized image.
VL_PIXELS_PER_TOKEN = 32 * 32

#: Smallest per-reference token budget the VL mode accepts; below it a
#: reference would be shown to the encoder as a 64 x 64 px thumbnail at best.
VL_MIN_TOKENS_PER_REFERENCE = 4

#: Tokens the chat template adds around every image: `<|vision_start|>` and
#: `<|vision_end|>`.
VL_WRAPPER_TOKENS_PER_REFERENCE = 2

#: Owner prefix of the loader's log records and refusal texts.
_OWNER = "FLUX.3 klein (текстовый энкодер)"



@dataclass(frozen=True)
class TextEncoderHandle:
    """A loaded encoder and its processor.

    `model` is a `Qwen3VLTextModel` in `EncoderMode.TEXT` and a `Qwen3VLModel`
    (vision tower + language model) in `EncoderMode.VL_REFS`, both with
    `keep_layers` decoder layers. `device` is where the forward runs and the
    inputs are sent (the run device, or the CPU when the residency plan runs
    the encoder there). `processor` is `None` only for a model-only handle
    built in tests.
    """

    model: Any
    processor: Any
    mode: EncoderMode
    device: torch.device
    keep_layers: int


@dataclass(frozen=True)
class TokenBatch:
    """One encoder input, already right-padded: `input_ids` / `attention_mask`
    are `[1, L]` with L a multiple of 80 and at most 4096, `real_length` the
    count of mask-1 tokens. `pixel_values` / `image_grid_thw` are set only for a
    VL batch (the image processor's own layout)."""

    input_ids: torch.Tensor
    attention_mask: torch.Tensor
    real_length: int
    pixel_values: torch.Tensor | None = None
    image_grid_thw: torch.Tensor | None = None


@dataclass(frozen=True)
class EncodedText:
    """DiT text conditioning: `tokens` `[L, 8 * hidden]` bf16 (every row, padding
    included, as the reference feeds them), `ids` `[L, 4]` = `(0, 0, 0, i)`,
    `real_length` the unpadded token count."""

    tokens: torch.Tensor
    ids: torch.Tensor
    real_length: int


# =====================================================================
#  Loading
# =====================================================================
def _truncated_config(directory: Path, keep_layers: int) -> Any:
    """`Qwen3VLConfig` of `directory` with SDPA attention and the language model cut to `keep_layers`.

    # Raises
    `ValueError` when the checkpoint has fewer decoder layers than `keep_layers`
    (the DiT would read states the encoder cannot produce).
    """
    from transformers import Qwen3VLConfig

    config = Qwen3VLConfig.from_pretrained(directory, attn_implementation="sdpa")
    text_config = config.text_config
    available = int(text_config.num_hidden_layers)
    if available < keep_layers:
        raise ValueError(
            f"{_OWNER}: в модели {available} слоёв декодера, а нужно не меньше {keep_layers}. "
            "Похоже, в папке text_encoder лежит другая модель."
        )
    text_config.num_hidden_layers = keep_layers
    layer_types = getattr(text_config, "layer_types", None)
    if layer_types is not None:
        text_config.layer_types = list(layer_types)[:keep_layers]
    return config


def build_text_encoder_meta(directory: Path, *, mode: EncoderMode, keep_layers: int = ENCODER_KEEP_LAYERS) -> Any:
    """The `mode` encoder model of `directory`, truncated to `keep_layers`, built on `meta`.

    TEXT builds a `Qwen3VLTextModel` (no vision tower, no `lm_head`); VL_REFS a
    `Qwen3VLModel`. Weights are on `meta`; the non-persistent rotary buffers
    are real (on the CPU), as every filler requires.

    # Raises
    `ValueError` from `_truncated_config`; whatever transformers raises for a
    broken `config.json`.
    """
    from accelerate import init_empty_weights
    from transformers.models.qwen3_vl.modeling_qwen3_vl import Qwen3VLModel, Qwen3VLTextModel

    config = _truncated_config(directory, keep_layers)
    # `include_buffers=False` explicitly: accelerate's default follows the
    # ACCELERATE_INIT_INCLUDE_BUFFERS env var, and the fill requires the
    # non-persistent rotary `inv_freq` buffers to be real already.
    with init_empty_weights(include_buffers=False):
        match mode:
            case EncoderMode.TEXT:
                return Qwen3VLTextModel(config.text_config)
            case EncoderMode.VL_REFS:
                return Qwen3VLModel(config)
            case _:
                assert_never(mode)


def encoder_key_map(mode: EncoderMode, keep_layers: int = ENCODER_KEEP_LAYERS) -> Any:
    """The checkpoint key -> state key map of `mode` (`config.text_mode_key_map` / `vl_mode_key_map`)."""
    match mode:
        case EncoderMode.TEXT:
            return text_mode_key_map(keep_layers)
        case EncoderMode.VL_REFS:
            return vl_mode_key_map(keep_layers)
        case _:
            assert_never(mode)


def load_text_encoder_model(
    directory: Path,
    *,
    mode: EncoderMode,
    device: Any,
    dtype: torch.dtype = torch.bfloat16,
    keep_layers: int = ENCODER_KEEP_LAYERS,
) -> tuple[Any, FillReport]:
    """Build the encoder model for `mode` on `meta` (`build_text_encoder_meta`) and fill it from `directory`'s shards.

    The standalone loader (the parity script, tests); a service run loads the
    encoder through the large-model manager (`large_model.py`) instead. Skipped
    tensors are never read. Floating-point weights are cast to `dtype`; the
    non-persistent rotary buffers follow the weights to `device`. The returned
    model is in eval mode.

    # Raises
    `FileNotFoundError` / `ValueError` from the index (`config.text_encoder_shards`),
    the config (`_truncated_config`) or the fill (wrong key set, shapes,
    leftover meta tensors) — see `fill_module_from_safetensors`.
    """
    started = time.monotonic()
    target = torch.device(device)
    files = text_encoder_shards(directory)
    model = build_text_encoder_meta(directory, mode=mode, keep_layers=keep_layers)

    def cast(_key: str, tensor: torch.Tensor) -> torch.Tensor:
        return tensor.to(dtype) if tensor.is_floating_point() else tensor

    report = fill_module_from_safetensors(
        model,
        files,
        device=target,
        key_map=encoder_key_map(mode, keep_layers),
        cast=cast,
        owner=_OWNER,
        logger=log,
    )
    # The fill moved every checkpoint tensor; only the small non-persistent
    # buffers are still on the CPU. A whole-module move goes through the ROCm
    # staging helper by rule (a no-op for tensors already on `target`).
    move_module_to(model, target)
    model.eval()
    log.info(
        "%s: модель %s загружена (%s, слоёв %d, %s, %s) за %.1f с",
        _OWNER,
        type(model).__name__,
        mode.value,
        keep_layers,
        dtype,
        target,
        time.monotonic() - started,
    )
    return model, report


def load_text_processor(directory: Path) -> Any:
    """`AutoProcessor` of `directory` with right padding, as the reference sets it.

    # Raises
    Whatever `AutoProcessor.from_pretrained` raises for a broken directory
    (`OSError` / `ValueError`).
    """
    from transformers import AutoProcessor

    processor = AutoProcessor.from_pretrained(directory)
    processor.tokenizer.padding_side = "right"
    return processor


def load_text_encoder(
    directory: Path,
    *,
    mode: EncoderMode,
    device: Any,
    dtype: torch.dtype = torch.bfloat16,
) -> TextEncoderHandle:
    """Load the processor and the `mode` encoder model (`load_text_encoder_model`) onto `device`.

    # Raises
    See `load_text_encoder_model` and `load_text_processor`.
    """
    processor = load_text_processor(directory)
    model, _report = load_text_encoder_model(directory, mode=mode, device=device, dtype=dtype)
    return TextEncoderHandle(
        model=model,
        processor=processor,
        mode=mode,
        device=torch.device(device),
        keep_layers=ENCODER_KEEP_LAYERS,
    )


# =====================================================================
#  Tokenization
# =====================================================================
def padded_length(real: int, *, multiple: int = TEXT_PAD_MULTIPLE, cap: int = TEXT_MAX_TOKENS) -> int:
    """The reference's padded sequence length: `min(ceil(real / multiple) * multiple, cap)`.

    # Raises
    `ValueError` for `real < 1` (an empty sequence has no padded form).
    """
    if real < 1:
        raise ValueError(f"Длина последовательности должна быть положительной, а не {real}")
    return min(math.ceil(real / multiple) * multiple, cap)


def reference_position_ids(attention_mask: torch.Tensor) -> torch.Tensor:
    """The reference's text-only position ids `[4, 1, L]` for a `[1, L]` mask.

    Real tokens count `0, 1, ...`; padding gets 1 (the reference's
    `cumsum - 1` with `masked_fill(mask == 0, 1)`). The four rows are the text
    axis plus the three mRoPE axes, all equal for text.
    """
    positions = (attention_mask.long().cumsum(-1) - 1).masked_fill(attention_mask == 0, 1)
    return positions[None].expand(4, -1, -1)


def _caption_template(processor: Any, caption: str) -> str:
    """The reference chat template: one user message, generation prompt appended."""
    return processor.apply_chat_template(
        [{"role": "user", "content": caption}], tokenize=False, add_generation_prompt=True
    )


def tokenize_caption(processor: Any, caption: str) -> TokenBatch:
    """Tokenize `caption` exactly as the reference text encoder does.

    The templated text is truncated to `TEXT_MAX_TOKENS` (logged as a warning)
    and right-padded with the tokenizer's pad token to `padded_length(real)`.
    """
    text = _caption_template(processor, caption)
    tokenizer = processor.tokenizer
    full = len(tokenizer(text)["input_ids"])
    real = min(full, TEXT_MAX_TOKENS)
    if full > TEXT_MAX_TOKENS:
        log.warning(
            "%s: запрос длиной %d токенов обрезан до %d, как в эталонной реализации",
            _OWNER,
            full,
            TEXT_MAX_TOKENS,
        )
    length = padded_length(real)
    inputs = tokenizer(text, return_tensors="pt", padding="max_length", truncation=True, max_length=length)
    return TokenBatch(
        input_ids=inputs["input_ids"],
        attention_mask=inputs["attention_mask"],
        real_length=real,
    )


def caption_token_count(processor: Any, caption: str) -> int:
    """Untruncated token count of the text-only template of `caption` (the VL budget input)."""
    return len(processor.tokenizer(_caption_template(processor, caption))["input_ids"])


# =====================================================================
#  The VL experiment
# =====================================================================
def vl_pixel_budget(
    text_only_tokens: int,
    n_refs: int,
    *,
    vl_ref_max_pixels: int,
    cap: int = TEXT_MAX_TOKENS,
) -> int:
    """Per-reference pixel cap for the VL mode.

    `min(vl_ref_max_pixels, floor((cap - text_only_tokens - 2 * n_refs) / n_refs) * 1024)`:
    the tokens left after the text-only template and the two vision wrapper
    tokens per image are shared equally, one token per 32 x 32 px.

    # Raises
    `ValueError` for `n_refs < 1`, a `vl_ref_max_pixels` below
    `VL_MIN_TOKENS_PER_REFERENCE` tokens' worth of pixels, or when the shared
    budget falls below that minimum (the caption leaves no room).
    """
    if n_refs < 1:
        raise ValueError("Режим VL требует хотя бы одно изображение-референс")
    minimum = VL_MIN_TOKENS_PER_REFERENCE * VL_PIXELS_PER_TOKEN
    if vl_ref_max_pixels < minimum:
        raise ValueError(
            f"Предельный размер референса для энкодера ({vl_ref_max_pixels} пикселей) меньше "
            f"допустимого минимума {minimum}"
        )
    free_tokens = cap - text_only_tokens - VL_WRAPPER_TOKENS_PER_REFERENCE * n_refs
    per_reference = free_tokens // n_refs
    budget = min(vl_ref_max_pixels, per_reference * VL_PIXELS_PER_TOKEN)
    if budget < minimum:
        raise ValueError(
            f"Запрос слишком длинный для режима VL: на каждый из {n_refs} референсов остаётся "
            f"{max(budget, 0)} пикселей, нужно не меньше {minimum}. Сократите запрос, уберите "
            "часть референсов или переключите энкодер в текстовый режим."
        )
    return budget


def right_pad(input_ids: torch.Tensor, *, length: int, pad_id: int) -> tuple[torch.Tensor, torch.Tensor]:
    """Right-pad a `[1, real]` id row to `[1, length]` with `pad_id`; returns `(ids, mask)`.

    # Raises
    `ValueError` when the row is longer than `length` (padding never truncates).
    """
    real = int(input_ids.shape[1])
    if real > length:
        raise ValueError(f"Последовательность из {real} токенов не помещается в {length}")
    pad = torch.full((1, length - real), pad_id, dtype=input_ids.dtype)
    mask = torch.cat([torch.ones((1, real), dtype=torch.long), torch.zeros((1, length - real), dtype=torch.long)], 1)
    return torch.cat([input_ids, pad], dim=1), mask


def prepare_reference_batch(
    processor: Any,
    caption: str,
    references: Sequence[Image],
    *,
    vl_ref_max_pixels: int,
) -> TokenBatch:
    """The VL_REFS input: every reference as an image, in order, before the caption.

    `references` are the same prepared images the autoencoder receives. Each
    is resized by the processor under the `vl_pixel_budget` cap; the sequence
    is right-padded to `padded_length(real)`.

    # Raises
    `ValueError` from `vl_pixel_budget`, when the tokenizer has no pad token,
    or when the processed sequence exceeds `TEXT_MAX_TOKENS` (image tokens are
    never truncated; an extreme aspect ratio can push a resized image past its
    budget).
    """
    n_refs = len(references)
    text_only = caption_token_count(processor, caption)
    budget = vl_pixel_budget(text_only, n_refs, vl_ref_max_pixels=vl_ref_max_pixels)
    content: list[dict[str, str]] = [{"type": "image"} for _ in range(n_refs)]
    content.append({"type": "text", "text": caption})
    text = processor.apply_chat_template(
        [{"role": "user", "content": content}], tokenize=False, add_generation_prompt=True
    )
    default_min = int(processor.image_processor.size["shortest_edge"])
    images = [image if image.mode == "RGB" else image.convert("RGB") for image in references]
    # A fresh dict per call: the processor pops the kwargs it is given.
    size = {"shortest_edge": min(default_min, budget), "longest_edge": budget}
    inputs = processor(text=[text], images=images, return_tensors="pt", size=size)
    input_ids = inputs["input_ids"]
    real = int(input_ids.shape[1])
    if real > TEXT_MAX_TOKENS:
        raise ValueError(
            f"Запрос с референсами занял {real} токенов при пределе {TEXT_MAX_TOKENS}. "
            "Уменьшите число референсов или их предельный размер для энкодера."
        )
    pad_id = processor.tokenizer.pad_token_id
    if pad_id is None:
        raise ValueError(f"{_OWNER}: у токенизатора нет токена заполнения")
    ids, mask = right_pad(input_ids, length=padded_length(real), pad_id=int(pad_id))
    grid = inputs["image_grid_thw"]
    log.info(
        "%s: режим VL: %d референсов, лимит %d пикселей на референс, сетки %s, %d токенов (текст %d)",
        _OWNER,
        n_refs,
        budget,
        grid.tolist(),
        real,
        text_only,
    )
    return TokenBatch(
        input_ids=ids,
        attention_mask=mask,
        real_length=real,
        pixel_values=inputs["pixel_values"],
        image_grid_thw=grid,
    )


# =====================================================================
#  Forward
# =====================================================================
def _dit_ids(length: int, device: torch.device) -> torch.Tensor:
    """DiT text ids `[L, 4]` = `(0, 0, 0, i)` (reference)."""
    positions = torch.arange(length, device=device)
    zeros = torch.zeros_like(positions)
    return torch.stack([zeros, zeros, zeros, positions], dim=-1)


@torch.inference_mode()
def run_text_encoder(handle: TextEncoderHandle, batch: TokenBatch) -> EncodedText:
    """Run the encoder on `batch` and stack `hidden_states[k]` for k in `TEXT_OUTPUT_LAYERS`.

    A text batch (no `pixel_values`) uses `reference_position_ids` in either
    mode. A VL batch needs a VL_REFS handle and runs with `position_ids=None`
    so the model computes its mRoPE ids over the image tokens. Every row is
    returned, padding included.

    # Raises
    `ValueError` for a VL batch on a TEXT handle, or a sequence length that is
    not a positive multiple of `TEXT_PAD_MULTIPLE` within `TEXT_MAX_TOKENS`.
    """
    length = int(batch.input_ids.shape[1])
    if length < 1 or length % TEXT_PAD_MULTIPLE or length > TEXT_MAX_TOKENS:
        raise ValueError(
            f"Длина входа энкодера {length} должна быть кратна {TEXT_PAD_MULTIPLE} и не больше {TEXT_MAX_TOKENS}"
        )
    device = handle.device
    input_ids = batch.input_ids.to(device)
    mask = batch.attention_mask.to(device)
    if batch.pixel_values is None:
        output = handle.model(
            input_ids=input_ids,
            attention_mask=mask,
            position_ids=reference_position_ids(mask),
            output_hidden_states=True,
            use_cache=False,
        )
    else:
        if handle.mode is not EncoderMode.VL_REFS:
            raise ValueError(f"{_OWNER}: изображения переданы энкодеру, загруженному без зрительной части")
        if batch.image_grid_thw is None:
            raise ValueError(f"{_OWNER}: у пакета с изображениями нет image_grid_thw")
        output = handle.model(
            input_ids=input_ids,
            attention_mask=mask,
            pixel_values=batch.pixel_values.to(device),
            image_grid_thw=batch.image_grid_thw.to(device),
            output_hidden_states=True,
            use_cache=False,
        )
    hidden_states = output.hidden_states
    # embedding output + one state per kept layer; anything else means the
    # recording contract of transformers changed under us.
    if len(hidden_states) != handle.keep_layers + 1:
        raise ValueError(
            f"{_OWNER}: энкодер вернул {len(hidden_states)} скрытых состояний, ожидалось {handle.keep_layers + 1}"
        )
    stacked = torch.stack([hidden_states[k] for k in TEXT_OUTPUT_LAYERS], dim=1)  # (1, 8, L, hidden)
    tokens = stacked.permute(0, 2, 1, 3).reshape(length, -1).to(torch.bfloat16)
    return EncodedText(tokens=tokens, ids=_dit_ids(length, device), real_length=batch.real_length)


def encode_text(handle: TextEncoderHandle, caption: str) -> EncodedText:
    """Text-only encoding of `caption` (the reference path); works on either mode's handle."""
    return run_text_encoder(handle, tokenize_caption(handle.processor, caption))


def encode_with_references(
    handle: TextEncoderHandle,
    caption: str,
    references: Sequence[Image],
    *,
    vl_ref_max_pixels: int,
) -> EncodedText:
    """VL_REFS encoding: `caption` with `references` shown to the vision tower.

    # Raises
    `ValueError` for a TEXT handle, and everything `prepare_reference_batch`
    raises.
    """
    if handle.mode is not EncoderMode.VL_REFS:
        raise ValueError(f"{_OWNER}: режим с референсами требует энкодер, загруженный в режиме VL")
    batch = prepare_reference_batch(
        handle.processor, caption, references, vl_ref_max_pixels=vl_ref_max_pixels
    )
    return run_text_encoder(handle, batch)
