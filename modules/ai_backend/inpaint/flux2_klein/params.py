"""
File: modules/ai_backend/inpaint/flux2_klein/params.py

Purpose:
Request shaping for the FLUX.2 klein service: normalization and validation of
the parameter dictionary that arrives over IPC, validation of the region size
the client asked to edit, the model key a normalized request maps to, and the
small coercion helpers everything else in the package uses.

Main responsibilities:
- `normalize_flux2_klein_params` - the single place a raw `params` dict becomes
  the typed, clamped, validated dictionary the rest of the package consumes;
- `validate_region_size` - the region contract (multiple, minimum side, pixel
  budget, aspect ratio); nothing is silently resized;
- `_lenient_paths` - the tolerant path view `status` uses before a run has
  ever happened;
- `encoder_on_cpu` - the «Энкодер на CPU» flag and its default (on);
- `derived_format` / `derived_encoder_format` / `_weight_formats` - the
  weight-format fields of the wire (protocol 21) for the transformer and the
  text encoder: the selected format id and the candidate files, validated,
  or the id derived from the container when the request names none;
- `text_encoder_dtype_name` - the ONE place that answers what dtype the
  text encoder runs in; it is deliberately NOT the request's
  `dtype`, which governs the transformer and the VAE alone;
- `prompt_library_fields` - the prompt-library fields of the wire (protocol
  23): the library id, the selected encoder source and the precision order,
  validated without touching any path (`.list` / `.export` / `.import` read
  them before a transformer is configured).

Key functions:
- normalize_flux2_klein_params()
- _whole_region_overrides()
- validate_region_size()
- effective_steps(), _lenient_paths(), encoder_on_cpu()
- derived_format(), derived_encoder_format(), _weight_formats()
- prompt_library_fields()
- text_encoder_dtype_name()

Notes:
Pure Python: no torch. Filesystem access is limited to `Path.exists` in
`_require_existing_path` and the 4-byte container sniff of `derived_format`
(`runtime.tensor_container.container_kind`, torch-free), so importing this
module costs nothing.
- Weight-format ids are OPAQUE here: they come from the client (the Rust
  format list is their one owner) and only their shape is checked; this
  package lists no quantization names. A GGUF file's quantization is read
  from the file itself.
"""

from __future__ import annotations

import logging
import re
from pathlib import Path
from collections.abc import Callable, Mapping
from typing import Any

from ...runtime.large_models.tiers import RamStep, VramStep
from ...runtime.tensor_container import container_kind

log = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Contract constants
# ---------------------------------------------------------------------------

#: Values of the optional `vram_step` / `ram_step` request fields (`tiers.VramStep`
#: / `tiers.RamStep` values, `auto` included). An absent or `null` axis is `auto`
#: (`memory.requested_steps`).
VALID_VRAM_STEPS = tuple(step.value for step in VramStep)
VALID_RAM_STEPS = tuple(step.value for step in RamStep)

#: Retired request fields -> `(the protocol that retired it, what replaced it)`.
#: A request carrying one is REFUSED by name (`_refuse_retired_fields`): a stale
#: client gets a clear error instead of a silently ignored choice. Protocol 14
#: retired the placement wire; protocol 17 moved the region post-processing
#: (latent-mask dilation, feather, colour match) to the client, which now sends
#: the latent mask already dilated and post-processes the raw answer itself.
RETIRED_FIELDS: dict[str, tuple[int, str]] = {
    "placement": (14, "передавайте «vram_step» и «ram_step»"),
    "profile": (14, "передавайте «vram_step» и «ram_step»"),
    "unload_transformer_before_vae": (14, "где трансформер находится во время декодирования, решает «vram_step»"),
    "unload_text_encoder_after_encode": (14, "остаётся ли текстовый энкодер между запусками, решает «ram_step»"),
    "low_cpu_mem_usage": (14, "загрузка всегда идёт тензор за тензором; уберите поле"),
    "vae_slicing": (14, "на декодирование одной области оно никогда не влияло; уберите поле"),
    "mask_dilate_px": (17, "присылайте маску латентов уже расширенной; уберите поле"),
    "mask_feather_px": (17, "растушёвку делает клиент над сырым ответом; уберите поле"),
    "color_match": (17, "подгонку цвета делает клиент над сырым ответом; уберите поле"),
    "color_match_local_pct": (17, "подгонку цвета делает клиент над сырым ответом; уберите поле"),
    "color_match_scale_px": (17, "подгонку цвета делает клиент над сырым ответом; уберите поле"),
}

