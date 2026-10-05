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
- `_model_key` / `_lenient_paths` - the resident-model key and the tolerant path
  view `status` uses before a run has ever happened;
- `text_encoder_dtype_name` - the ONE place that answers what dtype the
  host-resident text encoder runs in; it is deliberately NOT the request's
  `dtype`, which governs the transformer and the VAE alone.

Key functions:
- normalize_flux2_klein_params()
- _whole_region_overrides()
- validate_region_size()
- _model_key(), effective_steps(), _lenient_paths()
- text_encoder_dtype_name()

Notes:
Pure Python: no torch, no filesystem access beyond `Path.exists` in
`_require_existing_path`, so importing this module costs nothing.
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any

log = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Contract constants
# ---------------------------------------------------------------------------

#: Weight placement modes for the TRANSFORMER AND THE VAE. `full_gpu` and
#: `encoder_cpu` both put them on the accelerator (they differ only in whether
#: `low_cpu_mem_usage` is offered alongside); the two offload modes hand placement
#: to accelerate. The text encoder is not placed by these at all any more — since
#: the load reorder it always encodes in host memory, because the transformer is
#: already on the card by the time it is read (`_encode_prompts_locked`).
VALID_PLACEMENTS = ("full_gpu", "encoder_cpu", "model_cpu_offload", "sequential_cpu_offload")

#: Compute dtypes offered to the user. Both are 2 bytes per parameter. Since the
#: encoder dtype was fixed (`text_encoder_dtype_name`) this governs the
#: TRANSFORMER AND THE VAE only.
VALID_DTYPES = ("bfloat16", "float16")

#: The dtype the text encoder always runs in, whatever `dtype` the request asks
#: for. See `text_encoder_dtype_name` for why it is not a choice.
TEXT_ENCODER_DTYPE = "bfloat16"

#: Placements that are meaningless without an accelerator.
_GPU_ONLY_PLACEMENTS = ("encoder_cpu", "model_cpu_offload", "sequential_cpu_offload")

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


# =====================================================================
#  The text encoder's dtype
# =====================================================================
def text_encoder_dtype_name() -> str:
    """THE single answer to "what dtype does the host-resident text encoder use".

    Always `bfloat16`, whatever `normalized["dtype"]` says, because the encoder
    always runs on the HOST (`_ensure_text_encoder_locked` pins
    `torch.device("cpu")`) and x86 has no native float16 arithmetic: a fp16
    forward is emulated element by element, while bfloat16 lowers onto the same
    fp32 paths as everything else. A sibling project measured 59 s against 2.4 s
    per encode on a Zen host for exactly this pair. bfloat16 is also the dtype
    the shipped klein checkpoints declare for their encoder, so this is the
    cheapest AND the most faithful choice at once.

    **The scope of that argument is x86**, which is what this project targets
    (`x86_64-unknown-linux-gnu`, `x86_64-pc-windows-gnu`), and it is a statement
    about the HOST's CPU, not about the GPU vendor: it holds identically on a
    CUDA machine and on a ROCm one, because the encoder never reaches either
    accelerator. On an ARM host the premise would be the other way round — NEON
    has native fp16 while bf16 needs FEAT_BF16 — so if this service is ever
    supported there, re-measure before assuming this constant still points the
    right way. It is deliberately NOT branched on the architecture today:
    nobody has measured that case, and a speculative branch would be a guess
    wearing the clothes of a measurement.

    `normalized["dtype"]` keeps governing the transformer and the VAE, which run
    on the accelerator where float16 is a real format — that is why `float16`
    stays in `VALID_DTYPES`.

    Everything that describes an EMBEDDING rather than a run reports this value:
    `_encoder_key`, `_prompt_cache_key` and the `.msprompt` metadata. A
    `.msprompt` written earlier under `float16` therefore no longer validates —
    correctly, because its embedding really did come from a different encoder
    precision — and `validate_prompt_file_metadata` says so by name.
    """
    return TEXT_ENCODER_DTYPE


