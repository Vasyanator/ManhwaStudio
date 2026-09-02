"""
File: modules/ai_backend/inpaint/flux2_klein.py

Purpose:
FLUX.2 klein 9B region-editing service for the Python AI backend (methods
`inpaint.flux2_klein`, `.status`, `.estimate`, `.unload` and the
`.prompt_cache.*` family; streaming). It edits a user-selected page REGION with
`diffusers.Flux2KleinInpaintPipeline`.

Prompt-cache library:
Encoding a prompt costs a 16 GB read of the Qwen3 encoder, and the resulting
embedding is ~4 MiB, so embeddings are both cached in memory (`_prompt_cache`,
LRU) and persistable to disk. On-disk entries live in
`<program root>/prompt_cache/<encoder family>/<name>.msprompt` — a safetensors
container holding one `prompt_embeds` tensor plus a `__metadata__` map that
names the encoder the embedding came from. That name is CHECKED on load
(`validate_prompt_file_metadata`): embeddings of another encoder would load
without an error and denoise into something the user did not ask for. The
in-memory cache, the `.prompt_cache.build`/`.load` methods and `status`'s
`prompt_cached` all go through ONE key (`_prompt_cache_key`).

The text encoder is OPTIONAL when the prompt is already cached:
`text_encoder_path` may be empty or point nowhere, and a `.msprompt` carried
from another machine is then enough to generate — the denoise and the VAE decode
never look at the encoder. What the encoder's absence costs is the identity
check: `validate_prompt_file_metadata` still verifies the format marker, the
version, the sequence length, the dtype and the fp8 flag, but the fingerprint is
taken on trust and the answer says so (`encoder_verified`). Encoding a NEW prompt
is refused instead (`require_text_encoder`), and `status` reports
`text_encoder_available` so the client can warn that only ready caches will work.

Mask semantics (this is NOT classic inpainting):
The mask is a PERMISSION TO CHANGE. Everything outside it must come back
byte-identical. The pipeline itself keeps the latent outside the mask locked, but
the VAE decode still returns the whole window slightly different, so the final
color alignment and the composite are done HERE, not by the pipeline. The wire
format is L8 and only L8: an RGB/RGBA mask is refused, never converted, because
guessing which channel means "edit this" edits the wrong pixels.

`whole_region=True` is the "no mask" mode: the whole validated region may change.
The request format does not fork — the client still sends a mask, a solid one —
and the service verifies that it really is solid (`_require_solid_mask`) instead
of trusting the flag. The mode settles two other parameters by itself
(`_whole_region_overrides`): the dilate is pointless on a full mask, and the
color match has no unchanged ring to take its statistics from. The feather is
NOT disabled; on a solid mask it ramps inwards from the region border, which is
what joins the regenerated region to the rest of the page.

Run order (this is a memory contract, see `inpaint_image_bytes`):
transformer + VAE are loaded, placed and warmed up FIRST; only then is the 16 GB
text encoder read, into the host memory the transformer has just vacated, where
it encodes the prompt and stays for the next one.

Model layout:
Nothing is downloaded. The user supplies three paths — a Qwen3 text-encoder
folder, a transformer (`.safetensors` single file or a diffusers folder) and an
`AutoencoderKLFlux2` folder. The pipeline needs five components, so the tokenizer
and the scheduler are DISCOVERED next to those paths (see
`discover_component_dir`); when they are not found the service raises an explicit
error naming what to put where. There is no built-in default scheduler config: a
silently invented one would produce plausible-looking garbage. The same holds for
a single-file transformer: its `transformer/config.json` must be next to the
checkpoint, and neither the Hub's `flux-2-dev` config nor a config guessed from
the tensor shapes is accepted (see `_load_transformer`).

Main responsibilities:
- parameter normalization/validation (`normalize_flux2_klein_params`) and region
  validation (`validate_region_size`) — no silent resizing;
- lazy pipeline build from the three user paths + the two discovered ones, with
  four placement modes (`full_gpu`, `encoder_cpu`, `model_cpu_offload`,
  `sequential_cpu_offload`);
- generation with a dilated LATENT mask, then our own color matching and
  feathered composite over the original region;
- denoising and VAE decoding as two SEPARATE steps (`output_type="latent"`), so
  the transformer can leave the GPU before the decode peak — with an explicit
  out-of-memory recovery path that never repeats the denoise;
- a RAM/VRAM forecast (`estimate`) and an on-disk/component `status`;
- progress streamed as `progress_callback(phase, step, total, label)` where phase
  is "load" or "generate";
- health / unload hooks and the shared resident-model lease protocol.

Notes:
- torch / diffusers / transformers / cv2 are imported lazily inside the methods
  that need them, so importing this module costs nothing.
- On a ROCm build every host->device weight move goes through
  `runtime/rocm_mmap_transfer.py`; the offload placements cannot hold that patch
  across inference, so they re-home the file-backed components up front (see
  `_materialize_components_for_offload`). Both are strict no-ops off ROCm. One
  exception is documented in place, with the measurement behind it:
  `low_cpu_mem_usage` loads weights straight into VRAM through accelerate's
  `device_map`, which never calls `nn.Module.to` and therefore cannot be staged
  (see the comment in `_ensure_pipeline_locked`).
- Generation is cancellable only at call boundaries: the handler checks
  `cancel_event` before and after the service call, and a running diffusion step
  is not interrupted. That is the shared contract of every inpaint service here.
- Placement is applied, not assumed: `_apply_placement` moves the GPU-resident
  components even when a loader was asked to put them there, because
  `from_single_file` accepts `device_map` and silently ignores it
  (`_single_file_device`). The Qwen3 encoder is the one transformers loader here
  and takes `dtype=`; the diffusers loaders take `torch_dtype=`.
"""

from __future__ import annotations

import io
import json
import logging
import os
import struct
import threading
import time
from collections import OrderedDict
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable

if TYPE_CHECKING:
    import numpy as np

try:
    from ai_device import AIDevice
except Exception:  # pragma: no cover - one of the two import roots always works
    from modules.ai_device import AIDevice

from ..runtime.model_manager import LoadedModelManager
from ..runtime.paths import program_root
from ..runtime.rocm_mmap_transfer import (
    mmap_staging_required,
    patched_module_to,
    tensor_needs_staging,
)
from ..runtime.torch_support import is_torch_available

try:
    from config import UserConfig
except Exception:  # pragma: no cover - config is always importable in-app
    UserConfig = None

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

#: Compute dtypes offered to the user. Both are 2 bytes per parameter.
VALID_DTYPES = ("bfloat16", "float16")

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

#: `phase:"load"` progress step numbers, in the order a run actually performs
#: them. The transformer and the VAE are loaded, placed and warmed up FIRST; the
#: text encoder is read only afterwards, when its 16 GB arrive into a host that
#: the transformer has just left (see `inpaint_image_bytes`). The `phase` values
#: on the wire stay `"load"` / `"generate"`, so the Rust side needs no protocol
#: change — only the labels moved.
LOAD_STEP_PREPARE = 0
LOAD_STEP_TRANSFORMER = 1
LOAD_STEP_TOKENIZER = 2
LOAD_STEP_VAE = 3
LOAD_STEP_SCHEDULER = 4
LOAD_STEP_PLACEMENT = 5
LOAD_STEP_WARMUP = 6
LOAD_STEP_TEXT_ENCODER = 7
LOAD_STEP_ENCODE = 8
LOAD_STEP_ENCODER_DONE = 9

#: Total number of `phase:"load"` progress steps, i.e. the last step number above.
LOAD_PHASE_STEPS = LOAD_STEP_ENCODER_DONE

#: Spatial size of the warm-up latent handed to the VAE, in latent cells. The
#: decode it triggers is what materializes the placed weights and initializes the
#: allocator and (on ROCm) the MIOpen convolution kernels, so it must be a REAL
#: forward — but it costs `WARMUP_LATENT_CELLS * vae_scale_factor` pixels, i.e. a
#: 64x64 image, which is nothing next to the region itself.
WARMUP_LATENT_CELLS = 8

#: Peak activation bytes of ONE text-encoder forward pass at 512 tokens: the
#: `[1, 512, vocab]` logits Qwen3ForCausalLM produces (~150 MiB at bf16 for a
#: 151k vocabulary), the three requested hidden-state layers, and the attention
#: working set. Coarse and deliberately generous, like the other constants here.
ENCODE_ACTIVATION_BYTES = 512 * 1024 * 1024

#: Prompt embeddings kept between runs. One entry is `[1, <=512, 4096]` at 2
#: bytes, i.e. ~4 MiB, so eight of them cost ~32 MiB — small enough to keep
#: always on, bounded so that a session of prompt edits is not a leak.
PROMPT_EMBED_CACHE_ENTRIES = 8

#: Progress callback: (phase, step, total, label). phase in {"load","generate"}.
ProgressCb = Callable[[str, int, int, str], None]

# ---------------------------------------------------------------------------
# The `.msprompt` prompt-cache file
# ---------------------------------------------------------------------------
# A prompt embedding is worth persisting for exactly one reason: producing it
# costs a 16 GB read of the Qwen3 encoder (~106 s measured on this project's
# reference host), while the embedding itself is ~4 MiB. A user who always edits
# with the same prompt should never have to hold that encoder at all.
#
# The container is safetensors — already a dependency, readable by third-party
# tooling, and its header can be inspected without materializing a tensor, which
# is what makes the compatibility check below torch-free and cheap.

#: File suffix of a saved prompt cache. Checked on every client-supplied path:
#: those are untrusted input, and a wrong suffix is far more likely to be a
#: mis-wired path than a deliberate choice.
PROMPT_CACHE_SUFFIX = ".msprompt"

#: Library directory, in the program root next to `fonts/`. Layout:
#: `prompt_cache/<encoder family>/<entry name>.msprompt`. The family level
#: exists because an embedding is only valid for the encoder that produced it,
#: so entries of two encoders must never share a listing.
PROMPT_CACHE_DIRNAME = "prompt_cache"

#: Length of the fingerprint fragment appended to a family directory name. Eight
#: hex characters is 32 bits: enough that two encoders a user actually has
#: installed will not collide, short enough to keep the directory readable.
PROMPT_CACHE_FAMILY_HASH_CHARS = 8

#: Characters kept verbatim in a family or entry name. Everything else becomes
#: `_`. Path separators, `..`, control characters and the Windows-reserved
#: `<>:"/\|?*` are therefore all removed by construction rather than by a
#: blacklist that has to stay complete.
_SAFE_NAME_EXTRA = " ._-()"

#: Longest sanitized name component we write. Well under every filesystem's
#: limit even after the `.msprompt` suffix and a `.<pid>.part` staging suffix.
_MAX_NAME_LENGTH = 100

#: `__metadata__` marker. Present and equal, or the file is not ours and is
#: refused before anything else is looked at.
PROMPT_CACHE_FORMAT = "manhwastudio.flux2_klein.prompt_cache"

#: Format version. Bumped when the meaning of an existing field changes; a file
#: from a NEWER version is refused rather than read with today's rules.
PROMPT_CACHE_VERSION = 1

#: The single tensor a `.msprompt` file carries.
PROMPT_CACHE_TENSOR = "prompt_embeds"

#: safetensors dtype tokens for the two compute dtypes this service offers,
#: keyed by the `dtype` param name. The container records the tensor's own
#: dtype, so the declared one can be cross-checked against it from the header
#: alone — a hand-edited `__metadata__` cannot make float16 embeddings pass as
#: bfloat16 ones.
_PROMPT_CACHE_DTYPE_TOKENS = {"bfloat16": "BF16", "float16": "F16"}

#: Weight-file suffixes that take part in the text-encoder fingerprint.
_ENCODER_WEIGHT_SUFFIXES = (".safetensors", ".bin", ".pt", ".pth")

#: Subdirectory names searched for the two components the user does not supply.
_TOKENIZER_SUBDIR = "tokenizer"
_SCHEDULER_SUBDIR = "scheduler"

#: Subdirectory searched for the transformer's own `config.json` when the user
#: supplied the transformer as a single `.safetensors` file.
_TRANSFORMER_SUBDIR = "transformer"

#: `_class_name` a discovered transformer `config.json` must carry, when it
#: carries one at all. A config of another component (a VAE, a text encoder)
#: found next to the checkpoint would otherwise build a different architecture.
_TRANSFORMER_CONFIG_CLASS_NAME = "Flux2Transformer2DModel"

#: A directory is a tokenizer when it carries one of these files.
_TOKENIZER_MARKERS = ("tokenizer.json", "tokenizer_config.json")

#: A directory is a scheduler when it carries this file.
_SCHEDULER_MARKER = "scheduler_config.json"

#: A directory is a diffusers/transformers model when it carries this file.
_MODEL_CONFIG_MARKER = "config.json"

#: Upper bound on the JSON header of a safetensors file we are willing to parse.
_MAX_SAFETENSORS_HEADER_BYTES = 100 * 1024 * 1024

#: Suffixes of the per-tensor scale entries an fp8-scaled checkpoint carries.
#: diffusers 0.39 has no converter for them, so such a file must be rejected with
#: a readable message instead of failing deep inside the loader.
_FP8_SCALE_SUFFIXES = (".weight_scale", ".input_scale", ".scale_weight", ".scale_input")

#: safetensors dtype tokens for the two 8-bit float formats.
_FP8_DTYPES = ("F8_E4M3", "F8_E5M2")

#: Minimum number of pixels outside the mask required for a meaningful
#: mean/std color match; below it the statistics are noise.
_MIN_COLOR_MATCH_SAMPLES = 256

#: Largest inward distance the cv2-free fallback of `_mask_distance_inside`
#: measures, in pixels. One above the widest feather `normalize_flux2_klein_params`
#: accepts, so the ramp width is never clamped by the probe itself; the fallback
#: costs one erosion per level, which is why it is bounded at all.
MAX_MASK_DISTANCE_PROBE = 33

# ---------------------------------------------------------------------------
# Memory forecast constants
# ---------------------------------------------------------------------------
# These are COARSE, advisory upper bounds, not measurements: nothing in this
# repository profiles FLUX.2 klein, and `estimate` exists to keep a user from
# starting a run that obviously cannot fit. Every number below is per unit of
# work and independent of the checkpoint, so a wrong one is off by a constant
# factor and never by an order of magnitude.

#: Peak transformer activation bytes per latent token (residual stream plus the
#: attention working set at bf16, ~16 buffers of a 3072-wide hidden state).
ACTIVATION_BYTES_PER_LATENT_TOKEN = 96 * 1024

#: Peak VAE decode activation bytes per output pixel, untiled: full-resolution
#: feature maps of ~128 channels at 2 bytes, a few buffers deep.
VAE_DECODE_BYTES_PER_PIXEL = 1024

#: The same with tiling/slicing enabled — only one tile is live at a time.
VAE_DECODE_TILED_BYTES_PER_PIXEL = 256

#: Share of the transformer resident on the device under sequential offload
#: (roughly one block plus the layer being prefetched).
SEQUENTIAL_RESIDENT_TRANSFORMER_FRACTION = 0.08

# ---------------------------------------------------------------------------
# Pre-load memory guard
# ---------------------------------------------------------------------------
# The guard exists because a host-side shortfall is NOT an exception: the kernel
# OOM killer picks a victim among everything running, and on this project's
# reference host it has already closed a user's editor with unsaved work while
# the 9B transformer and the 8B encoder were being loaded side by side. A
# `torch.OutOfMemoryError` would have been the good outcome.

#: Host memory the guard refuses to plan into. It covers what the forecast does
#: not model and what a shortfall costs someone else: the interpreter and torch
#: runtime already resident, the transient copy buffers a safetensors read needs,
#: and the margin the kernel wants before it starts killing processes. 2 GiB is
#: roughly one large shard's working set — small enough not to reject a run that
#: genuinely fits, large enough that the OOM killer is not the next event.
HOST_MEMORY_RESERVE_BYTES = 2 * 1024**3

#: Device memory the guard refuses to plan into: allocator fragmentation plus the
#: BLAS/attention workspaces that are not part of the per-token activation
#: constant. Smaller than the host reserve because running out here raises a
#: catchable `OutOfMemoryError` and the decode has its own recovery ladder.
DEVICE_MEMORY_RESERVE_BYTES = 512 * 1024**2

#: The four memory profiles the UI offers, as (label, placement,
#: low_cpu_mem_usage). MUST stay in sync with `MemoryPreset::values` in
#: `src/tabs/cleaning/tools/flux2_klein.rs` — the guard names the ones that fit,
#: so a stale entry here is advice the user cannot follow.
_MEMORY_PRESETS = (
    ("Максимальная скорость", "full_gpu", False),
    ("Сбалансированный", "encoder_cpu", False),
    ("Минимум RAM", "encoder_cpu", True),
    ("Минимум VRAM", "sequential_cpu_offload", True),
)

#: Components loaded from safetensors, i.e. the ones diffusers hands out backed
#: by a writable private file mapping and which therefore hit the ROCm amdkfd
#: stall. The transformer is LAST on purpose: it is by far the largest, so a
#: failed (OOM) round trip of it still leaves the smaller one re-homed. The text
#: encoder is absent because it is not part of the pipeline any more — the prompt
#: phase loads it after this one is placed, and it never goes on the device.
_MMAP_BACKED_COMPONENTS = ("vae", "transformer")


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

    `whole_region=True` is a MODE, not a hint, and it settles two other keys on
    the caller's behalf (`mask_dilate_px` -> 0, `color_match` -> False); see
    `_whole_region_overrides` for why neither is meaningful there.

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
    }
    if whole_region:
        normalized.update(_whole_region_overrides(normalized))
    return normalized