#: Compute dtypes offered to the user. Both are 2 bytes per parameter. Since the
#: encoder dtype was fixed (`text_encoder_dtype_name`) this governs the
#: TRANSFORMER AND THE VAE only.
VALID_DTYPES = ("bfloat16", "float16")

#: The dtype the text encoder always runs in, whatever `dtype` the request asks
#: for. See `text_encoder_dtype_name` for why it is not a choice.
TEXT_ENCODER_DTYPE = "bfloat16"

#: The pipeline crops (never pads) its input to a multiple of
#: `vae_scale_factor * 2`; for `AutoencoderKLFlux2` that is 16.
REGION_SIZE_MULTIPLE = 16

#: Minimum side we accept. The pipeline itself needs at least 64 px per side;
#: below 128 px the 9B transformer has too little context to be useful.
MIN_REGION_SIDE = 128
PIPELINE_MIN_REGION_SIDE = 64

#: The pipeline downscales anything larger than 1 MP before cropping, which would
#: silently change the region size. We refuse instead.
MAX_REGION_PIXELS = 1024 * 1024

#: Extreme aspect ratios collapse one latent axis to a handful of tokens.
MAX_REGION_ASPECT_RATIO = 8.0

#: The weight-format id derived for a safetensors file or a diffusers folder
#: when the request names no format (protocol 21).
SAFETENSORS_FORMAT = "bf16"

#: The weight-format id derived for a GGUF file when the request names no format.
GGUF_FORMAT = "gguf"

#: The shape every weight-format id on the wire must have; the ids themselves
#: are opaque to the backend (module notes).
WEIGHT_FORMAT_ID_PATTERN = re.compile(r"^[a-z0-9_]{1,32}$")

#: Shape of `prompt_library` (a library id: a directory name, Rust
#: `Flux2Variant::prompt_library_id`) and of `text_encoder_source` (protocol 23).
PROMPT_LIBRARY_ID_PATTERN = re.compile(r"^[a-z0-9_]{1,40}$")
ENCODER_SOURCE_PATTERN = re.compile(r"^[a-z0-9_]{1,32}$")


# =====================================================================
#  The text encoder's dtype
# =====================================================================
def text_encoder_dtype_name() -> str:
    """THE single answer to "what dtype does the text encoder use".

    Always `bfloat16`, whatever `normalized["dtype"]` says. The encoder runs on
    the HOST CPU whenever the step pair (or the free device memory) puts it there,
    and x86 has no native float16 arithmetic: a fp16
    forward is emulated element by element, while bfloat16 lowers onto the same
    fp32 paths as everything else. A sibling project measured 59 s against 2.4 s
    per encode on a Zen host for exactly this pair. bfloat16 is also the dtype
    the shipped klein checkpoints declare for their encoder, so this is the
    cheapest AND the most faithful choice at once.

    **The scope of that argument is x86**, which is what this project targets
    (`x86_64-unknown-linux-gnu`, `x86_64-pc-windows-gnu`), and it is a statement
    about the HOST's CPU, not about the GPU vendor. When the encoder runs on the
    accelerator instead (a step pair that keeps it there beside the pipeline),
    bfloat16 is a native format on both CUDA and ROCm, so the same constant is
    right there too. On an ARM host the premise would be the other way round — NEON
    has native fp16 while bf16 needs FEAT_BF16 — so if this service is ever
    supported there, re-measure before assuming this constant still points the
    right way. It is deliberately NOT branched on the architecture today:
    nobody has measured that case, and a speculative branch would be a guess
    wearing the clothes of a measurement.

    `normalized["dtype"]` keeps governing the transformer and the VAE, which run
    on the accelerator where float16 is a real format — that is why `float16`
    stays in `VALID_DTYPES`.

    Everything that describes an EMBEDDING rather than a run reports this value:
    `_encoder_key`, the library spec (`prompt_cache.flux2_library_spec`) and
    the `.msprompt` metadata. A v1 `.msprompt` written earlier under `float16`
    is therefore never migrated (`prompt_cache.classify_v1_entry`): its
    embedding really did come from a different encoder precision.
    """
    return TEXT_ENCODER_DTYPE


