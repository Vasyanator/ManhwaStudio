"""
File: modules/ai_backend/inpaint/flux3_klein/params.py

Purpose:
Typed run options and request shaping for FLUX.3 klein: the option enums, the
per-load `Flux3Options`, the per-call `Flux3Request`, and the one place a request
is validated and given its defaults (`normalize_request`).

Key declarations:
- `EncoderMode`, `Fp8Compute`, `AeDtype` (and `VramStep` / `RamStep` / `Steps`,
  re-used from the large-model core)
- `Flux3Options`, `Flux3Request`, `NormalizedRequest`
- `normalize_request()`, `default_seed()`
- `MIN_REFERENCE_PIXELS` (lower bound of `Flux3Options.reference_max_pixels`)

Notes:
- Torch-free; PIL appears only as a type annotation.
- The size bounds (64..4096, multiples of 16) and the default seed rule
  (sha256 of the caption mod 2**32) mirror black-forest-labs/flux-3-image-klein
  (commit e53f4ea, Apache-2.0, see `LICENSE` in this directory): `pipeline.py`
  `MIN_SIDE`/`MAX_SIDE`, `autoencoder.py` `DOWNSAMPLING_RATIO`, `generate.py`.
- A missing width/height stays `None` here: the default size depends on the
  references and is decided by the packing step.
"""

from __future__ import annotations

import hashlib
import logging
from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import TYPE_CHECKING

from ...runtime.large_models.tiers import RamStep, Steps, VramStep
from .config import MAX_REFERENCES, SamplerConfig, WeightFormat

if TYPE_CHECKING:
    from PIL.Image import Image

log = logging.getLogger(__name__)

#: Output side bounds in pixels (reference `pipeline.py`).
MIN_SIDE = 64
MAX_SIDE = 4096

#: Smallest `Flux3Options.reference_max_pixels`: the smallest single-reference
#: canvas the side bounds allow (`MIN_SIDE` x `MIN_SIDE`).
MIN_REFERENCE_PIXELS = MIN_SIDE * MIN_SIDE

#: Output sides must be multiples of the autoencoder's pixels-per-latent-token
#: ratio (reference `autoencoder.py` `DOWNSAMPLING_RATIO`).
SIDE_MULTIPLE = 16

#: Accepted Euler step range.
MIN_STEPS = 1
MAX_STEPS = 64

#: Seeds are unsigned 64-bit, the range `torch.Generator.manual_seed` accepts.
MAX_SEED = 2**64 - 1


class EncoderMode(Enum):
    """How the caption is encoded: text only (the reference), or Qwen3-VL with the
    reference images also shown to the encoder (experimental)."""

    TEXT = "text"
    VL_REFS = "vl_refs"


class Fp8Compute(Enum):
    """fp8r matmul path: probe and choose (`AUTO`), rowwise `_scaled_mm`, or
    per-forward dequantization to bf16."""

    AUTO = "auto"
    SCALED_MM = "scaled_mm"
    DEQUANT = "dequant"


class AeDtype(Enum):
    """Autoencoder run dtype; float32 is the reference."""

    FLOAT32 = "float32"
    BFLOAT16 = "bfloat16"