# =====================================================================
#  Parameter normalization
# =====================================================================
def normalize_flux2_klein_params(params: dict[str, Any] | None) -> dict[str, Any]:
    """Validate and clamp the request params into a fully populated dict.

    Out-of-range numbers are clamped; an unknown `placement`/`dtype`, a missing
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

    `whole_region=True` is a MODE, not a hint, and it settles three other keys on
    the caller's behalf (`mask_dilate_px` -> 0, `color_match` -> False,
    `text_attention_in_mask` -> False); see `_whole_region_overrides` for why
    none is meaningful there.

    Returns a dict with every key of the wire contract present.

    # Raises
    `ValueError` when the transformer or VAE path is empty or absent, or when an
    enum value is not one of `VALID_PLACEMENTS` / `VALID_DTYPES`.
    """
    merged: dict[str, Any] = {}
    if isinstance(params, dict):
        merged.update(params)

    text_encoder_path = str(merged.get("text_encoder_path") or "").strip()
    transformer_path = _require_existing_path(merged.get("transformer_path"), "transformer_path")
    vae_path = _require_existing_path(merged.get("vae_path"), "vae_path")

    placement = str(merged.get("placement", "full_gpu") or "").strip()
    if placement not in VALID_PLACEMENTS:
        raise ValueError(
            f"Неизвестный режим размещения FLUX.2 klein: {placement!r}. "
            f"Допустимые значения: {', '.join(VALID_PLACEMENTS)}"
        )

    dtype = str(merged.get("dtype", "bfloat16") or "").strip()
    if dtype not in VALID_DTYPES:
        raise ValueError(
            f"Неизвестный тип данных FLUX.2 klein: {dtype!r}. "
            f"Допустимые значения: {', '.join(VALID_DTYPES)}"
        )

    whole_region = _to_bool(merged.get("whole_region"), False)

    normalized: dict[str, Any] = {
        "text_encoder_path": text_encoder_path,
        "transformer_path": transformer_path,
        "vae_path": vae_path,
        "prompt": str(merged.get("prompt", "") or "").strip(),
        "steps": _clamp_int(merged.get("steps"), default=4, low=1, high=50),
        "guidance_scale": _clamp_float(
            merged.get("guidance_scale"), default=1.0, low=1.0, high=10.0
        ),
        "strength": _clamp_float(merged.get("strength"), default=1.0, low=0.25, high=1.0),
        "seed": _to_optional_int(merged.get("seed")),
        "placement": placement,
        "dtype": dtype,
        "low_cpu_mem_usage": _to_bool(merged.get("low_cpu_mem_usage"), False),
        "vae_tiling": _to_bool(merged.get("vae_tiling"), True),
        "vae_slicing": _to_bool(merged.get("vae_slicing"), True),
        # The VAE decode peaks ON TOP of the resident transformer and is the most
        # common source of OOM here, so parking the transformer first is the
        # default everywhere except `full_gpu`, whose whole point is that
        # everything stays on the GPU.
        "unload_transformer_before_vae": _to_bool(
            merged.get("unload_transformer_before_vae"), placement != "full_gpu"
        ),
        # The text encoder now arrives LAST, into a host that the transformer has
        # already left for the accelerator, and it encodes there in every
        # placement. Keeping it resident therefore costs host memory that nothing
        # else in the run wants back, and buys an instant prompt change plus a
        # skipped 16 GB read from disk on every cache miss. Measured on this
        # project's reference host (see `inpaint/MODULE_README.md`), so the
        # default is now to KEEP it in every placement. A settings file written
        # before this field existed has no value for it, and that absence must
        # resolve to this default — which is what `_to_bool(None, default)` does.
        "unload_text_encoder_after_encode": _to_bool(
            merged.get("unload_text_encoder_after_encode"), False
        ),
        # Weight-only fp8 for the encoder's linear layers: a real quality/memory
        # trade, never made on the user's behalf, so the default is False in
        # every placement and in every preset. It shrinks the RESIDENT encoder,
        # not the load peak (the bf16 weights exist before they are quantized),
        # so it is only useful together with
        # `unload_text_encoder_after_encode=False`.
        "text_encoder_fp8": _to_bool(merged.get("text_encoder_fp8"), False),
        "mask_dilate_px": _clamp_int(merged.get("mask_dilate_px"), default=16, low=0, high=64),
        # 12 px, not 6: `_feather_mask_inwards` now ramps over exactly this many
        # pixels, where the old construction spread a nominal 6 over ~22. Measured
        # on a real page (384x384 region, 56 px blob mask, 4 steps, two prompts —
        # one strong edit, one text removal) as the excess Sobel gradient on the
        # mask contour over the same contour in the untouched original: 6 px
        # leaves +9.8% / +5.1%, 8 px +4.4% / +2.4%, 12 px +1.1% / +0.5%, 16 px
        # +0.3% / +0.1%. 12 is the knee: it removes ~90% of the visible seam while
        # keeping 81-84% of the edit, where 16 keeps only 75-79%.
        "mask_feather_px": _clamp_int(merged.get("mask_feather_px"), default=12, low=0, high=32),
        "color_match": _to_bool(merged.get("color_match"), True),
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
    }
    if whole_region:
        normalized.update(_whole_region_overrides(normalized))
    return normalized


