"""
File: modules/ai_backend/inpaint/flux3_klein/wire.py

Purpose:
The ONE mapping of the `inpaint.flux3_klein` wire `params` (also read by
`.status` and by `large_models.profiles` with `model: "flux3_klein"`) onto the
service's typed options, and the region-size rule of the cleaning tab's region editor. Torch-free.

Key declarations:
- `normalize_flux3_params(params, *, require_prompt)` -> `Flux3WireParams`
  (unknown keys and bad values refused by name, defaults filled in);
- `validate_region_size(width, height)` — the frame rule the Rust
  `FrameConstraints` of the FLUX.3 engine mirror (multiple of 16, side >= 128,
  area <= 4 MP, aspect <= 4:1), re-checked here; nothing is ever resized;
- `MAX_EXTRA_REFERENCES` — user references beside the region (the region is
  reference #0);
- `FIELDS` — every accepted key.

Wire fields (`PROTOCOL.md`, FLUX.3 klein section), all optional unless noted:
`source_root` (required: the package root), `format` (`bf16` / `fp8r` /
`nvfp4`, default `fp8r`), `encoder_mode` (`text` / `vl_refs`, default `text`),
`ae_dtype` (`float32` / `bfloat16`, default `float32`), `fp8_compute` (`auto` /
`scaled_mm` / `dequant`, default `auto`), `reference_max_pixels` (`null` or an
int >= `MIN_REFERENCE_PIXELS`), `prompt` (required non-empty for a run),
`steps` (1..64, `null` = the package default), `seed` (`null` = random, drawn by
`region.py`; else 0..2**64-1), `vram_step` / `ram_step` (`auto` default),
`encoder_on_cpu` (bool, default `true`: «Энкодер на CPU»), `whole_region`
(bool, default `false`). The region post-processing (feather, colour match) is
the client's since protocol 17: its former fields are unknown keys now and are
refused like any other.

Notes:
- Strict on types: a bool field must be a JSON bool, an int field a JSON
  integer (never a bool), an enum one of its values; a value out of range is a
  refusal, never a clamp — the client's settings validate the same ranges.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import Any, TypeVar

from ...runtime.large_models.tiers import RamStep, VramStep
from .config import MAX_REFERENCES, WeightFormat
from .params import MAX_SEED, MAX_STEPS, MIN_REFERENCE_PIXELS, MIN_STEPS, SIDE_MULTIPLE, AeDtype, EncoderMode, Flux3Options, Fp8Compute

#: User references a request may carry beside the region (the region is #0).
MAX_EXTRA_REFERENCES = MAX_REFERENCES - 1

#: Region frame rule (mirrored by the Rust FLUX.3 engine's `FrameConstraints`).
#: The area cap equals `packing.MATCH_MAX_PIXELS` (the single-reference canvas
#: cap): a region within it is reference #0 unchanged (`test_wire` guards the
#: equality; `packing` imports torch, this module must not).
REGION_SIDE_MULTIPLE = SIDE_MULTIPLE
REGION_MIN_SIDE = 128
REGION_MAX_PIXELS = 4 * 1024 * 1024
REGION_MAX_ASPECT = 4

#: The weight format a request without `format` runs.
DEFAULT_FORMAT = WeightFormat.FP8R

#: Every accepted `params` key.
FIELDS = frozenset({
    "source_root",
    "format",
    "encoder_mode",
    "ae_dtype",
    "fp8_compute",
    "reference_max_pixels",
    "prompt",
    "steps",
    "seed",
    "vram_step",
    "ram_step",
    "encoder_on_cpu",
    "whole_region",
})

_EnumT = TypeVar("_EnumT", bound=Enum)


@dataclass(frozen=True)
class Flux3WireParams:
    """One normalized `params` object.

    `options` are the service's run options (device `None`: the backend's
    selected device); `prompt` is stripped (may be empty unless a run asked
    for it); `steps` / `seed` `None` = the package default / a random seed;
    `whole_region` as sent (the region editor's "no mask" mode).
    """

    options: Flux3Options
    prompt: str
    steps: int | None
    seed: int | None
    whole_region: bool


def _enum(params: Mapping[str, Any], field: str, kind: type[_EnumT], default: _EnumT) -> _EnumT:
    value = params.get(field)
    if value is None:
        return default
    if isinstance(value, str):
        for member in kind:
            if member.value == value:
                return member
    raise ValueError(f"FLUX.3 klein: поле «{field}» должно быть одним из {[member.value for member in kind]}, получено {value!r}")


def _bool(params: Mapping[str, Any], field: str, default: bool) -> bool:
    value = params.get(field)
    if value is None:
        return default
    if not isinstance(value, bool):
        raise ValueError(f"FLUX.3 klein: поле «{field}» должно быть логическим значением, получено {value!r}")
    return value


def _int(params: Mapping[str, Any], field: str, *, low: int, high: int) -> int | None:
    value = params.get(field)
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
        raise ValueError(f"FLUX.3 klein: поле «{field}» должно быть целым числом в пределах {low}..{high}, получено {value!r}")
    return value


def normalize_flux3_params(params: Mapping[str, Any] | None, *, require_prompt: bool) -> Flux3WireParams:
    """Validate a wire `params` object and fill in its defaults (module notes).

    `require_prompt`: a generation needs a non-empty `prompt`; `.status` and
    `large_models.profiles` do not.

    # Raises
    `ValueError` naming the field for an unknown key, a missing or empty
    `source_root`, a value of the wrong type or out of range, an unknown enum
    value, or (with `require_prompt`) an empty prompt.
    """
    if params is None:
        params = {}
    if not isinstance(params, Mapping):
        raise ValueError("FLUX.3 klein: поле «params» должно быть объектом")
    unknown = sorted(set(params) - FIELDS)
    if unknown:
        raise ValueError(f"FLUX.3 klein: неизвестные поля запроса {unknown}; допустимы {sorted(FIELDS)}")
    root = params.get("source_root")
    if not isinstance(root, str) or not root.strip():
        raise ValueError("FLUX.3 klein: не указан каталог модели («source_root»)")
    prompt_raw = params.get("prompt", "")
    if prompt_raw is None:
        prompt_raw = ""
    if not isinstance(prompt_raw, str):
        raise ValueError(f"FLUX.3 klein: поле «prompt» должно быть строкой, получено {prompt_raw!r}")
    prompt = prompt_raw.strip()
    if require_prompt and not prompt:
        raise ValueError("Пустой запрос: опишите, что нужно сделать с областью")
    reference_max_pixels = _int(params, "reference_max_pixels", low=MIN_REFERENCE_PIXELS, high=REGION_MAX_PIXELS)
    whole_region = _bool(params, "whole_region", False)
    options = Flux3Options(
        model_root=Path(root.strip()),
        variant=_enum(params, "format", WeightFormat, DEFAULT_FORMAT),
        encoder_mode=_enum(params, "encoder_mode", EncoderMode, EncoderMode.TEXT),
        fp8_compute=_enum(params, "fp8_compute", Fp8Compute, Fp8Compute.AUTO),
        ae_dtype=_enum(params, "ae_dtype", AeDtype, AeDtype.FLOAT32),
        reference_max_pixels=reference_max_pixels,
        vram_step=_enum(params, "vram_step", VramStep, VramStep.AUTO),
        ram_step=_enum(params, "ram_step", RamStep, RamStep.AUTO),
        encoder_on_cpu=_bool(params, "encoder_on_cpu", True),
    )
    return Flux3WireParams(
        options=options,
        prompt=prompt,
        steps=_int(params, "steps", low=MIN_STEPS, high=MAX_STEPS),
        seed=_int(params, "seed", low=0, high=MAX_SEED),
        whole_region=whole_region,
    )


def validate_region_size(width: int, height: int) -> None:
    """Refuse a region the FLUX.3 region editor cannot take unchanged (module notes).

    # Raises
    `ValueError` with the numbers when a side is not a multiple of 16 or below
    128, the area exceeds 4 MP (4 194 304 pixels), or the aspect exceeds 4:1.
    """
    if width % REGION_SIDE_MULTIPLE or height % REGION_SIDE_MULTIPLE:
        raise ValueError(f"FLUX.3 klein: стороны области {width}x{height} должны быть кратны {REGION_SIDE_MULTIPLE}")
    if min(width, height) < REGION_MIN_SIDE:
        raise ValueError(f"FLUX.3 klein: область {width}x{height} меньше минимальной стороны {REGION_MIN_SIDE}")
    if width * height > REGION_MAX_PIXELS:
        raise ValueError(f"FLUX.3 klein: область {width}x{height} больше {REGION_MAX_PIXELS} пикселей (4 Мп)")
    if max(width, height) > REGION_MAX_ASPECT * min(width, height):
        raise ValueError(f"FLUX.3 klein: соотношение сторон области {width}x{height} больше {REGION_MAX_ASPECT}:1")