# =====================================================================
#  Parameter normalization
# =====================================================================
def normalize_flux2_klein_params(params: dict[str, Any] | None) -> dict[str, Any]:
    """Validate and clamp the request params into a fully populated dict.

    Out-of-range numbers are clamped; an unknown step id or `dtype`, a missing
    transformer/VAE path, or one that does not exist on disk is an error, because
    every one of them would otherwise surface much later as an unreadable
    failure inside a loader.

    **`text_encoder_path` is the one OPTIONAL path**, and its absence is
    deliberately not an error here. A `.msprompt` file carries a finished
    embedding, and the four denoising steps plus the VAE decode never look at the
    encoder — so a machine where the 16 GB Qwen3 was never downloaded can still
    generate from a cached prompt. The encoder becomes REQUIRED at the moment a
    prompt actually has to be encoded, and that is where the refusal lives
    (`require_text_encoder`), naming both ways out. An empty path and a path that
    is not on disk mean the same thing — "no local encoder" — because the second
    is what a settings file carried over from another machine looks like;
    `status` still reports the path and its `exists: false` flag, so nothing
    about a mistyped path is hidden.

    **Weight format** (protocol 21): `transformer_format` is the selected
    format id and `transformer_formats` the candidate files `[{id, path}]`,
    best first (`_weight_formats`); when both are absent the id is derived
    from the container (`derived_format`) and `transformer_formats` is `()`.
    The normalized `transformer_formats` is a tuple of `(id, path)` pairs.
    `text_encoder_format` / `text_encoder_formats` follow the same contract
    for the encoder; an absent or missing encoder derives `SAFETENSORS_FORMAT`
    (`derived_encoder_format`), since nothing is loaded from it.

    **Prompt library** (protocol 23): `prompt_library`, `text_encoder_source`
    and `prompt_cache_precision_order` (`prompt_library_fields`).

    `whole_region=True` is a MODE, not a hint, and it settles
    `text_attention_in_mask` -> False on the caller's behalf; see
    `_whole_region_overrides` for why it is not meaningful there. The mask is
    the LATENT mask as sent (the client dilates it), so no mask morphology is
    configured here.

    Returns a dict with every key of the wire contract present.

    # Raises
    `ValueError` when the transformer or VAE path is empty or absent, when an
    enum value is not one of `VALID_DTYPES` / `VALID_VRAM_STEPS` /
    `VALID_RAM_STEPS`, when a weight-format field breaks its contract
    (`_weight_formats`), when a prompt-library field breaks its contract
    (`prompt_library_fields`), or when a retired field (`RETIRED_FIELDS`) is
    present, naming it and its replacement.
    """
    merged: dict[str, Any] = {}
    if isinstance(params, dict):
        merged.update(params)

    _refuse_retired_fields(merged)
    text_encoder_path = str(merged.get("text_encoder_path") or "").strip()
    transformer_path = _require_existing_path(merged.get("transformer_path"), "transformer_path")
    vae_path = _require_existing_path(merged.get("vae_path"), "vae_path")
    transformer_format, transformer_formats = _weight_formats(
        merged, format_field="transformer_format", formats_field="transformer_formats", path=transformer_path, path_field="transformer_path"
    )
    text_encoder_format, text_encoder_formats = _weight_formats(
        merged,
        format_field="text_encoder_format",
        formats_field="text_encoder_formats",
        path=text_encoder_path,
        path_field="text_encoder_path",
        derive=derived_encoder_format,
    )

    vram_step = _optional_choice(merged.get("vram_step"), VALID_VRAM_STEPS, "шаг «Видеопамять»")
    ram_step = _optional_choice(merged.get("ram_step"), VALID_RAM_STEPS, "шаг «Оперативная память»")

    dtype = str(merged.get("dtype", "bfloat16") or "").strip()
    if dtype not in VALID_DTYPES:
        raise ValueError(
            f"Неизвестный тип данных FLUX.2 klein: {dtype!r}. "
            f"Допустимые значения: {', '.join(VALID_DTYPES)}"
        )

    whole_region = _to_bool(merged.get("whole_region"), False)
    prompt_library, text_encoder_source, precision_order = prompt_library_fields(merged)

    normalized: dict[str, Any] = {
        "text_encoder_path": text_encoder_path,
        "transformer_path": transformer_path,
        # The selected weight format and its alternatives (`_weight_formats`);
        # `transformer_path` is always the selected format's file.
        "transformer_format": transformer_format,
        "transformer_formats": transformer_formats,
        # The same pair for the text encoder (`text_encoder_path` is always
        # the selected format's folder or GGUF file).
        "text_encoder_format": text_encoder_format,
        "text_encoder_formats": text_encoder_formats,
        "vae_path": vae_path,
        "prompt": str(merged.get("prompt", "") or "").strip(),
        "steps": _clamp_int(merged.get("steps"), default=4, low=1, high=50),
        "guidance_scale": _clamp_float(
            merged.get("guidance_scale"), default=1.0, low=1.0, high=10.0
        ),
        "strength": _clamp_float(merged.get("strength"), default=1.0, low=0.25, high=1.0),
        "seed": _to_optional_int(merged.get("seed")),
        # The manager's step pair (`None` = not sent = «Авто» on that axis,
        # `memory.requested_steps`).
        "vram_step": vram_step,
        "ram_step": ram_step,
        "dtype": dtype,
        "vae_tiling": _to_bool(merged.get("vae_tiling"), True),
        # Weight-only fp8 for the encoder's linear layers: a real quality/memory
        # trade, never made on the user's behalf, so the default is False. It
        # shrinks the RESIDENT encoder, not the load peak (the bf16 weights exist
        # before they are quantized), so it pays off when the RAM step keeps the
        # encoder between runs.
        "text_encoder_fp8": _to_bool(merged.get("text_encoder_fp8"), False),
        # «Энкодер на CPU» (default on): the text encoder runs on the CPU from
        # RAM whatever the VRAM step; the RAM step alone keeps it, releases it
        # after the run or re-reads it per run (`large_model.encoder_spec`'s
        # host pin, `runtime/large_models` "Host pin").
        "encoder_on_cpu": encoder_on_cpu(merged),
        "max_sequence_length": _clamp_int(
            merged.get("max_sequence_length"), default=512, low=64, high=512
        ),
        # "No mask" mode: the whole validated region may change. The client still
        # sends a mask — a solid one — so the request format does not fork; the
        # service checks that it really is solid (`_require_solid_mask`).
        "whole_region": whole_region,
        # Confine the prompt's attention to the masked image tokens (see
        # `attention.py`): off by default, because it narrows what the prompt can
        # see and a request written before the field existed must keep running
        # exactly as it did. A no-op under `whole_region`, which therefore forces
        # it off (`_whole_region_overrides`).
        "text_attention_in_mask": _to_bool(merged.get("text_attention_in_mask"), False),
        # The prompt library (protocol 23, `prompt_library_fields`): the
        # model's library id (`None` = memory only), the selected encoder
        # source (`""` = unknown, manual mode) and the precision ids, most
        # precise first (the selection rule's preference).
        "prompt_library": prompt_library,
        "text_encoder_source": text_encoder_source,
        "prompt_cache_precision_order": precision_order,
    }
    if whole_region:
        normalized.update(_whole_region_overrides(normalized))
    return normalized


