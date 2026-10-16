"""
Package: modules/ai_backend/inpaint/flux2_klein

Purpose:
FLUX.2 klein (9B / 4B) region-editing service for the Python AI backend (methods
`inpaint.flux2_klein`, `.status`, `.estimate`, `.unload`, `.component_action`
and the `.prompt_cache.*` family; streaming). It edits a user-selected page
REGION with `diffusers.Flux2KleinInpaintPipeline`. Its weights are two handles
of the large-model manager (`runtime.large_models`): the PIPELINE (transformer
+ VAE) and the TEXT ENCODER (Qwen3, truncated); the manager decides where every
byte lives in every phase, admits a run, degrades it on a memory failure and
keeps the model between runs per the two resource steps («Видеопамять» x
«Оперативная память»). This package describes
FLUX.2 to it (`large_model.py`) and drives runs through its sessions
(`service.py`); the legacy `placement` flags of the wire are mapped onto a
step pair (`memory.requested_steps`).

Mask semantics (this is NOT classic inpainting):
The mask is a PERMISSION TO CHANGE. Everything outside it must come back
byte-identical; the color alignment and the composite are done HERE. The wire
format is L8 and only L8. `text_attention_in_mask=True` confines the prompt to
the (dilated) mask's tokens; `whole_region=True` is the "no mask" mode, verified
against a solid mask, and settles the dilate, the color match and the attention
flag by itself.

Run order (a memory contract, see `service._run_locked`): the pipeline handle
is admitted and loaded first, the text encoder handle second (on the GPU when
the device holds it beside the pipeline, else on the CPU from RAM), then the
denoise and the VAE decode run as manager phases. The prompt-embedding cache
(in memory, LRU; a library of `.msprompt` files on disk) lets a run skip the
encoder entirely, including on a machine where none is installed.

Model layout:
Nothing is downloaded. The user supplies three paths — a Qwen3 text-encoder
folder, a transformer (`.safetensors` single file or a diffusers folder) and an
`AutoencoderKLFlux2` folder; the tokenizer and the scheduler are DISCOVERED next
to them, and a single-file transformer needs its own `config.json` beside it.

Module layout (this file re-exports the package's public surface):
- `params.py`       - request normalization, region validation, coercion helpers.
- `progress.py`     - `phase:"load"` step numbering and the progress callback type.
- `conversion.py`   - the transformer checkpoint's BFL -> diffusers key rule.
- `components.py`   - the five components as files on disk.
- `attention.py`    - `text_attention_in_mask`: token grid, layout, the mask and its bytes.
- `hardware.py`     - device resolution, memory snapshot, torch cache clearing.
- `prompt_cache.py` - encoder fingerprint, the `.msprompt` container, the library.
- `imaging.py`      - mask/region decoding, color match, feathered composite.
- `pipeline.py`     - what a phase does with the placed modules; residency / actions.
- `large_model.py`  - the manager specs, options, phase work, decode fallbacks.
- `memory.py`       - stage-1 wire adapters: step mapping, `estimate`, advice.
- `service.py`      - `Flux2KleinInpaintService`: the lock, the sessions, the run.

Notes:
- torch / diffusers / transformers / cv2 are imported lazily inside the
  functions that need them, so importing this package costs nothing.
- Generation is cancellable only at call boundaries (the handler checks
  `cancel_event` before and after the service call), like every inpaint service.
"""

from __future__ import annotations

import logging

#: Package-level logger. Every submodule logs under `<this name>.<submodule>`, so
#: this logger is their common parent and captures all of their records.
log = logging.getLogger(__name__)

# The names below are the package's public surface (`server.py`, the FLUX.2 test
# modules). WARNING: the patch points (`memory_snapshot`, `_clear_torch_cache`,
# `_resolve_selected_backend_device`, `_tables_for`, `write_prompt_file`,
# `build_text_attention_mask`, ...) must be patched on their DEFINING module, never
# on this package: the re-export here is a separate binding, so patching it would
# be a silent no-op for every real caller.

