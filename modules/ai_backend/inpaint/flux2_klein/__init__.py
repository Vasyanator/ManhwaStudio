"""
Package: modules/ai_backend/inpaint/flux2_klein

Purpose:
FLUX.2 klein (9B / 4B) region-editing service for the Python AI backend (methods
`inpaint.flux2_klein`, `.status`, `.unload`, `.component_action`
and the `.prompt_cache.*` family; streaming). It edits a user-selected page
REGION with `diffusers.Flux2KleinInpaintPipeline`. Its weights are two handles
of the large-model manager (`runtime.large_models`): the PIPELINE (transformer
+ VAE) and the TEXT ENCODER (Qwen3, truncated); the manager decides where every
byte lives in every phase, admits a run, degrades it on a memory failure and
keeps the model between runs per the two resource steps («Видеопамять» x
«Оперативная память»). This package describes
FLUX.2 to it (`large_model.py`) and drives runs through its sessions
(`service.py`); a request names its step pair as `vram_step` / `ram_step`
(`memory.requested_steps`).

Mask semantics (this is NOT classic inpainting):
The mask is a PERMISSION TO CHANGE. The wire mask is the LATENT mask, already
dilated by the client, and is used as-is as the pipeline's `mask_image`; the
answer is the RAW decoded region, and the client does the colour alignment and
the composite that make everything outside the painted mask come back
byte-identical. The wire format is L8 and only L8. `text_attention_in_mask=True`
confines the prompt to the latent mask's tokens; `whole_region=True` is the "no
mask" mode, verified against a solid mask, and settles the attention flag by
itself.

Run order (a memory contract, see `service._run_locked`): the pipeline handle
is admitted and loaded first, the text encoder handle second (on the GPU when
the device holds it beside the pipeline, else on the CPU from RAM), then the
denoise and the VAE decode run as manager phases. The prompt-embedding cache
(in memory, LRU; one library of `.msprompt` presets per model on disk, served
by an exact text match whatever encoder variant made the entry) lets a run
skip the encoder entirely, including on a machine where none is installed.

Model layout:
Nothing is downloaded. The user supplies three paths — a Qwen3 text encoder (a
folder, or a llama.cpp GGUF file), a transformer (`.safetensors` single file,
a GGUF file or a diffusers folder) and an `AutoencoderKLFlux2` folder; the
tokenizer and the scheduler are DISCOVERED next to them, and a single-file
transformer needs its own `config.json` beside it. A GGUF file kept outside
the packages borrows its config, tokenizer and scheduler from an installed
FLUX.2 klein package (4B or 9B) whose config matches the file's header. The
text encoder's config goes through one kwargs entry (truncation plus the
RoPE base of a transformers-v5 config, `components.text_encoder_config_kwargs`).

Module layout (this file re-exports the package's public surface):
- `params.py`       - request normalization, region validation, coercion helpers.
- `progress.py`     - `phase:"load"` step numbering and the progress callback type.
- `conversion.py`   - the transformer checkpoint's BFL -> diffusers key rule.
- `encoder_gguf.py` - llama.cpp GGUF <-> HF Qwen3 names of the text encoder.
- `components.py`   - the five components as files on disk (and a GGUF file's config).
- `attention.py`    - `text_attention_in_mask`: token grid, layout, the mask and its bytes.
- `hardware.py`     - device resolution, memory snapshot, torch cache clearing.
- `prompt_cache.py` - encoder identity and provenance, the FLUX.2 libraries over
                      `runtime.prompt_library`, the v1 migration.
- `pipeline.py`     - what a phase does with the placed modules; residency / actions.
- `large_model.py`  - the manager specs, options, phase work, decode fallbacks.
- `memory.py`       - step pair and step keys, the combined VRAM-axis rule, the two-handle forecast.
- `service.py`      - `Flux2KleinInpaintService`: the lock, the sessions, the run.

Notes:
- torch / diffusers / transformers are imported lazily inside the
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
# `_resolve_selected_backend_device`, `_tables_for`, `program_root`,
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
    RETIRED_FIELDS,
    VALID_DTYPES,
    VALID_RAM_STEPS,
    VALID_VRAM_STEPS,
    GGUF_FORMAT,
    SAFETENSORS_FORMAT,
    WEIGHT_FORMAT_ID_PATTERN,
    derived_encoder_format,
    derived_format,
    effective_steps,
    encoder_on_cpu,
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
    FILE_COMPONENT_TEXT_ENCODER,
    FILE_COMPONENT_TRANSFORMER,
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
    LoadFrames,
    ProgressCb,
    _progress_reporter,
)
from .components import (  # noqa: F401  - re-exported public surface
    checkpoint_is_distilled,
    component_dir_for_path,
    component_probe_order,
    component_safetensors_shards,
    component_search_roots,
    discover_component_dir,
    find_text_encoder_config_dir,
    find_transformer_config_dir,
    gguf_config_candidates,
    gguf_layout,
    existing_klein_package_roots,
    installed_klein_package_roots,
    is_gguf_file,
    text_encoder_config_dir,
    KLEIN_PACKAGE_DIR_NAMES,
    GGUF_ENCODER_ARCHITECTURE,
    GgufConfigSearch,
    is_fp8_scaled_checkpoint,
    model_index_search_roots,
    read_safetensors_header,
    require_encoder_transformer_compatible,
    require_text_encoder,
    text_encoder_available,
    text_encoder_config_kwargs,
    text_encoder_rope_kwargs,
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
from .encoder_gguf import (  # noqa: F401  - re-exported public surface
    encoder_state_key_of_gguf,
    gguf_encoder_quant_types,
    is_gguf_encoder_name,
    require_known_encoder_names,
)
from .conversion import (  # noqa: F401  - re-exported public surface
    FLUX2_BFL_MARKER_KEY,
    LDM_UNET_PREFIX,
    convert_bfl_tensor,
    is_bfl_key,
    require_known_layout,
)
from .prompt_cache import (  # noqa: F401  - re-exported public surface
    FLUX2_LIBRARY_IDS,
    LEGACY_LIBRARY_BY_WIDTH,
    PROMPT_CACHE_DIRNAME,
    PROMPT_CACHE_TENSOR,
    PROMPT_EMBED_CACHE_ENTRIES,
    V1_FORMAT,
    KnownEncoder,
    MigrationReport,
    V1Verdict,
    classify_v1_entry,
    convert_v1_metadata,
    encoder_label,
    encoder_provenance,
    ensure_legacy_migrated,
    flux2_library_spec,
    is_current_encoder,
    known_local_encoders,
    library_of,
    migrate_legacy_families,
    prompt_cache_root,
    read_v1_header,
    text_encoder_fingerprint,
    transformer_text_width,
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
    STEP_FIELD_KEYS,
    forecast_memory,
    is_host_run_device,
    requested_steps,
    steps_fields,
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
    COMPUTE_GGUF,
    COMPUTE_NATIVE,
    ENCODE_VARIANT,
    ENCODE_VARIANT_GGUF,
    ENCODER_WEIGHT_FORMAT,
    EncoderOptions,
    PipelineOptions,
    encoder_model_name,
    encoder_options,
    encoder_path_for,
    encoder_spec,
    gguf_quant_types,
    pipeline_model_name,
    pipeline_options,
    pipeline_spec,
    register_specs,
    require_loadable,
    require_loadable_encoder,
    transformer_path_for,
    weight_format_of,
)
from .service import (  # noqa: F401  - re-exported public surface
    Flux2KleinInpaintService,
)
