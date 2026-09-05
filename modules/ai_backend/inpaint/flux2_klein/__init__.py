"""
Package: modules/ai_backend/inpaint/flux2_klein

Purpose:
FLUX.2 klein 9B region-editing service for the Python AI backend (methods
`inpaint.flux2_klein`, `.status`, `.estimate`, `.unload`, `.component_action`
and the `.prompt_cache.*` family; streaming). It edits a user-selected page
REGION with `diffusers.Flux2KleinInpaintPipeline`.

Per-component residency (`dev-docs/flux2_component_residency.md`):
`status` reports, per weight-bearing component, WHERE its weights are
(`_module_residency` → `not_loaded` / `ram` / `gpu` / `offloaded` / `mixed`) and
WHICH actions are possible right now (`_component_actions`). That action list is
the authority the client renders — the matrix lives here and nowhere else.
`component_action` performs one of them, streaming `phase:"load"` progress, under
the same memory guard and the same lease protocol a generation uses. Both the
probe and the whole of `status` take the service lock WITHOUT waiting, because a
generation holds it for its entire run.

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
it encodes the prompt and stays for the next one. The warm-up belongs to the
PLACEMENT, not to the request: a run that cache-hits a pipeline whose weights
have not moved since skips it (`_warmup_pipeline_if_needed_locked`).

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

Module layout (this file re-exports the package's whole public surface, so
`from ...inpaint.flux2_klein import Flux2KleinInpaintService` and every
`flux2_klein.<name>` reference resolve against the package object):

- `params.py`       - request normalization, region validation, coercion helpers.
- `progress.py`     - `phase:"load"` step numbering and the progress callback type.
- `components.py`   - the five components as files on disk: discovery, validation,
                      sizes, and the `components` map `status` returns.
- `prompt_cache.py` - encoder fingerprint, the `.msprompt` container, the library.
- `imaging.py`      - mask/region decoding, color match, feathered composite.
- `hardware.py`     - device resolution, memory snapshot, torch cache clearing.
- `memory.py`       - the RAM/VRAM forecast and the pre-load guard.
- `pipeline.py`     - component loaders, placement, residency, warm-up, decode.
- `service.py`      - `Flux2KleinInpaintService`: the lock, the lease, the run.

Dependency direction is strictly downwards in that list; `service.py` is the only
module that may import from all of the others.
"""

from __future__ import annotations

import logging

#: Package-level logger. Every submodule logs under `<this name>.<submodule>`, so
#: this logger is their common parent and captures all of their records.
log = logging.getLogger(__name__)

# The names below are the surface the single-file `flux2_klein.py` exposed, kept
# complete so that `server.py`, `test_lease_protocol.py` and the FLUX.2 test
# modules keep resolving. WARNING: a few of them (`memory_snapshot`,
# `_clear_torch_cache`, `_resolve_selected_backend_device`, `_weight_bytes`,
# `_quantize_text_encoder_fp8`, `_restore_transformer_to_device`,
# `write_prompt_file`) are monkeypatched by the test suite. Patch them on their
# DEFINING module (`hardware`, `components`, `pipeline`, `prompt_cache`), never on
# this package: the re-export here is a separate binding, so patching it would be
# a silent no-op for every real caller.

