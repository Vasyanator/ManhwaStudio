"""
File: modules/ai_backend/inpaint/flux2_klein/attention.py

Purpose:
The "text attention only inside the mask" contract (`text_attention_in_mask`):
which transformer tokens the prompt may talk to, the additive attention mask
that enforces it, and what that mask costs in device memory.

Why it exists: the pipeline always conditions on a clean copy of the WHOLE
region, so an instruction like "make the cup red" can be grounded on a matching
object OUTSIDE the mask. The model edits it there, the per-step latent blend and
the composite throw that edit away, and the user sees nothing change under the
mask. Confining the prompt to the masked image tokens removes the first-order
path by which the prompt reaches anything outside. The marks reference is a
copy of the same region with marks drawn on it, so when it is spatially aligned
with the region its outside tokens are cut off from the prompt the same way;
otherwise it would hand the prompt a second, aligned path back to the very
object outside the mask.

Main responsibilities:
- `token_grid_inside` - the region mask on the 16 px transformer token grid
  (a token is inside when ANY of its pixels is set);
- `condition_tokens` - the token count of one condition image, refusing a size
  whose count would depend on a diffusers resize this module does not replicate;
- `require_text_attention_target` - the early, pre-load refusal of a mask that
  has no set pixel (the prompt would reach nothing);
- `plan_text_attention` / `TextAttentionLayout` - the joint sequence layout of one
  run, whether the reference follows the region's inside/outside grid, and the
  last-resort refusal of a mask with no inside token;
- `text_attention_mask_bytes` - THE device-memory arithmetic of the mask, shared
  by the run and `memory.forecast_memory`;
- `build_text_attention_mask` - the additive `(S, S)` mask itself (real torch).

Key structures:
- TextAttentionLayout

Key functions:
- token_grid_inside(), condition_tokens(), require_text_attention_target()
- plan_text_attention()
- text_attention_mask_bytes(), build_text_attention_mask()

Notes:
The joint sequence layout is diffusers 0.39's, verified in the installed code:
`[text L | noisy N | region clean copy N | reference R]` —
`Flux2KleinInpaintPipeline.__call__` concatenates the noisy latents before the
condition latents, and `prepare_image_latents` packs the condition images in list
order (region first, then the optional `image_reference`); the transformer puts
the text stream first in both the double- and the single-stream blocks. SDPA
refuses an `(S, S)` mask only when the real sequence LENGTH differs; a diffusers
change that reorders the stream at the same `S` (e.g. `[text | reference | image]`,
the order diffusers' KV-cache processors already use, or the reference before
the region — `R == N` here) would silently mis-apply the mask. The real guard is
`test_attention.RealTransformerLayoutTests`, which runs a tiny real
`Flux2Transformer2DModel` with this mask and asserts that the text reaches
exactly the inside tokens.

numpy is needed by the planning helpers; torch is imported lazily inside
`build_text_attention_mask` only, so planning and the memory arithmetic are
importable (and testable) without torch.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from .params import MAX_REGION_PIXELS, REGION_SIZE_MULTIPLE, VALID_DTYPES

if TYPE_CHECKING:
    import numpy as np

log = logging.getLogger(__name__)

#: Pixels per transformer token side: the klein VAE downsamples by 8 and the
#: pipeline packs 2x2 latent patches, which is the same 16 that every region
#: side must be a multiple of.
TOKEN_PIXELS = REGION_SIZE_MULTIPLE

#: Row alignment, in elements, of the buffer the mask is a view of. SDPA's
#: memory-efficient kernel copies an additive mask whose row length is not a
#: multiple of 8 into a padded buffer on EVERY attention call (measured on the
#: reference ROCm host: 281 MB instead of 136 MB extra peak at S = 8706), while a
#: view of an already-padded buffer is used in place with identical output.
MASK_ROW_ALIGNMENT = 8

#: Bytes per element of every dtype `VALID_DTYPES` allows for the transformer.
_DTYPE_BYTES = {"bfloat16": 2, "float16": 2}
if set(_DTYPE_BYTES) != set(VALID_DTYPES):  # pragma: no cover - import-time contract
    raise ImportError(
        f"attention._DTYPE_BYTES {sorted(_DTYPE_BYTES)} does not cover VALID_DTYPES "
        f"{sorted(VALID_DTYPES)}"
    )


@dataclass(frozen=True)
class TextAttentionLayout:
    """The joint sequence of one run, as the text-attention mask sees it.

    `inside` is the row-major `(N,)` bool vector of the region's token grid —
    the same `h * W_tok + w` order the pipeline packs its latents in. The noisy
    tokens and the region's clean copy share it.

    `reference_inside` is the `(R,)` grid the reference tokens follow, or `None`
    when they are fully open to the text. It is `inside` itself when the
    reference's token grid equals the region's: the marks reference is the
    region with marks drawn on it, so its token (i, j) shows the same pixels as
    the region's, and leaving its outside tokens open would give the prompt an
    aligned second path to the object outside the mask. It is `None` without a
    reference and for a reference of another size, where no spatial alignment
    can be assumed.
    """

    text_tokens: int
    image_tokens: int
    reference_tokens: int
    inside: Any
    reference_inside: Any | None = None

    @property
    def sequence_length(self) -> int:
        """`S = L + 2N + R`: text, noisy latents, the region's clean copy, reference."""
        return self.text_tokens + 2 * self.image_tokens + self.reference_tokens

    @property
    def inside_tokens(self) -> int:
        """How many of the region's `N` tokens the prompt may reach."""
        return int(self.inside.sum())

    def mask_bytes(self, dtype_name: str) -> int:
        """Device bytes of this run's mask; see `text_attention_mask_bytes`."""
        return text_attention_mask_bytes(
            self.text_tokens, self.image_tokens, self.reference_tokens, dtype_name
        )


