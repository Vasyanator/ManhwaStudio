# Module: modules/ai_backend/inpaint/flux2_klein

## Purpose
The FLUX.2 klein region-editing backend: everything behind the IPC methods `inpaint.flux2_klein`,
`.status`, `.estimate`, `.unload`, `.component_action` and the `.prompt_cache.*` family. It edits a
user-selected page REGION with `diffusers.Flux2KleinInpaintPipeline`.

It serves BOTH model variants (klein 9B and klein 4B) and carries no variant field: everything is
driven by the three user-supplied paths, and `_model_key` / `_encoder_key` already guarantee one
resident pipeline and one resident encoder. The one place the variants are told apart is
`components.require_encoder_transformer_compatible`, which refuses a MIXED pair before any weight
is read — see the FLUX.2 klein sections of `../MODULE_README.md`.

**This file documents the LAYOUT only.** The domain contracts — mask semantics, run order,
placement modes, the memory forecast, per-component residency, the prompt-cache library — are
documented in the FLUX.2 klein sections of `../MODULE_README.md`, which is the authority on them.

## Architecture
Strict one-directional layering. A module may import from any module ABOVE it in this list and from
`../../runtime/`, never from a module below it:

```
params, progress, imaging          (no intra-package dependencies)
attention                          (-> params)
components                         (-> params, runtime.torch_support)
streaming                          (-> components)
hardware                           (-> runtime.torch_support, ai_device, config)
prompt_cache                       (-> components, params, runtime.paths)
memory                             (-> attention, components, hardware, params, streaming)
pipeline                           (-> components, hardware, progress, streaming,
                                        runtime.rocm_mmap_transfer)
service                            (-> all of the above, runtime.model_manager)
__init__                           (-> all of the above; re-export only)
```

`__init__.py` defines no behaviour. It carries the package docstring and re-exports the package's
complete public surface, so `server.py`, `test_lease_protocol.py` and any `flux2_klein.<name>`
reference resolve against the package object itself.

## Files and submodules
- `__init__.py`: the package docstring (mask semantics, run order, model layout) and the re-export
  block. Nothing is defined here except the package-level `log`.
- `params.py`: `normalize_flux2_klein_params`, `_whole_region_overrides`, `validate_region_size`,
  `_model_key`, `effective_steps`, `_lenient_paths`, `text_encoder_dtype_name` and the coercion
  helpers. Edit it to change the wire contract, a clamp, the region limits, or the dtype the text
  encoder runs in.
- `progress.py`: the `phase:"load"` step numbering (`LOAD_STEP_*`, `LOAD_PHASE_STEPS`), the
  `ProgressCb` protocol with its three OPTIONAL file fields, the `FileProgressCb` type a loader is
  handed, `_progress_reporter` (step level) and `_file_progress_reporter` (byte level INSIDE one
  step). A wire contract shared with the Rust client; it deliberately imports nothing from the
  package.
- `components.py`: the five components AS FILES ON DISK — search roots and probe order, tokenizer and
  scheduler discovery, safetensors header reading (`read_safetensors_header_and_data_start` is the
  parser; `read_safetensors_header` is its header-only view), the fp8-scaled refusal, the transformer-config
  contract, text-encoder availability, the encoder<->transformer width contract
  (`require_encoder_transformer_compatible`), the encoder's truncation policy
  (`ENCODER_KEEP_LAYERS`, `text_encoder_truncation_kwargs`), what the checkpoint's own
  `model_index.json` declares about guidance distillation (`model_index_search_roots`,
  `checkpoint_is_distilled` — a tri-state), the TWO size answers
  (`_weight_bytes` = disk, `text_encoder_resident_bytes` = RAM), the VAE's tiling threshold
  (`vae_tile_threshold_pixels`), and the `components` map `status` returns.
- `streaming.py`: the single-file transformer loader that reads ONE TENSOR AT A TIME, so the host
  peak is the largest tensor instead of the whole checkpoint: the safetensors reader
  (`parse_tensor_spans`, `StreamingSafetensorsReader` — file order, and every malformed entry an
  explicit refusal, INCLUDING two spans that overlap: aliased weights are the one corruption that
  would otherwise load "successfully"), the one cross-key layout decision (`checkpoint_layout`), the per-key conversion
  driver (`convert_tensor_to_diffusers`, `iter_converted_transformer_tensors`), the loader
  (`load_transformer_streaming`), the predicate a caller consults first (`streaming_load_eligible`)
  and the byte-level progress channel (`StreamingProgressCb`, `_ByteProgress`). It imports
  `components` and nothing else from the package — notably NOT `progress`, whose `LOAD_STEP_*`
  numbering is a wire contract this module has no business touching.