def _whole_region_overrides(normalized: dict[str, Any]) -> dict[str, Any]:
    """The keys `whole_region` settles on the caller's behalf, with the reasons.

    Each would otherwise operate on an input it has no meaning for, and each
    is silent about it — which is exactly the failure mode this module refuses
    everywhere else:

    - **`mask_dilate_px` -> 0.** The dilate exists to give a thin painted mask a
      full latent cell of room; a mask that already covers the whole region has
      nothing left to grow into, and growing it would only push the latent mask
      past the region's own edge.
    - **`color_match` -> False.** `_match_color_outside_mask` takes its
      statistics from the pixels OUTSIDE the mask — the ring the model was not
      allowed to touch, and therefore the only place where the two images are
      supposed to agree. Here that ring is empty. Computing the match from the
      changed pixels instead would force the edit's own mean and standard
      deviation back onto the original's, i.e. undo the very change the user
      asked for (a "make this panel darker" edit would be re-brightened), and
      computing it from an empty sample is a division by zero. Neither is a
      correction, so the match is switched off and `mask_feather_px` — which is
      NOT switched off — is what joins the regenerated region to the page.

    - **`text_attention_in_mask` -> False.** It confines the prompt to the
      tokens inside the mask, and a solid mask has every token inside: the
      attention mask would block nothing and still cost `S²` bytes of VRAM and
      the flash kernel. Forcing it off keeps the run, the memory forecast and
      the reported effective value truthful.

    Logged whenever it actually overrides a value the caller asked for, so the
    override is visible in the backend log rather than inferred from the result.
    """
    overrides: dict[str, Any] = {
        "mask_dilate_px": 0,
        "color_match": False,
        "text_attention_in_mask": False,
    }
    contradicted = sorted(key for key, value in overrides.items() if normalized[key] != value)
    if contradicted:
        log.info(
            "FLUX.2 klein: режим «без маски» переопределяет %s — расширять маску, сверять цвет "
            "по кольцу вокруг неё и ограничивать внимание текста в нём нечем "
            "(см. _whole_region_overrides).",
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


def _model_key(normalized: dict[str, Any]) -> str:
    """Resident-model key: everything that changes the loaded object.

    `vae_tiling` / `vae_slicing` are excluded on purpose — they are re-applied to
    the cached pipeline on every request instead of forcing a reload. The text
    encoder is excluded too, unless it is kept resident: see below.
    """
    parts = [
        normalized["dtype"],
        normalized["placement"],
        "lowram" if normalized["low_cpu_mem_usage"] else "normal",
        normalized["transformer_path"],
        normalized["vae_path"],
    ]
    # The text encoder is NOT part of the pipeline: the prompt phase uses it and
    # may release it. It belongs to the key only when the user asked to keep it resident,
    # because that is exactly when the key would otherwise claim less than what
    # this service is holding.
    if not normalized["unload_text_encoder_after_encode"]:
        parts.append(normalized["text_encoder_path"])
        parts.append("encfp8" if normalized["text_encoder_fp8"] else "encbf16")
    return "flux2_klein:" + "|".join(parts)


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