def _whole_region_overrides(normalized: dict[str, Any]) -> dict[str, Any]:
    """The keys `whole_region` settles on the caller's behalf, with the reasons.

    - **`text_attention_in_mask` -> False.** It confines the prompt to the
      tokens inside the mask, and a solid mask has every token inside: the
      attention mask would block nothing and still cost `S²` bytes of VRAM and
      the flash kernel. Forcing it off keeps the run, the memory forecast and
      the reported effective value truthful.

    Logged whenever it actually overrides a value the caller asked for, so the
    override is visible in the backend log rather than inferred from the result.
    """
    overrides: dict[str, Any] = {
        "text_attention_in_mask": False,
    }
    contradicted = sorted(key for key, value in overrides.items() if normalized[key] != value)
    if contradicted:
        log.info(
            "FLUX.2 klein: режим «без маски» переопределяет %s — ограничивать внимание текста "
            "сплошной маской нечем (см. _whole_region_overrides).",
            ", ".join(contradicted),
        )
    return overrides


def validate_region_size(width: int, height: int) -> None:
    """Check that a region can be fed to the pipeline unchanged.

    The pipeline resizes anything above 1 MP and then CROPS to a multiple of 16
    (`pipeline_flux2_klein_inpaint.py`, "2. Preprocess image"), which would make
    the returned window a different size than the one the caller painted a mask
    for. Rather than resizing silently we require the caller to send a region
    that survives both steps untouched.

    # Raises
    `ValueError` with the concrete numbers when a side is not a multiple of
    `REGION_SIZE_MULTIPLE`, a side is below `MIN_REGION_SIDE`, the area exceeds
    `MAX_REGION_PIXELS`, or the aspect ratio exceeds `MAX_REGION_ASPECT_RATIO`.
    """
    width = int(width)
    height = int(height)
    if width <= 0 or height <= 0:
        raise ValueError(f"Некорректный размер области: {width}x{height}")
    if width % REGION_SIZE_MULTIPLE or height % REGION_SIZE_MULTIPLE:
        raise ValueError(
            f"Стороны области должны быть кратны {REGION_SIZE_MULTIPLE}: получено {width}x{height} "
            f"(ближайшие подходящие: {_floor_to(width)}x{_floor_to(height)})"
        )
    if width < MIN_REGION_SIDE or height < MIN_REGION_SIDE:
        raise ValueError(
            f"Каждая сторона области должна быть не меньше {MIN_REGION_SIDE} px "
            f"(пайплайну нужно минимум {PIPELINE_MIN_REGION_SIDE} px): получено {width}x{height}"
        )
    pixels = width * height
    if pixels > MAX_REGION_PIXELS:
        raise ValueError(
            f"Площадь области {pixels} px² превышает предел {MAX_REGION_PIXELS} px² "
            f"({width}x{height}); уменьшите выделение"
        )
    longer, shorter = (width, height) if width >= height else (height, width)
    if longer > shorter * MAX_REGION_ASPECT_RATIO:
        raise ValueError(
            f"Соотношение сторон области {longer}:{shorter} превышает предел "
            f"{int(MAX_REGION_ASPECT_RATIO)}:1"
        )