- `prompt_cache.py`: encoder identity (`text_encoder_fingerprint`, `encoder_family_name`), the
  `.msprompt` container (`write_prompt_file`, `read_prompt_file_header`,
  `validate_prompt_file_metadata`) and the on-disk library under `<program root>/prompt_cache/`.
- `imaging.py`: wire decode/encode of the region and the mask, the color match against the ring
  outside the mask, the feathered composite, and the mask morphology they need.
- `attention.py`: the `text_attention_in_mask` contract — the region mask on the 16 px token grid
  (`token_grid_inside`), the exact token count of a condition image (`condition_tokens`), one
  run's joint-sequence layout (`plan_text_attention` -> `TextAttentionLayout`, including whether
  the reference follows the region's inside/outside grid), the pre-load refusal of an empty mask
  (`require_text_attention_target`), THE device-byte
  formula of the mask (`text_attention_mask_bytes`, shared by the run's log and
  `memory.forecast_memory`) and the additive mask itself (`build_text_attention_mask`, the only
  torch user here). It sits directly under `params` so that both `memory` and `service` can reach
  the one formula; the domain contract is in `../MODULE_README.md`.
- `hardware.py`: `memory_snapshot`, `_resolve_selected_backend_device`, `_clear_torch_cache`,
  `_cuda_device_index` — the accelerator and what is free on it.
- `memory.py`: `forecast_memory` and the pre-load guard `_require_memory_headroom` built on it,
  `_transformer_avoids_the_host` (whether the transformer load really keeps the checkpoint out of
  host memory), plus `_preset_advice` and the advisory constants. `estimate` and the guard must stay
  ONE piece of arithmetic; do not add a second forecast.
- `pipeline.py`: component loaders, the choice between `streaming.load_transformer_streaming` and
  diffusers' `from_single_file` (and the log line that records a refusal), the four placements and
  the ROCm staging they run under, per-component residency and the action matrix, the warm-up, the
  prompt-encode phase, the fp8 quantization, and the VAE decode with its out-of-memory recovery.
- `service.py`: `Flux2KleinInpaintService` — the service lock, the resident pipeline, the in-memory
  prompt cache, the model lease, the request methods the IPC handlers call, and
  `_effective_guidance_scale` — the one place a run learns whether classifier-free guidance can
  happen at all.
- `_test_fixtures.py`: shared test fixtures (the on-disk model tree — whose VAE config carries the
  real `sample_size` / `block_out_channels`, so the tiling threshold is exercised rather than
  faked, and whose `model_index.json` is `DEFAULT_MODEL_INDEX`, faithful to the shipped 4B one —
  the fake `torch`/`diffusers`/`transformers` stack, `_TempTreeCase` with its `declare_distilled`
  helper, `_PlacementFixture`, `_ResidencyModule`, and the two safetensors writers —
  `_write_safetensors` for a header-only container and `_write_safetensors_with_data` for a real one
  whose tensor bytes sit at chosen offsets). The leading underscore keeps pytest from collecting it.
- `test_params.py`, `test_components.py`, `test_prompt_cache.py`, `test_imaging.py`, `test_attention.py`,
  `test_hardware.py`, `test_memory.py`, `test_pipeline.py`, `test_streaming.py`,
  `test_service.py`: unit tests, one per module they cover. No torch, no diffusers, no weights, no
  GPU — except `PromptCacheRoundTripTests` (real torch for `safetensors.torch`) and
  `test_streaming.py`'s `_RealTorchFixture` subclasses (real torch for the converter and the meta
  fill) and `test_attention.py`'s `AttentionMaskBuildTests` (real torch on the CPU for the
  mask) and `RealTransformerLayoutTests` (real torch AND diffusers: a tiny randomly initialised
  `Flux2Transformer2DModel` that pins the mask layout to diffusers' real concatenation — no
  download, no file), which skip themselves without them and still need no GPU. Run them with
  `python -m pytest modules/ai_backend/inpaint/ -q` from the repo root.

## Contracts and invariants

### Monkeypatched symbols must be reached through their defining module
The test suite replaces module-level attributes to keep torch, the filesystem and the memory
snapshot out of the way. Monkeypatching couples the DEFINITION site to the USE site: across several
modules, one `patch.object` reaches every caller only if every caller looks the name up on the
module that defines it.