def token_grid_inside(mask_u8: np.ndarray) -> np.ndarray:
    """The `(H/16, W/16)` bool grid of tokens that touch the mask.

    A token is inside when ANY of its 16x16 pixels is non-zero. That is a
    deliberate one-token dilation against the pipeline's own latent mask (a
    bilinear downsample, fractional at the edges): the text must reach every
    token the latent blend lets change, including the partially masked band.

    # Raises
    `ValueError` when the mask is not 2-D or a side is not a multiple of
    `TOKEN_PIXELS` — `validate_region_size` already guarantees both for a run.
    """
    import numpy as np

    if mask_u8.ndim != 2:
        raise ValueError(
            f"Маска для внимания текста должна быть двумерной, получено {mask_u8.shape}"
        )
    height, width = (int(side) for side in mask_u8.shape)
    if height % TOKEN_PIXELS or width % TOKEN_PIXELS:
        raise ValueError(
            f"Стороны маски {width}x{height} должны быть кратны {TOKEN_PIXELS} px "
            "для сетки токенов"
        )
    blocks = np.asarray(mask_u8).reshape(
        height // TOKEN_PIXELS, TOKEN_PIXELS, width // TOKEN_PIXELS, TOKEN_PIXELS
    )
    return np.ascontiguousarray((blocks > 0).any(axis=(1, 3)))