# =====================================================================
#  Small helpers
# =====================================================================
#: Request keys that name a component on disk.
_PATH_KEYS = ("text_encoder_path", "transformer_path", "vae_path")


def effective_steps(num_steps: int, strength: float) -> int:
    """Denoising steps the pipeline will actually run for `strength`.

    Mirrors `Flux2KleinInpaintPipeline.get_timesteps`, which drops
    `int(max(n - min(n * strength, n), 0))` steps from the front. At four steps
    the quantization is coarse: `strength` 0.8 still runs all four.
    """
    num_steps = int(num_steps)
    init = min(num_steps * float(strength), float(num_steps))
    dropped = int(max(num_steps - init, 0))
    return max(num_steps - dropped, 1)


def _lenient_paths(params: dict[str, Any] | None) -> dict[str, str]:
    """Extract the three component paths without validating them.

    `status` is called while the user is still picking files, so an absent or
    non-existent path must be reportable rather than fatal.
    """
    if not isinstance(params, dict):
        return {}
    out = {key: str(params.get(key) or "").strip() for key in _PATH_KEYS}
    return out if any(out.values()) else {}


def _require_existing_path(value: Any, field: str) -> str:
    """Non-empty path that exists on disk, or a `ValueError` naming the field."""
    raw = str(value or "").strip()
    if not raw:
        raise ValueError(f"Не задан параметр {field} для FLUX.2 klein")
    if not Path(raw).exists():
        raise ValueError(f"Путь {field} не найден: {raw}")
    return raw


def derived_format(path: str) -> str:
    """The weight-format id of an unnamed format: `GGUF_FORMAT` for a GGUF file, `SAFETENSORS_FORMAT` for a folder or a safetensors file.

    # Raises
    `ValueError` when a file cannot be read (`tensor_container.container_kind`).
    """
    source = Path(path)
    if source.is_dir():
        return SAFETENSORS_FORMAT
    return GGUF_FORMAT if container_kind(source) == "gguf" else SAFETENSORS_FORMAT


def _format_id(value: Any, field: str) -> str:
    """`value` when it is a weight-format id (`WEIGHT_FORMAT_ID_PATTERN`), else a `ValueError` naming `field`."""
    if not isinstance(value, str) or WEIGHT_FORMAT_ID_PATTERN.fullmatch(value) is None:
        raise ValueError(
            f"FLUX.2 klein: поле «{field}» должно быть идентификатором формата весов "
            f"(1–32 символа a-z, 0-9, «_»), получено {value!r}"
        )
    return value