def _whole_region_overrides(normalized: dict[str, Any]) -> dict[str, Any]:
    """The keys `whole_region` settles on the caller's behalf, with the reasons.

    Both would otherwise operate on an input they have no meaning for, and both
    are silent about it — which is exactly the failure mode this module refuses
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

    Logged whenever it actually overrides a value the caller asked for, so the
    override is visible in the backend log rather than inferred from the result.
    """
    overrides: dict[str, Any] = {"mask_dilate_px": 0, "color_match": False}
    contradicted = sorted(key for key, value in overrides.items() if normalized[key] != value)
    if contradicted:
        log.info(
            "FLUX.2 klein: режим «без маски» переопределяет %s — расширять и по чему сверять цвет "
            "в нём нечего (см. _whole_region_overrides).",
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
#  Component discovery (tokenizer / scheduler / transformer config)
# =====================================================================
def component_search_roots(normalized_or_paths: dict[str, Any]) -> list[Path]:
    """Directories searched for the components the user does not supply.

    Order (first match wins): the text-encoder directory and its parent, then the
    transformer directory and its parent, then the VAE directory and its parent.
    A klein checkout keeps `text_encoder/`, `tokenizer/`, `scheduler/`, `vae/` and
    `transformer/` side by side, so the parent of any supplied path is usually
    the repository root.
    """
    roots: list[Path] = []
    for key in ("text_encoder_path", "transformer_path", "vae_path"):
        raw = str(normalized_or_paths.get(key) or "").strip()
        if not raw:
            continue
        candidate = Path(raw)
        base = candidate.parent if candidate.is_file() else candidate
        for root in (base, base.parent):
            if root not in roots:
                roots.append(root)
    return roots


def component_probe_order(roots: list[Path], subdir: str) -> list[Path]:
    """Directories `discover_component_dir` probes for `subdir`, in search order.

    Pure path arithmetic — nothing is touched on disk. It exists so that an error
    message listing "we looked here" cannot drift away from where the search
    actually looked; both go through this function.
    """
    order: list[Path] = []
    for root in roots:
        for candidate in (root / subdir, root):
            if candidate not in order:
                order.append(candidate)
    return order


def discover_component_dir(roots: list[Path], subdir: str, markers: tuple[str, ...]) -> Path | None:
    """First directory under `roots` that holds one of `markers`.

    Both `<root>/<subdir>` and `<root>` itself are probed, because a
    transformers-style text-encoder folder carries its tokenizer files directly
    while a diffusers checkout keeps them in a sibling subfolder.
    """
    for candidate in component_probe_order(roots, subdir):
        try:
            if candidate.is_dir() and any((candidate / m).is_file() for m in markers):
                return candidate
        except OSError:
            continue
    return None


def _require_component_dir(
    roots: list[Path], subdir: str, markers: tuple[str, ...], human_name: str
) -> Path:
    """`discover_component_dir` or an explicit error naming where to put it."""
    found = discover_component_dir(roots, subdir, markers)
    if found is not None:
        return found
    searched = ", ".join(str(root) for root in roots) or "(пути не заданы)"
    raise FileNotFoundError(
        f"Не найден {human_name} для FLUX.2 klein. Ожидается каталог с файлом "
        f"{markers[0]} — положите его как «{subdir}» рядом с текстовым энкодером или "
        f"трансформером. Просмотрены каталоги: {searched}"
    )


# =====================================================================
#  Checkpoint inspection
# =====================================================================
def read_safetensors_header(path: Path) -> dict[str, Any]:
    """Parse the JSON header of a safetensors file without loading any tensor.

    # Raises
    `ValueError` when the file is not a safetensors container or its header is
    implausibly large / not valid UTF-8 JSON.
    """
    try:
        with path.open("rb") as handle:
            raw_len = handle.read(8)
            if len(raw_len) != 8:
                raise ValueError(f"Файл слишком мал для safetensors: {path}")
            header_len = struct.unpack("<Q", raw_len)[0]
            if header_len == 0 or header_len > _MAX_SAFETENSORS_HEADER_BYTES:
                raise ValueError(
                    f"Некорректный заголовок safetensors ({header_len} байт): {path}"
                )
            payload = handle.read(header_len)
    except OSError as exc:
        raise ValueError(f"Не удалось прочитать {path}: {exc}") from exc
    if len(payload) != header_len:
        raise ValueError(f"Обрезанный заголовок safetensors: {path}")
    try:
        header = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError(f"Заголовок safetensors не является JSON: {path}") from exc
    if not isinstance(header, dict):
        raise ValueError(f"Заголовок safetensors не является объектом: {path}")
    return header


def is_fp8_scaled_checkpoint(header: dict[str, Any]) -> bool:
    """Whether a safetensors header describes an fp8_scaled checkpoint.

    Such files carry per-tensor `weight_scale`/`input_scale` entries and/or
    8-bit float tensors. diffusers 0.39 has no converter for that layout, so the
    caller must reject the file with a readable message instead of letting the
    single-file loader fail on unknown keys.
    """
    for key, entry in header.items():
        if key == "__metadata__":
            continue
        if key.endswith(_FP8_SCALE_SUFFIXES):
            return True
        if isinstance(entry, dict) and str(entry.get("dtype", "")).upper() in _FP8_DTYPES:
            return True
    return False


def _fp8_scaled_message(source: Path) -> str:
    """The single wording used to refuse an fp8_scaled file or shard."""
    return (
        f"Чекпоинт {source.name} сохранён в формате fp8_scaled (тензоры *_scale). "
        "diffusers 0.39 не умеет его конвертировать — используйте bf16/fp16 "
        "safetensors или каталог в формате diffusers."
    )


def component_safetensors_shards(source: Path) -> list[Path]:
    """Every safetensors shard belonging to the diffusers folder `source`.

    A sharded checkout names its parts in a `*.index.json` weight map, which may
    point at names the directory glob would order differently, so the index is
    consulted first and the plain `*.safetensors` glob fills in the rest. An
    unreadable or malformed index is skipped rather than raised on: it is only a
    hint here, and the real loader gives a better message for a broken checkout.
    """
    shards: list[Path] = []
    seen: set[Path] = set()
    for index_path in sorted(source.glob("*.index.json")):
        try:
            data = json.loads(index_path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError):
            continue
        weight_map = data.get("weight_map") if isinstance(data, dict) else None
        if not isinstance(weight_map, dict):
            continue
        for name in weight_map.values():
            candidate = source / str(name)
            if candidate.is_file() and candidate not in seen:
                seen.add(candidate)
                shards.append(candidate)
    for candidate in sorted(source.glob("*.safetensors")):
        if candidate not in seen:
            seen.add(candidate)
            shards.append(candidate)
    return shards


def transformer_config_roots(source: Path) -> list[Path]:
    """Roots searched for the transformer `config.json` next to a single file.

    The checkpoint's own directory first, then its parent: a klein checkout keeps
    `transformer/config.json` beside the checkpoint, while a user who dropped the
    file inside `transformer/` is one level deeper. Deduplicated, because at the
    filesystem root a directory is its own parent.
    """
    roots: list[Path] = []
    for root in (source.parent, source.parent.parent):
        if root not in roots:
            roots.append(root)
    return roots


def find_transformer_config_dir(source: Path) -> Path | None:
    """Directory holding the transformer `config.json` for checkpoint `source`.

    `None` when there is none: the caller must refuse the load, because the
    parameters that config carries cannot be recovered from the weights (see
    `_missing_transformer_config_message`).
    """
    return discover_component_dir(
        transformer_config_roots(source), _TRANSFORMER_SUBDIR, (_MODEL_CONFIG_MARKER,)
    )


def validate_transformer_config_dir(config_dir: Path) -> None:
    """Refuse a discovered `config.json` that belongs to a different model class.

    The search also probes the checkpoint's own directory, so a file sitting in a
    VAE or text-encoder folder would otherwise hand `from_single_file` that
    component's config and build the wrong architecture. A config without a
    `_class_name` (a hand-written one) is accepted, since there is nothing to
    contradict.

    # Raises
    `ValueError` when the file is unreadable, is not a JSON object, or names a
    class other than `Flux2Transformer2DModel`.
    """
    path = config_dir / _MODEL_CONFIG_MARKER
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError(f"Не удалось прочитать конфиг трансформера {path}: {exc}") from exc
    if not isinstance(data, dict):
        raise ValueError(f"Конфиг трансформера не является JSON-объектом: {path}")
    class_name = data.get("_class_name")
    if isinstance(class_name, str) and class_name != _TRANSFORMER_CONFIG_CLASS_NAME:
        raise ValueError(
            f"Файл {path} — конфиг «{class_name}», а не «{_TRANSFORMER_CONFIG_CLASS_NAME}». "
            "Рядом с чекпоинтом трансформера должен лежать каталог «transformer» с его "
            "собственным config.json."
        )
    if class_name is None:
        log.debug("FLUX.2 klein: конфиг трансформера %s без _class_name, принят как есть", path)


def _missing_transformer_config_message(source: Path) -> str:
    """The refusal shown when a single-file transformer has no config beside it.

    States the remedy (which file, from which model, in which directory) and the
    reason there is no fallback: `rope_theta`, `eps` and `patch_size` are not
    derivable from the weights, and a wrong value for any of them produces a
    model whose tensor shapes are IDENTICAL — nothing would catch it before the
    user got quietly wrong images.
    """
    searched = ", ".join(
        str(candidate)
        for candidate in component_probe_order(transformer_config_roots(source), _TRANSFORMER_SUBDIR)
    )
    expected = source.parent / _TRANSFORMER_SUBDIR / _MODEL_CONFIG_MARKER
    return (
        f"Не найден config.json трансформера для FLUX.2 klein: одиночный файл {source.name} "
        "не содержит конфигурации.\n"
        f"Что сделать: положите config.json трансформера ОТ ЭТОЙ ЖЕ модели как «{expected}».\n"
        "Почему без него нельзя: diffusers 0.39 распознаёт любой чекпоинт FLUX.2 как "
        "flux-2-dev и взял бы конфиг закрытого репозитория black-forest-labs/FLUX.2-dev — "
        "конфиг ДРУГОЙ модели; этот путь заблокирован намеренно. Подобрать параметры самим "
        "тоже нельзя: rope_theta, eps и patch_size не выводятся из весов, а модель с неверным "
        "rope_theta имеет те же формы тензоров и молча выдаёт неправильные изображения.\n"
        f"Просмотрены каталоги: {searched}"
    )


def _reject_fp8_scaled_directory(source: Path) -> None:
    """Refuse a diffusers transformer folder whose shards are fp8_scaled.

    The single-file path reads the header and refuses before loading anything;
    a folder must get the same treatment, otherwise `from_pretrained` starts a
    multi-GiB load and fails deep inside the loader with an unrelated message
    after the RAM and I/O have already been spent.

    # Raises
    `ValueError` naming the first fp8_scaled shard found.
    """
    for shard in component_safetensors_shards(source):
        try:
            header = read_safetensors_header(shard)
        except ValueError:
            # Not a readable safetensors container: it says nothing about the
            # layout, and the real loader reports it better than we could.
            continue
        if is_fp8_scaled_checkpoint(header):
            raise ValueError(_fp8_scaled_message(shard))


# =====================================================================
#  Text-encoder availability
# =====================================================================
def text_encoder_available(paths: dict[str, Any]) -> bool:
    """Whether a text encoder is present ON THIS MACHINE for `paths`.

    `paths` may be a normalized parameter dict or the lenient `_lenient_paths`
    map; only `text_encoder_path` is read. The answer is `False` both for an
    empty path and for one that does not exist, because those are the same
    situation from the run's point of view: nothing here can encode a prompt.
    A `.msprompt` file loaded into the cache is what makes a run possible
    anyway — see `normalize_flux2_klein_params`.

    It says nothing about whether the directory is a USABLE encoder; that is
    `text_encoder_fingerprint`'s job, and it is asked only when this returns
    `True`.
    """
    raw = str(paths.get("text_encoder_path") or "").strip()
    return bool(raw) and Path(raw).exists()


def require_text_encoder(normalized: dict[str, Any], *, what: str) -> None:
    """Refuse an operation that genuinely needs the encoder when there is none.

    THE single point where a missing encoder becomes an error for anything that
    must ENCODE — called both early (before a run reads 18 GB of transformer it
    would have to throw away) and at the encode itself
    (`_encode_prompts_locked`, which cannot be reached any other way). `what`
    names the operation in the message. A SAVE needs the encoder for its identity
    rather than for an encode and refuses with its own wording, in
    `Flux2KleinInpaintService._require_current_family`.

    The message states BOTH ways out on purpose: the user either points the
    settings at an encoder, or loads a ready `.msprompt` for this exact prompt.
    Naming only the first would tell a user who cannot download 16 GB that the
    feature is closed to them, when it is not.

    # Raises
    `ValueError` when no local encoder is available.
    """
    if text_encoder_available(normalized):
        return
    configured = str(normalized.get("text_encoder_path") or "").strip()
    where = f" (путь не найден: {configured})" if configured else ""
    raise ValueError(
        f"Текстовый энкодер недоступен{where}, а {what} требует кодирования промпта. "
        "Либо укажите каталог текстового энкодера в настройках, либо загрузите готовый кэш "
        "промпта для этого текста (inpaint.flux2_klein.prompt_cache.load) — с ним генерация "
        "энкодер не читает."
    )


def local_encoder_identity(text_encoder_path: str) -> tuple[str, str] | None:
    """`(fingerprint, family)` of the encoder on disk, or `None` when there is none.

    The library needs an identity to file entries under and to compare a file
    against; without an encoder on this machine there is no identity to compute,
    and `None` is that fact rather than a placeholder. Callers must branch on it:
    `None` means "the file's own metadata is all we know", which is exactly what
    `validate_prompt_file_metadata` documents as the unverified path.

    # Raises
    `ValueError` when the path DOES exist but cannot be fingerprinted (no
    `config.json`, unreadable directory). An encoder that cannot be identified
    cannot be loaded either, so that stays an error instead of degrading into
    "no encoder" — degrading it would hide a broken checkout behind a silently
    weaker check.
    """
    if not text_encoder_available({"text_encoder_path": text_encoder_path}):
        return None
    encoder_id = text_encoder_fingerprint(text_encoder_path)
    return encoder_id, encoder_family_name(text_encoder_path, encoder_id)


# =====================================================================
#  The `.msprompt` prompt-cache file
# =====================================================================
def text_encoder_fingerprint(path: str) -> str:
    """Cheap, stable identity of the Qwen3 encoder a `.msprompt` file was built with.

    **Why an identity is needed at all.** Embeddings produced by a different
    encoder have the same shape and the same dtype as ours and load without a
    murmur — they simply denoise into something the user did not ask for. That
    is the silent wrong answer this package forbids everywhere else, so a saved
    file names the encoder it came from and `validate_prompt_file_metadata`
    refuses a mismatch.

    **Why not a hash of the weights.** The encoder is ~16 GB. Reading it to
    answer "may I load this 4 MiB file" would cost more than re-encoding the
    prompt, i.e. it would defeat the feature. The fingerprint is therefore taken
    from the METADATA of the directory: the bytes of `config.json` plus the
    sorted `(file name, size in bytes)` list of its weight files.

    What that catches: another model (a different architecture, hidden size,
    vocabulary or layer count — all of it lives in `config.json`), another
    precision or another shard layout of the same model, a partially downloaded
    checkout, and a file swapped for one of a different length.

    What it does NOT catch: a fine-tune saved with the identical config and
    byte-for-byte identical file SIZES, and corruption inside a weight file that
    preserves its length. Both need the full read this function exists to avoid,
    and neither happens by accident — a user who deliberately replaces weights
    in place gets what they asked for.

    `path` may be the encoder directory or a weights file inside it
    (`component_dir_for_path`).

    # Raises
    `ValueError` when `path` does not resolve to a directory carrying a
    `config.json` — an encoder that cannot be identified is one that cannot be
    loaded either, so this is the same error the loader would raise later.
    """
    import hashlib

    source = component_dir_for_path(Path(path))
    if not source.is_dir():
        raise ValueError(f"Путь текстового энкодера должен быть каталогом: {source}")
    config = source / _MODEL_CONFIG_MARKER
    try:
        config_bytes = config.read_bytes()
    except OSError as exc:
        raise ValueError(
            f"Не удалось прочитать {config}: {exc}. Без config.json энкодер нельзя ни "
            "опознать, ни загрузить."
        ) from exc

    digest = hashlib.sha256()
    digest.update(config_bytes)
    try:
        weights = sorted(
            (entry.name, entry.stat().st_size)
            for entry in source.iterdir()
            if entry.is_file() and entry.suffix.lower() in _ENCODER_WEIGHT_SUFFIXES
        )
    except OSError as exc:
        raise ValueError(f"Не удалось перечислить файлы энкодера в {source}: {exc}") from exc
    for name, size in weights:
        # The separator is part of the hashed text so that ("ab", 1) and
        # ("a", 12) cannot collide into the same byte string.
        digest.update(f"\n{name}\x00{size}".encode("utf-8"))
    return digest.hexdigest()


def prompt_file_metadata(
    normalized: dict[str, Any], text: str, encoder_id: str, family: str
) -> dict[str, str]:
    """The `__metadata__` map of a `.msprompt` file. Values are strings; safetensors
    accepts nothing else.

    It carries everything `validate_prompt_file_metadata` needs to decide whether
    the embedding may be used, plus the ORIGINAL prompt text — which is the point
    of the file for the user: loading it must be able to show what was cached.

    `family` is the library subdirectory the entry belongs to. It is written into
    the file so that IMPORTING one on a machine (or at a moment) where another
    encoder is selected still files it under its own family instead of losing it
    among another encoder's entries.
    """
    return {
        "format": PROMPT_CACHE_FORMAT,
        "format_version": str(PROMPT_CACHE_VERSION),
        "prompt": str(text),
        "max_sequence_length": str(int(normalized["max_sequence_length"])),
        "dtype": str(normalized["dtype"]),
        "text_encoder_fp8": "true" if normalized["text_encoder_fp8"] else "false",
        "text_encoder_id": str(encoder_id),
        "text_encoder_family": str(family),
        # Informational only. It is where the encoder lived when the file was
        # written, which helps a user recognize a file; it is NEVER what
        # compatibility is decided on, because a path proves nothing about the
        # weights that sit there now.
        "text_encoder_path": str(normalized["text_encoder_path"]),
        "created_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }


def read_prompt_file_header(path: Path) -> tuple[dict[str, str], dict[str, Any]]:
    """Read a `.msprompt` header: `(metadata, tensor description)`. No tensor is loaded.

    Deliberately torch-free and cheap — a file that is not ours, or that was
    built for another encoder, must be refused without allocating anything and
    without importing torch.

    # Raises
    `ValueError` when the file is not a safetensors container, carries no
    `__metadata__`, is not marked as our format, comes from a newer format
    version, or does not carry exactly the `prompt_embeds` tensor.
    """
    header = read_safetensors_header(path)
    raw_meta = header.get("__metadata__")
    if not isinstance(raw_meta, dict):
        raise ValueError(_foreign_prompt_file_message(path))
    metadata = {str(key): str(value) for key, value in raw_meta.items()}
    if metadata.get("format") != PROMPT_CACHE_FORMAT:
        raise ValueError(_foreign_prompt_file_message(path))
    version = _to_int(metadata.get("format_version"), 0)
    if version <= 0 or version > PROMPT_CACHE_VERSION:
        raise ValueError(
            f"Файл кэша промпта {path} версии «{metadata.get('format_version')}» не поддерживается "
            f"(эта сборка читает версии 1..{PROMPT_CACHE_VERSION}). Обновите ManhwaStudio или "
            "постройте кэш заново."
        )
    tensor = header.get(PROMPT_CACHE_TENSOR)
    if not isinstance(tensor, dict):
        raise ValueError(
            f"В файле кэша промпта {path} нет тензора «{PROMPT_CACHE_TENSOR}»: файл повреждён."
        )
    return metadata, tensor


def validate_prompt_file_metadata(
    metadata: dict[str, str],
    tensor: dict[str, Any],
    normalized: dict[str, Any],
    encoder_id: str | None,
    path: Path,
) -> None:
    """Refuse a `.msprompt` file that was not built for the current settings.

    Every field of `_prompt_cache_key` except the prompt text itself has to
    agree, because the text is what the file DEFINES while the rest is what the
    caller is about to run with. A disagreement is an explicit refusal naming the
    field: silently loading foreign embeddings would produce a plausible image
    that answers a different prompt, computed by a different encoder — a wrong
    result with no error attached to it.

    The encoder is compared by `text_encoder_fingerprint`, never by path; the
    tensor's own dtype is compared against the declared one, so an edited
    `__metadata__` cannot smuggle float16 embeddings into a bfloat16 run.

    **`encoder_id=None` means there is no encoder on this machine to compare
    against** (`local_encoder_identity`), which is the whole point of carrying a
    `.msprompt` to a machine that never downloaded the 16 GB Qwen3. Only the
    fingerprint comparison is skipped there — the format marker, the version, the
    sequence length, the dtype (metadata AND the tensor's own token) and the fp8
    flag are checked in every case, because none of them needs an encoder. The
    skip is not silent: it is logged here and reported to the client as
    `encoder_verified: false`, so "checked" and "taken on trust" are never
    presented as the same thing.

    # Raises
    `ValueError` naming the first field that does not match.
    """
    wanted_length = int(normalized["max_sequence_length"])
    file_length = _to_int(metadata.get("max_sequence_length"), 0)
    if file_length != wanted_length:
        raise ValueError(
            f"Файл кэша промпта {path} построен для max_sequence_length={file_length}, "
            f"а сейчас выбрано {wanted_length}. Эмбеддинги другой длины последовательности "
            "несовместимы — постройте кэш заново или верните прежнее значение."
        )

    wanted_dtype = str(normalized["dtype"])
    file_dtype = metadata.get("dtype", "")
    if file_dtype != wanted_dtype:
        raise ValueError(
            f"Файл кэша промпта {path} построен для типа данных «{file_dtype}», "
            f"а сейчас выбран «{wanted_dtype}»."
        )
    expected_token = _PROMPT_CACHE_DTYPE_TOKENS.get(wanted_dtype)
    actual_token = str(tensor.get("dtype", ""))
    if expected_token is not None and actual_token != expected_token:
        raise ValueError(
            f"Файл кэша промпта {path} объявляет тип «{file_dtype}», но хранит тензор "
            f"{actual_token or '?'} вместо {expected_token}: файл повреждён или изменён вручную."
        )

    wanted_fp8 = bool(normalized["text_encoder_fp8"])
    file_fp8 = _to_bool(metadata.get("text_encoder_fp8"), False)
    if file_fp8 != wanted_fp8:
        raise ValueError(
            f"Файл кэша промпта {path} построен "
            f"{'с' if file_fp8 else 'без'} fp8-квантованием энкодера, а сейчас выбрано "
            f"{'с' if wanted_fp8 else 'без'} ним."
        )

    file_encoder = metadata.get("text_encoder_id", "")
    if encoder_id is None:
        # Nothing on this machine can produce a fingerprint, so the file's own
        # identity is all there is. It is still worth logging WHICH encoder the
        # embedding claims: that line is the only trace connecting a later
        # generation to the model it was really encoded with.
        log.info(
            "FLUX.2 klein: отпечаток энкодера для %s не сверялся — локального энкодера нет; "
            "файл объявляет энкодер %s (%s).",
            path,
            _short_id(file_encoder),
            metadata.get("text_encoder_family", "—"),
        )
        return
    if file_encoder != encoder_id:
        raise ValueError(
            f"Файл кэша промпта {path} построен другим текстовым энкодером "
            f"(в файле {_short_id(file_encoder)}, сейчас выбран {_short_id(encoder_id)}"
            f"{_encoder_origin_hint(metadata)}). Эмбеддинги чужого энкодера дали бы не ошибку, "
            "а неверный результат, поэтому файл отклонён."
        )


def _short_id(value: str) -> str:
    """First 12 hex characters of a fingerprint, for a user-facing message."""
    text = str(value or "")
    return text[:12] if text else "—"


def _encoder_origin_hint(metadata: dict[str, str]) -> str:
    """`, файл собран по пути …` when the file recorded one; empty otherwise."""
    origin = metadata.get("text_encoder_path", "")
    return f", файл собран по пути {origin}" if origin else ""


def _foreign_prompt_file_message(path: Path) -> str:
    """The message for a file that is not a `.msprompt` container at all."""
    return (
        f"Файл {path} не является кэшем промпта ManhwaStudio: в нём нет метки формата "
        f"«{PROMPT_CACHE_FORMAT}»."
    )


def require_prompt_file_destination(path: str) -> Path:
    """Validate a client-supplied SAVE path and return it.

    The path is untrusted input, so it is checked instead of being written to:
    it must be absolute (a relative one would land in the backend's working
    directory, which is not a place the user can find), it must carry the
    `.msprompt` suffix, its parent must already exist as a directory (a save
    dialog always produces one, so a missing parent means a mis-wired path
    rather than a folder to create), and it must not name an existing directory.

    # Raises
    `ValueError` naming what is wrong with the path.
    """
    raw = str(path or "").strip()
    if not raw:
        raise ValueError("Не задан путь для сохранения кэша промпта.")
    dest = Path(raw)
    if not dest.is_absolute():
        raise ValueError(f"Путь сохранения кэша промпта должен быть абсолютным: {raw}")
    if dest.suffix.lower() != PROMPT_CACHE_SUFFIX:
        raise ValueError(
            f"Кэш промпта сохраняется только в файл «*{PROMPT_CACHE_SUFFIX}», получено: {raw}"
        )
    if not dest.name or dest.name == PROMPT_CACHE_SUFFIX:
        raise ValueError(f"Пустое имя файла кэша промпта: {raw}")
    if dest.is_dir():
        raise ValueError(f"Путь сохранения кэша промпта — каталог: {raw}")
    if not dest.parent.is_dir():
        raise ValueError(f"Каталог для кэша промпта не найден: {dest.parent}")
    return dest


def require_prompt_file_source(path: str) -> Path:
    """Validate a client-supplied LOAD path and return it.

    Same suffix rule as the save side — the suffix is the format's name, and a
    file that does not carry it is not one we wrote — plus the file has to exist
    and be a regular file.

    # Raises
    `ValueError` naming what is wrong with the path.
    """
    raw = str(path or "").strip()
    if not raw:
        raise ValueError("Не задан путь к файлу кэша промпта.")
    source = Path(raw)
    if not source.is_absolute():
        raise ValueError(f"Путь к кэшу промпта должен быть абсолютным: {raw}")
    if source.suffix.lower() != PROMPT_CACHE_SUFFIX:
        raise ValueError(
            f"Кэш промпта читается только из файла «*{PROMPT_CACHE_SUFFIX}», получено: {raw}"
        )
    if not source.is_file():
        raise ValueError(f"Файл кэша промпта не найден: {raw}")
    return source


def publish_bytes_atomically(dest: Path, payload: bytes) -> int:
    """Write `payload` to `dest` atomically; returns the number of bytes written.

    The publish recipe is this project's (`engines/model_download.py`): the bytes
    go into a process-private `<name>.<pid>.part` sibling, are flushed and
    `fsync`ed, the handle is closed, and only then does a single `os.replace`
    make `dest` appear — so a crash or a full disk can never leave a truncated
    file where a valid cache used to be. The staging file is removed on any
    failure. The pid is in the staging name because a second backend process
    saving the same entry must not write into the same temporary file.
    """
    dest.parent.mkdir(parents=True, exist_ok=True)
    staging = dest.with_name(f"{dest.name}.{os.getpid()}.part")
    published = False
    try:
        with staging.open("wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(staging, dest)
        published = True
    finally:
        if not published:
            try:
                staging.unlink()
            except OSError:
                log.debug("FLUX.2 klein: не удалось удалить временный файл %s", staging)
    return len(payload)


def write_prompt_file(dest: Path, embeds: Any, metadata: dict[str, str]) -> int:
    """Serialize one embedding into a `.msprompt` file at `dest`; returns its size.

    `embeds` is the host-resident tensor from `_prompt_cache`. The bytes are
    built in memory (an entry is ~4 MiB) and published by
    `publish_bytes_atomically`.
    """
    from safetensors.torch import save as safetensors_save

    payload = safetensors_save({PROMPT_CACHE_TENSOR: embeds.contiguous()}, metadata=metadata)
    return publish_bytes_atomically(dest, payload)


def read_prompt_file_tensor(source: Path) -> Any:
    """Load the `prompt_embeds` tensor of an ALREADY VALIDATED `.msprompt` file.

    Callers must have run `read_prompt_file_header` +
    `validate_prompt_file_metadata` first: this function imports torch and
    allocates, and neither should happen for a file that is going to be refused.
    """
    from safetensors.torch import load_file

    tensors = load_file(str(source))
    embeds = tensors.get(PROMPT_CACHE_TENSOR)
    if embeds is None:
        raise ValueError(
            f"В файле кэша промпта {source} нет тензора «{PROMPT_CACHE_TENSOR}»: файл повреждён."
        )
    return embeds


# =====================================================================
#  The prompt-cache library: prompt_cache/<family>/<name>.msprompt
# =====================================================================
def prompt_cache_root() -> Path:
    """The library directory in the program root (`<root>/prompt_cache`).

    The root comes from `runtime.paths.program_root()`, which is this package's
    single owner of the directory-depth assumption; a local
    `Path(__file__).parents[N]` here would break the moment this module moves.
    The directory is NOT created by this function — only a write creates it.
    """
    return program_root() / PROMPT_CACHE_DIRNAME


def sanitize_name_component(name: str, *, what: str) -> str:
    """One filesystem-safe path COMPONENT, or a `ValueError` naming what was wrong.

    Both the family directory and the entry name are built from untrusted text
    (a user-chosen name, a directory name coming from a user-chosen model path),
    so the rule is an ALLOW-list rather than a blacklist of dangerous characters:
    letters, digits and `_SAFE_NAME_EXTRA` survive, everything else — path
    separators, control characters, the Windows-reserved set — becomes `_`. That
    makes `..`, `a/../../b` and an absolute path structurally impossible to
    express, instead of merely unlikely.

    Leading/trailing dots and spaces are stripped as well: a name like `..` or
    `.` would traverse, and a trailing dot or space is silently dropped by
    Windows, which would make the stored name differ from the reported one.

    Returns the sanitized component, truncated to `_MAX_NAME_LENGTH`.

    # Raises
    `ValueError` when nothing usable is left — an empty or dots-only name is a
    request error, never a silently invented placeholder.
    """
    raw = str(name or "")
    cleaned = "".join(
        char if (char.isalnum() or char in _SAFE_NAME_EXTRA) else "_" for char in raw
    )
    cleaned = cleaned.strip(" .")[:_MAX_NAME_LENGTH].strip(" .")
    if not cleaned or set(cleaned) <= {"_"}:
        raise ValueError(
            f"Недопустимое {what}: «{raw}». Оставьте буквы, цифры, пробел, «.», «_», «-»."
        )
    return cleaned


def encoder_family_name(text_encoder_path: str, encoder_id: str) -> str:
    """Library subdirectory for one text encoder: `<readable name>-<short id>`.

    Two halves, each doing a job the other cannot. The readable half is the
    encoder DIRECTORY's own name, so a user opening `prompt_cache/` sees which
    model a folder belongs to. The hash half is the first
    `PROMPT_CACHE_FAMILY_HASH_CHARS` of `text_encoder_fingerprint`, so two
    different encoders that happen to live in identically named directories
    (`.../model/text_encoder` is the common case) cannot pour their entries into
    one folder — where `load` would then offer embeddings the current encoder did
    not produce.

    The readable half is sanitized (`sanitize_name_component`), so a model
    directory named by the user cannot escape the library root. A directory whose
    name sanitizes to nothing falls back to `encoder`; the hash still keeps the
    family unique.
    """
    source = component_dir_for_path(Path(text_encoder_path))
    try:
        readable = sanitize_name_component(source.name, what="имя каталога энкодера")
    except ValueError:
        # A directory name made entirely of separators or exotic characters is
        # still a legitimate encoder; only its readability is lost.
        readable = "encoder"
    return f"{readable}-{str(encoder_id)[:PROMPT_CACHE_FAMILY_HASH_CHARS]}"


def prompt_cache_family_dir(family: str) -> Path:
    """Directory of one family inside the library. Not created by this call.

    `family` is sanitized again even when it came from a file we wrote: an
    imported `.msprompt` carries its family in metadata, and that metadata is
    attacker-controlled input like any other file content.
    """
    return prompt_cache_root() / sanitize_name_component(family, what="имя семейства энкодера")


def prompt_cache_entry_path(family: str, name: str) -> Path:
    """Full path of one library entry, with both components sanitized."""
    safe = sanitize_name_component(name, what="имя кэша промпта")
    return prompt_cache_family_dir(family) / f"{safe}{PROMPT_CACHE_SUFFIX}"


def list_prompt_cache_entries(family: str) -> tuple[list[dict[str, Any]], list[dict[str, str]]]:
    """List one family's entries: `(entries, skipped)`.

    A directory of caches is a place a user can drop files into, so a single
    corrupt or foreign file must not take the listing down with it: every file is
    read through `read_prompt_file_header` and, when that fails, it is reported in
    `skipped` with the reason instead of raising. Only the header is read, so a
    listing costs no tensor allocation and no torch import.

    Entries are sorted by name; the family directory not existing yet is an empty
    listing, not an error. Every entry names its own `family`, so a listing that
    spans several of them (the machine with no encoder — see
    `Flux2KleinInpaintService.prompt_cache_list`) stays unambiguous.
    """
    directory = prompt_cache_family_dir(family)
    entries: list[dict[str, Any]] = []
    skipped: list[dict[str, str]] = []
    if not directory.is_dir():
        return entries, skipped
    for path in sorted(directory.glob(f"*{PROMPT_CACHE_SUFFIX}")):
        if not path.is_file():
            continue
        try:
            metadata, _tensor = read_prompt_file_header(path)
        except ValueError as exc:
            skipped.append({"name": path.stem, "reason": str(exc)})
            continue
        try:
            size = int(path.stat().st_size)
        except OSError:
            size = 0
        entries.append(
            {
                "name": path.stem,
                # The directory the entry really sits in, not the argument: both
                # go through `sanitize_name_component`, and the stored one is what
                # a later `load` has to be given.
                "family": directory.name,
                "prompt": metadata.get("prompt", ""),
                "created_at": metadata.get("created_at", ""),
                "size_bytes": size,
                "max_sequence_length": _to_int(metadata.get("max_sequence_length"), 0),
                "dtype": metadata.get("dtype", ""),
            }
        )
    return entries, skipped


def list_prompt_cache_families() -> list[str]:
    """Family directory names present in the library, sorted; empty when there is none.

    Only the directory layout is read — no file is opened — because this exists
    to answer "which families could hold an entry" on a machine where no encoder
    is installed and the current family is therefore unknown.
    """
    root = prompt_cache_root()
    if not root.is_dir():
        return []
    try:
        return sorted(entry.name for entry in root.iterdir() if entry.is_dir())
    except OSError as exc:
        log.debug("FLUX.2 klein: не удалось перечислить библиотеку кэшей промптов (%s)", exc)
        return []


def find_prompt_cache_entry(name: str, family: str | None) -> Path:
    """Path of the library entry called `name`, looking it up across families when needed.

    With a `family` this is the plain `prompt_cache_entry_path` and the entry
    must be there — the current encoder's family is the only listing a
    generation may load from.

    `family is None` means no encoder is installed, so no family is the current
    one and the name is searched in ALL of them. An ambiguous name (the same
    entry name saved under two encoders) is REFUSED naming both families rather
    than resolved by an arbitrary rule: picking one would feed a generation
    embeddings from an encoder the user did not choose, which is the silent wrong
    answer this module refuses everywhere else.

    # Raises
    `ValueError` when nothing matches, or when several families do.
    """
    safe = sanitize_name_component(name, what="имя кэша промпта")
    if family is not None:
        path = prompt_cache_entry_path(family, safe)
        if not path.is_file():
            raise ValueError(
                f"Кэш промпта «{path.stem}» не найден в библиотеке семейства «{family}» "
                f"({path.parent})."
            )
        return path

    matches = [
        candidate
        for candidate in (
            prompt_cache_entry_path(known, safe) for known in list_prompt_cache_families()
        )
        if candidate.is_file()
    ]
    if not matches:
        raise ValueError(
            f"Кэш промпта «{safe}» не найден в библиотеке ({prompt_cache_root()}). "
            "Текстовый энкодер не выбран, поэтому поиск шёл по всем семействам."
        )
    if len(matches) > 1:
        families = ", ".join(f"«{match.parent.name}»" for match in matches)
        raise ValueError(
            f"Кэш промпта «{safe}» есть сразу в нескольких семействах ({families}), а текстовый "
            "энкодер не выбран — неизвестно, какой из них ваш. Выберите энкодер или "
            "переименуйте одну из записей."
        )
    return matches[0]


def require_free_entry_path(path: Path, *, overwrite: bool) -> Path:
    """Refuse an existing entry unless overwriting was asked for explicitly.

    Rebuilding a lost cache costs a 16 GB encoder read the user may have saved it
    precisely to avoid, so a name collision is a decision, not a detail: without
    `overwrite` it is a named error telling the caller both options. With
    `overwrite` the publish is still atomic, so a failed write cannot destroy the
    entry that is already there.

    # Raises
    `ValueError` when the entry exists and `overwrite` is false.
    """
    if path.exists() and not overwrite:
        raise ValueError(
            f"Кэш промпта «{path.stem}» уже существует в этой библиотеке. Выберите другое имя "
            "или разрешите перезапись (overwrite): восстановление удалённого кэша стоит "
            "полного чтения текстового энкодера."
        )
    return path


# =====================================================================
#  Service
# =====================================================================
class Flux2KleinInpaintService:
    """Lazy-loading FLUX.2 klein region editor for `inpaint.flux2_klein`.

    One pipeline is resident at a time, guarded by an `RLock` and leased from the
    shared `LoadedModelManager` under a key derived from the three user paths and
    the placement/dtype choice.
    """

    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._pipe: Any = None
        self._active_key: str | None = None
        self._device: Any = None
        self._last_error: str | None = None
        # Paths of the last accepted request, so `status()` can report component
        # state without being handed the params again.
        self._last_paths: dict[str, str] = {}
        # Phase-1 results, keyed by everything that changes them
        # (`_prompt_cache_key`). Always on, LRU, bounded by
        # `PROMPT_EMBED_CACHE_ENTRIES`: a mask edit, a new seed or a repeated
        # prompt must not re-read 16 GB of encoder from disk. Entries live on the
        # HOST, so the cache never pins device memory.
        self._prompt_cache: OrderedDict[tuple[Any, ...], Any] = OrderedDict()
        # The text encoder between runs, when the user asked to keep it
        # (`unload_text_encoder_after_encode=False`). It is NOT part of the
        # pipeline object and NOT part of `_active_key` unless it is resident —
        # see `_model_key`.
        self._text_encoder: Any = None
        self._text_encoder_key: tuple[Any, ...] | None = None

    # ---- status / health ----
    def status(self, params: dict[str, Any] | None = None) -> dict[str, Any]:
        """Component availability and free memory for the given (or last) paths.

        `params` is optional and is read leniently: an absent or empty path is
        reported as "not configured" rather than raising, because the UI calls
        this before the user has finished choosing files.

        `prompt_cached` answers whether a READY embedding exists for the exact
        combination in `params` (prompt + encoder + `max_sequence_length` +
        dtype + fp8); see `_prompt_cached`.

        `text_encoder_available` says whether an encoder is present ON THIS
        MACHINE. It is reported separately from `available` because the two
        answer different questions: a run whose prompt is already cached needs no
        encoder at all, so `available` stays `true` while this flag is `false`,
        and the client is expected to warn that only ready caches will work —
        `prompt_cache.build` and any new prompt are refused until an encoder is
        configured.
        """
        paths = _lenient_paths(params) or dict(self._last_paths)
        roots = component_search_roots(paths)
        tokenizer_dir = discover_component_dir(roots, _TOKENIZER_SUBDIR, _TOKENIZER_MARKERS)
        scheduler_dir = discover_component_dir(roots, _SCHEDULER_SUBDIR, (_SCHEDULER_MARKER,))

        components = {
            "text_encoder": _path_state(paths.get("text_encoder_path")),
            "transformer": _path_state(paths.get("transformer_path")),
            "vae": _path_state(paths.get("vae_path")),
            "tokenizer": {
                "found": tokenizer_dir is not None,
                "path": str(tokenizer_dir) if tokenizer_dir is not None else "",
            },
            "scheduler": {
                "found": scheduler_dir is not None,
                "path": str(scheduler_dir) if scheduler_dir is not None else "",
            },
        }
        prompt_cached = self._prompt_cached(params)
        reason = _first_unavailable_reason(components, prompt_cached=prompt_cached)
        with self._lock:
            loaded = self._pipe is not None
        device = self._device_label()
        return {
            "available": reason is None,
            "reason": reason,
            "components": components,
            "memory": memory_snapshot(device),
            "loaded": loaded,
            "device": device,
            "prompt_cached": prompt_cached,
            "text_encoder_available": text_encoder_available(paths),
        }

    def _device_label(self) -> str:
        """The device this service runs on: the loaded one, else the planned one.

        Before the first load `self._device` is empty, and answering `"cpu"`
        there is a lie with consequences — it tells the user that a run costing
        tens of minutes will happen on the CPU while it will in fact happen on
        the accelerator selected in `General.ai_device`. The planned value comes
        from the same `_resolve_selected_backend_device("cuda")` the pipeline
        build uses, so the two cannot disagree. Callers pair it with
        `loaded` / `ready`, which say whether the answer is a fact or a plan.

        Takes `self._lock` itself and resolves the plan outside it: device
        detection imports torch on its first call and must not extend the lock.
        """
        with self._lock:
            device = self._device
        if device is not None:
            return str(device)
        return _resolve_selected_backend_device("cuda")

    def estimate(
        self,
        *,
        params: dict[str, Any] | None,
        region_width: int,
        region_height: int,
    ) -> dict[str, Any]:
        """Forecast the RAM/VRAM cost of one run with `params` on that region.

        The arithmetic lives in `forecast_memory`, which is also what the
        pre-load memory guard uses — the UI's advice and the guard's refusal must
        never be two independent calculations. `fits` compares the forecast
        against the currently free memory and is `True` when a side is unknown;
        HERE it is advice for the UI, while `_require_memory_headroom` is the
        gate. The free VRAM is read from the accelerator this service would
        actually use, so a forecast on a two-GPU host is not compared against the
        wrong card.

        # Raises
        `ValueError` for invalid params (see `normalize_flux2_klein_params`) or
        an invalid region (see `validate_region_size`).
        """
        normalized = normalize_flux2_klein_params(params)
        validate_region_size(region_width, region_height)

        forecast = forecast_memory(normalized, region_width, region_height)
        memory = memory_snapshot(self._device_label())
        return {
            "vram_bytes": forecast["vram_bytes"],
            "ram_bytes": forecast["ram_bytes"],
            "vram_free": int(memory["vram_free"]),
            "ram_free": int(memory["ram_free"]),
            "fits": _fits(forecast["vram_bytes"], memory["vram_free"])
            and _fits(forecast["ram_bytes"], memory["ram_free"]),
            "breakdown": forecast["breakdown"],
        }

    def health(self) -> dict[str, Any]:
        """Snapshot for the periodic backend health event.

        `device` follows `_device_label`: the loaded device, or the one that
        would be used, never a placeholder. `ready` tells the two apart.
        """
        device = self._device_label()
        with self._lock:
            return {
                "ready": self._pipe is not None,
                "model": "flux2_klein",
                "device": device,
                "active_key": self._active_key,
                "last_error": self._last_error,
            }

    def unload(self) -> bool:
        """Drop the resident pipeline and text encoder; `False` when nothing was loaded.

        The prompt cache is deliberately KEPT: it holds a few MB of embeddings,
        not weights, and dropping it would make the next run re-read 16 GB of
        encoder for a prompt that has not changed.
        """
        with self._lock:
            had_encoder = self._text_encoder is not None
            self._release_text_encoder_locked()
            if self._pipe is None:
                return had_encoder
            key = self._active_key
            self._pipe = None
            self._active_key = None
            _clear_torch_cache()
            if key is not None:
                self._model_manager.mark_unloaded(key)
            return True

    # ---- main entry ----
    def inpaint_image_bytes(
        self,
        image_bytes: bytes,
        mask_bytes: bytes,
        *,
        params: dict[str, Any] | None = None,
        progress_callback: ProgressCb | None = None,
    ) -> dict[str, Any]:
        """Regenerate the masked part of `image_bytes` and composite it back.

        `image_bytes` is the region PNG and `mask_bytes` an L8 mask of exactly the
        same size, where non-zero means "may change". The returned `image_png` has
        the region's size and is byte-identical to the input outside the mask.
        Under `whole_region` the mask must be solid (every pixel 255) and the
        whole region is regenerated; a non-solid mask there is a request error,
        not a silently narrowed edit.

        **The order of the two phases is a memory contract.** The transformer and
        the VAE are loaded and placed FIRST and then warmed up, so their weights
        are provably on the accelerator; only then is the 16 GB text encoder read,
        into a host that the transformer has just left. It encodes the prompt into
        a few MB of embeddings and stays in host memory for the next prompt. The
        reverse order — the one this replaced — made the encoder's host peak and
        the transformer's host peak overlap, which is what a run has to avoid on a
        machine whose host memory is smaller than their sum.

        # Raises
        `ValueError` for bad params, a bad region size, a mask size mismatch or a
        non-solid mask under `whole_region`; `FileNotFoundError` when a component
        is missing; `RuntimeError` when a phase does not fit in the free memory;
        whatever the pipeline raises during generation.
        """
        normalized = normalize_flux2_klein_params(params)
        region_rgb = _decode_image_rgb(image_bytes)
        height, width = region_rgb.shape[:2]
        validate_region_size(width, height)
        mask_u8 = _decode_mask(mask_bytes, expected_hw=(height, width))
        if normalized["whole_region"]:
            _require_solid_mask(mask_u8)
        self._last_paths = {key: normalized[key] for key in _PATH_KEYS}

        model_key = _model_key(normalized)
        lease = self._model_manager.begin_model_use(
            model_key, unload_callback=lambda: self._unload_key(model_key)
        )
        report = _progress_reporter(progress_callback, "load", LOAD_PHASE_STEPS)
        with self._lock:
            try:
                # Load scope: only a failure in here is a failed LOAD, and only
                # then may the manager drop its entry for `model_key`. It now ends
                # at `_ensure_pipeline_locked`, because everything after it runs
                # with the pipeline already resident — see below.
                try:
                    # Before anything is read: a prompt that is not in the cache
                    # needs the encoder, and on a machine that has none the run
                    # cannot succeed. `_encode_prompts_locked` would refuse it
                    # too, but only after 18 GB of transformer had been loaded
                    # and placed for nothing.
                    if self._prompts_to_encode(normalized):
                        require_text_encoder(normalized, what="генерация с этим промптом")
                    self._require_headroom_locked(normalized, width, height, model_key)
                    report(LOAD_STEP_PREPARE, "Подготовка запуска FLUX.2 klein")
                    pipe = self._ensure_pipeline_locked(
                        normalized, model_key, report, region_hw=(height, width)
                    )
                except Exception:
                    if lease.needs_load:
                        lease.mark_load_failed()
                    raise
                # The pipeline is resident from here on, so it is registered
                # before ANYTHING else: the warm-up and the prompt phase can both
                # fail, and a failure there is a failed run, not a failed load.
                # Reporting it as a failed load would clear the manager's
                # `resident` flag and drop the unload callback while the 9B
                # transformer still occupies VRAM — see the lease-protocol
                # section of `inpaint/MODULE_README.md`.
                if lease.needs_load:
                    lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
                self._warmup_pipeline_locked(pipe, normalized, report)
                embeds = self._prompt_embeds_locked(normalized, report)
                out_rgb, applied, oom_recovered = self._generate_locked(
                    pipe, region_rgb, mask_u8, normalized, embeds, progress_callback
                )
                self._last_error = None
            except Exception as exc:
                self._last_error = str(exc)
                raise
            finally:
                lease.release()

        return {
            "image_png": _encode_png_bytes_rgb(out_rgb),
            "region_size": [int(width), int(height)],
            # The pipeline is loaded at this point, so this is the device the run
            # actually happened on, not a plan.
            "device": self._device_label(),
            "placement": normalized["placement"],
            # The settings actually in force after any OOM recovery, so the Rust
            # side can persist them and take the cheap path next time.
            "applied": applied,
            "oom_recovered": oom_recovered,
        }

    # ---- prompt cache: build / save / load ----
    def prompt_cache_build(
        self,
        params: dict[str, Any] | None,
        *,
        progress_callback: ProgressCb | None = None,
    ) -> dict[str, Any]:
        """Encode the prompt into the cache WITHOUT generating anything.

        This is the "Кэшировать" button: it loads the text encoder, encodes the
        prompt into the very cache a generation reads from (`_prompt_cache`,
        keyed by `_prompt_cache_key` — there is no second cache and no second
        key), and then lets the encoder go again. Nothing else is loaded: the
        transformer and the VAE take no part in a prompt, so the memory guard
        checks the `encode_standalone` phase only and this call never demands the
        18 GB a run would.

        **The encoder is released afterwards**, because releasing it is the point
        of the button: a user caches a prompt precisely so that the next runs can
        happen without 16 GB of Qwen3 resident. The one exception is an encoder
        that was ALREADY resident with the same key when the call arrived — that
        one belongs to the previous run's settings, and dropping it here would
        cost that user a 16 GB re-read they never asked for, so their own
        `unload_text_encoder_after_encode` decides.

        Progress is the shared `phase:"load"` scale: step 0, then the prompt
        phase's own steps 7-9. Steps 1-6 belong to the pipeline, which this call
        does not build, so they are simply never emitted.

        **No model-manager lease is taken**, and that is consistent rather than
        an omission: a lease exists to account for what stays RESIDENT, and this
        call leaves nothing new behind (see the paragraph above). An encoder it
        keeps because the call found it there was already covered by the lease of
        the run that loaded it.

        Returns `{"prompt", "encoded", "prompt_cached", "device"}`; `encoded` is
        `False` when the cache already covered the prompt and nothing was read.

        # Raises
        `ValueError` for invalid params or an empty prompt; `RuntimeError` when
        the encode phase does not fit in the free host memory; whatever the
        encoder loader raises.
        """
        normalized = normalize_flux2_klein_params(params)
        text = normalized["prompt"]
        if not text:
            raise ValueError(
                "Кэшировать нечего: промпт пуст. Введите текст промпта и повторите."
            )
        self._last_paths = {key: normalized[key] for key in _PATH_KEYS}
        report = _progress_reporter(progress_callback, "load", LOAD_PHASE_STEPS)

        with self._lock:
            try:
                if self._prompt_cache_key(normalized, text) in self._prompt_cache:
                    report(LOAD_STEP_ENCODER_DONE, "Промпт уже в кэше")
                    encoded = False
                else:
                    require_text_encoder(normalized, what="кэширование промпта")
                    self._require_encode_headroom_locked(normalized)
                    report(LOAD_STEP_PREPARE, "Подготовка кэширования промпта")
                    self._encode_prompts_locked(
                        self._build_encode_params_locked(normalized), [text], report
                    )
                    encoded = True
                self._last_error = None
            except Exception as exc:
                self._last_error = str(exc)
                raise

        log.info(
            "FLUX.2 klein: кэш промпта готов (%s), длина промпта %d символов.",
            "закодирован" if encoded else "уже был в памяти",
            len(text),
        )
        return {
            "prompt": text,
            "encoded": encoded,
            "prompt_cached": True,
            "device": self._device_label(),
        }

    def _build_encode_params_locked(self, normalized: dict[str, Any]) -> dict[str, Any]:
        """`normalized` with the encoder-release flag `prompt_cache_build` needs.

        Caller must hold `self._lock`. The flag is not part of
        `_prompt_cache_key` nor of `_encoder_key`, so overriding it changes only
        what happens to the encoder AFTER the encode — see `prompt_cache_build`
        for why an already-resident encoder is left to the user's own setting.
        """
        resident = (
            self._text_encoder is not None
            and self._text_encoder_key == self._encoder_key(normalized, "cpu")
        )
        if resident:
            return normalized
        params = dict(normalized)
        params["unload_text_encoder_after_encode"] = True
        return params

    def _require_encode_headroom_locked(self, normalized: dict[str, Any]) -> None:
        """Gate a standalone prompt encode on the free host memory.

        Caller must hold `self._lock`. Only the `encode_standalone` phase is
        checked: `prompt_cache_build` reads the text encoder and nothing else, so
        demanding room for the transformer would refuse a request that fits. The
        region size is passed because `forecast_memory` takes one, but this phase
        does not depend on it — no latent and no pixel is produced here — so the
        smallest valid region is used.
        """
        encoder_resident = (
            self._text_encoder is not None
            and self._text_encoder_key == self._encoder_key(normalized, "cpu")
        )
        _require_memory_headroom(
            normalized,
            MIN_REGION_SIDE,
            MIN_REGION_SIDE,
            _resolve_selected_backend_device("cuda"),
            phases=("encode_standalone",),
            # The pipeline takes no part in this phase, so there is nothing of it
            # to discount: `encode_standalone` charges neither its VRAM nor its
            # host copy.
            pipeline_resident=False,
            encoder_resident=encoder_resident,
        )

    def _current_family(self, params: dict[str, Any] | None) -> tuple[str, str] | None:
        """`(encoder fingerprint, library family)` for the encoder named in `params`,
        or `None` when no encoder is installed on this machine.

        Reads the encoder path leniently — the library methods that only browse,
        export or import do not need a transformer or a VAE, and requiring them
        would refuse a perfectly meaningful request. `None` is a legitimate
        answer, not a failure: without an encoder there is no current family, and
        the callers say so instead of inventing one.

        # Raises
        `ValueError` when the path exists but cannot be fingerprinted (see
        `local_encoder_identity`).
        """
        return local_encoder_identity(_lenient_paths(params).get("text_encoder_path", ""))

    def _require_current_family(
        self, params: dict[str, Any] | None, *, what: str
    ) -> tuple[str, str]:
        """`_current_family`, but the encoder is mandatory. `what` names the operation.

        Used by the one library operation that cannot fall back on a file's own
        metadata: a SAVE has to record which encoder produced the embedding, and
        an unidentifiable encoder leaves nothing to record. The message therefore
        does NOT offer "load a ready cache" as an alternative the way
        `require_text_encoder` does — here it would not be one.

        # Raises
        `ValueError` when there is no local encoder, or it cannot be fingerprinted.
        """
        paths = _lenient_paths(params)
        encoder_path = paths.get("text_encoder_path", "")
        if not text_encoder_available(paths):
            where = f" (путь не найден: {encoder_path})" if encoder_path else ""
            raise ValueError(
                f"Текстовый энкодер недоступен{where}, а {what} без него невозможно: запись "
                "библиотеки обязана назвать энкодер, которым построена, а опознать его нечем. "
                "Укажите каталог текстового энкодера в настройках."
            )
        encoder_id = text_encoder_fingerprint(encoder_path)
        return encoder_id, encoder_family_name(encoder_path, encoder_id)

    def prompt_cache_list(self, params: dict[str, Any] | None) -> dict[str, Any]:
        """List the library entries of the CURRENT encoder's family — or of all of them.

        Only the encoder path is needed, so the listing works while the
        transformer and the VAE are still unset. A corrupt or foreign file in the
        directory is reported in `skipped` rather than failing the call — see
        `list_prompt_cache_entries`.

        **Without an encoder on this machine there is no current family**, and
        refusing the call there would hide the very entries that make an
        encoder-less machine usable. The listing then spans EVERY family in the
        library, `family` comes back empty (nothing is active) and `directory` is
        the library root. Each entry names its own `family` in both cases, which
        is how the client tells the active listing from a foreign one — no second
        naming scheme is introduced, the family in an entry is the one recorded
        in its file.

        Returns `{"family", "directory", "entries", "skipped",
        "text_encoder_available"}`.

        # Raises
        `ValueError` when an encoder path is present but cannot be fingerprinted.
        """
        identity = self._current_family(params)
        if identity is not None:
            family = identity[1]
            entries, skipped = list_prompt_cache_entries(family)
            return {
                "family": family,
                "directory": str(prompt_cache_family_dir(family)),
                "entries": entries,
                "skipped": skipped,
                "text_encoder_available": True,
            }

        entries: list[dict[str, Any]] = []
        skipped: list[dict[str, str]] = []
        for known in list_prompt_cache_families():
            family_entries, family_skipped = list_prompt_cache_entries(known)
            entries.extend(family_entries)
            # A skipped file is named by its family too, or two corrupt files of
            # the same name in different families would report as one.
            skipped.extend({**item, "family": known} for item in family_skipped)
        entries.sort(key=lambda entry: (str(entry["family"]), str(entry["name"])))
        return {
            "family": "",
            "directory": str(prompt_cache_root()),
            "entries": entries,
            "skipped": skipped,
            "text_encoder_available": False,
        }

    def prompt_cache_save(
        self, params: dict[str, Any] | None, name: str, *, overwrite: bool = False
    ) -> dict[str, Any]:
        """Store the cached embedding of `params["prompt"]` in the library under `name`.

        Saving NEVER encodes: a prompt that is not in the cache is a named error
        pointing at `prompt_cache.build`, because a save that silently spent two
        minutes reading a 16 GB encoder would be a different operation than the
        one the user asked for.

        The entry lands in `prompt_cache/<family>/<name>.msprompt`, where the
        family is the current encoder's (`encoder_family_name`). The file records
        the encoder it was built with, its family, the sequence length, the dtype
        and the fp8 flag, so `prompt_cache_load` can refuse an incompatible one;
        the write itself is atomic (`write_prompt_file`), and an existing name is
        refused unless `overwrite` was asked for (`require_free_entry_path`).

        **Saving needs the encoder itself**, unlike loading: the file records the
        fingerprint of the encoder that produced the embedding, and there is
        nothing to record when none is installed. A cache that arrived as a file
        already has that file, so nothing is lost by refusing here.

        Returns `{"family", "name", "path", "size_bytes", "prompt", "created_at"}`;
        `name` is the SANITIZED name actually written, which may differ from the
        one that was asked for.

        # Raises
        `ValueError` for invalid params, an empty prompt, an unusable name, a
        missing or unidentifiable encoder, an existing entry without `overwrite`,
        or a prompt that is not in the cache.
        """
        normalized = normalize_flux2_klein_params(params)
        text = normalized["prompt"]
        if not text:
            raise ValueError("Сохранять нечего: промпт пуст.")
        encoder_id, family = self._require_current_family(
            params, what="сохранение кэша промпта в библиотеку"
        )
        dest = require_free_entry_path(
            prompt_cache_entry_path(family, name), overwrite=overwrite
        )

        key = self._prompt_cache_key(normalized, text)
        with self._lock:
            embeds = self._prompt_cache.get(key)
            if embeds is not None:
                self._prompt_cache.move_to_end(key)
        if embeds is None:
            raise ValueError(
                "Промпт ещё не закодирован — сохранять нечего. Сначала постройте кэш "
                "(«Кэшировать»/inpaint.flux2_klein.prompt_cache.build) или выполните "
                "генерацию с этим промптом: сохранение ничего не кодирует само."
            )

        # Deliberately outside the lock: the tensor is already ours (a later
        # eviction cannot invalidate the reference), and serialization plus a
        # disk write must not block `status()`, `unload()` or an eviction
        # callback.
        metadata = prompt_file_metadata(normalized, text, encoder_id, family)
        size = write_prompt_file(dest, embeds, metadata)
        log.info("FLUX.2 klein: кэш промпта сохранён в %s (%d байт).", dest, size)
        return {
            "family": family,
            "name": dest.stem,
            "path": str(dest),
            "size_bytes": int(size),
            "prompt": text,
            "created_at": metadata["created_at"],
        }

    def prompt_cache_load(self, params: dict[str, Any] | None, name: str) -> dict[str, Any]:
        """Load a library entry of the current family into the cache.

        The entry's own prompt text is what the embedding belongs to, so it is
        returned for the client to show; everything else about the file
        (encoder, sequence length, dtype, fp8) must match the current settings or
        the load is refused by name — see `validate_prompt_file_metadata` for why
        a mismatch cannot be tolerated. **An entry of another family is refused
        here even though `prompt_cache_import` accepts one**: importing files a
        cache away for later, while loading puts embeddings into the very cache a
        generation reads from.

        The header is checked BEFORE torch is imported and before a byte of
        tensor is allocated.

        **Without an encoder on this machine** the entry is looked up across
        every family (`find_prompt_cache_entry`) — there is no current one to
        restrict the search to — and the fingerprint is the only check that is
        skipped: the format marker, the version, `max_sequence_length`, the
        dtype, the tensor's own dtype token and the fp8 flag are verified as
        always, because none of them needs an encoder. The answer says which of
        the two happened in `encoder_verified`, so a client never has to guess
        whether the identity was checked or taken on trust.

        Returns `{"family", "name", "path", "prompt", "prompt_cached",
        "max_sequence_length", "dtype", "created_at", "encoder_verified"}`.

        # Raises
        `ValueError` for invalid params, an unusable name, a missing entry, a
        name that exists in several families while no encoder selects one, a
        corrupt file, or one built for other settings.
        """
        normalized = normalize_flux2_klein_params(params)
        identity = self._current_family(params)
        encoder_id = identity[0] if identity is not None else None
        source = find_prompt_cache_entry(name, identity[1] if identity is not None else None)
        metadata, tensor = read_prompt_file_header(source)
        validate_prompt_file_metadata(metadata, tensor, normalized, encoder_id, source)

        text = metadata.get("prompt", "")
        embeds = read_prompt_file_tensor(source)
        with self._lock:
            self._store_embeds(self._prompt_cache_key(normalized, text), embeds)
        log.info(
            "FLUX.2 klein: кэш промпта загружен из %s (создан %s, длина промпта %d символов, "
            "отпечаток энкодера %s).",
            source,
            metadata.get("created_at", "?"),
            len(text),
            "сверен" if encoder_id is not None else "не сверялся — локального энкодера нет",
        )
        return {
            # The family the entry actually came from: with no encoder selected
            # that is the file's own, and the caller must not be told it belongs
            # to a current family that does not exist.
            "family": source.parent.name,
            "name": source.stem,
            "path": str(source),
            "prompt": text,
            "prompt_cached": True,
            "max_sequence_length": int(normalized["max_sequence_length"]),
            "dtype": str(normalized["dtype"]),
            "created_at": metadata.get("created_at", ""),
            "encoder_verified": encoder_id is not None,
        }

    def prompt_cache_export(
        self, params: dict[str, Any] | None, name: str, path: str
    ) -> dict[str, Any]:
        """Copy a library entry of the current family to an arbitrary `.msprompt` path.

        A byte copy, published atomically: the file already carries everything
        that identifies it, so re-serializing it would only risk changing it.
        Like `prompt_cache_list` this needs the encoder path alone.

        Unlike a LIBRARY entry, an existing destination is overwritten without a
        flag: the path comes from the client's own save dialog, which is where
        the "replace this file?" question belongs, and asking it twice would make
        the second answer meaningless.

        With no encoder installed the entry is resolved across families, exactly
        as `prompt_cache_load` does: copying a file out never feeds a generation,
        so the lookup is the only thing the missing encoder changes.

        Returns `{"family", "name", "path", "size_bytes"}`; `family` is the one
        the entry was taken FROM.

        # Raises
        `ValueError` when the name is unusable, the entry is missing or ambiguous,
        or the destination path is rejected (`require_prompt_file_destination`).
        """
        identity = self._current_family(params)
        source = find_prompt_cache_entry(name, identity[1] if identity is not None else None)
        family = source.parent.name
        dest = require_prompt_file_destination(path)
        try:
            payload = source.read_bytes()
        except OSError as exc:
            raise ValueError(f"Не удалось прочитать кэш промпта {source}: {exc}") from exc
        size = publish_bytes_atomically(dest, payload)
        log.info("FLUX.2 klein: кэш промпта «%s» выгружен в %s (%d байт).", source.stem, dest, size)
        return {"family": family, "name": source.stem, "path": str(dest), "size_bytes": int(size)}

    def prompt_cache_import(
        self,
        params: dict[str, Any] | None,
        path: str,
        *,
        name: str | None = None,
        overwrite: bool = False,
    ) -> dict[str, Any]:
        """Copy an outside `.msprompt` file into the library.

        **The entry lands in the family recorded in the FILE, not in the family
        currently selected.** An embedding belongs to the encoder that produced
        it; filing it under the encoder that happens to be selected right now
        would hide it from the encoder it actually works with, and would put a
        foreign entry into a listing `prompt_cache_load` reads from. When the two
        differ the import still succeeds — the file is a valid cache for
        SOMETHING — and the answer says so (`family_matches: false`) so the
        client can warn instead of pretending the entry is usable now.

        The file is verified as ours (`read_prompt_file_header`) before anything
        is written; the copy is a byte copy, published atomically, and an
        existing name is refused unless `overwrite` was asked for.

        `name` defaults to the source file's stem. Both it and the family are
        sanitized, so a hand-made file cannot place itself outside the library.

        Returns `{"family", "name", "path", "size_bytes", "prompt", "created_at",
        "current_family", "family_matches"}`.

        # Raises
        `ValueError` for a rejected source path, a file that is not a
        `.msprompt`, a file whose metadata names no family, an unusable name, or
        an existing entry without `overwrite`.
        """
        source = require_prompt_file_source(path)
        metadata, _tensor = read_prompt_file_header(source)
        file_family = metadata.get("text_encoder_family", "")
        if not file_family:
            raise ValueError(
                f"Файл кэша промпта {source} не указывает семейство энкодера "
                "(text_encoder_family): неизвестно, куда его положить."
            )
        dest = require_free_entry_path(
            prompt_cache_entry_path(file_family, name if name else source.stem),
            overwrite=overwrite,
        )
        stored_family = dest.parent.name

        # The current family is informational here: an import must work even when
        # no encoder is configured yet, which is exactly the case where a user is
        # setting a machine up from someone else's files. A path that names a
        # broken encoder is caught and treated the same way, for the same reason.
        try:
            identity = self._current_family(params)
        except ValueError:
            identity = None
        current_family = identity[1] if identity is not None else ""

        try:
            payload = source.read_bytes()
        except OSError as exc:
            raise ValueError(f"Не удалось прочитать файл кэша промпта {source}: {exc}") from exc
        size = publish_bytes_atomically(dest, payload)
        family_matches = bool(current_family) and current_family == stored_family
        if not family_matches:
            log.info(
                "FLUX.2 klein: импортированный кэш промпта положен в семейство «%s», а сейчас "
                "выбрано «%s» — с текущим энкодером он не подойдёт.",
                stored_family,
                current_family or "—",
            )
        return {
            "family": stored_family,
            "name": dest.stem,
            "path": str(dest),
            "size_bytes": int(size),
            "prompt": metadata.get("prompt", ""),
            "created_at": metadata.get("created_at", ""),
            "current_family": current_family,
            "family_matches": family_matches,
        }

    def _prompt_cached(self, params: dict[str, Any] | None) -> bool:
        """Whether a READY embedding exists for the combination named by `params`.

        Reported by `status` and read leniently, like the rest of it: an empty
        prompt, a missing path or an unknown enum answers `False` instead of
        raising, because the UI polls this while the user is still typing.

        The answer comes from `_prompt_cache_key` — the same key a generation
        looks up — so it can never claim a hit the run would miss. `whole_region`
        is cleared before normalizing: it cannot change the key, and clearing it
        keeps `_whole_region_overrides`'s log line out of a polling path.
        """
        if not isinstance(params, dict):
            return False
        if not str(params.get("prompt", "") or "").strip():
            return False
        probe = dict(params)
        probe["whole_region"] = False
        try:
            normalized = normalize_flux2_klein_params(probe)
        except ValueError:
            return False
        key = self._prompt_cache_key(normalized, normalized["prompt"])
        with self._lock:
            return key in self._prompt_cache

    # ---- phase 2: the prompt ----
    def _prompt_cache_key(self, normalized: dict[str, Any], text: str) -> tuple[Any, ...]:
        """Everything that changes an embedding: encoder, text, length, dtype, fp8.

        The placement is deliberately absent — the same encoder computes the same
        embedding whether it ran on the host or the accelerator, and including it
        would evict the cache on a profile change for nothing.
        """
        return (
            normalized["text_encoder_path"],
            text,
            int(normalized["max_sequence_length"]),
            normalized["dtype"],
            bool(normalized["text_encoder_fp8"]),
        )

    def _prompts_to_encode(self, normalized: dict[str, Any]) -> list[str]:
        """Prompt texts the phase still has to run; empty when the cache covers the run."""
        wanted = [normalized["prompt"]]
        if float(normalized["guidance_scale"]) > 1.0:
            # Classifier-free guidance needs the empty prompt too; encoding it now
            # is what keeps the pipeline from reaching for an encoder that is gone.
            wanted.append("")
        return [
            text
            for text in wanted
            if self._prompt_cache_key(normalized, text) not in self._prompt_cache
        ]

    def _require_headroom_locked(
        self, normalized: dict[str, Any], region_width: int, region_height: int, model_key: str
    ) -> None:
        """Gate the request on the free memory, before a single byte is read.

        Caller must hold `self._lock`. Only the phases that will actually load
        something are checked: a cached prompt skips `encode`, a resident
        pipeline skips `denoise`/`decode`. They are listed in the order the run
        performs them — pipeline first, prompt second — so the refusal message
        names the phase the user would have hit first.

        What the service ALREADY holds is reported too, and discounted: a request
        that only needs a new prompt still runs its `encode` phase, but that
        phase's forecast includes the placed pipeline's VRAM and the kept
        encoder's RAM — both of which are already allocated and therefore already
        missing from the free-memory figures the guard compares against. Charging
        for them twice refused a run whose memory was, literally, already in
        place; that was measured on this project's reference host the first time
        a second prompt was sent to a resident pipeline.
        """
        phases: list[str] = []
        pipeline_resident = self._pipe is not None and self._active_key == model_key
        if not pipeline_resident:
            phases.extend(("denoise", "decode"))
        if self._prompts_to_encode(normalized):
            phases.append("encode")
        encoder_resident = (
            self._text_encoder is not None
            and self._text_encoder_key == self._encoder_key(normalized, "cpu")
        )
        _require_memory_headroom(
            normalized,
            region_width,
            region_height,
            _resolve_selected_backend_device("cuda"),
            phases=tuple(phases),
            pipeline_resident=pipeline_resident,
            encoder_resident=encoder_resident,
        )

    def _prompt_embeds_locked(
        self, normalized: dict[str, Any], report: Callable[[int, str], None]
    ) -> dict[str, Any]:
        """Phase 2: return `{"prompt", "negative"}` embeddings, host-resident.

        Caller must hold `self._lock`, and the pipeline must already be built and
        placed — the encoder is read into the host memory the transformer has
        just vacated. On a full cache hit the encoder is not touched at all: a
        mask edit, a new seed or a repeated prompt goes straight to the denoise.
        On a miss the encoder is loaded, used once and (when
        `unload_text_encoder_after_encode` is on) released again; the freed
        memory is measured, not assumed.

        `negative` is `None` unless classifier-free guidance is active.
        """
        missing = self._prompts_to_encode(normalized)
        if missing:
            self._encode_prompts_locked(normalized, missing, report)
        else:
            report(LOAD_STEP_ENCODER_DONE, "Промпт взят из кэша")

        embeds: dict[str, Any] = {"prompt": None, "negative": None}
        embeds["prompt"] = self._cached_embeds(normalized, normalized["prompt"])
        if float(normalized["guidance_scale"]) > 1.0:
            embeds["negative"] = self._cached_embeds(normalized, "")
        return embeds

    def _cached_embeds(self, normalized: dict[str, Any], text: str) -> Any:
        """Fetch one cached embedding and mark it most-recently used."""
        key = self._prompt_cache_key(normalized, text)
        value = self._prompt_cache[key]
        self._prompt_cache.move_to_end(key)
        return value

    def _encode_prompts_locked(
        self, normalized: dict[str, Any], texts: list[str], report: Callable[[int, str], None]
    ) -> None:
        """Load the encoder, encode `texts` into the cache, release the encoder.

        Caller must hold `self._lock`. **The encoder always encodes in HOST
        memory, in every placement.** It used to go on the accelerator under
        `full_gpu`, which was safe only while it ran BEFORE the transformer was
        loaded; now that the transformer is already resident on the card when
        this runs, the two would have to fit there together — 18.3 GB + 16.4 GB
        on a 34.2 GB card, i.e. they do not. The host is what the new order
        frees, so the host is where this phase belongs.

        This is also THE place a missing encoder becomes fatal: every path that
        reads the encoder goes through here, so the check cannot be bypassed by a
        caller that forgot it (the two public entry points check earlier only to
        avoid loading a pipeline they would then throw away).
        """
        require_text_encoder(normalized, what="кодирование промпта")

        import torch

        from diffusers import Flux2KleinInpaintPipeline
        from transformers import Qwen2TokenizerFast, Qwen3ForCausalLM

        dtype = torch.bfloat16 if normalized["dtype"] == "bfloat16" else torch.float16
        device = torch.device("cpu")
        encoder_key = self._encoder_key(normalized, str(device))

        report(LOAD_STEP_TEXT_ENCODER, "Загрузка текстового энкодера")
        encoder = self._text_encoder if self._text_encoder_key == encoder_key else None
        if encoder is None:
            self._release_text_encoder_locked()
            # No `device_map`: it exists to load straight into VRAM, and this
            # phase deliberately never touches the accelerator. `patched_module_to`
            # still wraps the load because `low_cpu_mem_usage` moves tensors with
            # `nn.Module.to` even between host allocations.
            with patched_module_to():
                encoder = _load_text_encoder(
                    Qwen3ForCausalLM,
                    normalized["text_encoder_path"],
                    dtype=dtype,
                    device_map=None,
                    low_cpu_mem_usage=normalized["low_cpu_mem_usage"],
                )
            if normalized["text_encoder_fp8"]:
                _quantize_text_encoder_fp8(encoder)

        roots = component_search_roots(normalized)
        tokenizer_dir = _require_component_dir(
            roots, _TOKENIZER_SUBDIR, _TOKENIZER_MARKERS, "токенизатор Qwen"
        )
        tokenizer = Qwen2TokenizerFast.from_pretrained(str(tokenizer_dir))

        report(LOAD_STEP_ENCODE, "Кодирование промпта")
        for text in texts:
            embeds = _encode_prompt_phase(
                Flux2KleinInpaintPipeline,
                encoder,
                tokenizer,
                text,
                int(normalized["max_sequence_length"]),
                device,
            )
            self._store_embeds(self._prompt_cache_key(normalized, text), embeds)

        if normalized["unload_text_encoder_after_encode"]:
            report(LOAD_STEP_ENCODER_DONE, "Выгрузка текстового энкодера")
            before = memory_snapshot(str(device))
            self._text_encoder = None
            self._text_encoder_key = None
            del encoder
            _clear_torch_cache()
            after = memory_snapshot(str(device))
            # Measured, not assumed: the release has to be visible in the host
            # figures before the denoise starts, or it did not happen.
            log.info(
                "FLUX.2 klein: текстовый энкодер выгружен после кодирования — освободилось "
                "%.2f ГиБ RAM и %.2f ГиБ VRAM.",
                (after["ram_free"] - before["ram_free"]) / (1024**3),
                (after["vram_free"] - before["vram_free"]) / (1024**3),
            )
        else:
            report(LOAD_STEP_ENCODER_DONE, "Текстовый энкодер оставлен в памяти")
            self._text_encoder = encoder
            self._text_encoder_key = encoder_key

    def _encoder_key(self, normalized: dict[str, Any], device: str) -> tuple[Any, ...]:
        """Identity of a resident text encoder: path, dtype, fp8 and where it sits."""
        return (
            normalized["text_encoder_path"],
            normalized["dtype"],
            bool(normalized["text_encoder_fp8"]),
            device,
        )

    def _store_embeds(self, key: tuple[Any, ...], value: Any) -> None:
        """Insert into the LRU prompt cache, evicting the oldest entry when full."""
        self._prompt_cache[key] = value
        self._prompt_cache.move_to_end(key)
        while len(self._prompt_cache) > PROMPT_EMBED_CACHE_ENTRIES:
            self._prompt_cache.popitem(last=False)

    def _release_text_encoder_locked(self) -> None:
        """Drop a resident text encoder, if any. Caller must hold `self._lock`."""
        if self._text_encoder is None:
            return
        self._text_encoder = None
        self._text_encoder_key = None
        _clear_torch_cache()

    # ---- pipeline ----
    def _ensure_pipeline_locked(
        self,
        normalized: dict[str, Any],
        model_key: str,
        report: Callable[[int, str], None],
        *,
        region_hw: tuple[int, int],
    ) -> Any:
        """Phase 1: return the cached pipeline for `model_key`, building it if needed.

        Caller must hold `self._lock`. This runs BEFORE the text encoder is read,
        so at this point the host holds nothing but this pipeline's own weights on
        their way to the accelerator. **The pipeline is built WITHOUT a text
        encoder** (`text_encoder=None`): the prompt is embedded in the next phase,
        the denoise never touches the encoder, and holding 8B of Qwen3 next to the
        9B transformer is precisely the residency this design removes.
        `Flux2KleinInpaintPipeline` tolerates the `None` — `pipe.components` only
        validates the KEY set, `DiffusionPipeline.device` and `_execution_device`
        skip non-modules, and `encode_prompt` is never reached because
        `prompt_embeds` is supplied.

        A pipeline built for another key is dropped and reported to the model
        manager first. Placement follows `normalized["placement"]` and routes
        every host->device weight move through `rocm_mmap_transfer` (a no-op off
        ROCm). `region_hw` is the `(height, width)` this pipeline is built for,
        kept for the log line that names what the residency is being spent on.

        # Raises
        `FileNotFoundError` when a component cannot be found, `ValueError` for an
        unsupported checkpoint layout (fp8_scaled), `RuntimeError` when the
        selected placement needs a GPU and none is available.
        """
        if self._pipe is not None and self._active_key == model_key:
            _apply_vae_memory_options(self._pipe, normalized)
            return self._pipe

        prev = self._active_key
        self._pipe = None
        self._active_key = None
        _clear_torch_cache()
        if prev is not None:
            self._model_manager.mark_unloaded(prev)

        import torch

        from diffusers import (
            AutoencoderKLFlux2,
            FlowMatchEulerDiscreteScheduler,
            Flux2KleinInpaintPipeline,
            Flux2Transformer2DModel,
        )
        from transformers import Qwen2TokenizerFast

        dtype = torch.bfloat16 if normalized["dtype"] == "bfloat16" else torch.float16
        device = torch.device(_resolve_selected_backend_device("cuda"))
        placement = normalized["placement"]
        if device.type == "cpu":
            if placement in _GPU_ONLY_PLACEMENTS:
                raise RuntimeError(
                    f"Режим размещения «{placement}» требует GPU, но доступен только CPU. "
                    "Выберите устройство в настройках или режим «full_gpu»."
                )
            log.warning(
                "FLUX.2 klein: GPU не найден, модель будет работать на CPU — это очень медленно."
            )

        region_height, region_width = region_hw
        log.info(
            "FLUX.2 klein: сборка пайплайна (трансформер + VAE, без текстового энкодера) для "
            "области %dx%d в режиме «%s» на %s.",
            region_width,
            region_height,
            normalized["placement"],
            device,
        )

        roots = component_search_roots(normalized)
        tokenizer_dir = _require_component_dir(
            roots, _TOKENIZER_SUBDIR, _TOKENIZER_MARKERS, "токенизатор Qwen"
        )
        scheduler_dir = _require_component_dir(
            roots, _SCHEDULER_SUBDIR, (_SCHEDULER_MARKER,), "планировщик (scheduler)"
        )

        # `device_map` is accelerate's direct-to-VRAM path: it never calls
        # `nn.Module.to`, so on ROCm it cannot be staged through
        # `rocm_mmap_transfer`. Measured 2026-09-02 on this project's ROCm host
        # (AMD Radeon AI PRO R9700 / gfx1201, torch 2.12.0+rocm7.2,
        # diffusers 0.39.0, accelerate 1.12.0) with the klein VAE (168 MB, 250
        # BF16 tensors, 42 of them >= 1 MiB), page cache dropped before each run,
        # two runs per variant: `device_map` 1.35 s / 1.12 s, CPU load + staged
        # `.to()` 1.38 s / 1.13 s, CPU load + UNSTAGED `.to()` 1.46 s / 1.27 s.
        # All 42 large tensors report `tensor_needs_staging() == True`, yet the
        # unstaged move itself costs 0.10 s, not the 42-84 s the amdkfd stall
        # would imply — the pathology does not reproduce through this loader on
        # this driver. Skipping the staging seam therefore costs nothing
        # measurable here; re-measure before assuming it still holds on another
        # ROCm host. `device_map` is also incompatible with accelerate's own
        # offload hooks, so the offload placements only get the
        # `low_cpu_mem_usage` kwarg itself.
        device_map: dict[str, str] | None = None
        if normalized["low_cpu_mem_usage"] and placement in ("full_gpu", "encoder_cpu"):
            device_map = {"": str(device)}
        elif normalized["low_cpu_mem_usage"]:
            log.info(
                "FLUX.2 klein: режим «%s» управляет размещением через accelerate, поэтому веса "
                "грузятся в обычную память, а low_cpu_mem_usage применяется только к самой загрузке.",
                placement,
            )

        report(LOAD_STEP_TRANSFORMER, "Загрузка трансформера")
        transformer = _load_transformer(
            Flux2Transformer2DModel,
            normalized["transformer_path"],
            dtype=dtype,
            device_map=device_map,
            low_cpu_mem_usage=normalized["low_cpu_mem_usage"],
        )

        # The tokenizer is a pipeline component even though the denoise never
        # uses it: `prompt_embeds` are already computed. It costs a few MB.
        report(LOAD_STEP_TOKENIZER, "Загрузка токенизатора")
        tokenizer = Qwen2TokenizerFast.from_pretrained(str(tokenizer_dir))

        report(LOAD_STEP_VAE, "Загрузка VAE")
        vae = _load_vae(
            AutoencoderKLFlux2,
            normalized["vae_path"],
            dtype=dtype,
            device_map=device_map,
            low_cpu_mem_usage=normalized["low_cpu_mem_usage"],
        )

        report(LOAD_STEP_SCHEDULER, "Загрузка планировщика")
        scheduler = FlowMatchEulerDiscreteScheduler.from_pretrained(str(scheduler_dir))

        # `is_distilled` is deliberately left at False: the pipeline uses it for
        # nothing but `do_classifier_free_guidance`
        # (`guidance_scale > 1 and not is_distilled`), and the transformer always
        # receives `guidance=None`. With the default `guidance_scale` of 1.0 the
        # run is therefore identical to a distilled configuration, while a user
        # who raises the scale gets real classifier-free guidance instead of a
        # silently ignored slider. Nothing about the user's checkpoint is assumed.
        pipe = Flux2KleinInpaintPipeline(
            scheduler=scheduler,
            vae=vae,
            # The prompt phase runs AFTER this build and hands its embeddings to
            # `__call__`; see this method's docstring for why the `None` is safe.
            text_encoder=None,
            tokenizer=tokenizer,
            transformer=transformer,
        )
        pipe.set_progress_bar_config(disable=True)
        _apply_vae_memory_options(pipe, normalized)

        report(LOAD_STEP_PLACEMENT, "Размещение модели")
        _apply_placement(pipe, placement, device)

        self._pipe = pipe
        self._device = device
        self._active_key = model_key
        return pipe

    # ---- warm-up: proof that the weights really left the host ----
    def _warmup_pipeline_locked(
        self, pipe: Any, normalized: dict[str, Any], report: Callable[[int, str], None]
    ) -> bool:
        """Force the placed weights to materialize, and prove that they did.

        Caller must hold `self._lock`. Returns whether a warm-up actually ran.

        This is the hinge of the new load order. The text encoder is read
        immediately afterwards, and the only reason it fits is that the
        transformer's 18 GB have left host memory by then. "Placed" is not the
        same as "materialized": `nn.Module.to` returns as soon as the copies are
        QUEUED, accelerate's `device_map` path can leave a `meta` parameter
        behind, and the host-side source pages are released only once the copy
        has retired. So the warm-up does three things, in order:

        1. checks that no parameter of the transformer or the VAE is still on
           `meta` or on the host (`_require_components_materialized`) — a named
           error here beats a device mismatch several frames inside the VAE;
        2. runs ONE tiny VAE decode (`WARMUP_LATENT_CELLS` latent cells, i.e. a
           64x64 image), which is a real forward: it retires the weight copies,
           initializes the caching allocator and, on ROCm, compiles the MIOpen
           convolution kernels the real decode will reuse;
        3. releases the transient blocks and logs the measured host/device
           figures, so the claim "the host is free now" is a measurement.

        It is a `phase:"load"` step (`LOAD_STEP_WARMUP`) and deliberately NOT a
        generation step: `_generate_locked` owns the `phase:"generate"` counter,
        and a warm-up counted there would make the progress bar report a step the
        user did not ask for.

        Skipped under the two accelerate offload placements: there the weights are
        SUPPOSED to sit in host memory between forwards, so there is nothing to
        materialize and a warm-up would drag all 9B onto the card and back for
        nothing. Also skipped when the pipeline exposes no VAE (a test double).
        """
        placement = normalized["placement"]
        if placement in ("model_cpu_offload", "sequential_cpu_offload"):
            log.debug(
                "FLUX.2 klein: прогрев пропущен — в режиме «%s» веса намеренно живут в "
                "оперативной памяти между проходами.",
                placement,
            )
            return False
        vae = getattr(pipe, "vae", None)
        if vae is None or not hasattr(vae, "decode"):
            return False

        _require_components_materialized(pipe, self._device)
        report(LOAD_STEP_WARMUP, "Прогрев модели")
        before = memory_snapshot(str(self._device))
        started_at = time.perf_counter()
        _warmup_vae_decode(pipe, self._device)
        _clear_torch_cache()
        after = memory_snapshot(str(self._device))
        log.info(
            "FLUX.2 klein: прогрев выполнен за %.2f с — веса трансформера и VAE материализованы на "
            "%s. Свободно: %.2f ГиБ RAM (было %.2f), %.2f ГиБ VRAM (было %.2f). Текстовый энкодер "
            "загружается следующим, уже в освободившуюся оперативную память.",
            time.perf_counter() - started_at,
            self._device,
            after["ram_free"] / (1024**3),
            before["ram_free"] / (1024**3),
            after["vram_free"] / (1024**3),
            before["vram_free"] / (1024**3),
        )
        return True

    def _unload_key(self, model_key: str) -> bool:
        """Eviction callback: drop the pipeline only if it still holds `model_key`."""
        with self._lock:
            if self._pipe is None or self._active_key != model_key:
                return False
            return self.unload()

    # ---- generation ----
    def _generate_locked(
        self,
        pipe: Any,
        region_rgb: np.ndarray,
        mask_u8: np.ndarray,
        normalized: dict[str, Any],
        embeds: dict[str, Any],
        progress_callback: ProgressCb | None,
    ) -> tuple[np.ndarray, dict[str, bool], bool]:
        """Run the pipeline once and composite the result over the region.

        `embeds` is the prompt phase's output: `{"prompt", "negative"}`, host-resident.
        The pipeline is ALWAYS called with `prompt=None` and those embeddings —
        it has no text encoder to fall back on, in any placement.

        Caller must hold `self._lock`. The LATENT mask handed to the pipeline is
        dilated by `mask_dilate_px` so the model has room to blend; the COMPOSITE
        uses the original mask feathered inwards by `mask_feather_px`, which is
        what keeps every pixel outside the mask byte-identical.

        Under `whole_region` this method needs no special case: normalization has
        already set `mask_dilate_px` to 0 (a solid mask has nothing to grow into)
        and `color_match` to `False` (there is no unchanged ring to match
        against) — see `_whole_region_overrides`. The feather still applies, and
        on a solid mask it ramps inwards from the region's own border, which is
        exactly the soft join to the rest of the page that mode wants.

        Denoising and decoding are two separate steps: the pipeline is asked for
        latents, a CPU copy of them is kept, and the VAE decode runs afterwards —
        optionally with the transformer parked off the GPU, and with an OOM
        recovery path that retries the decode instead of the whole run.

        Returns `(rgb, applied, oom_recovered)` where `applied` names the memory
        settings actually in force at the end and `oom_recovered` says whether a
        retry was needed.
        """
        import numpy as np
        import torch
        from PIL import Image

        height, width = region_rgb.shape[:2]
        latent_mask = _dilate_mask(mask_u8, normalized["mask_dilate_px"])

        seed = normalized["seed"]
        generator = torch.Generator("cpu")
        generator = generator.manual_seed(
            int(seed) if seed is not None else int.from_bytes(os.urandom(4), "little")
        )

        requested_steps = int(normalized["steps"])
        total_steps = effective_steps(requested_steps, float(normalized["strength"]))
        cb = progress_callback

        def _on_step(_pipe: Any, step: int, _t: Any, kwargs: dict[str, Any]) -> dict[str, Any]:
            if cb is not None:
                try:
                    cb("generate", int(step) + 1, total_steps, "Генерация")
                except Exception:  # noqa: BLE001 - a dead peer must not kill the run
                    pass
            return kwargs

        if cb is not None:
            cb("generate", 0, total_steps, "Генерация")

        call_kwargs: dict[str, Any] = {
            "image": Image.fromarray(region_rgb, "RGB"),
            "mask_image": Image.fromarray(latent_mask, "L"),
            "height": height,
            "width": width,
            "strength": float(normalized["strength"]),
            "num_inference_steps": requested_steps,
            "guidance_scale": float(normalized["guidance_scale"]),
            "max_sequence_length": int(normalized["max_sequence_length"]),
            "generator": generator,
            # Latents only: the VAE decode is a separate step so the transformer
            # can leave the GPU first, and the pipeline's own postprocess is
            # useless to us anyway — the composite, color match and feather are
            # ours.
            "output_type": "latent",
            "callback_on_step_end": _on_step,
            "callback_on_step_end_tensor_inputs": ["latents"],
        }

        # `self._device` is the placement target `_ensure_pipeline_locked` chose,
        # which is what the run must happen on; the probe is checked against it
        # rather than against a component's own device, because a component that
        # failed to be placed would otherwise define the target as wherever it
        # happens to sit.
        if normalized["placement"] in ("full_gpu", "encoder_cpu"):
            _require_execution_device(pipe, self._device)
        negative = embeds["negative"]
        result = pipe(
            prompt=None,
            prompt_embeds=embeds["prompt"].to(device=self._device),
            negative_prompt_embeds=None if negative is None else negative.to(device=self._device),
            **call_kwargs,
        )

        # Keep the latents in host memory before anything else touches the GPU:
        # at 1 MP they are a few hundred KiB, and holding them is what lets an
        # OOM in the decode be retried without repeating the denoise.
        latents_cpu = result.images.detach().to("cpu")

        decoded, applied, oom_recovered = self._decode_locked(pipe, latents_cpu, normalized)
        generated = np.ascontiguousarray(np.asarray(decoded.convert("RGB"), dtype=np.uint8))
        if generated.shape[:2] != (height, width):
            raise RuntimeError(
                f"Пайплайн вернул область {generated.shape[1]}x{generated.shape[0]} вместо "
                f"{width}x{height}; композит невозможен"
            )

        if normalized["color_match"]:
            generated = _match_color_outside_mask(generated, region_rgb, latent_mask)
        composed = _composite_over_region(
            region_rgb, generated, mask_u8, int(normalized["mask_feather_px"])
        )
        return composed, applied, oom_recovered

    def _decode_locked(
        self, pipe: Any, latents_cpu: Any, normalized: dict[str, Any]
    ) -> tuple[Any, dict[str, bool], bool]:
        """VAE-decode `latents_cpu`, parking the transformer and recovering from OOM.

        Caller must hold `self._lock`. When the transformer was moved off the
        device it is moved back before returning, so the resident pipeline still
        matches its model key and the next request is a plain cache hit.

        The move back can itself run out of memory (it is a full 9B host->device
        copy). That failure never reaches the caller: it must not mask a decode
        that already succeeded, and it must not mask a decode that already
        failed for a more informative reason. Instead the cached pipeline is
        invalidated — its transformer is on the host, so the next cache hit would
        skip placement and fail on a device mismatch — and the failure, with the
        decode error still in its `__context__` chain, goes to the log.
        """
        placement = normalized["placement"]
        parked = False

        def park() -> bool:
            """Take the transformer off the device once; `True` if it moved."""
            nonlocal parked
            if parked:
                return False
            moved = _park_transformer_off_device(pipe, placement)
            parked = parked or moved
            return moved

        if normalized["unload_transformer_before_vae"]:
            park()
        try:
            return _decode_region_latents(pipe, latents_cpu, normalized, park)
        finally:
            if parked:
                try:
                    _restore_transformer_to_device(pipe, self._device)
                except Exception as restore_exc:  # noqa: BLE001 - becomes an invalidation
                    self._invalidate_pipeline_locked(
                        "трансформер не удалось вернуть на устройство после декодирования VAE",
                        restore_exc,
                    )

    def _invalidate_pipeline_locked(self, reason: str, cause: BaseException) -> None:
        """Drop the cached pipeline after its state stopped matching its key.

        Caller must hold `self._lock`. `cause` is logged with its full chain (its
        `__context__` still carries whatever failed first, if anything did), and
        the model manager is told the key is gone, so the entry stops counting
        toward `max_loaded_models` and the next request rebuilds instead of
        taking a cache hit on a pipeline whose components are half-placed.
        """
        key = self._active_key
        self._pipe = None
        self._active_key = None
        _clear_torch_cache()
        if key is not None:
            self._model_manager.mark_unloaded(key)
        log.error(
            "FLUX.2 klein: %s. Кэш пайплайна сброшен (ключ %s), следующий запрос загрузит "
            "модель заново.",
            reason,
            key if key is not None else "—",
            exc_info=cause,
        )


# =====================================================================
#  Pipeline construction helpers
# =====================================================================
def _load_transformer(
    model_cls: Any,
    path: str,
    *,
    dtype: Any,
    device_map: dict[str, str] | None,
    low_cpu_mem_usage: bool,
) -> Any:
    """Load `Flux2Transformer2DModel` from a diffusers folder or a single file.

    Two things hold for BOTH inputs and are handled before the branch:
    - klein has no `guidance_in` block, so `guidance_embeds=False` must override
      whatever config is resolved — a diffusers folder carrying the flux-2-dev
      value of `true` would otherwise build the wrong architecture;
    - an fp8_scaled checkpoint is refused up front, sharded folders included, so
      the failure costs a header read instead of a partial multi-GiB load.

    A single file additionally goes through `from_single_file`, where diffusers
    detects every flux2 checkpoint as `flux-2-dev` and would fetch that (gated,
    and simply DIFFERENT) repo's config from the Hub. That path is blocked: a
    local config directory must be found next to the checkpoint, otherwise the
    load is refused. There is no reconstruction of the config from the weights —
    `rope_theta`, `eps` and `patch_size` do not follow from any tensor shape, and
    a model built with the wrong `rope_theta` has exactly the same shapes and
    quietly produces wrong images.

    The two loaders also take the target device through DIFFERENT kwargs, which
    is why `device_map` is only forwarded to the directory branch; see
    `_single_file_device`.

    # Raises
    `ValueError` for an fp8_scaled checkpoint (unsupported by diffusers 0.39) or
    for a config that belongs to another model class; `FileNotFoundError` when a
    single-file checkpoint has no transformer config next to it.
    """
    source = Path(path)
    kwargs: dict[str, Any] = {"torch_dtype": dtype, "low_cpu_mem_usage": low_cpu_mem_usage}
    # `guidance_embeds` lands in the model kwargs that override the resolved
    # config; klein has no guidance embedder. It belongs to both loaders, so it
    # is set before the directory branch returns.
    kwargs["guidance_embeds"] = False

    if source.is_dir():
        _reject_fp8_scaled_directory(source)
        if device_map is not None:
            kwargs["device_map"] = device_map
        return model_cls.from_pretrained(str(source), **kwargs)

    header = read_safetensors_header(source)
    if is_fp8_scaled_checkpoint(header):
        raise ValueError(_fp8_scaled_message(source))

    config_dir = find_transformer_config_dir(source)
    if config_dir is None:
        raise FileNotFoundError(_missing_transformer_config_message(source))
    validate_transformer_config_dir(config_dir)
    # `config` as a local directory is what keeps the loader off the Hub: with it
    # set, diffusers never calls `fetch_diffusers_config`, which is where the
    # flux-2-dev repo id comes from. `local_files_only` closes the door a second
    # time, so a future loader change cannot silently reopen the network path.
    kwargs["config"] = str(config_dir)
    kwargs["local_files_only"] = True
    single_file_device = _single_file_device(device_map)
    if single_file_device is not None:
        kwargs["device"] = single_file_device
    log.info("FLUX.2 klein: конфиг трансформера взят из %s", config_dir)
    return model_cls.from_single_file(str(source), **kwargs)


def _single_file_device(device_map: dict[str, str] | None) -> str | None:
    """Translate a whole-model `device_map` into the kwarg `from_single_file` honours.

    diffusers 0.39's single-file loader accepts `device_map`, pops it, and then
    throws it away: `single_file_model.py` reassigns `device_map = None` right
    before the `low_cpu_mem_usage` branch and rebuilds it from its own separate
    `device` kwarg, which defaults to the CPU. Passing `device_map` there is
    therefore silently ignored — an 18 GB transformer stays in host RAM while the
    caller believes it is on the accelerator. Returns the single device the map
    names, or `None` when there is no map or it is not one whole-model entry (in
    which case the caller must place the weights itself).
    """
    if not device_map:
        return None
    if set(device_map) != {""}:
        return None
    return str(device_map[""])


def component_dir_for_path(path: Path) -> Path:
    """Directory of a diffusers component, given a folder OR a file inside it.

    `AutoencoderKLFlux2` and the Qwen3 encoder have no single-file loader, so a
    weights file is only usable through the folder that carries its
    `config.json`. Users pick the `.safetensors` file far more often than its
    folder, and the folder holding a file IS the component, so the file is
    normalized to its parent instead of being refused. Anything else (a folder,
    or a file with no sibling config) is returned unchanged, leaving the caller
    to produce its own diagnosis.
    """
    if path.is_file() and (path.parent / _MODEL_CONFIG_MARKER).is_file():
        return path.parent
    return path


def _load_text_encoder(
    model_cls: Any,
    path: str,
    *,
    dtype: Any,
    device_map: dict[str, str] | None,
    low_cpu_mem_usage: bool,
) -> Any:
    """Load the Qwen3 text encoder from a transformers/diffusers folder.

    A file path pointing INTO such a folder is accepted and normalized to it;
    see `component_dir_for_path`.

    The dtype goes through `dtype=`, not `torch_dtype=`: this is the one
    transformers loader here, and transformers 4.57 deprecated the old spelling
    (`modeling_utils.py`, "`torch_dtype` is deprecated! Use `dtype` instead!").
    The diffusers loaders in this module keep `torch_dtype` — diffusers 0.39
    recognizes nothing else.
    """
    source = component_dir_for_path(Path(path))
    if not source.is_dir():
        raise ValueError(
            f"Путь текстового энкодера должен быть каталогом в формате transformers: {source}"
        )
    kwargs: dict[str, Any] = {"dtype": dtype, "low_cpu_mem_usage": low_cpu_mem_usage}
    if device_map is not None:
        kwargs["device_map"] = device_map
    return model_cls.from_pretrained(str(source), **kwargs)


def _load_vae(
    model_cls: Any,
    path: str,
    *,
    dtype: Any,
    device_map: dict[str, str] | None,
    low_cpu_mem_usage: bool,
) -> Any:
    """Load `AutoencoderKLFlux2` from a diffusers folder (or try a single file).

    diffusers 0.39 registers no single-file mapping for this class. A file that
    sits next to a `config.json` is normalized to its folder
    (`component_dir_for_path`), which is what a user selecting
    `diffusion_pytorch_model.safetensors` means. A file without that sibling is
    still attempted, and the loader failure is translated into an actionable
    message rather than surfacing as an internal error.
    """
    source = component_dir_for_path(Path(path))
    kwargs: dict[str, Any] = {"torch_dtype": dtype, "low_cpu_mem_usage": low_cpu_mem_usage}
    if device_map is not None:
        kwargs["device_map"] = device_map
    if source.is_dir():
        return model_cls.from_pretrained(str(source), **kwargs)
    try:
        return model_cls.from_single_file(str(source), **kwargs)
    except Exception as exc:
        raise ValueError(
            f"Не удалось загрузить VAE из файла {source.name}: {exc}. Укажите каталог VAE в "
            "формате diffusers (config.json + diffusion_pytorch_model.safetensors)."
        ) from exc


def _apply_vae_memory_options(pipe: Any, options: dict[str, Any]) -> None:
    """Enable/disable VAE tiling and slicing to match `options`.

    `options` is any mapping carrying `vae_tiling` / `vae_slicing` — the
    normalized params, or the `applied` dict during OOM recovery. Applied on
    every request, including a cache hit, so toggling the option in the UI takes
    effect without rebuilding the pipeline.
    """
    vae = getattr(pipe, "vae", None)
    if vae is None:
        return
    for enabled, enable_name, disable_name in (
        (options["vae_tiling"], "enable_tiling", "disable_tiling"),
        (options["vae_slicing"], "enable_slicing", "disable_slicing"),
    ):
        method = getattr(vae, enable_name if enabled else disable_name, None)
        if callable(method):
            method()


def _apply_placement(pipe: Any, placement: str, device: Any) -> None:
    """Put the pipeline's weights where `placement` says they belong.

    The move is UNCONDITIONAL for the two non-offload placements, even when the
    components were asked to load straight onto `device`: `nn.Module.to` is a
    no-op per tensor that is already there, so the cost of being sure is nil,
    while trusting the loader kwargs is not safe. diffusers 0.39's
    `from_single_file` accepts `device_map` and discards it (see
    `_single_file_device`), which used to leave the transformer in host memory
    while this function skipped its move — the pipeline then reported `cpu` as
    its execution device and the run died inside the VAE's first `conv2d` with a
    CPU input against CUDA weights.

    Every move is wrapped in `patched_module_to()`, whose contract allows the
    weight transfer and nothing else inside the block.
    """
    if placement == "full_gpu":
        with patched_module_to():
            pipe.to(device)
    elif placement == "encoder_cpu":
        # Named for the era when the pipeline still carried the text encoder. It
        # does not any more, so this moves exactly what `full_gpu` moves — the
        # two components are the whole pipeline — and the two branches stay
        # separate only because a component-wise move is what makes
        # `_require_execution_device`'s per-component report meaningful when a
        # loader has ignored its placement kwarg.
        with patched_module_to():
            pipe.transformer.to(device)
            pipe.vae.to(device)
    elif placement == "model_cpu_offload":
        _materialize_components_for_offload(pipe, device)
        pipe.enable_model_cpu_offload(device=str(device))
    elif placement == "sequential_cpu_offload":
        _materialize_components_for_offload(pipe, device)
        pipe.enable_sequential_cpu_offload(device=str(device))
    else:  # pragma: no cover - normalization already rejected everything else
        raise ValueError(f"Неизвестный режим размещения: {placement!r}")


#: Components whose weights must be on the accelerator once placement is done.
#: The tokenizer and the scheduler carry no tensors, and the text encoder is not
#: part of the pipeline at all.
_PLACED_COMPONENTS = ("transformer", "vae")


def _require_components_materialized(pipe: Any, device: Any) -> None:
    """Refuse a placement that only LOOKS done: weights still on host or `meta`.

    Called right after `_apply_placement`, before the text encoder is read. It
    exists because "placed" and "materialized" are different claims and this
    module has already been bitten by the difference twice: diffusers 0.39's
    `from_single_file` accepts `device_map` and discards it, and accelerate's
    `device_map` path can leave a parameter on `meta` when a shard fails to map.
    Either way the run dies much later, several frames inside the VAE, naming
    neither the component nor the placement — and, worse for the new load order,
    the host memory the text encoder is about to need was never actually freed.

    Compares device TYPES, not full specs: `cuda` and `cuda:0` are the same card
    for this purpose, and the ordinal is already fixed by `_apply_placement`. A
    component with no `parameters()` (a scheduler, a test double) is skipped.

    # Raises
    `RuntimeError` naming each component that is not fully on `device`, with the
    number of offending tensors and the devices they sit on.
    """
    target = getattr(device, "type", None) or str(device or "").split(":")[0]
    if not target:
        return
    offenders: list[str] = []
    for name in _PLACED_COMPONENTS:
        module = getattr(pipe, name, None)
        parameters = getattr(module, "parameters", None)
        buffers = getattr(module, "buffers", None)
        if not callable(parameters) or not callable(buffers):
            continue
        strays: dict[str, int] = {}
        for tensor in list(parameters()) + list(buffers()):
            where = getattr(getattr(tensor, "device", None), "type", None)
            if where is not None and where != target:
                strays[where] = strays.get(where, 0) + 1
        if strays:
            detail = ", ".join(f"{count} на «{where}»" for where, count in sorted(strays.items()))
            offenders.append(f"{name}: {detail}")
    if not offenders:
        return
    raise RuntimeError(
        f"FLUX.2 klein: после размещения часть весов осталась не на «{device}» — {'; '.join(offenders)}. "
        "Запуск остановлен здесь, а не внутри VAE: загрузчик проигнорировал параметр размещения, "
        "и оперативная память, которая нужна текстовому энкодеру следующим шагом, не освободилась."
    )


def _warmup_vae_decode(pipe: Any, device: Any) -> bool:
    """Run one tiny VAE decode so the placed weights actually retire on `device`.

    Returns whether the decode ran. The latent is `WARMUP_LATENT_CELLS` cells
    square, i.e. a 64x64 image at this VAE's scale factor — a real forward, but
    a few hundred KiB of activations. It is what turns queued host->device copies
    into completed ones, primes the caching allocator, and on ROCm compiles the
    MIOpen convolution kernels the real decode then reuses.

    When the VAE's config does not name `latent_channels` there is no way to
    synthesize a valid input, so the decode is skipped and only the device
    synchronization is performed — the part that actually governs when the host
    pages are released. That is logged rather than silent, because a klein VAE
    always carries the field and its absence means the component is not what this
    service thinks it is.

    `torch.no_grad()` is explicit for the same reason as in `_decode_once`: this
    call does not go through `pipeline.__call__`, where diffusers puts the
    decorator.
    """
    import torch

    vae = pipe.vae
    channels = getattr(getattr(vae, "config", None), "latent_channels", None)
    if not isinstance(channels, int) or isinstance(channels, bool) or channels <= 0:
        log.warning(
            "FLUX.2 klein: у VAE нет config.latent_channels — прогревочный проход пропущен, "
            "выполнена только синхронизация устройства."
        )
        _synchronize_device(device)
        return False

    with torch.no_grad():
        latents = torch.zeros(
            (1, int(channels), WARMUP_LATENT_CELLS, WARMUP_LATENT_CELLS),
            dtype=vae.dtype,
            device=_vae_input_device(pipe),
        )
        vae.decode(latents, return_dict=False)
    _synchronize_device(device)
    return True


def _synchronize_device(device: Any) -> None:
    """Block until every queued kernel and copy on `device` has retired.

    A host->device weight copy is asynchronous, so `nn.Module.to` can return
    while the source pages are still live in host memory. The new load order
    depends on those pages being gone before the text encoder is read, which is
    what this waits for. A strict no-op on CPU and wherever torch has no
    accelerator.
    """
    kind = getattr(device, "type", None) or str(device or "").split(":")[0]
    if kind != "cuda":
        return
    try:
        import torch

        if torch.cuda.is_available():
            torch.cuda.synchronize()
    except Exception as exc:  # noqa: BLE001 - a missing accelerator must not fail a run
        log.debug("FLUX.2 klein: синхронизация устройства недоступна (%s)", exc)


def _encode_prompt_phase(
    pipeline_cls: Any,
    text_encoder: Any,
    tokenizer: Any,
    prompt: str,
    max_sequence_length: int,
    device: Any,
) -> Any:
    """Encode one prompt with nothing but the encoder loaded; returns CPU embeddings.

    The prompt phase of a run. The encoder is used exactly once and must not be alive at
    the same time as the 9B transformer, so the encoding cannot go through the
    run pipeline — that pipeline does not exist yet. A pipeline instance holding
    ONLY the encoder and its tokenizer is built instead: every other component is
    `None`, which `Flux2KleinInpaintPipeline.__init__` tolerates (its two uses of
    `self.vae` are guarded by `getattr(self, "vae", None)`), and `encode_prompt`
    needs nothing else.

    Going through the pipeline's own `encode_prompt` rather than reimplementing
    it is deliberate: the Qwen3 chat template, the attention mask and the
    `text_encoder_out_layers` default all live there, and a second copy of them
    would drift from the version diffusers actually denoises with.

    The result is moved to the HOST before it is returned, so the prompt cache
    never pins device memory; the caller moves it to the run device.

    `torch.no_grad()` is explicit for the same reason as in `_decode_once`: this
    call does not go through `pipeline.__call__`, which is where diffusers puts
    the decorator, and an 8B forward pass that builds an autograd graph would
    both waste memory and hand the cache tensors that require grad.
    """
    import torch

    holder = pipeline_cls(
        scheduler=None, vae=None, text_encoder=text_encoder, tokenizer=tokenizer, transformer=None
    )
    with torch.no_grad():
        prompt_embeds, _text_ids = holder.encode_prompt(
            prompt=prompt or "", device=device, max_sequence_length=max_sequence_length
        )
    return prompt_embeds.detach().to("cpu")


def _quantize_text_encoder_fp8(text_encoder: Any) -> int:
    """Replace the encoder's `nn.Linear` weights with float8_e4m3fn + row scales.

    Weight-only quantization done with torch alone — no torchao, bitsandbytes or
    quanto, none of which is installed in this project's environment. Each linear
    keeps a per-output-row scale so one outlier channel cannot flatten the rest,
    and the weight is dequantized to the compute dtype inside `forward`. Measured
    on this project's ROCm host (gfx1201, torch 2.12.0+rocm7.2) the quantize /
    dequantize round trip costs a relative max error of ~3.4% per weight tensor.

    Returns the number of bytes saved. **It does not lower the load peak**: the
    bf16 weights must exist before they can be quantized, so it pays off only
    while the encoder stays resident (`unload_text_encoder_after_encode=False`).

    # Raises
    `RuntimeError` when the running torch build has no `float8_e4m3fn`. The flag
    is never silently ignored: the user would be billed for a quality trade they
    did not actually get.
    """
    import torch

    fp8_dtype = getattr(torch, "float8_e4m3fn", None)
    if fp8_dtype is None:
        raise RuntimeError(
            f"fp8 для текстового энкодера не поддерживается этой сборкой torch "
            f"({torch.__version__}): нет типа float8_e4m3fn. Отключите параметр "
            "«fp8 для текстового энкодера»."
        )

    class _Fp8Linear(torch.nn.Module):
        """`nn.Linear` whose weight is stored as float8 with per-output-row scales.

        The class is built here rather than at module level because this module
        must import without torch. `bytes_saved` reports how much one swap freed;
        the bias stays in the original dtype, being tiny and the most numerically
        sensitive part of the layer.
        """

        def __init__(self, source: torch.nn.Linear) -> None:
            super().__init__()
            weight = source.weight.data
            self.compute_dtype = weight.dtype
            # 448 is the largest finite magnitude of float8_e4m3fn; scaling each
            # output row to that maximum keeps the row's full dynamic range.
            scale = weight.abs().amax(dim=1, keepdim=True).clamp(min=1e-6) / 448.0
            self.register_buffer("weight_fp8", (weight / scale).to(fp8_dtype))
            self.register_buffer("weight_scale", scale.to(weight.dtype))
            self.bias = source.bias
            self.bytes_saved = weight.numel() * weight.element_size() - (
                self.weight_fp8.numel() * self.weight_fp8.element_size()
                + self.weight_scale.numel() * self.weight_scale.element_size()
            )

        def forward(self, x: torch.Tensor) -> torch.Tensor:
            weight = self.weight_fp8.to(self.compute_dtype) * self.weight_scale
            return torch.nn.functional.linear(x, weight, self.bias)

    saved = 0
    swapped = 0
    for parent in list(text_encoder.modules()):
        for name, child in list(parent.named_children()):
            if not isinstance(child, torch.nn.Linear):
                continue
            replacement = _Fp8Linear(child)
            saved += replacement.bytes_saved
            # Swapping in place drops the last reference to the bf16 weight, so
            # the peak stays one layer above the original size, not twice it.
            setattr(parent, name, replacement)
            swapped += 1
    log.info(
        "FLUX.2 klein: текстовый энкодер квантован в fp8 (%d линейных слоёв, освобождено %.2f ГиБ). "
        "На ПИК памяти это не влияет — экономия видна только пока энкодер остаётся резидентным.",
        swapped,
        saved / (1024**3),
    )
    return saved


def _require_execution_device(pipe: Any, device: Any) -> None:
    """Check the pipeline will build its tensors on `device`, or raise saying why.

    `_execution_device` is what `__call__` uses to place the region image, the
    noise and the mask latents, and it is derived from the components rather
    than passed in: with no accelerate hooks it degrades to the device of the
    first component in sorted signature order. A component left on the host
    therefore turns into a `conv2d` "Input type (CPUBFloat16Type) and weight
    type (CUDABFloat16Type)" several frames deep inside the VAE, naming neither
    the component nor the placement. This turns the same condition into one
    sentence the user can act on, and is a no-op whenever placement did its job.
    A pipeline without the property (a test double) is not probed.

    # Raises
    `RuntimeError` naming every component's device when the probe disagrees with
    `device`.
    """
    probed = getattr(pipe, "_execution_device", None)
    if probed is None or str(probed) == str(device):
        return
    devices = ", ".join(
        f"{name}={getattr(getattr(pipe, name, None), 'device', 'нет')}"
        for name in ("transformer", "vae", "text_encoder")
    )
    raise RuntimeError(
        f"FLUX.2 klein: пайплайн собирается считать на «{probed}», хотя трансформер размещён на "
        f"«{device}». Размещение компонентов: {devices}. Запуск остановлен до обращения к модели — "
        "иначе ошибка всплыла бы внутри VAE как несовпадение устройств."
    )


# =====================================================================
#  VAE decode: transformer parking and OOM recovery
# =====================================================================
def _is_out_of_memory(exc: BaseException) -> bool:
    """Whether `exc` is an accelerator out-of-memory failure.

    Two shapes have to be recognised: `torch.OutOfMemoryError` (and its
    `torch.cuda` alias) on recent builds, and a plain `RuntimeError` whose text
    contains "out of memory", which is what ROCm/HIP and older builds raise.
    """
    try:
        import torch
    except ImportError:  # pragma: no cover - torch is present whenever we decode
        return isinstance(exc, RuntimeError) and "out of memory" in str(exc).lower()

    for holder in (torch, getattr(torch, "cuda", None)):
        oom_type = getattr(holder, "OutOfMemoryError", None)
        if isinstance(oom_type, type) and isinstance(exc, oom_type):
            return True
    return isinstance(exc, RuntimeError) and "out of memory" in str(exc).lower()


def _park_transformer_off_device(pipe: Any, placement: str) -> bool:
    """Free the transformer's device memory before the VAE decode.

    Returns whether the module was actually moved, i.e. whether the caller owes
    it a move back. Under the two accelerate offload placements it never is:
    the hooks already returned the transformer to host memory after the last
    forward, so all that is left to do is release the allocator's blocks.
    """
    _clear_torch_cache()
    if placement in ("model_cpu_offload", "sequential_cpu_offload"):
        return False
    transformer = getattr(pipe, "transformer", None)
    if transformer is None:
        return False
    if getattr(getattr(transformer, "device", None), "type", "cpu") == "cpu":
        return False
    transformer.to("cpu")
    _clear_torch_cache()
    return True


def _restore_transformer_to_device(pipe: Any, device: Any) -> None:
    """Move a parked transformer back so the cached pipeline stays usable.

    Dropping it instead would force a multi-second reload from disk on the next
    request and leave `_active_key` describing a pipeline that no longer exists;
    the move back is a plain host->device copy out of anonymous memory.

    # Raises
    Whatever the copy raises — at 9B it can itself run out of memory. The caller
    owns that case: see `Flux2KleinInpaintService._decode_locked`.
    """
    transformer = getattr(pipe, "transformer", None)
    if transformer is None or device is None:
        return
    with patched_module_to():
        transformer.to(device)


def _decode_once(pipe: Any, latents_cpu: Any) -> Any:
    """Decode a CPU copy of the latents into one PIL image.

    The latents go to `_vae_input_device`, which is the VAE's own device except
    under accelerate's sequential offload, where the parameters live on `meta`
    between forwards.

    `torch.no_grad()` is explicit here because this decode is deliberately NOT
    inside `pipeline.__call__`, which carries the decorator: without it the VAE
    builds an autograd graph over a full-resolution image and
    `image_processor.postprocess` dies on `Can't call numpy() on Tensor that
    requires grad`.
    """
    import torch

    with torch.no_grad():
        latents = latents_cpu.to(device=_vae_input_device(pipe), dtype=pipe.vae.dtype)
        image = pipe.vae.decode(latents, return_dict=False)[0]
        return pipe.image_processor.postprocess(image, output_type="pil")[0]


def _vae_input_device(pipe: Any) -> Any:
    """Device the VAE's `decode` input must be on.

    Two different answers, and picking the wrong one is a device mismatch several
    frames inside torch:

    - **No accelerate hook** (`full_gpu`, `encoder_cpu`): the VAE's OWN device.
      Not the pipeline's execution device — by decode time the transformer may
      have been parked on the host, which is exactly what
      `unload_transformer_before_vae` does, and `_execution_device` would then
      answer `cpu` while the VAE sits on the accelerator.
    - **Under an accelerate offload hook** (`model_cpu_offload`,
      `sequential_cpu_offload`): the pipeline's execution device. `vae.device` is
      `cpu` or `meta` there, and diffusers' `@apply_forward_hook` calls
      `pre_forward(self)` WITHOUT the arguments, so the hook moves the WEIGHTS to
      the accelerator and leaves our latents behind — a CPU input against CUDA
      weights, or "Cannot copy out of meta tensor" if we followed `meta`.
    """
    vae = getattr(pipe, "vae", None)
    device = getattr(vae, "device", None)
    hooked = hasattr(vae, "_hf_hook")
    if not hooked and device is not None and getattr(device, "type", None) != "meta":
        return device
    return pipe._execution_device


def _decode_region_latents(
    pipe: Any,
    latents_cpu: Any,
    normalized: dict[str, Any],
    park_transformer: Callable[[], bool],
) -> tuple[Any, dict[str, bool], bool]:
    """Decode the latents, escalating memory savings on an out-of-memory failure.

    The denoise is never repeated: every attempt starts from the same host copy
    of the latents. Escalation order — park the transformer, then enable VAE
    tiling and slicing. Returns `(image, applied, oom_recovered)`.

    # Raises
    `RuntimeError` with the free-memory figures when even the last attempt runs
    out of memory, and re-raises unchanged anything that is not an OOM.
    """
    applied = {
        "unload_transformer_before_vae": bool(normalized["unload_transformer_before_vae"]),
        "vae_tiling": bool(normalized["vae_tiling"]),
        "vae_slicing": bool(normalized["vae_slicing"]),
        # Not touched by the OOM ladder, but part of the same "what actually ran"
        # answer the Rust side persists back into the settings.
        "unload_text_encoder_after_encode": bool(normalized["unload_text_encoder_after_encode"]),
        "text_encoder_fp8": bool(normalized["text_encoder_fp8"]),
    }
    try:
        return _decode_once(pipe, latents_cpu), applied, False
    except Exception as exc:  # noqa: BLE001 - re-raised below unless it is an OOM
        if not _is_out_of_memory(exc):
            raise
        last_error: BaseException = exc
        before = memory_snapshot()
        log.warning(
            "FLUX.2 klein: VAE decode ran out of memory (%s). Free VRAM %d B of %d B; "
            "recovering without repeating the denoise (settings were "
            "unload_transformer_before_vae=%s, vae_tiling=%s, vae_slicing=%s).",
            exc,
            before["vram_free"],
            before["vram_total"],
            applied["unload_transformer_before_vae"],
            applied["vae_tiling"],
            applied["vae_slicing"],
        )

    # Step 1: get the transformer out of the way and retry.
    if not applied["unload_transformer_before_vae"]:
        park_transformer()
        applied["unload_transformer_before_vae"] = True
        after = memory_snapshot()
        log.warning(
            "FLUX.2 klein: retrying the VAE decode with the transformer parked on the host "
            "(free VRAM %d B).",
            after["vram_free"],
        )
        try:
            return _decode_once(pipe, latents_cpu), applied, True
        except Exception as exc:  # noqa: BLE001 - checked immediately below
            if not _is_out_of_memory(exc):
                raise
            last_error = exc

    # Step 2: cut the decode's own peak with tiling and slicing.
    if not (applied["vae_tiling"] and applied["vae_slicing"]):
        applied["vae_tiling"] = True
        applied["vae_slicing"] = True
        _apply_vae_memory_options(pipe, applied)
        after = memory_snapshot()
        log.warning(
            "FLUX.2 klein: retrying the VAE decode with tiling and slicing enabled "
            "(free VRAM %d B).",
            after["vram_free"],
        )
        try:
            return _decode_once(pipe, latents_cpu), applied, True
        except Exception as exc:  # noqa: BLE001 - checked immediately below
            if not _is_out_of_memory(exc):
                raise
            last_error = exc

    final = memory_snapshot()
    raise RuntimeError(
        "Не хватило видеопамяти на декодирование VAE даже после выгрузки трансформера и "
        f"включения тайлинга. Свободно {final['vram_free']} байт из {final['vram_total']}; "
        f"уменьшите выделенную область или выберите режим размещения с меньшим расходом "
        f"видеопамяти. Исходная ошибка: {last_error}"
    ) from last_error


# =====================================================================
#  ROCm staging for the offload placements
# =====================================================================
def _largest_cpu_tensor(module: Any) -> Any:
    """Largest CPU-resident parameter or buffer of `module`, or `None`.

    Used as the probe for `_component_is_file_backed`. Only CPU tensors qualify:
    a component that already sits on the GPU has nothing left to re-home.
    """
    largest = None
    largest_bytes = -1
    for tensor in list(module.parameters()) + list(module.buffers()):
        if getattr(getattr(tensor, "device", None), "type", None) != "cpu":
            continue
        nbytes = int(tensor.numel()) * int(tensor.element_size())
        if nbytes > largest_bytes:
            largest = tensor
            largest_bytes = nbytes
    return largest


def _component_is_file_backed(module: Any) -> bool:
    """Whether `module`'s weights still live in the safetensors file mapping.

    Only such weights hit the amdkfd stall, so only they are worth the round
    trip. A component is loaded in one pass from its own file, so its largest
    CPU tensor is representative of all of them.
    """
    probe = _largest_cpu_tensor(module)
    if probe is None:
        return False
    return tensor_needs_staging(probe)


def _materialize_components_for_offload(pipe: Any, device: Any) -> None:
    """Re-home the safetensors components in anonymous host memory before offload.

    Accelerate's offload hooks move a component to the GPU lazily from inside the
    forward pass. On ROCm that first lazy move copies straight out of the
    safetensors mapping and stalls in amdkfd (~1-2 s per tensor of >=1 MiB), and
    `patched_module_to` cannot be held there — it is process-global and its
    contract forbids wrapping inference. So each component makes one staged
    round trip now, which leaves its resident CPU copy in freshly allocated
    anonymous memory.

    Skipped entirely off ROCm, and per component when its weights are no longer
    file-backed. A failed move is a lost optimization, not a lost model: it is
    logged and the load continues. The transformer is attempted last because it
    is the one component whose round trip can plausibly exhaust VRAM.

    This mirrors `flux_fill.py`'s helper; the two are deliberate copies rather
    than a shared import, because `inpaint/MODULE_README.md` forbids a service in
    this package from importing a sibling service.
    """
    if not mmap_staging_required():
        return

    import torch

    started_at = time.perf_counter()
    rehomed: list[str] = []
    skipped: list[str] = []
    for name in _MMAP_BACKED_COMPONENTS:
        module = getattr(pipe, name, None)
        if not isinstance(module, torch.nn.Module):
            continue
        if not _component_is_file_backed(module):
            skipped.append(name)
            continue
        try:
            with patched_module_to():
                module.to(device)
            # The way back allocates fresh anonymous host memory for every
            # tensor — that copy is the whole point of the round trip.
            module.to("cpu")
        except (RuntimeError, MemoryError) as exc:
            log.warning(
                "FLUX.2 klein: could not re-home component %r in anonymous host memory before "
                "CPU offload (%s). The model still works, but on ROCm the first generation may "
                "stall in the mmap->GPU weight copy.",
                name,
                exc,
            )
            break
        # Release this component's device blocks before the next, larger one is
        # staged: the allocator would otherwise reserve on top of them.
        _clear_torch_cache()
        rehomed.append(name)

    _clear_torch_cache()
    if rehomed:
        log.info(
            "FLUX.2 klein: re-homed %s in anonymous host memory in %.2f s before enabling CPU "
            "offload (ROCm mmap->GPU stall workaround).",
            ", ".join(rehomed),
            time.perf_counter() - started_at,
        )
    if skipped:
        log.debug(
            "FLUX.2 klein: %s already live in anonymous host memory; skipped the round trip.",
            ", ".join(skipped),
        )


# =====================================================================
#  Post-processing (ours, not the pipeline's)
# =====================================================================
def _match_color_outside_mask(
    generated: np.ndarray, original: np.ndarray, mask: np.ndarray
) -> np.ndarray:
    """Align the generated region's per-channel mean/std to the original.

    The statistics are taken over the pixels OUTSIDE `mask`, i.e. the ring the
    model was not allowed to change: the VAE round trip shifts the whole window's
    tone, and that ring is the only place where the two images are supposed to be
    identical. Returns `generated` unchanged when the ring is too small to give
    meaningful statistics.
    """
    import numpy as np

    outside = mask == 0
    sample_count = int(np.count_nonzero(outside))
    if sample_count < _MIN_COLOR_MATCH_SAMPLES:
        log.debug(
            "FLUX.2 klein: only %d pixels outside the mask, skipping the color match.",
            sample_count,
        )
        return generated

    reference = original[outside].astype(np.float32)
    produced = generated[outside].astype(np.float32)
    matched = generated.astype(np.float32)
    for channel in range(3):
        ref_mean = float(reference[:, channel].mean())
        ref_std = float(reference[:, channel].std())
        gen_mean = float(produced[:, channel].mean())
        gen_std = float(produced[:, channel].std())
        if gen_std < 1e-3:
            # A flat channel carries no scale to correct; shift it only.
            matched[..., channel] += ref_mean - gen_mean
        else:
            matched[..., channel] = (matched[..., channel] - gen_mean) * (
                ref_std / gen_std
            ) + ref_mean
    return np.clip(matched, 0, 255).astype(np.uint8)


def _composite_over_region(
    original: np.ndarray, generated: np.ndarray, mask: np.ndarray, feather_px: int
) -> np.ndarray:
    """Alpha-blend `generated` into `original` under a feathered `mask`.

    The feather is applied INWARDS (see `_feather_mask_inwards`), so the blend
    weight is exactly zero on every pixel the user did not paint. The final
    `np.where` then guarantees those pixels come back byte-identical, which is
    this service's core contract.

    The blend is ROUNDED, not truncated. Truncation biases every blended pixel
    towards zero by up to one level, and that bias is confined to the mask and to
    nothing else — a faint dark patch in exactly the mask's shape, with a hard
    edge on its contour. Measured on a real page (384x384 region, blob mask,
    `feather_px=6`): blending a region with ITSELF changed 1244 of 30117 masked
    channel values, every one of them one level darker. A coherent one-level step
    along a long contour is far more visible than its magnitude suggests, and it
    was a real part of the seam users reported. Rounding makes the blend an exact
    identity when `generated == original`, which is the property that matters.
    """
    import numpy as np

    inside = mask > 0
    alpha = _feather_mask_inwards(mask, feather_px).astype(np.float32) / 255.0
    alpha = np.where(inside, alpha, 0.0)[..., None]
    blended = original.astype(np.float32) * (1.0 - alpha) + generated.astype(np.float32) * alpha
    blended = np.clip(np.rint(blended), 0, 255).astype(np.uint8)
    return np.ascontiguousarray(np.where(inside[..., None], blended, original))


def _feather_mask_inwards(mask: np.ndarray, feather_px: int) -> np.ndarray:
    """Blend weights that rise from 0 on the mask contour to 1 `feather_px` inside.

    `feather_px` is the RAMP WIDTH in pixels, and the ramp is a smoothstep of the
    distance to the contour, so the weight is exactly zero outside the mask (the
    distance is zero there) and exactly one everywhere at least `feather_px`
    inside it. A mask thinner than `feather_px` compresses the ramp to its own
    half-width instead of losing the edit: the weight still reaches one at the
    mask's core.

    This replaced an erode-then-Gaussian-blur construction whose ramp was neither
    `feather_px` wide nor bounded by it. PIL's `GaussianBlur` takes the radius as
    a standard deviation, so eroding by `f` and blurring by `f` produced a ramp
    about `4*f` wide: measured on a 56 px blob, `f=6` reached weight 1.0 only 22 px
    inside, `f=20` peaked at 0.69 and `f=32` at 0.129 — i.e. asking for a wide
    feather silently discarded up to 87% of the edit, uniformly, across the whole
    mask. It also fell back to a HARD mask whenever the erosion emptied the mask,
    which is the worst possible edge for the case it was meant to protect.
    """
    import numpy as np

    if feather_px <= 0:
        return mask
    distance = _mask_distance_inside(mask)
    reach = float(distance.max())
    if reach <= 0.0:  # pragma: no cover - an empty mask never reaches the blend
        return mask
    # A mask narrower than the requested ramp gets the widest ramp that still
    # reaches full strength somewhere, rather than a partial blend everywhere.
    width = min(float(feather_px), reach)
    t = np.clip(distance / width, 0.0, 1.0)
    smooth = t * t * (3.0 - 2.0 * t)  # smoothstep: zero slope at both ends
    return np.ascontiguousarray((smooth * 255.0 + 0.5).astype(np.uint8))


def _mask_distance_inside(mask: np.ndarray) -> np.ndarray:
    """Distance in pixels from every masked pixel to the nearest unmasked one.

    Zero outside the mask, so a ramp built on it cannot leak past the contour.
    cv2 gives the exact Euclidean distance; without it the distance is built from
    successive erosions, which measures the same thing on the elliptical
    structuring element `_morph_mask` already uses. The fallback costs one
    erosion per level and is bounded by `MAX_MASK_DISTANCE_PROBE`, which is above
    the largest feather the contract accepts.

    **Everything beyond the region's own border counts as unmasked**, which is
    what the one-pixel zero ring below encodes. The region is a WINDOW onto a
    larger page and the pixels past its edge belong to that page, which this
    request may not change — so the region border is as much a mask contour as
    the painted outline is, and the feather has to ramp inwards from it too.
    Without the ring both backends answer "no contour here": `distanceTransform`
    measures only to zeros that exist inside the array, and `cv2.erode` /
    `ImageFilter.MinFilter` extend the border rather than eating into it. A mask
    painted up to the region edge would then meet the untouched page with a hard
    step, and under `whole_region`, where the mask covers everything, the feather
    would do nothing at all.
    """
    import numpy as np

    binary = np.pad((mask > 0).astype(np.uint8), 1)
    try:
        import cv2
    except ImportError:
        pass
    else:
        return np.ascontiguousarray(
            cv2.distanceTransform(binary, cv2.DIST_L2, 5)[1:-1, 1:-1]
        )

    distance = np.zeros(binary.shape, dtype=np.float32)
    # A masked pixel that survives no erosion is one pixel from the outside, so
    # the whole mask starts at 1 and each survived erosion adds another pixel.
    distance[binary > 0] = 1.0
    eroded = binary * 255
    for step in range(2, MAX_MASK_DISTANCE_PROBE + 1):
        eroded = _erode_mask(eroded, 1)
        remaining = eroded > 0
        if not remaining.any():
            break
        distance[remaining] = float(step)
    return np.ascontiguousarray(distance[1:-1, 1:-1])


def _dilate_mask(mask: np.ndarray, radius: int) -> np.ndarray:
    """Grow the mask by `radius` pixels (cv2 when available, PIL otherwise)."""
    return _morph_mask(mask, radius, grow=True)


def _erode_mask(mask: np.ndarray, radius: int) -> np.ndarray:
    """Shrink the mask by `radius` pixels (cv2 when available, PIL otherwise)."""
    return _morph_mask(mask, radius, grow=False)


def _morph_mask(mask: np.ndarray, radius: int, *, grow: bool) -> np.ndarray:
    """Dilate (`grow`) or erode the mask with an elliptical structuring element.

    cv2 is preferred; the PIL fallback exists because OpenCV is an optional
    dependency of this backend. PIL's rank filters cap their window at 31 px, so
    a large radius is applied in several passes.
    """
    if radius <= 0:
        return mask
    kernel_size = 2 * int(radius) + 1
    try:
        import cv2
    except ImportError:
        pass
    else:
        kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (kernel_size, kernel_size))
        operation = cv2.dilate if grow else cv2.erode
        return operation(mask, kernel, iterations=1)

    import numpy as np
    from PIL import Image, ImageFilter

    rank_filter = ImageFilter.MaxFilter if grow else ImageFilter.MinFilter
    image = Image.fromarray(mask, "L")
    remaining = int(radius)
    while remaining > 0:
        window = min(2 * remaining + 1, 31)
        image = image.filter(rank_filter(window))
        remaining -= (window - 1) // 2
    return np.ascontiguousarray(np.asarray(image, dtype=np.uint8))


# =====================================================================
#  Memory forecast and the pre-load guard
# =====================================================================
def forecast_memory(
    normalized: dict[str, Any], region_width: int, region_height: int
) -> dict[str, Any]:
    """Forecast the device and host memory one run costs, in bytes.

    THE single forecast in this module: `estimate` reports it to the UI and
    `_require_memory_headroom` gates the load on it. Two independent
    calculations would drift, and a guard that disagrees with the number on
    screen is worse than no guard.

    **A run is a sequence of phases, so the answer is their MAXIMUM, not their
    sum.** `encode`, `denoise` and `decode` are forecast separately and reported
    in `breakdown` (`encode_standalone` is a fourth entry that no run performs:
    it is what a prompt-cache BUILD costs, and it is dominated by `encode`, so it
    changes none of the totals), and the arithmetic follows the order the run
    actually uses:
    the transformer and the VAE are loaded and placed FIRST, and the text encoder
    is read afterwards, into host memory, in every placement.

    That order is what the host figures below turn on. Under the two non-offload
    placements the pipeline's host copy exists only while it is on its way to the
    accelerator, and the encoder arrives after it is gone — so the two host peaks
    never coexist and the `denoise` host term is their MAXIMUM, not their sum.
    Under the two offload placements the pipeline stays in host memory for the
    whole run, and that same maximum degenerates into the sum it should be.

    The weight terms are read from the files on disk (a bf16/fp16 checkpoint
    stores 2 bytes per parameter, which is also what it occupies once loaded);
    the activation terms use the coarse per-token / per-pixel constants
    documented at the top of this module.

    **With no text encoder installed both encode phases are zero**, because
    neither can run there: a prompt that is not cached is refused before the load
    and a cached one skips the phase. The forecast is lower on such a machine,
    and it is lower in the guard and on screen at once — there is still exactly
    one calculation.

    Returns `{"vram_bytes", "ram_bytes", "phases", "resident", "breakdown"}`.
    `phases` maps each phase name to its own `{"vram_bytes", "ram_bytes"}` — that
    is what the guard checks one at a time. `resident` names the two costs a run
    can arrive with ALREADY PAID, so the guard can discount them without doing
    its own arithmetic; see `_require_memory_headroom`.

    # Raises
    `ValueError` for a placement outside `VALID_PLACEMENTS`.
    """
    transformer_bytes = _weight_bytes(normalized["transformer_path"])
    # No encoder on this machine means no encode phase can ever run: a prompt
    # that is not already cached is refused before a single byte is read
    # (`require_text_encoder`), and a cached one skips the phase entirely. Its
    # cost is therefore ZERO rather than "the size of a file that is not there" —
    # which is the same number `_weight_bytes` would return for a path that
    # exists but holds no weights, and those two must not be confused.
    encoder_installed = text_encoder_available(normalized)
    text_encoder_bytes = _weight_bytes(normalized["text_encoder_path"]) if encoder_installed else 0
    vae_bytes = _weight_bytes(normalized["vae_path"])

    latent_tokens = (int(region_width) // REGION_SIZE_MULTIPLE) * (
        int(region_height) // REGION_SIZE_MULTIPLE
    )
    vae_per_pixel = (
        VAE_DECODE_TILED_BYTES_PER_PIXEL
        if normalized["vae_tiling"] or normalized["vae_slicing"]
        else VAE_DECODE_BYTES_PER_PIXEL
    )
    denoise_activations = latent_tokens * ACTIVATION_BYTES_PER_LATENT_TOKEN
    decode_activations = int(region_width) * int(region_height) * vae_per_pixel

    placement = normalized["placement"]
    low_cpu = bool(normalized["low_cpu_mem_usage"])
    pipeline_bytes = transformer_bytes + vae_bytes

    # fp8 halves the weights that STAY resident. It cannot lower the encode peak:
    # the bf16 weights have to exist before they can be quantized.
    resident_encoder = 0
    if not normalized["unload_text_encoder_after_encode"]:
        resident_encoder = (
            text_encoder_bytes // 2 if normalized["text_encoder_fp8"] else text_encoder_bytes
        )

    if placement in ("full_gpu", "encoder_cpu"):
        # The pipeline ends up on the accelerator, so its host copy is transient:
        # it exists between the safetensors read and the placement move, and
        # `low_cpu_mem_usage` removes even that by loading straight into VRAM.
        pipeline_device = pipeline_bytes
        pipeline_host_resident = 0
        pipeline_host_transient = 0 if low_cpu else pipeline_bytes
        decode_weights_parked = vae_bytes
        # Parking the transformer before the VAE decode copies all 9B of it back
        # into anonymous host memory. That peak is what the OOM killer sees, so
        # it belongs in the RAM forecast of the decode phase.
        parked_ram = transformer_bytes if normalized["unload_transformer_before_vae"] else 0
    elif placement == "model_cpu_offload":
        # Accelerate keeps exactly one component on the device at a time and the
        # rest in host memory, for the whole run.
        pipeline_device = max(transformer_bytes, vae_bytes)
        pipeline_host_resident = pipeline_bytes
        pipeline_host_transient = pipeline_bytes
        decode_weights_parked = vae_bytes
        parked_ram = 0
    elif placement == "sequential_cpu_offload":
        pipeline_device = int(transformer_bytes * SEQUENTIAL_RESIDENT_TRANSFORMER_FRACTION)
        pipeline_host_resident = pipeline_bytes
        pipeline_host_transient = pipeline_bytes
        decode_weights_parked = vae_bytes
        parked_ram = 0
    else:  # pragma: no cover - normalization already rejected everything else
        raise ValueError(f"Неизвестный режим размещения: {placement!r}")

    # The encoder now always runs in HOST memory (see `_encode_prompts_locked`),
    # and it does so while the pipeline is already placed — so this phase carries
    # the pipeline's residency on both sides, but computes nothing on the device.
    # `pipeline_device` is therefore both the encode phase's whole device cost and
    # the denoise phase's weight term: the same weights, sitting still.
    encode_vram = pipeline_device
    encode_ram = text_encoder_bytes + ENCODE_ACTIVATION_BYTES + pipeline_host_resident

    decode_weights = (
        decode_weights_parked if normalized["unload_transformer_before_vae"] else pipeline_device
    )
    # The pipeline's transient host copy and the resident encoder never overlap
    # under the non-offload placements — the encoder is read only after the
    # transformer has left the host — so the host term is their maximum. Under
    # the offload placements `pipeline_host_transient == pipeline_host_resident`,
    # and the maximum is the sum it has to be there.
    run_ram = max(pipeline_host_transient, pipeline_host_resident + resident_encoder)
    if not encoder_installed:
        # Both encode phases are impossible here (see `encoder_installed` above),
        # so they cost nothing at all — not even the activation scratch of an
        # encode that will never happen. Leaving them at their nominal cost would
        # make `estimate` report a peak for work this machine cannot perform, and
        # `_preset_advice` weigh candidates against it.
        encode_vram = 0
        encode_ram = 0
    phases = {
        "encode": {"vram_bytes": int(encode_vram), "ram_bytes": int(encode_ram)},
        # The same encode WITHOUT a pipeline: what
        # `inpaint.flux2_klein.prompt_cache.build` costs. It reads the encoder
        # and nothing else, so it carries neither the placed weights on the card
        # nor the pipeline's host copy. Its device cost is zero and its host cost
        # is never above the `encode` phase's, so adding it here cannot move
        # `vram_bytes` / `ram_bytes`, `estimate` or `_preset_fits` — it only
        # gives the guard a phase to check for a build.
        "encode_standalone": {
            "vram_bytes": 0,
            "ram_bytes": int(text_encoder_bytes + ENCODE_ACTIVATION_BYTES)
            if encoder_installed
            else 0,
        },
        "denoise": {
            "vram_bytes": int(pipeline_device + denoise_activations),
            "ram_bytes": int(run_ram),
        },
        "decode": {
            "vram_bytes": int(decode_weights + decode_activations),
            "ram_bytes": int(pipeline_host_resident + resident_encoder + parked_ram),
        },
    }
    return {
        "vram_bytes": max(phase["vram_bytes"] for phase in phases.values()),
        "ram_bytes": max(phase["ram_bytes"] for phase in phases.values()),
        "phases": phases,
        # The two costs a repeat request can arrive with already paid: the placed
        # pipeline on the card, and the kept text encoder in host memory. The
        # guard subtracts whichever the service is actually holding — see
        # `_require_memory_headroom`. They are published here, and not
        # recomputed there, so there stays exactly ONE calculation in this module.
        "resident": {
            "pipeline_device": int(pipeline_device),
            "text_encoder_host": int(text_encoder_bytes),
        },
        "breakdown": {
            "transformer": int(transformer_bytes),
            "text_encoder": int(text_encoder_bytes),
            "vae": int(vae_bytes),
            "activations": int(denoise_activations + decode_activations),
            # The encode phase is now the one phase whose cost is dominated by
            # HOST memory (the 16 GB encoder) while it also holds the pipeline on
            # the card, so its peak is the larger of the two sides rather than
            # "the VRAM figure, or the RAM one when there is no VRAM cost".
            "peak_encode": max(
                phases["encode"]["vram_bytes"], phases["encode"]["ram_bytes"]
            ),
            "peak_denoise": phases["denoise"]["vram_bytes"],
            "peak_decode": phases["decode"]["vram_bytes"],
        },
    }


#: Human-readable phase names for the guard's message.
_PHASE_LABELS = {
    "encode": "кодирование промпта",
    "encode_standalone": "кодирование промпта (кэширование)",
    "denoise": "денойз",
    "decode": "декодирование VAE",
}


def _require_memory_headroom(
    normalized: dict[str, Any],
    region_width: int,
    region_height: int,
    device: str,
    *,
    phases: tuple[str, ...],
    pipeline_resident: bool = False,
    encoder_resident: bool = False,
) -> None:
    """Refuse the run when a phase's forecast does not fit in the free memory.

    Called BEFORE the first component is read, because the failure this prevents
    is not an exception: a 9B transformer and an 8B encoder that do not fit make
    the kernel's OOM killer pick a victim, and the victim is whatever else the
    user has open — it has already cost one editor session with unsaved work.
    A `torch.OutOfMemoryError` we could have caught is the good case; the host
    side has no such thing.

    `phases` names the phases that will actually load something on this request:
    a cached prompt skips `encode`, a resident pipeline skips `denoise`/`decode`.
    Each listed phase is checked SEPARATELY, because they run one after another
    and a run is limited by its largest phase, not by their sum.

    `pipeline_resident` / `encoder_resident` say what the service is ALREADY
    holding. Their cost is subtracted from every phase, because the free-memory
    figures this compares against already exclude it: the placed pipeline's VRAM
    and the kept encoder's RAM are allocated, not pending. Without the discount a
    repeat request with a new prompt is refused for memory it is already sitting
    on — measured on this project's reference host, where the second prompt to a
    resident pipeline was told it needed 17.6 GiB of VRAM that the very same
    pipeline was occupying. The discounted figures are what the message reports,
    so the numbers the user sees are the ones that were compared.

    `device` is the resolved torch device string, so the VRAM figures come from
    the card the run would actually use. A memory figure reported as `0` (no
    psutil, no accelerator) is unknown, not zero, and never refuses a run.

    # Raises
    `RuntimeError` naming the short resource, the phase that needs it, how much
    it needs, how much is free, and which settings do fit right now.
    """
    if not phases:
        return
    forecast = forecast_memory(normalized, region_width, region_height)
    memory = memory_snapshot(device)
    held_vram = forecast["resident"]["pipeline_device"] if pipeline_resident else 0
    held_ram = forecast["resident"]["text_encoder_host"] if encoder_resident else 0

    short: list[str] = []
    for phase in phases:
        cost = forecast["phases"][phase]
        label = _PHASE_LABELS[phase]
        ram_cost = max(cost["ram_bytes"] - held_ram, 0)
        vram_cost = max(cost["vram_bytes"] - held_vram, 0)
        ram_need = ram_cost + HOST_MEMORY_RESERVE_BYTES
        vram_need = vram_cost + DEVICE_MEMORY_RESERVE_BYTES
        if ram_cost and not _fits(ram_need, memory["ram_free"]):
            short.append(
                f"оперативной памяти на этап «{label}»: нужно {_gib(ram_need)} (прогноз "
                f"{_gib(ram_cost)} + резерв {_gib(HOST_MEMORY_RESERVE_BYTES)}), "
                f"свободно {_gib(memory['ram_free'])}"
            )
        if vram_cost and not _fits(vram_need, memory["vram_free"]):
            short.append(
                f"видеопамяти на {device} на этап «{label}»: нужно {_gib(vram_need)} (прогноз "
                f"{_gib(vram_cost)} + резерв {_gib(DEVICE_MEMORY_RESERVE_BYTES)}), "
                f"свободно {_gib(memory['vram_free'])}"
            )
    if not short:
        return

    log.error(
        "FLUX.2 klein: запуск отклонён до чтения весов — %s (режим «%s», область %dx%d).",
        "; ".join(short),
        normalized["placement"],
        int(region_width),
        int(region_height),
    )
    raise RuntimeError(
        f"Недостаточно {'; '.join(short)}. Загрузка не начата, чтобы система не осталась без "
        f"памяти. {_preset_advice(normalized, region_width, region_height, memory)}"
    )


def _preset_advice(
    normalized: dict[str, Any], region_width: int, region_height: int, memory: dict[str, int]
) -> str:
    """One sentence naming the settings whose forecast fits `memory` right now.

    Computed rather than hard-coded, so the advice cannot recommend the very
    preset that just failed. Besides the four presets it considers one separate
    lever: switching OFF `unload_transformer_before_vae`. Parking a 9B
    transformer copies it into host memory, so that single flag can be the whole
    difference on a machine that is short of RAM — and the decode's own recovery
    ladder still parks lazily if the VAE actually runs out of VRAM. When nothing
    fits, the region is the only remaining lever and the message says so.

    Deliberately undiscounted, unlike the guard itself: switching preset changes
    the model key, so the resident pipeline is evicted before the new one loads
    and the candidate really does start from the free memory measured here. The
    cost is that a "current mode without X" candidate — which keeps the key, and
    therefore the residency — is judged more strictly than it would run. That
    errs towards offering fewer options than exist, never towards offering one
    that would fail.
    """
    candidates: list[tuple[str, dict[str, Any]]] = []
    for label, placement, low_cpu in _MEMORY_PRESETS:
        # The preset owns `unload_transformer_before_vae` too, but that one flag
        # is worth offering separately: it is the difference between keeping the
        # transformer on the device and copying all 9B of it back into host
        # memory, so a preset that does not fit with it often fits without.
        for parked in (placement != "full_gpu", False):
            suffix = "" if parked else " без выгрузки трансформера перед VAE"
            candidates.append(
                (
                    f"{label}{suffix}",
                    {
                        "placement": placement,
                        "low_cpu_mem_usage": low_cpu,
                        "unload_transformer_before_vae": parked,
                        # A preset carries the shipped default, which is now to
                        # KEEP the encoder in host memory in every placement.
                        "unload_text_encoder_after_encode": False,
                    },
                )
            )
    # Dropping the encoder after the encode is its own lever: it is the shipped
    # default no longer, so on a host that is short of RAM it is the first thing
    # to offer — 16 GB back, at the price of re-reading the encoder on the next
    # prompt that misses the cache.
    if not normalized["unload_text_encoder_after_encode"]:
        candidates.append(
            (
                "текущий режим с выгрузкой энкодера после кодирования",
                {"unload_text_encoder_after_encode": True},
            )
        )
    # Only when the user is on a custom combination: for a preset the same
    # advice is already in the list under the preset's own name.
    current = (normalized["placement"], bool(normalized["low_cpu_mem_usage"]))
    if normalized["unload_transformer_before_vae"] and current not in {
        (placement, low_cpu) for _label, placement, low_cpu in _MEMORY_PRESETS
    }:
        candidates.append(
            (
                "текущий режим без выгрузки трансформера перед VAE",
                {"unload_transformer_before_vae": False},
            )
        )

    fitting: list[str] = []
    for label, overrides in candidates:
        if label in fitting:
            continue
        candidate = dict(normalized)
        candidate.update(overrides)
        if _preset_fits(candidate, region_width, region_height, memory):
            fitting.append(label)
    if not fitting:
        return (
            "Ни один из встроенных профилей памяти сейчас не помещается: уменьшите выделенную "
            "область или освободите память, закрыв другие программы."
        )
    return "Сейчас помещаются: " + ", ".join(f"«{label}»" for label in fitting) + "."


def _preset_fits(
    candidate: dict[str, Any], region_width: int, region_height: int, memory: dict[str, int]
) -> bool:
    """Whether every phase of one preset fits the free memory, reserves included."""
    forecast = forecast_memory(candidate, region_width, region_height)
    for cost in forecast["phases"].values():
        if cost["ram_bytes"] and not _fits(
            cost["ram_bytes"] + HOST_MEMORY_RESERVE_BYTES, memory["ram_free"]
        ):
            return False
        if cost["vram_bytes"] and not _fits(
            cost["vram_bytes"] + DEVICE_MEMORY_RESERVE_BYTES, memory["vram_free"]
        ):
            return False
    return True


def _gib(value: int) -> str:
    """Bytes as a human-readable GiB figure for a user-facing message."""
    return f"{int(value) / (1024**3):.1f} ГиБ"


# =====================================================================
#  Memory reporting
# =====================================================================
def memory_snapshot(device: str | None = None) -> dict[str, int]:
    """Total/free host and device memory in bytes; `0` where unknown.

    `device` is the torch device string this service would use. When it names a
    concrete CUDA index (`cuda:1`) the VRAM figures come from THAT card instead
    of the process's current one, so a forecast on a two-accelerator host is not
    compared against the wrong card's free memory. `None`, `"cuda"` and CPU-like
    names leave the current device.

    Deliberately tolerant: `status` must answer on a machine with no Torch, no
    GPU and no psutil, and a missing figure is reported as `0` rather than
    failing the whole call.
    """
    ram_total = 0
    ram_free = 0
    try:
        import psutil

        virtual = psutil.virtual_memory()
        ram_total = int(virtual.total)
        ram_free = int(virtual.available)
    except Exception as exc:  # noqa: BLE001 - psutil is optional at runtime
        log.debug("FLUX.2 klein: host memory unavailable (%s)", exc)

    vram_total = 0
    vram_free = 0
    if is_torch_available():
        try:
            import torch

            if torch.cuda.is_available():
                free, total = torch.cuda.mem_get_info(_cuda_device_index(device))
                vram_free = int(free)
                vram_total = int(total)
        except Exception as exc:  # noqa: BLE001 - no GPU / driver mismatch
            log.debug("FLUX.2 klein: device memory unavailable (%s)", exc)

    return {
        "vram_total": vram_total,
        "vram_free": vram_free,
        "ram_total": ram_total,
        "ram_free": ram_free,
    }


def _cuda_device_index(device: str | None) -> int | None:
    """Explicit CUDA ordinal in `device`, or `None` for the current device.

    `"cuda:1"` -> `1`; `"cuda"`, `"cpu"`, `None` and anything unparsable -> `None`,
    which every `torch.cuda` query reads as "the current device".
    """
    text = str(device or "").strip().lower()
    prefix = "cuda:"
    if not text.startswith(prefix):
        return None
    try:
        return int(text[len(prefix) :])
    except ValueError:
        return None


def _fits(required: float, free: int) -> bool:
    """Whether `required` bytes fit in `free`; unknown (`0`) free memory passes."""
    return True if free <= 0 else int(required) <= int(free)


def _weight_bytes(path: str) -> int:
    """On-disk size of a component: one file, or every weight file in a folder.

    A bf16/fp16 checkpoint stores two bytes per parameter, which is also what it
    occupies once loaded, so the file size doubles as the weight-memory estimate.
    """
    source = Path(path)
    try:
        if source.is_file():
            return int(source.stat().st_size)
        if not source.is_dir():
            return 0
        total = 0
        for entry in source.rglob("*"):
            if entry.is_file() and entry.suffix.lower() in (".safetensors", ".bin", ".pt", ".pth"):
                total += int(entry.stat().st_size)
        return total
    except OSError as exc:
        log.debug("FLUX.2 klein: could not size %s (%s)", path, exc)
        return 0


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


def _progress_reporter(
    callback: ProgressCb | None, phase: str, total: int
) -> Callable[[int, str], None]:
    """Bind a progress callback to one phase; the result never raises."""

    def report(step: int, label: str) -> None:
        if callback is None:
            return
        try:
            callback(phase, int(step), int(total), str(label))
        except Exception:  # noqa: BLE001 - a dead peer must not kill the load
            pass

    return report


def _lenient_paths(params: dict[str, Any] | None) -> dict[str, str]:
    """Extract the three component paths without validating them.

    `status` is called while the user is still picking files, so an absent or
    non-existent path must be reportable rather than fatal.
    """
    if not isinstance(params, dict):
        return {}
    out = {key: str(params.get(key) or "").strip() for key in _PATH_KEYS}
    return out if any(out.values()) else {}


def _path_state(path: str | None) -> dict[str, Any]:
    """`{path, exists, size_bytes}` for one user-supplied component path."""
    raw = str(path or "").strip()
    if not raw:
        return {"path": "", "exists": False, "size_bytes": 0}
    return {"path": raw, "exists": Path(raw).exists(), "size_bytes": _weight_bytes(raw)}


def _first_unavailable_reason(components: dict[str, Any], *, prompt_cached: bool) -> str | None:
    """First blocking reason for `status.available`, or `None` when all is well.

    `prompt_cached` says a READY embedding exists for the request in hand, and
    that changes what "available" means: the encode phase is then skipped
    entirely, so the text encoder is never read and must not block the run.
    Reporting `available: false` because a 16 GB encoder is absent would hide a
    run that works — which is the whole point of carrying a `.msprompt` to a
    machine that never downloaded it.

    **The Qwen tokenizer is NOT waived along with it**, even though the denoise
    never tokenizes anything: `_ensure_pipeline_locked` builds the pipeline with
    a real `Qwen2TokenizerFast` and `_require_component_dir` raises without one,
    so a run on a machine that has no `tokenizer/` fails at the load. Verified on
    the reference host — the pipeline build emits its own "Загрузка токенизатора"
    step. The transformer, the VAE and the scheduler are needed in every case
    too.
    """
    if not is_torch_available():
        return "PyTorch не установлен"
    if not prompt_cached:
        encoder = components["text_encoder"]
        # Both branches name the cache as the second way out: this is exactly the
        # state a machine is in when a `.msprompt` was copied to it but not loaded
        # yet, and the settings still carry the other machine's encoder path.
        if not encoder["path"]:
            return (
                "Не выбран текстовый энкодер (или загрузите готовый кэш промпта — "
                "с ним энкодер не нужен)"
            )
        if not encoder["exists"]:
            return (
                f"Путь текстового энкодера не найден: {encoder['path']} "
                "(или загрузите готовый кэш промпта — с ним энкодер не нужен)"
            )
    for name, human in (("transformer", "трансформер"), ("vae", "VAE")):
        entry = components[name]
        if not entry["path"]:
            return f"Не выбран {human}"
        if not entry["exists"]:
            return f"Путь не найден: {entry['path']}"
    if not components["tokenizer"]["found"]:
        return "Не найден токенизатор Qwen (каталог «tokenizer» рядом с моделью)"
    if not components["scheduler"]["found"]:
        return "Не найден планировщик (каталог «scheduler» с scheduler_config.json)"
    return None


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
#  Image / mask codecs
# =====================================================================
def _decode_image_rgb(image_bytes: bytes) -> np.ndarray:
    import numpy as np
    from PIL import Image

    with Image.open(io.BytesIO(image_bytes)) as img:
        return np.ascontiguousarray(np.array(img.convert("RGB"), dtype=np.uint8))


def _decode_mask(mask_bytes: bytes, *, expected_hw: tuple[int, int]) -> np.ndarray:
    """Decode a strictly L8 mask and binarize it; the size must match the region.

    The wire contract is 8-bit greyscale and nothing else. Accepting RGB/RGBA and
    guessing which channel carries the permission to edit — the alpha, or the
    per-pixel maximum — turns a client bug into an edit of the wrong pixels, so a
    mask of any other mode is refused instead of converted.

    # Raises
    `ValueError` when the image is not mode `L`, when it does not decode to a 2D
    array, or when its size differs from `expected_hw` (height, width).
    """
    import numpy as np
    from PIL import Image

    with Image.open(io.BytesIO(mask_bytes)) as img:
        if img.mode != "L":
            raise ValueError(
                f"Маска должна быть 8-битной в градациях серого (L8), получено «{img.mode}»"
            )
        arr = np.array(img)
    if arr.ndim != 2:
        raise ValueError(f"Некорректная маска: ожидается 2D массив, получено {arr.ndim}D")
    mask = np.ascontiguousarray(arr.astype(np.uint8))
    if tuple(mask.shape[:2]) != tuple(expected_hw):
        raise ValueError(
            f"Размер маски {mask.shape[1]}x{mask.shape[0]} не совпадает с областью "
            f"{expected_hw[1]}x{expected_hw[0]}"
        )
    return np.where(mask > 0, 255, 0).astype(np.uint8)


def _require_solid_mask(mask_u8: np.ndarray) -> None:
    """Refuse a `whole_region` request whose mask is not solid.

    `whole_region` does not change the wire format: the client still sends a
    mask, and under this flag it must be filled — every pixel non-zero. Checking
    it turns a client-side disagreement between the flag and the data into an
    immediate, named request error instead of a run whose result silently
    contradicts the flag (the mask would win, and the user would see a partial
    edit while the UI showed "no mask needed").

    `mask_u8` is the already-binarized output of `_decode_mask`, so "non-zero"
    and "255" are the same test here.

    # Raises
    `ValueError` naming how many pixels are empty out of how many.
    """
    import numpy as np

    empty = int(np.count_nonzero(mask_u8 == 0))
    if not empty:
        return
    raise ValueError(
        f"Режим «без маски» требует сплошную маску, но {empty} из {mask_u8.size} пикселей "
        "нулевые. Флаг whole_region и присланная маска противоречат друг другу — это ошибка "
        "запроса, а не изображения."
    )


def _encode_png_bytes_rgb(image_rgb: np.ndarray) -> bytes:
    import numpy as np
    from PIL import Image

    arr = np.ascontiguousarray(image_rgb.astype(np.uint8))
    with io.BytesIO() as buf:
        Image.fromarray(arr, "RGB").save(buf, format="PNG")
        return buf.getvalue()


def _clear_torch_cache() -> None:
    import gc

    gc.collect()
    try:
        import torch

        if torch.cuda.is_available():
            torch.cuda.empty_cache()
            if hasattr(torch.cuda, "ipc_collect"):
                torch.cuda.ipc_collect()
    except Exception:  # noqa: BLE001 - no torch / no accelerator is fine here
        pass


# =====================================================================
#  Device selection (mirrors the other inpaint services)
# =====================================================================
def _resolve_selected_backend_device(fallback: str) -> str:
    """Resolve `General.ai_device` into a concrete torch device string.

    Unlike `flux_fill.py`, which pins itself to the discrete GPU, this service
    honours the user's device choice: a 9B model is the case where a user with
    two accelerators most needs to say which one to use.
    """
    fallback_norm = _normalize_backend_device(fallback, "cpu")
    configured = _read_configured_device()
    if configured is None:
        configured = fallback_norm

    normalized = _normalize_backend_device(configured, fallback_norm)
    available = _safe_available_devices()

    if normalized in available:
        return normalized
    if normalized.startswith("cuda") and "cuda" in available:
        return "cuda"
    if fallback_norm in available:
        return fallback_norm
    if "cuda" in available:
        return "cuda"
    return "cpu"


def _read_configured_device() -> str | None:
    """`General.ai_device` from the user config, `None` when unset."""
    config_root = getattr(UserConfig, "config", None)
    if not isinstance(config_root, dict):
        return None
    general = config_root.get("General")
    if not isinstance(general, dict):
        return None
    value = general.get("ai_device")
    if not isinstance(value, str):
        return None
    value = value.strip().lower()
    if value == "not-selected":
        return None
    return value or None


def _safe_available_devices() -> set[str]:
    try:
        return set(AIDevice.detect_available_devices())
    except Exception:  # noqa: BLE001 - detection must never break a request
        return {"cpu"}


def _normalize_backend_device(raw: str, fallback: str) -> str:
    value = str(raw or "").strip().lower()
    if value in {"cpu", "mps", "cuda"}:
        return value
    if value.startswith("cuda:"):
        return value
    return str(fallback or "cpu").strip().lower() or "cpu"


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