from .params import (  # noqa: F401  - re-exported public surface
    MAX_REGION_ASPECT_RATIO,
    MAX_REGION_PIXELS,
    MIN_REGION_SIDE,
    PIPELINE_MIN_REGION_SIDE,
    REGION_SIZE_MULTIPLE,
    TEXT_ENCODER_DTYPE,
    VALID_DTYPES,
    VALID_PLACEMENTS,
    VALID_PROFILES,
    VALID_RAM_STEPS,
    VALID_VRAM_STEPS,
    effective_steps,
    normalize_flux2_klein_params,
    text_encoder_dtype_name,
    validate_region_size,
    _PATH_KEYS,
    _clamp_float,
    _clamp_int,
    _floor_to,
    _lenient_paths,
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
    FileProgressCb,
    ProgressCb,
    _file_progress_reporter,
    _progress_reporter,
)
from .components import (  # noqa: F401  - re-exported public surface
    checkpoint_is_distilled,
    component_dir_for_path,
    component_probe_order,
    component_safetensors_shards,
    component_search_roots,
    discover_component_dir,
    find_transformer_config_dir,
    is_fp8_scaled_checkpoint,
    model_index_search_roots,
    read_safetensors_header,
    require_encoder_transformer_compatible,
    require_text_encoder,
    text_encoder_available,
    text_encoder_truncation_kwargs,
    transformer_config_dir,
    transformer_config_roots,
    vae_tile_threshold_pixels,
    validate_transformer_config_dir,
    ENCODER_KEEP_LAYERS,
    TEXT_ENCODER_OUT_LAYER_INDICES,
    TEXT_ENCODER_OUT_LAYERS,
    VAE_TILE_THRESHOLD_FALLBACK_PIXELS,
    _FP8_DTYPES,
    _FP8_SCALE_SUFFIXES,
    _MAX_SAFETENSORS_HEADER_BYTES,
    _MODEL_CONFIG_MARKER,
    _MODEL_INDEX_MARKER,
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
    read_safetensors_header_and_data_start,
)
from .conversion import (  # noqa: F401  - re-exported public surface
    FLUX2_BFL_MARKER_KEY,
    LDM_UNET_PREFIX,
    convert_bfl_tensor,
    is_bfl_key,
    require_known_layout,
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
from .attention import (  # noqa: F401  - re-exported public surface
    MASK_ROW_ALIGNMENT,
    TOKEN_PIXELS,
    TextAttentionLayout,
    build_text_attention_mask,
    condition_tokens,
    plan_text_attention,
    require_text_attention_target,
    text_attention_mask_bytes,
    token_grid_inside,
)
from .hardware import (  # noqa: F401  - re-exported public surface
    memory_snapshot,
    _clear_torch_cache,
    _cuda_device_index,
    _resolve_selected_backend_device,
)
from .memory import (  # noqa: F401  - re-exported public surface
    LEGACY_PROFILE_STEPS,
    RAM_STEP_LABELS,
    STEP_FIELD_KEYS,
    VRAM_STEP_LABELS,
    forecast_memory,
    legacy_profile_name,
    legacy_steps,
    encoder_cpu_pinned,
    is_host_run_device,
    requested_steps,
    steps_fields,
    _preset_advice,
)
from .pipeline import (  # noqa: F401  - re-exported public surface
    ACTIONABLE_COMPONENTS,
    COMPONENT_ACTIONS,
    EMERGENCY_TILE_LATENT_MIN_SIZE,
    RESIDENCY_GPU,
    RESIDENCY_MIXED,
    RESIDENCY_NOT_LOADED,
    RESIDENCY_OFFLOADED,
    RESIDENCY_RAM,
    WARMUP_LATENT_CELLS,
    _component_actions,
    _component_busy_message,
    _decode_once,
    _emergency_tile_latent_size,
    _encode_prompt_phase,
    _latent_spatial_sides,
    _lowered_vae_tile_thresholds,
    _require_action_name,
    _require_component_name,
    _require_execution_device,
    _set_vae_tiling,
    _synchronize_device,
    _vae_latent_scale_factor,
    _vae_tile_latent_min_size,
    _vae_tiling_engages,
    _warmup_vae_decode,
    decode_region_latents,
    residency_of,
)
from .large_model import (  # noqa: F401  - re-exported public surface
    EncoderOptions,
    PipelineOptions,
    encoder_model_name,
    encoder_spec,
    pipeline_model_name,
    pipeline_spec,
    register_specs,
)
from .service import (  # noqa: F401  - re-exported public surface
    Flux2KleinInpaintService,
)