Fifteen names are affected, listed with their owner:

| Name | Owner |
|---|---|
| `patched_module_to`, `mmap_staging_required`, `tensor_needs_staging`, `_quantize_text_encoder_fp8`, `_restore_transformer_to_device` | `pipeline` |
| `load_transformer_streaming` | `streaming` |
| `memory_snapshot`, `_clear_torch_cache`, `_resolve_selected_backend_device` | `hardware` |
| `_weight_bytes`, `text_encoder_resident_bytes` | `components` |
| `program_root`, `write_prompt_file` | `prompt_cache` |
| `build_text_attention_mask` | `attention` |
| `is_torch_available` | `components` AND `hardware` — TWO bindings, see below |

`is_torch_available` is the one name in this table WITHOUT a single owner, and it must not be
read as if it had one. It is defined in `../../runtime/torch_support.py`, which sits above the
whole package, and `components` and `hardware` each import it from there directly. Those are two
independent bindings: patching `components.is_torch_available` does NOT reach
`hardware.memory_snapshot`, and vice versa. A test must patch the module whose code path it is
exercising, or both. There is no single binding on purpose — funnelling `hardware` through
`components` would add a `hardware -> components` dependency edge that exists only for the tests,
and the layering block above deliberately gives `hardware` no knowledge of components at all.

Therefore:

- a module that consumes one of these from ANOTHER module writes `hardware.memory_snapshot(...)`,
  `pipeline._restore_transformer_to_device(...)`, `streaming.load_transformer_streaming(...)`,
  `components._weight_bytes(...)`,
  `components.text_encoder_resident_bytes(...)`, `prompt_cache.write_prompt_file(...)`,
  `attention.build_text_attention_mask(...)` — never
  `from .hardware import memory_snapshot`;
- tests patch the OWNER module, never the `flux2_klein` package. `__init__.py` re-exports all
  fifteen, but a re-export is a separate binding: patching it would be a silent no-op for every
  real caller. The comment above the re-export block says so.
- `_weight_bytes` and `text_encoder_resident_bytes` are BOTH patch points and must be patched
  TOGETHER wherever a test pins component sizes: `forecast_memory` asks the second for the text
  encoder and the first for the transformer and the VAE, so patching one alone silently drops the
  encoder out of the forecast under test.

Adding a new patch point means adding it to that table and to the consumer rule; adding a plain
`from .x import y` for one of these names is a defect.

### Tiling is a REQUEST, and three modules must agree on what it means
`vae_tiling` sets `use_tiling`; it moves no threshold. diffusers decodes tiled only when a LATENT
side exceeds `tile_latent_min_size`, i.e. above 1024 output px on the shipped klein VAE, so on a
smaller region the flag changes nothing at all. Three places therefore ask before they act, and none
of them may repeat the 1024/128 literals:

- `memory.forecast_memory` credits the cheap per-pixel constant only when the region reaches
  `components.vae_tile_threshold_pixels` (read from the VAE's `config.json`, with a fallback that
  assumes tiling will NOT help — the expensive answer, because this feeds the pre-load guard);
- `pipeline._decode_region_latents` sets `applied["vae_tiling"]` only when `_vae_tiling_engages`
  says the tiled path will really be entered — `applied` is persisted into the user's settings;
- the last rung lowers `tile_latent_min_size` AND `tile_sample_min_size` together
  (`_lowered_vae_tile_thresholds`) for ONE decode, then restores both.

`vae_slicing` is inert here in every case (`decode` slices only a batch larger than one) and is kept
only because the Rust `MemoryPreset` mirrors it: see `dev-docs/known_gaps.md`, KG-008.

### The re-export surface is load-bearing
`server.py` imports `Flux2KleinInpaintService` from the package, `test_lease_protocol.py` reaches
the service through the `flux2_klein` module object, and every test module reads constants and
helpers as `svc.<name>`. Removing a name from `__init__.py` breaks those without a type error at
import time in the case of the tests. If a declaration is deleted, delete it from the re-export
block in the same change.

### One logger tree
Every module defines `log = logging.getLogger(__name__)`, so records land under
`modules.ai_backend.inpaint.flux2_klein.<module>`. `__init__.py` defines the parent logger of that
tree, which is what makes `assertLogs(svc.log)` in the tests capture records emitted by the
submodules.