from .params import (  # noqa: F401  - re-exported public surface
    MAX_REGION_ASPECT_RATIO,
    MAX_REGION_PIXELS,
    MIN_REGION_SIDE,
    PIPELINE_MIN_REGION_SIDE,
    REGION_SIZE_MULTIPLE,
    VALID_DTYPES,
    VALID_PLACEMENTS,
    effective_steps,
    normalize_flux2_klein_params,
    validate_region_size,
    _GPU_ONLY_PLACEMENTS,
    _PATH_KEYS,
    _clamp_float,
    _clamp_int,
    _floor_to,
    _lenient_paths,
    _model_key,
    _require_existing_path,
    _to_bool,
    _to_int,
    _to_optional_int,
    _whole_region_overrides,
)
from .progress import (  # noqa: F401  - re-exported public surface
    LOAD_PHASE_STEPS,
    LOAD_STEP_ENCODE,
    LOAD_STEP_ENCODER_DONE,
    LOAD_STEP_PLACEMENT,
    LOAD_STEP_PREPARE,
    LOAD_STEP_SCHEDULER,
    LOAD_STEP_TEXT_ENCODER,
    LOAD_STEP_TOKENIZER,
    LOAD_STEP_TRANSFORMER,
    LOAD_STEP_VAE,
    LOAD_STEP_WARMUP,
    ProgressCb,
    _progress_reporter,
)
from .components import (  # noqa: F401  - re-exported public surface
    component_dir_for_path,
    component_probe_order,
    component_safetensors_shards,
    component_search_roots,
    discover_component_dir,
    find_transformer_config_dir,
    is_fp8_scaled_checkpoint,
    read_safetensors_header,
    require_encoder_transformer_compatible,
    require_text_encoder,
    text_encoder_available,
    transformer_config_dir,
    transformer_config_roots,
    validate_transformer_config_dir,
    TEXT_ENCODER_OUT_LAYERS,
    _FP8_DTYPES,
    _FP8_SCALE_SUFFIXES,
    _MAX_SAFETENSORS_HEADER_BYTES,
    _MODEL_CONFIG_MARKER,
    _SCHEDULER_MARKER,
    _SCHEDULER_SUBDIR,
    _TOKENIZER_MARKERS,
    _TOKENIZER_SUBDIR,
    _TRANSFORMER_CONFIG_CLASS_NAME,
    _TRANSFORMER_SUBDIR,
    _component_states,
    _first_unavailable_reason,
    _fp8_scaled_message,
    _missing_transformer_config_message,
    _path_state,
    _reject_fp8_scaled_directory,
    _require_component_dir,
    _weight_bytes,
)
from .prompt_cache import (  # noqa: F401  - re-exported public surface
    PROMPT_CACHE_DIRNAME,
    PROMPT_CACHE_FAMILY_HASH_CHARS,
    PROMPT_CACHE_FORMAT,
    PROMPT_CACHE_SUFFIX,
    PROMPT_CACHE_TENSOR,
    PROMPT_CACHE_VERSION,
    PROMPT_EMBED_CACHE_ENTRIES,
    encoder_family_name,
    find_prompt_cache_entry,
    list_prompt_cache_entries,
    list_prompt_cache_families,
    local_encoder_identity,
    prompt_cache_entry_path,
    prompt_cache_family_dir,
    prompt_cache_root,
    prompt_file_metadata,
    publish_bytes_atomically,
    read_prompt_file_header,
    read_prompt_file_tensor,
    require_free_entry_path,
    require_prompt_file_destination,
    require_prompt_file_source,
    sanitize_name_component,
    text_encoder_fingerprint,
    validate_prompt_file_metadata,
    write_prompt_file,
    _ENCODER_WEIGHT_SUFFIXES,
    _MAX_NAME_LENGTH,
    _PROMPT_CACHE_DTYPE_TOKENS,
    _SAFE_NAME_EXTRA,
    _encoder_origin_hint,
    _foreign_prompt_file_message,
    _short_id,
)
from .imaging import (  # noqa: F401  - re-exported public surface
    MAX_MASK_DISTANCE_PROBE,
    _MIN_COLOR_MATCH_SAMPLES,
    _composite_over_region,
    _decode_image_rgb,
    _decode_mask,
    _dilate_mask,
    _encode_png_bytes_rgb,
    _erode_mask,
    _feather_mask_inwards,
    _mask_distance_inside,
    _match_color_outside_mask,
    _morph_mask,
    _require_solid_mask,
)
from .hardware import (  # noqa: F401  - re-exported public surface
    memory_snapshot,
    _clear_torch_cache,
    _cuda_device_index,
    _normalize_backend_device,
    _read_configured_device,
    _resolve_selected_backend_device,
    _safe_available_devices,
)
from .memory import (  # noqa: F401  - re-exported public surface
    ACTIVATION_BYTES_PER_LATENT_TOKEN,
    DEVICE_MEMORY_RESERVE_BYTES,
    ENCODE_ACTIVATION_BYTES,
    HOST_MEMORY_RESERVE_BYTES,
    SEQUENTIAL_RESIDENT_TRANSFORMER_FRACTION,
    VAE_DECODE_BYTES_PER_PIXEL,
    VAE_DECODE_TILED_BYTES_PER_PIXEL,
    forecast_memory,
    _MEMORY_PRESETS,
    _PHASE_LABELS,
    _fits,
    _gib,
    _preset_advice,
    _preset_fits,
    _require_memory_headroom,
)
from .pipeline import (  # noqa: F401  - re-exported public surface
    ACTIONABLE_COMPONENTS,
    COMPONENT_ACTIONS,
    RESIDENCY_GPU,
    RESIDENCY_MIXED,
    RESIDENCY_NOT_LOADED,
    RESIDENCY_OFFLOADED,
    RESIDENCY_RAM,
    WARMUP_LATENT_CELLS,
    _LEASED_ACTIONS,
    _MMAP_BACKED_COMPONENTS,
    _PLACED_COMPONENTS,
    _apply_placement,
    _apply_vae_memory_options,
    _component_actions,
    _component_busy_message,
    _component_is_file_backed,
    _decode_once,
    _decode_region_latents,
    _encode_prompt_phase,
    _is_out_of_memory,
    _largest_cpu_tensor,
    _load_text_encoder,
    _load_transformer,
    _load_vae,
    _materialize_components_for_offload,
    _module_residency,
    _park_transformer_off_device,
    _quantize_text_encoder_fp8,
    _require_action_name,
    _require_component_name,
    _require_components_materialized,
    _require_execution_device,
    _restore_transformer_to_device,
    _single_file_device,
    _synchronize_device,
    _vae_input_device,
    _warmup_vae_decode,
)
from .service import (  # noqa: F401  - re-exported public surface
    Flux2KleinInpaintService,
)