def condition_tokens(width: int, height: int) -> int:
    """Token count of one condition image of `width x height` pixels.

    The pipeline caps a condition image to 1 MP and then floors each side to a
    multiple of 16 before encoding it. This function does not replicate that
    resize: it accepts only sizes on which both steps are no-ops, so the count
    it returns is exactly what the pipeline will pack.

    # Raises
    `ValueError` for a non-positive side, a side that is not a multiple of
    `TOKEN_PIXELS`, or an area above `MAX_REGION_PIXELS`.
    """
    width = int(width)
    height = int(height)
    if width <= 0 or height <= 0:
        raise ValueError(f"Некорректный размер условного изображения: {width}x{height}")
    if width % TOKEN_PIXELS or height % TOKEN_PIXELS or width * height > MAX_REGION_PIXELS:
        raise ValueError(
            f"Условное изображение {width}x{height} пайплайн изменит в размере, поэтому число "
            f"его токенов неизвестно заранее (нужны стороны, кратные {TOKEN_PIXELS}, "
            f"и площадь не больше {MAX_REGION_PIXELS} px²)"
        )
    return (width // TOKEN_PIXELS) * (height // TOKEN_PIXELS)


#: The user-facing refusal of a mask with no inside token, shared by the early
#: pre-load check and the planner's last-resort check so both say the same thing.
_EMPTY_MASK_MESSAGE = (
    "Внимание текста только внутри маски: в маске нет ни одного токена — промпт ни на что "
    "не повлиял бы. Нарисуйте маску или отключите этот режим."
)


def require_text_attention_target(mask_u8: np.ndarray) -> None:
    """Refuse, before anything loads, a mask that would leave the prompt nothing.

    `mask_u8` is the binarized wire mask (`imaging._decode_mask`). Checking it
    instead of the dilated mask is exact: dilation only grows a mask, so the
    dilated mask has a set pixel iff this one does, and any set pixel puts its
    token inside (`token_grid_inside`). This is what makes the planner's own
    refusal unreachable for a request that passed here; it exists so the user
    learns it in milliseconds, not after the transformer load and the prompt
    encode.

    # Raises
    `ValueError` with the user-facing message when no pixel is set; a warning
    with the mask's shape is logged first.
    """
    import numpy as np

    if np.any(mask_u8):
        return
    log.warning(
        "FLUX.2 klein: text_attention_in_mask refused before load: the mask has no set pixel "
        "(mask_shape=%s, possible cause: an empty mask sent with the flag on)",
        tuple(int(side) for side in mask_u8.shape),
    )
    raise ValueError(_EMPTY_MASK_MESSAGE)


def plan_text_attention(
    latent_mask_u8: np.ndarray,
    *,
    text_tokens: int,
    reference_hw: tuple[int, int] | None,
) -> TextAttentionLayout:
    """The layout of one run's joint sequence, with the inside tokens marked.

    `latent_mask_u8` must be the mask the pipeline receives as `mask_image` —
    the DILATED one — so the prompt reaches every token the latent blend may
    change. `text_tokens` is the real `prompt_embeds.shape[1]` (the tokenizer
    pads to `max_sequence_length`, and padding positions carry prompt content,
    so every one of them is a text token). `reference_hw` is the reference's
    `(height, width)`, or `None` without one; its token count is derived from
    that size (`condition_tokens`), never assumed equal to the region's. When
    that size equals the region's, the reference tokens follow the region's
    inside/outside grid (`TextAttentionLayout.reference_inside`); otherwise they
    stay fully open to the text and one info line says so (this function runs
    once per run).

    # Raises
    `ValueError` when no token is inside the mask: every text key would then be
    blocked for every image query and the prompt would reach nothing, which is a
    silent no-op edit. The service refuses that earlier, before any load
    (`require_text_attention_target`), so here it is a last-resort contract
    check. Also whatever `token_grid_inside` / `condition_tokens` raise.
    """
    grid = token_grid_inside(latent_mask_u8)
    grid_height, grid_width = (int(side) for side in grid.shape)
    image_tokens = condition_tokens(grid_width * TOKEN_PIXELS, grid_height * TOKEN_PIXELS)
    reference_tokens = (
        0 if reference_hw is None else condition_tokens(reference_hw[1], reference_hw[0])
    )
    inside = grid.reshape(-1)
    region_hw = (grid_height * TOKEN_PIXELS, grid_width * TOKEN_PIXELS)
    reference_inside = None
    if reference_hw is not None:
        if tuple(int(side) for side in reference_hw) == region_hw:
            reference_inside = inside
        else:
            log.info(
                "FLUX.2 klein: text_attention_in_mask: reference %dx%d is not the region's "
                "size %dx%d, so it is not spatially aligned with it; all %d reference tokens "
                "stay open to the text",
                int(reference_hw[1]),
                int(reference_hw[0]),
                region_hw[1],
                region_hw[0],
                reference_tokens,
            )
    layout = TextAttentionLayout(
        text_tokens=int(text_tokens),
        image_tokens=image_tokens,
        reference_tokens=reference_tokens,
        inside=inside,
        reference_inside=reference_inside,
    )
    if layout.text_tokens <= 0:
        raise ValueError(f"Некорректная длина текстовых эмбеддингов: {layout.text_tokens}")
    if layout.inside_tokens == 0:
        raise ValueError(_EMPTY_MASK_MESSAGE)
    return layout


def text_attention_mask_bytes(
    text_tokens: int, image_tokens: int, reference_tokens: int, dtype_name: str
) -> int:
    """Device bytes the mask occupies: `S` rows of `S` rounded up to `MASK_ROW_ALIGNMENT`.

    THE one formula for this cost: the run's log line and
    `memory.forecast_memory` both call it, so the guard and the allocation cannot
    disagree. The mask is allocated once per run and lives for the whole denoise;
    with the padded rows SDPA makes no per-call copy of it.

    # Raises
    `ValueError` for a dtype outside `VALID_DTYPES`.
    """
    if dtype_name not in _DTYPE_BYTES:
        raise ValueError(f"Неизвестный тип данных маски внимания: {dtype_name!r}")
    sequence = int(text_tokens) + 2 * int(image_tokens) + int(reference_tokens)
    return sequence * _padded_row_length(sequence) * _DTYPE_BYTES[dtype_name]


def build_text_attention_mask(
    layout: TextAttentionLayout, *, dtype_name: str, device: Any
) -> Any:
    """The additive `(S, S)` attention mask for `layout`, on `device`.

    Blocked pairs (query -> key), everything else is `0`:
    - an image query OUTSIDE the mask -> any text key;
    - any text query -> an image key OUTSIDE the mask;
    where "image" is the noisy tokens, the region's clean copy and — when
    `layout.reference_inside` is set (a region-aligned reference) — the
    reference tokens, each on the region's grid. Text <-> text, text <-> inside
    tokens, text <-> a non-aligned reference and every image <-> image pair
    stay open, so the image keeps its full context and only the prompt's reach
    is narrowed. Every row keeps at least one open key (a text
    row its text keys, an image row its image keys), so no softmax row is empty.

    The blocked value is `torch.finfo(dtype).min`, not `-inf`: it is the
    convention diffusers and transformers use for additive masks, and it fails
    soft — a row whose keys were ALL `-inf` would softmax to `NaN` and poison
    every later block, while a row of finite minima degrades to uniform weights.
    The layout above never produces such a row; this is defence in depth, not a
    path the run can reach.

    The result is a `(S, S)` view of an `(S, ceil8(S))` buffer (see
    `MASK_ROW_ALIGNMENT`) in the transformer's dtype — SDPA requires an additive
    mask to match the query dtype — and 2-D, which `diffusers`' native backend
    passes through to SDPA unchanged (it reinterprets a 2-D mask only when its
    first dimension equals the batch size, and `S` is far larger than the batch
    of one this service runs). It broadcasts over batch and heads.

    # Raises
    `ValueError` for a dtype outside `VALID_DTYPES`; whatever torch raises on
    allocation (an out-of-memory failure included).
    """
    import torch

    if dtype_name not in _DTYPE_BYTES:
        raise ValueError(f"Неизвестный тип данных маски внимания: {dtype_name!r}")
    dtype = getattr(torch, dtype_name)
    text = layout.text_tokens
    image = layout.image_tokens
    sequence = layout.sequence_length

    inside = torch.as_tensor(layout.inside, dtype=torch.bool, device=device)
    # Which image keys/queries the prompt may touch, in sequence order after the
    # text: the noisy tokens and the region's clean copy share the region grid;
    # an aligned reference follows it too, a non-aligned one stays open.
    image_open = torch.ones(sequence - text, dtype=torch.bool, device=device)
    image_open[:image] = inside
    image_open[image : 2 * image] = inside
    if layout.reference_inside is not None:
        image_open[2 * image :] = torch.as_tensor(
            layout.reference_inside, dtype=torch.bool, device=device
        )
    image_closed = ~image_open

    buffer = torch.zeros((sequence, _padded_row_length(sequence)), dtype=dtype, device=device)
    mask = buffer[:, :sequence]
    blocked = torch.finfo(dtype).min
    # Text queries must not read outside-mask image keys.
    mask[:text, text:].masked_fill_(image_closed.unsqueeze(0), blocked)
    # Outside-mask image queries must not read text keys.
    mask[text:, :text].masked_fill_(image_closed.unsqueeze(1), blocked)
    return mask


def _padded_row_length(sequence: int) -> int:
    """`sequence` rounded up to a multiple of `MASK_ROW_ALIGNMENT`."""
    return -(-int(sequence) // MASK_ROW_ALIGNMENT) * MASK_ROW_ALIGNMENT
