# Module: modules/ai_backend/inpaint/flux2_klein

## Purpose
The FLUX.2 klein region-editing backend: everything behind the IPC methods `inpaint.flux2_klein`,
`.status`, `.unload`, `.component_action` and the `.prompt_cache.*` family, plus the FLUX.2 adapter
of `large_models.profiles` (`service.large_model_profiles`). It edits a
user-selected page REGION with `diffusers.Flux2KleinInpaintPipeline`, whose weights are handles of
the large-model manager (`../../runtime/large_models/`): the manager decides where every byte lives
in every phase, admits, degrades and keeps the model between runs; this package describes FLUX.2 to
it (`large_model.py`) and drives runs through its sessions (`service.py`).

It serves BOTH model variants (klein 9B and klein 4B) and carries no variant field: everything is
driven by the three user-supplied paths (block and layer counts are read from the files). The one
place the variants are told apart is `components.require_encoder_transformer_compatible`, which
refuses a MIXED pair before any weight is read — see the FLUX.2 klein sections of
`../MODULE_README.md`. Only the 9B checkpoint is on this machine (2026-10-16); the 4B path is
exercised by the tiny real models of the tests.

**This file documents the LAYOUT only.** The domain contracts — mask semantics, run order, the
request's step pair and the refused placement wire, the decode fallbacks, per-component residency,
the prompt-cache library — are documented in the FLUX.2 klein sections of `../MODULE_README.md`, which
is the authority on them.

## Architecture
Strict one-directional layering. A module may import from any module ABOVE it in this list and from
`../../runtime/`, never from a module below it:

```
params, progress, conversion            (no intra-package dependencies; params -> runtime tiers)
attention                               (-> params)
components                              (-> params, runtime.torch_support, runtime.safetensors_stream,
                                             runtime.encoder_truncation)
hardware                                (-> runtime.torch_support, runtime.memory_probe,
                                             runtime.torch_device)
prompt_cache                            (-> components, params, runtime.paths)
pipeline                                (-> components)
large_model                             (-> attention, components, conversion, params, pipeline,
                                             runtime.large_models)
memory                                  (-> attention, components, hardware, large_model,
                                             runtime.large_models, runtime.memory_budget)
service                                 (-> all of the above, runtime.large_models, runtime.residency,
                                             runtime.model_manager)
__init__                                (-> all of the above; re-export only)
```