def derived_encoder_format(path: str) -> str:
    """`derived_format` of a text-encoder path, which may be empty or absent (the encoder is optional).

    No encoder on disk means no encoder load, so the id is the plain
    `SAFETENSORS_FORMAT`; the encode refuses separately (`require_text_encoder`).

    # Raises
    As `derived_format` for an existing file that cannot be read.
    """
    if not path or not Path(path).exists():
        return SAFETENSORS_FORMAT
    return derived_format(path)


def _weight_formats(
    merged: Mapping[str, Any],
    *,
    format_field: str,
    formats_field: str,
    path: str,
    path_field: str,
    derive: Callable[[str], str] = derived_format,
) -> tuple[str, tuple[tuple[str, str], ...]]:
    """`(selected id, alternatives)` of one component's weight-format fields (protocol 21).

    `merged[format_field]` is the selected id (absent or `null`: derived from
    the file at `path` by `derive`, default `derived_format`). `merged[formats_field]`, when
    present, is a non-empty list of `{id, path}` objects, best first, with
    distinct ids and non-empty paths, and it must contain the selected id with
    `path` equal to `path` (the component's `path_field`, compared exactly as
    strings): the alternatives describe the request, they never redirect it.
    Absent -> `()`. The paths of the alternatives need not exist (a format
    whose file is missing is reported unavailable, never refused here).

    # Raises
    `ValueError` naming the offending field.
    """
    raw_id = merged.get(format_field)
    selected = derive(path) if raw_id is None else _format_id(raw_id, format_field)
    raw_list = merged.get(formats_field)
    if raw_list is None:
        return selected, ()
    if not isinstance(raw_list, list) or not raw_list:
        raise ValueError(f"FLUX.2 klein: поле «{formats_field}» должно быть непустым списком объектов {{id, path}}")
    entries: list[tuple[str, str]] = []
    for index, item in enumerate(raw_list):
        where = f"{formats_field}[{index}]"
        if not isinstance(item, Mapping) or set(item) != {"id", "path"}:
            raise ValueError(f"FLUX.2 klein: элемент «{where}» должен быть объектом ровно с полями id и path, получено {item!r}")
        format_id = _format_id(item["id"], f"{where}.id")
        candidate = item["path"]
        if not isinstance(candidate, str) or not candidate.strip():
            raise ValueError(f"FLUX.2 klein: поле «{where}.path» должно быть непустой строкой, получено {candidate!r}")
        if any(format_id == known for known, _ in entries):
            raise ValueError(f"FLUX.2 klein: формат {format_id!r} указан в «{formats_field}» дважды")
        entries.append((format_id, candidate))
    paths = dict(entries)
    if selected not in paths:
        raise ValueError(f"FLUX.2 klein: выбранного формата {selected!r} («{format_field}») нет в списке «{formats_field}»")
    if paths[selected] != path:
        raise ValueError(
            f"FLUX.2 klein: путь формата {selected!r} в «{formats_field}» ({paths[selected]}) "
            f"не совпадает с «{path_field}» ({path})"
        )
    return selected, tuple(entries)