## Editing map
- To change a request parameter, a clamp, or the region limits, see `params.py`.
- To change where a component is looked for on disk, what a checkpoint may contain, the rule that
  tells a 9B component pair from a 4B one, how deep the text encoder is loaded, what the checkpoint
  declares about guidance distillation, or the pixel side above which the VAE really decodes tiled,
  see `components.py`.
- To change what happens to `guidance_scale` on a distilled checkpoint, see
  `service._effective_guidance_scale` — and nowhere else: `_prompts_to_encode`,
  `_prompt_embeds_locked` and `_generate_locked` all read the scale through it, so that the
  negative encode, the memory guard's `encode` phase and the value handed to diffusers cannot
  disagree. The pipeline's own `is_distilled` comes from the same
  `components.checkpoint_is_distilled`, in `_ensure_pipeline_locked`.
- To change the dtype the text encoder runs in, see `params.text_encoder_dtype_name` — and nowhere
  else: `_encoder_key`, `_prompt_cache_key` and the `.msprompt` metadata all read it.
- To change the prompt-cache file format or the library layout, see `prompt_cache.py`.
- To change the composite, the color match, or the mask morphology, see `imaging.py`.
- To change which token pairs `text_attention_in_mask` blocks, the token-grid rule, or the
  mask's dtype/padding/size, see `attention.py` — and keep `text_attention_mask_bytes` the ONE
  formula both the forecast and the allocation use. Whether the mask is sent at all, and its
  release from `pipe._attention_kwargs` after the call, is `service._generate_locked` (through
  `_text_attention_mask_locked`); the early empty-mask refusal is in
  `service.inpaint_image_bytes`; the whole_region override is `params._whole_region_overrides`.
  After a diffusers upgrade, `RealTransformerLayoutTests` is what proves the layout still holds.
- To change the memory forecast or the pre-load guard, see `memory.py` (and never split the
  arithmetic in two). The pipeline's transient HOST term is `_transformer_avoids_the_host`, which
  answers from the request alone and answers EXPENSIVELY (the whole checkpoint) whenever it cannot
  prove the saving — a `low_cpu_mem_usage` a loader ignores is not a saving.
- To change how a single-file transformer is read tensor by tensor, which safetensors dtypes are
  mappable, what makes a header malformed, when the converter runs, or the byte-level load
  progress, see `streaming.py`. `parse_tensor_spans` rejects OVERLAPPING spans but deliberately
  does NOT require full contiguity: a gap is unreadable-past-and-harmless because every span is
  seeked to absolutely, while an overlap is the only layout that fills every expected key and
  still hands back corrupted weights. Do not tighten it to contiguity — that would refuse an
  alignment-padded container this reader loads correctly. Its
  preconditions are the caller's to check through `streaming_load_eligible`, and the decision to
  use the ordinary loader instead must be logged AT THE CALL SITE — `streaming.py` deliberately
  contains no fallback. That call site is `pipeline._load_transformer`, inside the single-FILE
  branch and after the fp8 refusal and the transformer-config validation; the two guarantees that
  follow the load (`_apply_placement`, then `_require_components_materialized`) apply to a streamed
  model exactly as to any other and must not be made conditional on the loader.
- To change what the progress bar shows WHILE the transformer is read, see
  `progress._file_progress_reporter` and its one caller, `service._ensure_pipeline_locked`. It adds
  the wire's optional second level (`file_step`/`file_total`/`file_label`) to the SAME frame; the
  step numbers must not move, they are the contract the Rust client counts against. The label names
  the FILE, never the tensor. A `progress_callback` reaches the build only because
  `_ensure_pipeline_locked`, `_component_action_locked` and `_load_pipeline_action_locked` carry it
  beside the `(step, label)` reporter — only the raw callback can express the second level.
- To change a placement, a loader, the residency probe, the action matrix, or the decode recovery,
  see `pipeline.py`. The recovery's rungs are `_decode_region_latents`; whether tiling can engage at
  all is `_vae_tiling_engages` (never a hard-coded 1024/128), and the last rung's paired thresholds
  are `_lowered_vae_tile_thresholds` — they move TOGETHER or not at all.
- To change the lease protocol, the run order, or a request method's response shape, see
  `service.py` — and read the lease-protocol section of `../MODULE_README.md` first.
- To add a declaration that other modules or the tests must see, add it to the matching module AND
  to the re-export block of `__init__.py`.
