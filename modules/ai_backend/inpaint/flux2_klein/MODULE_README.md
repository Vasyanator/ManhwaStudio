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
`../MODULE_README.md`. The tiny real models of the tests exercise both; the recorded 9B GGUF
headers (`fixtures/flux2_9b_gguf_headers.json`) pin the 9B transformer and uncensored encoder
layouts without weights; real 4B / 9B GGUF files are checked by the opt-in
`test_real_gguf_header.py` / `test_real_gguf_encoder_header.py`.

**This file documents the LAYOUT only.** The domain contracts — mask semantics, run order, the
request's step pair and the refused placement wire, the decode fallbacks, per-component residency,
the prompt-cache library — are documented in the FLUX.2 klein sections of `../MODULE_README.md`, which
is the authority on them.

## Architecture
Strict one-directional layering. A module may import from any module ABOVE it in this list and from
`../../runtime/`, never from a module below it:

```
params, progress, conversion            (no intra-package dependencies; params -> runtime tiers,
                                             runtime.tensor_container)
attention                               (-> params)
components                              (-> params, runtime.torch_support, runtime.safetensors_stream,
                                             runtime.encoder_truncation, runtime.tensor_container,
                                             runtime.paths)
hardware                                (-> runtime.torch_support, runtime.memory_probe,
                                             runtime.torch_device)
prompt_cache                            (-> components, params, runtime.paths, runtime.prompt_library,
                                             runtime.prompt_cache_service, runtime.safetensors_stream)
pipeline                                (-> components)
large_model                             (-> attention, components, conversion, params, pipeline,
                                             runtime.large_models, runtime.qwen3_gguf,
                                             runtime.tensor_container; runtime.gguf_layers lazily)
memory                                  (-> attention, components, hardware, large_model,
                                             runtime.large_models, runtime.memory_budget)
service                                 (-> all of the above, runtime.large_models, runtime.residency,
                                             runtime.model_manager, runtime.prompt_cache_service)
__init__                                (-> all of the above; re-export only)
```