def prompt_library_fields(params: Mapping[str, Any] | None) -> tuple[str | None, str, tuple[str, ...]]:
    """`(prompt_library, text_encoder_source, prompt_cache_precision_order)` of a raw params object (protocol 23).

    - `prompt_library`: the model's library id (`PROMPT_LIBRARY_ID_PATTERN`);
      absent or `null` = no library (memory only: the calibration tool, tests);
    - `text_encoder_source`: the selected encoder source id
      (`ENCODER_SOURCE_PATTERN`); absent, `null` or `""` = unknown (manual mode);
    - `prompt_cache_precision_order`: a list of distinct precision ids
      (`WEIGHT_FORMAT_ID_PATTERN`), most precise first; absent or `null` = `()`.
    All three are opaque to the backend beyond their shape (Rust owns them).
    Reads no path, so it is valid before any component is configured.

    # Raises
    `ValueError` naming the field that breaks its contract.
    """
    merged = params or {}
    raw_library = merged.get("prompt_library")
    library: str | None = None
    if raw_library is not None:
        if not isinstance(raw_library, str) or PROMPT_LIBRARY_ID_PATTERN.fullmatch(raw_library) is None:
            raise ValueError(f"FLUX.2 klein: поле «prompt_library» должно быть идентификатором библиотеки (1–40 символов a-z, 0-9, «_»), получено {raw_library!r}")
        library = raw_library
    raw_source = merged.get("text_encoder_source")
    source = ""
    if raw_source is not None and raw_source != "":
        if not isinstance(raw_source, str) or ENCODER_SOURCE_PATTERN.fullmatch(raw_source) is None:
            raise ValueError(f"FLUX.2 klein: поле «text_encoder_source» должно быть идентификатором источника (1–32 символа a-z, 0-9, «_»), получено {raw_source!r}")
        source = raw_source
    raw_order = merged.get("prompt_cache_precision_order")
    order: tuple[str, ...] = ()
    if raw_order is not None:
        if not isinstance(raw_order, list):
            raise ValueError(f"FLUX.2 klein: поле «prompt_cache_precision_order» должно быть списком идентификаторов, получено {raw_order!r}")
        order = tuple(_format_id(item, f"prompt_cache_precision_order[{index}]") for index, item in enumerate(raw_order))
        if len(set(order)) != len(order):
            raise ValueError(f"FLUX.2 klein: в «prompt_cache_precision_order» есть повторы: {list(order)!r}")
    return library, source, order


def _refuse_retired_fields(params: dict[str, Any]) -> None:
    """Refuse a request that carries any retired field (`RETIRED_FIELDS`), even `null`.

    # Raises
    `ValueError` naming the first retired field present (in `RETIRED_FIELDS`
    order), the protocol that retired it and what replaced it.
    """
    for field, (protocol, replacement) in RETIRED_FIELDS.items():
        if field in params:
            raise ValueError(f"FLUX.2 klein: поле «{field}» больше не поддерживается (протокол {protocol}): {replacement}.")


def _optional_choice(value: Any, allowed: tuple[str, ...], what: str) -> str | None:
    """`None` for an absent / empty value, else the stripped value when it is one of `allowed`.

    # Raises
    `ValueError` naming `what` and the allowed values for anything else.
    """
    if value is None or not str(value).strip():
        return None
    choice = str(value).strip()
    if choice not in allowed:
        raise ValueError(
            f"Неизвестный {what} FLUX.2 klein: {choice!r}. "
            f"Допустимые значения: {', '.join(allowed)}"
        )
    return choice


def _floor_to(value: int, multiple: int = REGION_SIZE_MULTIPLE) -> int:
    """Largest multiple of `multiple` not greater than `value` (at least one)."""
    return max(multiple, (int(value) // multiple) * multiple)


# =====================================================================
#  Coercion helpers
# =====================================================================
def _to_int(value: Any, default: int) -> int:
    try:
        if isinstance(value, bool):
            return default
        return int(value)
    except (TypeError, ValueError):
        return default


def _to_optional_int(value: Any) -> int | None:
    """`None` for a null/absent seed, an int otherwise."""
    if value is None or isinstance(value, bool):
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def encoder_on_cpu(params: Mapping[str, Any] | None) -> bool:
    """The «Энкодер на CPU» flag of a (raw or normalized) params object: `encoder_on_cpu`, absent = `True`.

    The one owner of the default: `normalize_flux2_klein_params` and the
    lenient `status` path both read it here.
    """
    return _to_bool((params or {}).get("encoder_on_cpu"), True)


def _to_bool(value: Any, default: bool) -> bool:
    if isinstance(value, bool):
        return value
    if value is None:
        return default
    if isinstance(value, (int, float)):
        return bool(value)
    text = str(value).strip().lower()
    if text in {"1", "true", "yes", "on"}:
        return True
    if text in {"0", "false", "no", "off"}:
        return False
    return default


def _clamp_int(value: Any, *, default: int, low: int, high: int) -> int:
    return max(low, min(high, _to_int(value, default)))


def _clamp_float(value: Any, *, default: float, low: float, high: float) -> float:
    try:
        out = default if isinstance(value, bool) else float(value)
    except (TypeError, ValueError):
        out = default
    return max(low, min(high, out))