Two manager handles per run: the PIPELINE (`transformer` + `vae`; phases `denoise`, `decode`;
model `flux2_klein` / `flux2_klein.float16`) and the TEXT ENCODER (`text_encoder`; phase
`encode`; model `flux2_klein_text_encoder` / `.fp8`, computing on the run device or the CPU as the
plan decides, always on the CPU under «Энкодер на CPU»: `params.encoder_on_cpu`, default on,
`EncoderOptions.on_cpu` -> the spec's host pin, its own residency key). `service.py` admits the pipeline first and
the encoder second, under its lock, the encoder under a device ceiling that keeps the two handles
together monotone on the VRAM axis (`memory.combined_ceilings` / `encoder_device_cap`, planned
before the pipeline's admission; one rule for the run, `memory.forecast_memory` and
`large_model_profiles`).

`__init__.py` defines no behaviour. It carries the package docstring and re-exports the package's
public surface, so `server.py` and any `flux2_klein.<name>` reference resolve against the package
object itself.

## Files and submodules
- `__init__.py`: the package docstring and the re-export block (plus the package-level `log`).
- `params.py`: `normalize_flux2_klein_params` (including the optional `vram_step` / `ram_step`
  and the by-name refusal of the retired placement wire, `RETIRED_FIELDS`),
  `_whole_region_overrides`, `validate_region_size`, `effective_steps`,
  `_lenient_paths`, `encoder_on_cpu` (the «Энкодер на CPU» flag and its default, also read
  leniently by `status`), `text_encoder_dtype_name` and the coercion helpers.
- `progress.py`: the `phase:"load"` step numbering (`LOAD_STEP_*`, `LOAD_PHASE_STEPS`), the
  `ProgressCb` protocol with its three OPTIONAL file fields, `_progress_reporter` and
  `_file_progress_reporter`. A wire contract shared with the Rust client; imports nothing from the
  package.
- `conversion.py`: the per-key BFL / diffusers layout rule of the transformer checkpoint
  (`is_bfl_key`), diffusers' own per-key converter on one tensor (`convert_bfl_tensor`), the
  `model.diffusion_model.` prefix refusal.
- `components.py`: the five components AS FILES ON DISK — search roots and probe order, tokenizer
  and scheduler discovery, header reading, the fp8-scaled refusal, the transformer-config contract,
  text-encoder availability, the encoder<->transformer width contract, the encoder truncation
  (`ENCODER_KEEP_LAYERS`, `text_encoder_truncation_kwargs`), `checkpoint_is_distilled`, the disk
  sizes `status` reports, the VAE tiling threshold and the `components` map of `status`.
- `large_model.py`: the `LargeModelSpec`s (`pipeline_spec(dtype)`, `encoder_spec(fp8)`,
  `register_specs`), their options (`PipelineOptions`, `EncoderOptions`, `request_shape`), the key
  mapping and block structure (`transformer_state_keys`, `transformer_block_of`,
  `transformer_block_order`, the encoder's truncation / `model.` prefix / fp8 split), each phase's
  work, the decode fallbacks (`FALLBACK_VAE_TILING`, `FALLBACK_VAE_TILE_LOWERED`), the
  pre-admission refusals (`require_loadable`, `require_loadable_encoder`) and the model names.
- `prompt_cache.py`: encoder identity, the `.msprompt` container and the on-disk library.
- `attention.py`: the `text_attention_in_mask` contract — the token grid, the joint-sequence layout,
  the early empty-mask refusal, THE device-byte formula of the mask (shared by the run's log and the
  spec's denoise work) and the additive mask itself.
- `hardware.py`: `memory_snapshot` (the free figures `memory.forecast_memory` plans against in
  the calibration tool),
  `_resolve_selected_backend_device`, `_clear_torch_cache`, `_cuda_device_index`.
- `memory.py`: the glue over the manager — `requested_steps` (the request's step pair),
  `steps_fields` / `STEP_FIELD_KEYS` (the four step keys of an answer), the combined VRAM-axis
  rule (`combined_device_peak`, `combined_ceilings`, `encoder_device_cap`), `forecast_memory`
  (the two-handle run forecast from the manager's planner + forecast; consumers: the calibration
  tool and the golden test), `is_host_run_device`. No arithmetic of its own.
- `pipeline.py`: what a run does WITH the placed modules — the prompt encode, the warm-up decode,
  the execution-device check, ONE VAE decode attempt with its tiling options, and the residency
  vocabulary / action matrix (`residency_of`, `_component_actions`).
- `service.py`: `Flux2KleinInpaintService` — the service lock, the prompt cache, the tokenizer /
  scheduler caches, the manager sessions of a run and of every action (acquired under the lock, pipeline
  before encoder; `LockOrderTests`), `large_model_profiles` (the `large_models.profiles` adapter:
  pipeline pickers with the encoder's claim; `large_model_name` / `large_model_inputs` its
  single-handle fallback), `set_phase_observer` (the calibration hook) and
  `_effective_guidance_scale`.
- `fixtures/forecast_golden.json`: the golden `forecast_memory` snapshot of
  `test_characterization.py`, one case per step pair (regenerate with its `--write-golden`).
- `_test_fixtures.py`: shared test fixtures — the model tree (header-only weights, or tiny REAL
  models with `real_weights=True`), `_TempTreeCase`, `_FakeEmbeds`, and `_ManagedRunFixture` (a
  service over a REAL manager on the CPU; only the pipeline call, the tokenizer, the budget probes
  and the transport's device are faked).
- `test_*.py`: one per module; `test_characterization.py` holds the CONTRACT tests of the wire (and
  the GOLDEN forecast), `test_runtime_extraction.py` pins what FLUX.2 keeps seeing of names
  that live in `../../runtime/`. Torch / diffusers / transformers tests skip without them; no GPU,
  no network. Run them with `python -m pytest modules/ai_backend/inpaint/ -q` from the repo root.

## Memory behaviour under the manager
- The decode out-of-memory order is the manager ladder's: tiling, the lowered tile, then the next
  «Видеопамять» step (the transformer off the device).
- DiT blocks are juggled by the manager's block hooks (no accelerate offload); the encoder runs
  on the GPU when the step pair and the device allow it beside the pipeline, else on the CPU —
  always on the CPU under «Энкодер на CPU» (no VRAM step's forecast includes it then; the RAM
  step keeps it, releases it after the run, or re-reads it per run);
  forecasts are measured where the device class's calibration table has `flux2_klein` entries.
- `status.loaded` / `health.ready` mean "the pipeline handle holds bytes"; the pipeline is a LARGE
  holding and never counts in the small pool's counters.

## Contracts and invariants

### Monkeypatched symbols must be reached through their defining module
The test suite replaces module-level attributes to keep the GPU, the filesystem and the memory
snapshot out of the way. One `patch.object` reaches every caller only if every caller looks the name
up on the module that defines it:

| Name | Owner |
|---|---|
| `memory_snapshot`, `_clear_torch_cache`, `_resolve_selected_backend_device` | `hardware` |
| `_tables_for` (the forecast's cost tables) | `memory` |
| `_weight_bytes` (the disk size `status` reports) | `components` |
| `program_root`, `write_prompt_file` | `prompt_cache` |
| `build_text_attention_mask` | `attention` |
| `is_torch_available` | `components` AND `hardware` — TWO bindings, see below |

`is_torch_available` has no single owner: it is defined in `../../runtime/torch_support.py` and
`components` and `hardware` each import it from there, so patching one does not reach the other's
code path; a test patches the module whose path it exercises, or both.

Therefore a module that consumes one of these from ANOTHER module writes
`hardware.memory_snapshot(...)`, `memory._tables_for(...)`, `prompt_cache.write_prompt_file(...)`,
`attention.build_text_attention_mask(...)` — never `from .hardware import memory_snapshot`; and
tests patch the OWNER module, never the `flux2_klein` package (a re-export is a separate binding).

### Tiling is a REQUEST, and three places must agree on what it means
`vae_tiling` sets `use_tiling`; it moves no threshold. diffusers decodes tiled only when a LATENT
side exceeds `tile_latent_min_size` (above 1024 output px on the shipped klein VAE). None of the
three places may repeat the 1024/128 literals:

- `large_model._decode_work` counts one tile (`min(side, tile)` per side) only when the region
  exceeds `components.vae_tile_threshold_pixels` (read from the VAE's `config.json`, with a fallback
  no region reaches — the expensive answer);
- `pipeline.decode_region_latents` reports `tiled` (what `applied["vae_tiling"]` may claim) only
  when `_vae_tiling_engages` says the tiled path was really entered;
- the lowered-tile fallback lowers `tile_latent_min_size` AND `tile_sample_min_size` together
  (`_lowered_vae_tile_thresholds`) for ONE decode, then restores both.


### The manager owns every byte
Nothing in this package moves a module, picks a device for a weight, or decides what stays
resident: those are the plan's (`runtime/large_models/`). A phase function fetches its modules
from its `PhaseContext` on every call and is restartable from its inputs (the ladder may rebuild a
module or change the options); the host copy of the latents is what lets the decode retry without
the denoise.

### The re-export surface is load-bearing
`server.py` imports `Flux2KleinInpaintService` from the package and every test module reads
constants and helpers as `svc.<name>`. If a declaration is deleted, delete it from the re-export
block in the same change.

### One logger tree
Every module defines `log = logging.getLogger(__name__)`; `__init__.py` defines the parent logger,
which is what makes `assertLogs(svc.log)` capture records of the submodules.

## Editing map
- To change a request parameter, a clamp, the region limits, the accepted step ids or the
  retired-field refusals, see `params.py`.
- To change the step keys of an answer, the combined VRAM-axis rule or the two-handle forecast,
  see `memory.py`.
- To change what the manager knows of a component — files, keys, blocks, dtype, fp8, a phase's
  work, a decode fallback, a pre-admission refusal — see `large_model.py`; the BFL key rule is
  `conversion.py`. The steps, the forecast, the planner, the loader and the ladder are
  `../../runtime/large_models/` (model-agnostic: never special-case FLUX.2 there).
- To change where a component is looked for on disk, what a checkpoint may contain, the variant
  rule, how deep the text encoder is, what the checkpoint declares about guidance distillation, or
  the VAE's tiling threshold, see `components.py`.
- To change what happens to `guidance_scale` on a distilled checkpoint, see
  `service._effective_guidance_scale` — and nowhere else.
- To change the dtype the text encoder runs in, see `params.text_encoder_dtype_name`.
- To change the run order, the warm-up, the response shape or an action, see `service.py`
  (`_run_locked`, `_load_pipeline_locked`, `_denoise`, `_encode_prompts_locked`,
  `_component_action_locked`); what a phase DOES with the modules (encode, warm-up, decode,
  device check) is `pipeline.py`.
- To change the prompt-cache file format or the library layout, see `prompt_cache.py`.
- To change the composite, the color match (global and local; its params are normalized in
  `params.py`, applied in `service.py` against the dilated latent mask), or the mask morphology,
  see `../region_imaging.py`
  (shared with FLUX.3 klein).
- To change which token pairs `text_attention_in_mask` blocks, the token grid, or the mask's
  dtype/padding/size, see `attention.py` — and keep `text_attention_mask_bytes` the ONE formula
  the spec's denoise work and the allocation use. After a diffusers upgrade,
  `RealTransformerLayoutTests` is what proves the layout still holds.
- To change what the progress bar shows while the transformer is read, see
  `progress._file_progress_reporter` and its one caller, `service._load_pipeline_locked`; the step
  numbers must not move.
- To add a declaration that other modules or the tests must see, add it to the matching module AND
  to the re-export block of `__init__.py`.