Two manager handles per run: the PIPELINE (`transformer` + `vae`; phases `denoise`, `decode`;
model `flux2_klein` / `flux2_klein.float16`) and the TEXT ENCODER (`text_encoder`; phase
`encode`; model `flux2_klein_text_encoder` / `.fp8`, computing on the run device or the CPU as the
plan decides, always on the CPU under «Энкодер на CPU»: `params.encoder_on_cpu`, default on,
`EncoderOptions.on_cpu` -> the spec's host pin, its own residency key). `service.py` admits the pipeline first and
the encoder second, under its lock, the encoder under a device ceiling that keeps the two handles
together monotone on the VRAM axis (the two-handle rule of `../../runtime/large_models/paired.py`
bound to FLUX.2 by `memory.FLUX2_PAIR`: the encoder runs beside the denoise state; planned before
the pipeline's admission; one rule for the run, `memory.forecast_memory` and
`large_model_profiles`).

`__init__.py` defines no behaviour. It carries the package docstring and re-exports the package's
public surface, so `server.py` and any `flux2_klein.<name>` reference resolve against the package
object itself.

## Files and submodules
- `__init__.py`: the package docstring and the re-export block (plus the package-level `log`).
- `params.py`: `normalize_flux2_klein_params` (including the optional `vram_step` / `ram_step`,
  the weight-format fields `transformer_format` / `transformer_formats` with `derived_format`
  and the generic `_weight_formats`, and the by-name refusal of every retired field,
  `RETIRED_FIELDS`: the placement wire and the region post-processing fields),
  `_whole_region_overrides`, `validate_region_size`, `effective_steps`,
  `_lenient_paths`, `encoder_on_cpu` (the «Энкодер на CPU» flag and its default, also read
  leniently by `status`), `text_encoder_dtype_name` and the coercion helpers.
- `progress.py`: the `phase:"load"` step numbering (`LOAD_STEP_*`, `LOAD_PHASE_STEPS`), the
  `ProgressCb` protocol with its OPTIONAL file fields, `FILE_COMPONENT_*`, `_progress_reporter` and
  `LoadFrames` (the request's callback plus the manager's load-progress sink: a component read
  re-emits the CURRENT step frame with the byte level). A wire contract shared with the Rust client; imports nothing from the
  package.
- `conversion.py`: the per-key BFL / diffusers layout rule of the transformer checkpoint
  (`is_bfl_key`), diffusers' own per-key converter on one tensor (`convert_bfl_tensor`), the
  `model.diffusion_model.` prefix refusal.
- `components.py`: the five components AS FILES ON DISK — search roots and probe order, tokenizer
  and scheduler discovery, header reading, the fp8-scaled refusal, the transformer-config contract,
  the config of a GGUF component (beside it, else the installed klein packages
  `KLEIN_PACKAGE_DIR_NAMES`, checked against the file's header: `_search_gguf_config`,
  `find_text_encoder_config_dir`), text-encoder availability, the encoder<->transformer width
  contract, the encoder config kwargs (`text_encoder_config_kwargs` = the truncation
  `ENCODER_KEEP_LAYERS` / `text_encoder_truncation_kwargs` + the RoPE normalization
  `text_encoder_rope_kwargs`), `checkpoint_is_distilled`, the disk
  sizes `status` reports, the VAE tiling threshold and the `components` map of `status`.
- `large_model.py`: the `LargeModelSpec`s (`pipeline_spec(dtype)`, `encoder_spec(fp8)`,
  `register_specs`), their options (`PipelineOptions`, `EncoderOptions`, `request_shape`), the
  transformer's weight formats (`weight_format_of`, `transformer_path_for`, `_pipeline_formats`,
  `gguf_quant_types`, the GGUF module swap in `_build_transformer_meta`), the key
  mapping and block structure (`transformer_state_keys`, `transformer_block_of`,
  `transformer_block_order`, the encoder's truncation / `model.` prefix / fp8 split), each phase's
  work, the decode fallbacks (`FALLBACK_VAE_TILING`, `FALLBACK_VAE_TILE_LOWERED`), the
  pre-admission refusals (`require_loadable`, `require_loadable_encoder`) and the model names.
- `prompt_cache.py`: FLUX.2's side of the shared prompt library (`../../runtime/prompt_library.py`):
  encoder identity (`text_encoder_fingerprint` with the RoPE salt; `rope_salt=False` = the pre-fix
  identity, for the migration only), `encoder_label` / `encoder_provenance` ("made by the current
  encoder" is `prompt_library.same_encoder`),
  the libraries (`flux2_library_spec`, `library_of`, `prompt_cache_root`, `transformer_text_width`,
  `LEGACY_LIBRARY_BY_WIDTH` — a Rust test reads that literal), and the v1 migration
  (`classify_v1_entry`, `convert_v1_metadata`, `migrate_legacy_families`, `ensure_legacy_migrated`;
  links are never followed — `legacy_family_dirs` / `_family_files` refuse them — and a v1 file is
  unlinked only after its v2 copy's directory is fsynced), and `Flux2PromptCacheAdapter` — FLUX.2's
  adapter of the shared orchestration (`../../runtime/prompt_cache_service.py`): the texts a run
  needs (prompt + `""` under effective guidance, through the service's `_effective_guidance_scale`),
  the preference, the width filter, the current encoder, the v1 migration and the v1 import.
- `attention.py`: the `text_attention_in_mask` contract — the token grid, the joint-sequence layout,
  the early empty-mask refusal, THE device-byte formula of the mask (shared by the run's log and the
  spec's denoise work) and the additive mask itself.
- `hardware.py`: `memory_snapshot` (the free figures `memory.forecast_memory` plans against in
  the calibration tool),
  `_resolve_selected_backend_device`, `_clear_torch_cache`, `_cuda_device_index`.
- `memory.py`: the glue over the manager — `requested_steps` (the request's step pair),
  `steps_fields` / `STEP_FIELD_KEYS` (the four step keys of an answer), `FLUX2_PAIR` (FLUX.2's
  `paired.PairPhases`) and the combined VRAM-axis rule bound to it (`combined_device_peak`,
  `encoder_device_cap`; `combined_ceilings` re-exported from `paired`), `forecast_memory`
  (the two-handle run forecast from the manager's planner + forecast; consumers: the calibration
  tool and the golden test), `is_host_run_device`. No arithmetic of its own.
- `pipeline.py`: what a run does WITH the placed modules — the prompt encode, the warm-up decode,
  the execution-device check, ONE VAE decode attempt with its tiling options, and the residency
  vocabulary / action matrix (`residency_of`, `_component_actions`).
- `service.py`: `Flux2KleinInpaintService` — the service lock, the in-memory prompt cache
  (`EmbeddingMemory`) and the shared `PromptCacheOrchestrator` over it, `_prompt_plan` (THE answer
  to which embedding serves each needed text, delegated), the library methods (`prompt_cache_build`
  / `_list` / `_export` / `_import`: FLUX.2's encoder session, progress steps, refusals and answer
  keys around the orchestrator; the test seams `_store_embeds`, `_prompt_cached`, `MemoryEmbedding`,
  `_memory_key`, `_tensor_width` stay reachable here), the tokenizer /
  scheduler caches, the manager sessions of a run and of every action (acquired under the lock, pipeline
  before encoder; `LockOrderTests`), `_encoder_need` (the one decision, as a `paired.EncoderNeed`
  re-exported here, whether a
  request encodes and whether its forecast counts the encoder: not counted only when not resident
  AND fully cached), `_load_frames` (the request's `LoadFrames`; `_acquire` registers its sink on every session, so
  every checkpoint read of a session reports its bytes),
  `large_model_profiles` (the `large_models.profiles` adapter: `paired.plan_pair(...).to_wire()`,
  pipeline pickers with the encoder's claim; `large_model_name` / `large_model_inputs` its
  single-handle fallback), `_combined_ceilings` / `_encoder_cap` (the run's encoder ceiling,
  `paired.live_ceilings` / `encoder_cap_beside`), `set_phase_observer` (the calibration hook) and
  `_effective_guidance_scale`.
- `fixtures/forecast_golden.json`: the golden `forecast_memory` snapshot of
  `test_characterization.py`, one case per step pair (regenerate with its `--write-golden`).
- `fixtures/profiles_golden.json`: the golden LIVE `large_model_profiles` answers of
  `test_characterization.ProfilesGoldenTests` over a REAL manager with fake budgets (tiny real
  models: every step pair x encoder counted / cached / resident-and-cached / on the CPU, plus the
  device cap each real run passes to the encoder's `acquire`; header-only klein 9B files with the
  reference table, a host-short budget included), recorded before the two-handle rule moved to
  `runtime/large_models/paired.py`; a refactor keeps it byte-identical (`--write-profiles-golden`
  only for a deliberate behaviour change).
- `fixtures/prompt_cache_golden.json`: the golden prompt-library answers and refusal texts of
  `test_prompt_cache_golden.py` (`.status` cache keys, build new / already / rename, list, export,
  import of every kind, the v1 migration report), recorded before the orchestration moved to
  `runtime/prompt_cache_service.py`; a refactor keeps it byte-identical (`--write-golden` only for
  a deliberate behaviour change).
- `fixtures/flux2_9b_gguf_headers.json`: recorded headers (names, GGUF dims, ggml types,
  metadata; no tensor data) of the klein 9B GGUF files — full tensor lists for the unsloth 9B
  Q4_K_M transformer and the uncensored Q8_0 encoder, metadata and type histograms for the
  rest; `fixtures/flux2_9b_{transformer,text_encoder,uncensored_text_encoder}_config.json`: the
  pinned 9B configs (the uncensored one is the transformers-v5 file with `rope_parameters`).
  Read through `_test_fixtures.load_9b_header_fixture` / `load_9b_config`;
  `write_header_only_gguf` turns a recorded header into a header-only GGUF.
- `_test_fixtures.py`: shared test fixtures — the model tree (header-only weights, or tiny REAL
  models with `real_weights=True`), `_TempTreeCase`, `_FakeEmbeds`, and `_ManagedRunFixture` (a
  service over a REAL manager on the CPU; only the pipeline call, the tokenizer, the budget probes
  and the transport's device are faked), and the GGUF writers (`_write_tiny_gguf`,
  `_write_bfl_gguf_transformer`: the tiny real transformer as a BFL-layout GGUF plus the
  diffusers state dict it must load as), the 9B fixture loaders and the header-only GGUF writer.
- `test_*.py`: one per module; `test_characterization.py` holds the CONTRACT tests of the wire (and
  the GOLDEN forecast and live profiles), `test_runtime_extraction.py` pins what FLUX.2 keeps seeing of names
  that live in `../../runtime/`, `test_real_gguf_header.py` is the opt-in check of real GGUF
  transformer files (skips unless `side_models/FLUX.2-klein-{4B,9B}/transformer_gguf/*.gguf`
  exists or `MS_FLUX2_KLEIN_GGUF_FILES` names files), `test_real_gguf_encoder_header.py` the same
  for Qwen3 GGUF encoders (`<package>/{text_encoder_gguf,text_encoder_uncensored_gguf}/*.gguf` or
  `MS_FLUX2_KLEIN_GGUF_ENCODER_FILES`; a Q8_0 file is also checked row by row against the
  package's unquantized `text_encoder/` or `text_encoder_uncensored/`), `test_encoder_gguf.py`
  the GGUF encoder spec, the tiny real Qwen3 GGUF through the manager and the recorded
  uncensored 9B header (the name bridge's own tests are `runtime/test_qwen3_gguf.py`).
  Torch / diffusers / transformers tests skip without them; no GPU, no network. Run them with `python -m pytest modules/ai_backend/inpaint/ -q` from the repo root.

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

### Transformer weight formats (protocol 21)
- The request selects ONE transformer format (`transformer_format`, its file is always
  `transformer_path`) and may list candidates (`transformer_formats`, `[{id, path}]`, best
  first). Ids are opaque (`^[a-z0-9_]{1,32}$`): the Rust format list owns their names, this
  package lists no quantization. Absent fields: the id is derived from the CONTAINER
  (`params.derived_format`: `bf16` for safetensors / a folder, `gguf` for a GGUF file), never
  from a file name.
- `large_model.weight_format_of(options)` is the ONE answer to "which format is this handle":
  every `acquire`, `profiles` (a `None` format plans the request's selection), forecast and the
  calibration tool ask it. The encoder's formats: next section.
- One `FormatInfo` row per candidate (`quality_rank = count - index`; available = its file and
  the VAE exist; `dit_bytes` the file size; compute path `native` / `gguf` by container). The
  `gguf` compute path is measured by no calibration table, so a GGUF forecast is an estimate.
- The pipeline's residency key names the FORMAT'S FILE (`transformer_path_for`), never the id:
  the bf16 key is the pre-format key, and two formats are two handles.
- A GGUF transformer stays quantized in memory: `_build_transformer_meta` swaps every module
  the file stores block-quantized for a `runtime.gguf_layers.GgufLinear` (a BFL fused qkv gives
  its three row slices the fused tensor's type), the loader fills its `uint8` rows, and each
  forward dequantizes into the request's `dtype`. The BFL conversion stays diffusers' own and
  is valid on quantized bytes because it only renames, splits whole rows (qkv `chunk(dim=0)`)
  or swaps row halves (`swap_scale_shift`). Plain GGUF tensors (F32 / F16 / BF16) are ordinary
  weights.
- `require_loadable` refuses, before any admission, a GGUF transformer with a
  `model.diffusion_model.` prefix, a block type `gguf_layers` cannot dequantize (named with a
  tensor and the supported list), or no `transformer/config.json` found next to it (a file in
  `<package>/transformer_gguf/` finds `<package>/transformer/config.json`).

### Text-encoder weight formats and the GGUF encoder (protocol 21)
- The encoder has the transformer's contract: `text_encoder_format` (its file is always
  `text_encoder_path`) and optional `text_encoder_formats` candidates; absent, the id is derived from
  the container (`params.derived_encoder_format`: an absent or missing encoder derives `bf16`, since
  nothing is loaded from it). `weight_format_of` answers for both handles; one `FormatInfo` row per
  candidate (`dit_bytes` the weight files' size, compute path `native` / `gguf`), the residency key
  names the format's file (`encoder_path_for`: the bf16 key is unchanged).
- A GGUF encoder stays quantized in memory: `_make_build_encoder_meta` swaps every block-quantized
  module (the linears and `embed_tokens`, a `GgufEmbedding` that dequantizes only looked-up rows)
  before the loader fills it; the norms are F32 in the file and cast to the encoder dtype. Blocks
  `>= ENCODER_KEEP_LAYERS` and the tied `output.weight` map to nothing and are never read.
- The encode phase of a GGUF encoder is variant `ENCODE_VARIANT_GGUF` (`text_gguf`, D7), so no
  measured bf16 `encode text` entry is borrowed: its forecast is an estimate. `PhaseSpec.work` sees
  the options only, so the variant follows the SELECTED encoder file — an alternative format's row
  is planned with it.
- Weight-only fp8 and a GGUF encoder exclude each other (D8): `large_model.encoder_options` (every
  encoder use) and `require_loadable_encoder` refuse `text_encoder_fp8` with a GGUF file naming both
  fields; the fp8 spec lists no GGUF candidate. No silent fallback.
- Config of a GGUF file (encoder and transformer alike): probed `<dir>/<subdir>`, `<dir>`,
  `<parent>/<subdir>`, `<parent>`, then every installed klein package's `<subdir>`, 4B then 9B
  (`components.installed_klein_package_roots`, `<side_models>/<KLEIN_PACKAGE_DIR_NAMES>`; the
  names are the Rust `Flux2Variant::dir_name()`s, guarded by a Rust test that reads
  `components.py`). A candidate is accepted
  only when it does not contradict the GGUF header (encoder: `architectures == ["Qwen3ForCausalLM"]`,
  `general.architecture == "qwen3"`, layer / width / head counts; transformer: class name, double /
  single block counts, inner width); the package fallback must declare every required field. No fit
  is a Russian refusal listing what was probed and rejected. The package root also joins the
  tokenizer / scheduler search when a component is GGUF, never the `model_index.json` search
  (klein 4B and klein base 4B share every shape).
- Prompt cache: a GGUF encoder's fingerprint is `sha256(config bytes + "\ngguf\x00<name>\x00<size>")`,
  so a Q4 encoder never shares a folder's identity; an entry records its encoder's format id and the
  selection ranks it by the request's precision order (`../MODULE_README.md`, prompt library).

### Text-encoder config kwargs and the RoPE normalization
- Every `Qwen3Config.from_pretrained` of the encoder and every pre-admission check of its config
  goes through `components.text_encoder_config_kwargs(dir, keep)`: the truncation kwargs plus
  `text_encoder_rope_kwargs(dir)`. A transformers-v5 config that keeps the RoPE base only in
  `rope_parameters` (`rope_type: "default"`, no top-level `rope_theta`; the uncensored 9B
  encoder's) gets `rope_theta=<base>`, because the pinned transformers 4.57 reads only the
  top-level field and would silently use 10 000 instead of 1 000 000. Any other `rope_type`, a
  `rope_parameters` without a usable `rope_theta`, or a contradicting top-level value is refused
  naming the file.
- The prompt-cache fingerprint asks the same decision function and, when it normalizes, appends
  `"\nrope_theta=<repr(float(base))>"` to the hashed material: every other fingerprint (stock 9B,
  every 4B) is unchanged, and v1 entries recorded under the unsalted identity are recognized as
  pre-fix by the migration and deleted.

### Monkeypatched symbols must be reached through their defining module
The test suite replaces module-level attributes to keep the GPU, the filesystem and the memory
snapshot out of the way. One `patch.object` reaches every caller only if every caller looks the name
up on the module that defines it:

| Name | Owner |
|---|---|
| `memory_snapshot`, `_clear_torch_cache`, `_resolve_selected_backend_device` | `hardware` |
| `_tables_for` (the forecast's cost tables) | `memory` |
| `_weight_bytes` (the disk size `status` reports), `installed_klein_package_roots` | `components` |
| `program_root` | `prompt_cache` (`_TempTreeCase` points it at a temp root for every test) |
| `serialize_tensor`, `load_tensor` | `../../runtime/prompt_library` |
| `build_text_attention_mask` | `attention` |
| `is_torch_available` | `components` AND `hardware` — TWO bindings, see below |

`is_torch_available` has no single owner: it is defined in `../../runtime/torch_support.py` and
`components` and `hardware` each import it from there, so patching one does not reach the other's
code path; a test patches the module whose path it exercises, or both.

The Qwen3 GGUF name bridge (`../../runtime/qwen3_gguf`) is patched by no test. `large_model` reaches
it as `qwen3_gguf.<name>` (module attribute, never a `from` import), so a test that needs to replace
one of its functions patches `runtime.qwen3_gguf`; the names the package re-exports are separate
bindings.

Therefore a module that consumes one of these from ANOTHER module writes
`hardware.memory_snapshot(...)`, `memory._tables_for(...)`, `components.installed_klein_package_roots()`,
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
- To change the step keys of an answer, FLUX.2's phase binding of the two-handle rule or the
  two-handle offline forecast, see `memory.py`; the rule itself (ceilings, caps, the three-step
  profiles composition and its wire answer) is `../../runtime/large_models/paired.py`.
- To change what the manager knows of a component — files, keys, blocks, dtype, fp8, a phase's
  work, a decode fallback, a pre-admission refusal, the transformer's formats — see
  `large_model.py`; the weight-format wire fields are `params._weight_formats`; the BFL key rule is
  `conversion.py`. The steps, the forecast, the planner, the loader and the ladder are
  `../../runtime/large_models/` (model-agnostic: never special-case FLUX.2 there).
- To change the llama.cpp <-> HF names of the Qwen3 encoder or the wording of the GGUF encoder
  refusals, see `../../runtime/qwen3_gguf.py` (shared with FLUX.3 klein; FLUX.2 passes the accepted
  architecture `components.GGUF_ENCODER_ARCHITECTURE`, its owner name and its logger from
  `large_model.py`, and `test_runtime_extraction.py` pins the resulting wording); the encoder's
  formats, GGUF build and the D8 rule, `large_model.py`.
- To change where a component (or a GGUF file's config) is looked for on disk, which klein
  packages a GGUF file may borrow from, what a checkpoint may contain, the variant rule, how deep
  the text encoder is or how its RoPE config is normalized, what the checkpoint declares about
  guidance distillation, or the VAE's tiling threshold, see `components.py`.
- To change what happens to `guidance_scale` on a distilled checkpoint, see
  `service._effective_guidance_scale` — and nowhere else.
- To change the dtype the text encoder runs in, see `params.text_encoder_dtype_name`.
- To change the run order, the warm-up, the response shape or an action, see `service.py`
  (`_run_locked`, `_load_pipeline_locked`, `_denoise`, `_encode_prompts_locked`,
  `_component_action_locked`); what a phase DOES with the modules (encode, warm-up, decode,
  device check) is `pipeline.py`.
- To change the prompt library's container, index, file names or the selection rule, see
  `../../runtime/prompt_library.py` (model-agnostic); what is served, built, listed, exported or
  imported, `../../runtime/prompt_cache_service.py` (shared with FLUX.3); FLUX.2's library ids,
  encoder provenance, width filter, the v1 migration or its adapter, `prompt_cache.py`; what a run /
  `.status` / the forecast take from the library, `service._prompt_plan`.
- The latent-mask dilation, the colour match and the composite are not here: the backend answers
  the RAW decode and the Rust host post-processes it (`crates/ms-region-postprocess`). The wire
  codecs (region, L8 mask, solid-mask check, PNG answer) are `../region_imaging.py` (shared with
  FLUX.3 klein).
- To change which token pairs `text_attention_in_mask` blocks, the token grid, or the mask's
  dtype/padding/size, see `attention.py` — and keep `text_attention_mask_bytes` the ONE formula
  the spec's denoise work and the allocation use. After a diffusers upgrade,
  `RealTransformerLayoutTests` is what proves the layout still holds.
- To change what the progress bar shows while a component is read, see `progress.LoadFrames` and
  `service._load_frames` (which components report, their labels); the step numbers must not move.
- To add a declaration that other modules or the tests must see, add it to the matching module AND
  to the re-export block of `__init__.py`.