@dataclass(frozen=True)
class Flux3Options:
    """Per-load options: which package and weight format, and how the components run.

    `variant` is the user's weight FORMAT choice (bf16 / fp8r / nvfp4); the
    large-model manager never changes it (a format that does not fit is
    refused with the formats that would). `vram_step` («Видеопамять») and
    `ram_step` («Оперативная память») are the user's two resource axes (`AUTO`
    = the first step that fits on that axis); where the DiT, the AE and the
    text encoder live and run during a run follows from the pair (`steps`),
    never from a per-component option. Neither is part of the residency key. `vl_ref_max_pixels` caps each reference image shown
    to the encoder in `VL_REFS` mode; `device` `None` means the backend's
    selected device. `reference_max_pixels` lowers the pixel cap of every
    prepared reference (aspect kept, sides on the 16 grid, never upscaled)
    BEFORE the AE and, in `VL_REFS` mode, the encoder see it: fewer reference
    tokens, a shorter denoise sequence. `None` keeps the reference caps (4 MP
    single, 1 MP each of several) and so the reference's output; not part of
    the residency key.

    # Raises
    `ValueError` (construction) when `variant` is not a `WeightFormat`,
    `vram_step` not a `VramStep`, `ram_step` not a `RamStep`, `vl_ref_max_pixels` not a positive int, or
    `reference_max_pixels` neither `None` nor an int of at least
    `MIN_REFERENCE_PIXELS`.
    """

    model_root: Path
    variant: WeightFormat
    encoder_mode: EncoderMode = EncoderMode.TEXT
    fp8_compute: Fp8Compute = Fp8Compute.AUTO
    ae_dtype: AeDtype = AeDtype.FLOAT32
    vl_ref_max_pixels: int = 512 * 512
    device: str | None = None
    reference_max_pixels: int | None = None
    vram_step: VramStep = VramStep.AUTO
    ram_step: RamStep = RamStep.AUTO

    def __post_init__(self) -> None:
        if not isinstance(self.variant, WeightFormat):
            raise ValueError(f"variant должен быть форматом весов ({[f.value for f in WeightFormat]}), а не {self.variant!r}")
        if not isinstance(self.vram_step, VramStep):
            raise ValueError(f"vram_step должен быть шагом видеопамяти ({[s.value for s in VramStep]}), а не {self.vram_step!r}")
        if not isinstance(self.ram_step, RamStep):
            raise ValueError(f"ram_step должен быть шагом оперативной памяти ({[s.value for s in RamStep]}), а не {self.ram_step!r}")
        if not _is_int(self.vl_ref_max_pixels) or self.vl_ref_max_pixels <= 0:
            raise ValueError(f"vl_ref_max_pixels должен быть положительным целым, а не {self.vl_ref_max_pixels!r}")
        limit = self.reference_max_pixels
        if limit is not None and (not _is_int(limit) or limit < MIN_REFERENCE_PIXELS):
            raise ValueError(
                f"reference_max_pixels должен быть None или целым не меньше {MIN_REFERENCE_PIXELS}, а не {limit!r}"
            )

    @property
    def steps(self) -> Steps:
        """The requested position on both axes (may hold `AUTO` on either), as the manager takes it."""
        return Steps(self.vram_step, self.ram_step)


@dataclass(frozen=True)
class Flux3Request:
    """One generation as the caller states it; `None` fields take defaults in
    `normalize_request`."""

    caption: str
    references: tuple[Image, ...] = ()
    width: int | None = None
    height: int | None = None
    seed: int | None = None
    steps: int | None = None


@dataclass(frozen=True)
class NormalizedRequest:
    """A validated request: `seed` and `steps` are always set; `width`/`height`
    stay `None` when the caller left them to the reference-driven default."""

    caption: str
    references: tuple[Image, ...]
    width: int | None
    height: int | None
    seed: int
    steps: int


def _is_int(value: object) -> bool:
    """True for an `int` that is not a `bool`."""
    return isinstance(value, int) and not isinstance(value, bool)


def default_seed(caption: str) -> int:
    """The reference's deterministic seed for `caption`: sha256 of its UTF-8 bytes mod 2**32."""
    return int(hashlib.sha256(caption.encode("utf-8")).hexdigest(), 16) % 2**32


def _check_side(name: str, value: int | None) -> None:
    """Refuse a side that is not a multiple of `SIDE_MULTIPLE` within `MIN_SIDE..MAX_SIDE`."""
    if value is None:
        return
    if not _is_int(value) or not MIN_SIDE <= value <= MAX_SIDE or value % SIDE_MULTIPLE:
        raise ValueError(
            f"{name}={value!r}: сторона должна быть кратна {SIDE_MULTIPLE} "
            f"и лежать в пределах {MIN_SIDE}..{MAX_SIDE}"
        )


def normalize_request(request: Flux3Request, sampler: SamplerConfig) -> NormalizedRequest:
    """Validate `request` and fill in `seed` (`default_seed`) and `steps` (`sampler.steps`).

    # Raises
    `ValueError` for an empty (or whitespace-only) caption, more than
    `MAX_REFERENCES` references, a width/height that is not a multiple of 16 in
    64..4096, steps outside 1..64, or a seed outside 0..2**64-1.
    """
    if not isinstance(request.caption, str) or not request.caption.strip():
        raise ValueError("Пустой запрос: опишите, что нужно сгенерировать")
    references = tuple(request.references)
    if len(references) > MAX_REFERENCES:
        raise ValueError(f"Слишком много референсов: {len(references)}, максимум {MAX_REFERENCES}")
    _check_side("width", request.width)
    _check_side("height", request.height)

    steps = sampler.steps if request.steps is None else request.steps
    if not _is_int(steps) or not MIN_STEPS <= steps <= MAX_STEPS:
        raise ValueError(f"steps={steps!r}: число шагов должно быть в пределах {MIN_STEPS}..{MAX_STEPS}")

    seed = default_seed(request.caption) if request.seed is None else request.seed
    if not _is_int(seed) or not 0 <= seed <= MAX_SEED:
        raise ValueError(f"seed={seed!r}: сид должен быть целым в пределах 0..{MAX_SEED}")

    return NormalizedRequest(
        caption=request.caption,
        references=references,
        width=request.width,
        height=request.height,
        seed=seed,
        steps=steps,
    )
