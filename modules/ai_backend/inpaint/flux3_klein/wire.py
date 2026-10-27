"""
File: modules/ai_backend/inpaint/flux3_klein/wire.py

Purpose:
The ONE mapping of the `inpaint.flux3_klein` wire `params` (also read by
`.status` and by `large_models.profiles` with `model: "flux3_klein"`) onto the
service's typed options, and the region-size rule of the cleaning tab's region editor. Torch-free.

Key declarations:
- `normalize_flux3_params(params, *, require_prompt)` -> `Flux3WireParams`
  (unknown keys and bad values refused by name, defaults filled in);
- `normalize_flux3_status_params(params)` -> `Flux3WireParams | None` — the
  same validation for `.status`, where only a MISSING `source_root` is no
  error (`None`): every supplied field is still checked first;
- `normalize_flux3_library_params(params)` -> `(Flux3PromptCacheRequest,
  Flux3WireParams | None)` — the same validation for the browse-only library
  methods (`.prompt_cache.list` / `.export` / `.import`), which need the
  library fields even without a `source_root`;
- `validate_region_size(width, height)` — the frame rule the Rust
  `FrameConstraints` of the FLUX.3 engine mirror (multiple of 16, side >= 128,
  area <= 4 MP, aspect <= 4:1), re-checked here; nothing is ever resized;
- `MAX_EXTRA_REFERENCES` — user references beside the region (the region is
  reference #0);
- `FIELDS` — every accepted key.

Wire fields (`PROTOCOL.md`, FLUX.3 klein section), all optional unless noted:
`source_root` (required: the package root), `dit_formats` (the DiT formats the
client offers, best first: `[{id, path?}]`; a native id `bf16` / `fp8r` /
`nvfp4` carries no `path`, any other id is a GGUF file with an absolute
`path`; absent = the three natives), `format` (one of the offered ids,
default `fp8r`), `text_encoder_format` (`bf16` = the package's
`text_encoder/` shards, default; else one of `text_encoder_formats`),
`text_encoder_formats` (the GGUF encoder candidates `[{id, path}]`, `path`
absolute, `bf16` reserved; absent = none; with `bf16` first they are
`Flux3Options.encoder_formats`, the encoder handle's format rows), `encoder_mode` (`text` / `vl_refs`, default `text`;
`vl_refs` needs the `bf16` encoder),
`ae_dtype` (`float32` / `bfloat16`, default `float32`), `fp8_compute` (`auto` /
`scaled_mm` / `dequant`, default `auto`), `reference_max_pixels` (`null` or an
int >= `MIN_REFERENCE_PIXELS`), `prompt` (required non-empty for a run),
`steps` (1..64, `null` = the package default), `seed` (`null` = random, drawn by
`region.py`; else 0..2**64-1), `vram_step` / `ram_step` (`auto` default),
`encoder_on_cpu` (bool, default `true`: «Энкодер на CPU»), `whole_region`
(bool, default `false`), `prompt_library` (`null` / absent = no library, the
memory cache only; else exactly `prompt_cache.LIBRARY_ID`), and
`prompt_cache_precision_order` (distinct precision ids, most precise first;
absent = none). The two prompt-library fields are read by
`runtime.prompt_cache_service.read_library_fields` (FLUX.3 has no
`text_encoder_source`: its source is always `stock`, D12). The region post-processing (feather, colour match) is
the client's since protocol 17: its former fields are unknown keys now and are
refused like any other.

Notes:
- Strict on types: a bool field must be a JSON bool, an int field a JSON
  integer (never a bool), an enum one of its values; a value out of range is a
  refusal, never a clamp — the client's settings validate the same ranges.
- Format ids are the client's (`config.FORMAT_ID_PATTERN`); this module knows
  only the three native DiT ids and `bf16` for the encoder. It never derives a
  GGUF path and never checks a file: presence is `.status`'s, the header the
  service's (before the admission).
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import Any, TypeVar

from ...runtime.large_models.tiers import RamStep, VramStep
from ...runtime.prompt_cache_service import read_library_fields
from .config import ENCODER_BF16, ENCODER_BF16_SOURCE, FORMAT_ID_PATTERN, MAX_REFERENCES, NATIVE_DIT_FORMATS, DitSource, EncoderSource, GgufDit, WeightFormat
from .params import MAX_SEED, MAX_STEPS, MIN_REFERENCE_PIXELS, MIN_STEPS, SIDE_MULTIPLE, AeDtype, EncoderMode, Flux3Options, Fp8Compute
from .prompt_cache import LIBRARY_ID, Flux3PromptCacheRequest

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

#: The refusal of a run / forecast without a package root.
_NO_ROOT_MESSAGE = "FLUX.3 klein: не указан каталог модели («source_root»)"

#: Every accepted `params` key.
FIELDS = frozenset({
    "source_root",
    "dit_formats",
    "format",
    "text_encoder_format",
    "text_encoder_formats",
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
    "prompt_library",
    "prompt_cache_precision_order",
})

_EnumT = TypeVar("_EnumT", bound=Enum)


@dataclass(frozen=True)
class Flux3WireParams:
    """One normalized `params` object.

    `options` are the service's run options (device `None`: the backend's
    selected device; `options.dit_formats` the offered DiT formats,
    `options.encoder` the selected encoder, `options.encoder_formats` every
    encoder candidate = `text_encoder_formats`); `prompt` is stripped (may be
    empty unless a run asked for it); `steps` / `seed` `None` = the package
    default / a random seed; `whole_region` as sent (the region editor's "no
    mask" mode); `text_encoder_formats` every encoder candidate, `bf16` first
    (what `.status` reports presence for); `prompt_cache` the prompt-library
    fields (`library` `None` = the memory cache only).
    """

    options: Flux3Options
    prompt: str
    steps: int | None
    seed: int | None
    whole_region: bool
    text_encoder_formats: tuple[EncoderSource, ...] = (ENCODER_BF16_SOURCE,)
    prompt_cache: Flux3PromptCacheRequest = Flux3PromptCacheRequest()


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


_NATIVE_IDS = {fmt.value: fmt for fmt in WeightFormat}

#: Keys of one `dit_formats` / `text_encoder_formats` entry.
_CANDIDATE_KEYS = frozenset({"id", "path"})


def _candidates(params: Mapping[str, Any], field: str) -> list[tuple[str, str | None]] | None:
    """`field` as `[(id, path or None)]` in the sent order, `None` when absent.

    Refuses (naming `field[i]` and the key) anything but a non-empty list of
    objects with a string `id` and an optional string-or-null `path`,
    unknown keys, and duplicate ids (named at the SECOND occurrence,
    `field[i].id`). Id and path meaning: the callers.
    """
    raw = params.get(field)
    if raw is None:
        return None
    if not isinstance(raw, list) or not raw:
        raise ValueError(f"FLUX.3 klein: поле «{field}» должно быть непустым списком объектов {{id, path}}, получено {raw!r}")
    found: list[tuple[str, str | None]] = []
    for index, entry in enumerate(raw):
        where = f"{field}[{index}]"
        if not isinstance(entry, Mapping):
            raise ValueError(f"FLUX.3 klein: элемент «{where}» должен быть объектом {{id, path}}, получено {entry!r}")
        unknown = sorted(set(entry) - _CANDIDATE_KEYS)
        if unknown:
            raise ValueError(f"FLUX.3 klein: неизвестные поля {unknown} в «{where}»; допустимы {sorted(_CANDIDATE_KEYS)}")
        format_id, path = entry.get("id"), entry.get("path")
        if not isinstance(format_id, str) or FORMAT_ID_PATTERN.match(format_id) is None:
            raise ValueError(f"FLUX.3 klein: поле «{where}.id» должно соответствовать {FORMAT_ID_PATTERN.pattern}, получено {format_id!r}")
        if path is not None and not isinstance(path, str):
            raise ValueError(f"FLUX.3 klein: поле «{where}.path» должно быть строкой, получено {path!r}")
        if any(format_id == seen for seen, _ in found):
            raise ValueError(f"FLUX.3 klein: поле «{where}.id»: идентификатор «{format_id}» повторяется в «{field}»")
        found.append((format_id, path))
    return found


def _absolute(path: str | None, where: str) -> Path:
    """`path` as an absolute `Path`, or a refusal naming `where` (missing, empty, relative)."""
    if path is None or not path.strip() or not Path(path.strip()).is_absolute():
        raise ValueError(f"FLUX.3 klein: поле «{where}» должно быть абсолютным путём к файлу GGUF, получено {path!r}")
    return Path(path.strip())


def _dit_formats(params: Mapping[str, Any]) -> tuple[DitSource, ...]:
    """The offered DiT formats (`dit_formats`, absent = `NATIVE_DIT_FORMATS`): natives without a path, GGUF ids with an absolute one."""
    raw = _candidates(params, "dit_formats")
    if raw is None:
        return NATIVE_DIT_FORMATS
    sources: list[DitSource] = []
    for index, (format_id, path) in enumerate(raw):
        native = _NATIVE_IDS.get(format_id)
        if native is not None:
            if path is not None:
                raise ValueError(f"FLUX.3 klein: формат «{format_id}» (dit_formats[{index}].path) — собственный формат пакета, путь для него не передаётся")
            sources.append(native)
        else:
            sources.append(GgufDit(format_id, _absolute(path, f"dit_formats[{index}].path")))
    return tuple(sources)


def _encoder_formats(params: Mapping[str, Any]) -> tuple[EncoderSource, ...]:
    """`bf16` plus the GGUF encoder candidates (`text_encoder_formats`: absolute paths, `bf16` reserved)."""
    raw = _candidates(params, "text_encoder_formats") or []
    sources = [ENCODER_BF16_SOURCE]
    for index, (format_id, path) in enumerate(raw):
        if format_id == ENCODER_BF16:
            raise ValueError(f"FLUX.3 klein: «{ENCODER_BF16}» (text_encoder_formats[{index}].id) — энкодер из папки text_encoder пакета, в списке GGUF-кандидатов его нет")
        sources.append(EncoderSource(format_id, _absolute(path, f"text_encoder_formats[{index}].path")))
    return tuple(sources)


_SourceT = TypeVar("_SourceT")


def _selected(params: Mapping[str, Any], field: str, offered: Mapping[str, _SourceT], default: str) -> _SourceT:
    """The source of `offered` (by id, in offer order) named by `params[field]` (absent: `default`), or a refusal naming `field`."""
    value = params.get(field)
    if value is None:
        value = default
    if isinstance(value, str) and value in offered:
        return offered[value]
    raise ValueError(f"FLUX.3 klein: поле «{field}» должно быть одним из предложенных форматов {list(offered)}, получено {value!r}")


def normalize_flux3_params(params: Mapping[str, Any] | None, *, require_prompt: bool) -> Flux3WireParams:
    """Validate a wire `params` object and fill in its defaults (module notes).

    `require_prompt`: a generation needs a non-empty `prompt`; `.status` and
    `large_models.profiles` do not.

    # Raises
    `ValueError` naming the field for an unknown key, a missing or empty
    `source_root`, a value of the wrong type or out of range, an unknown enum
    value, a `format` / `text_encoder_format` that is not offered, a malformed
    candidate list (`dit_formats` / `text_encoder_formats`: a bad id, a native
    id with a path, a GGUF id without an absolute one, a duplicate, `bf16`
    among the encoder candidates), `encoder_mode` `vl_refs` with a non-`bf16`
    encoder, a `prompt_library` other than `prompt_cache.LIBRARY_ID`, a
    malformed `prompt_cache_precision_order`, or (with `require_prompt`) an
    empty prompt.
    """
    _cache, wire = _normalize(params, require_prompt=require_prompt)
    if wire is None:
        raise ValueError(_NO_ROOT_MESSAGE)
    return wire


def normalize_flux3_status_params(params: Mapping[str, Any] | None) -> Flux3WireParams | None:
    """The `.status` reading of `params`: `None` exactly when `source_root` is absent, `null` or blank.

    Every SUPPLIED field is validated first, with the same rules and refusals
    as `normalize_flux3_params` (no prompt required), so a malformed request
    is never hidden behind the "no model directory" answer.

    # Raises
    `ValueError` for everything `normalize_flux3_params` refuses except a
    missing `source_root` (a non-string one is still refused).
    """
    return _normalize(params, require_prompt=False)[1]


def normalize_flux3_library_params(params: Mapping[str, Any] | None) -> tuple[Flux3PromptCacheRequest, Flux3WireParams | None]:
    """The library methods' reading of `params`: the prompt-library fields, and the params (`None` exactly when `source_root` is missing).

    Every supplied field is validated as by `normalize_flux3_status_params`;
    the library fields survive a missing root (a lenient browse then has no
    width filter and no current encoder).

    # Raises
    See `normalize_flux3_status_params`.
    """
    return _normalize(params, require_prompt=False)


def _prompt_cache_fields(params: Mapping[str, Any]) -> Flux3PromptCacheRequest:
    """`prompt_library` (only `prompt_cache.LIBRARY_ID`, or none) and `prompt_cache_precision_order`.

    # Raises
    `ValueError` naming the field (`read_library_fields`), or a library id
    that is not FLUX.3's.
    """
    library, _source, order = read_library_fields(params, owner="FLUX.3 klein")
    if library is not None and library != LIBRARY_ID:
        raise ValueError(f"FLUX.3 klein: поле «prompt_library» должно быть «{LIBRARY_ID}» (библиотека промптов FLUX.3 klein), получено {library!r}")
    return Flux3PromptCacheRequest(library=library, precision_order=order)


def _normalize(params: Mapping[str, Any] | None, *, require_prompt: bool) -> tuple[Flux3PromptCacheRequest, Flux3WireParams | None]:
    """Validate EVERY field of `params`, then build the result: the prompt-library fields, and the params (`None` when, and only when, `source_root` is missing).

    The root is checked for presence LAST, so the `.status` shortcut can never
    skip the validation of the other fields.

    # Raises
    See `normalize_flux3_params` (all but the missing root).
    """
    if params is None:
        params = {}
    if not isinstance(params, Mapping):
        raise ValueError("FLUX.3 klein: поле «params» должно быть объектом")
    unknown = sorted(set(params) - FIELDS)
    if unknown:
        raise ValueError(f"FLUX.3 klein: неизвестные поля запроса {unknown}; допустимы {sorted(FIELDS)}")
    root = params.get("source_root")
    if root is not None and not isinstance(root, str):
        raise ValueError(f"FLUX.3 klein: поле «source_root» должно быть строкой, получено {root!r}")
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
    dit_formats = _dit_formats(params)
    encoder_formats = _encoder_formats(params)
    encoder = _selected(params, "text_encoder_format", {source.id: source for source in encoder_formats}, ENCODER_BF16)
    encoder_mode = _enum(params, "encoder_mode", EncoderMode, EncoderMode.TEXT)
    if encoder.is_gguf and encoder_mode is EncoderMode.VL_REFS:
        # D6: a GGUF encoder is Qwen3-VL's text half only; the client sends
        # `text` while a GGUF encoder is selected, so this is a client bug.
        raise ValueError(
            f"FLUX.3 klein: поле «encoder_mode» = «{EncoderMode.VL_REFS.value}» требует энкодер «{ENCODER_BF16}», "
            f"а «text_encoder_format» = «{encoder.id}» (GGUF: только текстовый режим)"
        )
    variant = _selected(params, "format", {source.value: source for source in dit_formats}, DEFAULT_FORMAT.value)
    fp8_compute = _enum(params, "fp8_compute", Fp8Compute, Fp8Compute.AUTO)
    ae_dtype = _enum(params, "ae_dtype", AeDtype, AeDtype.FLOAT32)
    vram_step = _enum(params, "vram_step", VramStep, VramStep.AUTO)
    ram_step = _enum(params, "ram_step", RamStep, RamStep.AUTO)
    encoder_on_cpu = _bool(params, "encoder_on_cpu", True)
    steps = _int(params, "steps", low=MIN_STEPS, high=MAX_STEPS)
    seed = _int(params, "seed", low=0, high=MAX_SEED)
    prompt_cache = _prompt_cache_fields(params)
    if root is None or not root.strip():
        return prompt_cache, None
    options = Flux3Options(
        model_root=Path(root.strip()),
        variant=variant,
        dit_formats=dit_formats,
        encoder=encoder,
        encoder_formats=encoder_formats,
        encoder_mode=encoder_mode,
        fp8_compute=fp8_compute,
        ae_dtype=ae_dtype,
        reference_max_pixels=reference_max_pixels,
        vram_step=vram_step,
        ram_step=ram_step,
        encoder_on_cpu=encoder_on_cpu,
    )
    return prompt_cache, Flux3WireParams(
        options=options,
        prompt=prompt,
        steps=steps,
        seed=seed,
        whole_region=whole_region,
        text_encoder_formats=encoder_formats,
        prompt_cache=prompt_cache,
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
