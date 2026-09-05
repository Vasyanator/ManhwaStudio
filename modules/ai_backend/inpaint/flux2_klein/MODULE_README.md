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
components                         (-> runtime.torch_support)
hardware                           (-> runtime.torch_support, ai_device, config)
prompt_cache                       (-> components, params, runtime.paths)
memory                             (-> components, hardware, params)
pipeline                           (-> components, hardware, runtime.rocm_mmap_transfer)
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
  `_model_key`, `effective_steps`, `_lenient_paths` and the coercion helpers. Edit it to change the
  wire contract, a clamp, or the region limits.
- `progress.py`: the `phase:"load"` step numbering (`LOAD_STEP_*`, `LOAD_PHASE_STEPS`), the
  `ProgressCb` type and `_progress_reporter`. A wire contract shared with the Rust client; it
  deliberately imports nothing from the package.
- `components.py`: the five components AS FILES ON DISK — search roots and probe order, tokenizer and
  scheduler discovery, safetensors header reading, the fp8-scaled refusal, the transformer-config
  contract, text-encoder availability, the encoder<->transformer width contract
  (`require_encoder_transformer_compatible`), `_weight_bytes`, and the `components` map `status`
  returns.
- `prompt_cache.py`: encoder identity (`text_encoder_fingerprint`, `encoder_family_name`), the
  `.msprompt` container (`write_prompt_file`, `read_prompt_file_header`,
  `validate_prompt_file_metadata`) and the on-disk library under `<program root>/prompt_cache/`.
- `imaging.py`: wire decode/encode of the region and the mask, the color match against the ring
  outside the mask, the feathered composite, and the mask morphology they need.
- `hardware.py`: `memory_snapshot`, `_resolve_selected_backend_device`, `_clear_torch_cache`,
  `_cuda_device_index` — the accelerator and what is free on it.
- `memory.py`: `forecast_memory` and the pre-load guard `_require_memory_headroom` built on it,
  plus `_preset_advice` and the advisory constants. `estimate` and the guard must stay ONE piece of
  arithmetic; do not add a second forecast.
- `pipeline.py`: component loaders, the four placements and the ROCm staging they run under,
  per-component residency and the action matrix, the warm-up, the prompt-encode phase, the fp8
  quantization, and the VAE decode with its out-of-memory recovery.
- `service.py`: `Flux2KleinInpaintService` — the service lock, the resident pipeline, the in-memory
  prompt cache, the model lease, and the request methods the IPC handlers call.
- `_test_fixtures.py`: shared test fixtures (the on-disk model tree, the fake
  `torch`/`diffusers`/`transformers` stack, `_TempTreeCase`, `_PlacementFixture`,
  `_ResidencyModule`). The leading underscore keeps pytest from collecting it.
- `test_params.py`, `test_components.py`, `test_prompt_cache.py`, `test_imaging.py`,
  `test_hardware.py`, `test_memory.py`, `test_pipeline.py`, `test_service.py`: unit tests, one per
  module they cover. No torch, no diffusers, no weights, no GPU — except
  `PromptCacheRoundTripTests`, which needs real torch for `safetensors.torch` and skips itself
  without it. Run them with `python -m pytest modules/ai_backend/inpaint/ -q` from the repo root.

## Contracts and invariants

### Monkeypatched symbols must be reached through their defining module
The test suite replaces module-level attributes to keep torch, the filesystem and the memory
snapshot out of the way. Monkeypatching couples the DEFINITION site to the USE site: across several
modules, one `patch.object` reaches every caller only if every caller looks the name up on the
module that defines it.

Twelve names are affected, listed with their owner:

| Name | Owner |
|---|---|
| `patched_module_to`, `mmap_staging_required`, `tensor_needs_staging`, `_quantize_text_encoder_fp8`, `_restore_transformer_to_device` | `pipeline` |
| `memory_snapshot`, `_clear_torch_cache`, `_resolve_selected_backend_device` | `hardware` |
| `is_torch_available`, `_weight_bytes` | `components` |
| `program_root`, `write_prompt_file` | `prompt_cache` |

Therefore:

- a module that consumes one of these from ANOTHER module writes `hardware.memory_snapshot(...)`,
  `pipeline._restore_transformer_to_device(...)`, `components._weight_bytes(...)`,
  `prompt_cache.write_prompt_file(...)` — never `from .hardware import memory_snapshot`;
- tests patch the OWNER module, never the `flux2_klein` package. `__init__.py` re-exports all
  twelve, but a re-export is a separate binding: patching it would be a silent no-op for every real
  caller. The comment above the re-export block says so.

Adding a new patch point means adding it to that table and to the consumer rule; adding a plain
`from .x import y` for one of these names is a defect.

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
- To change where a component is looked for on disk, what a checkpoint may contain, or the rule
  that tells a 9B component pair from a 4B one, see `components.py`.
- To change the prompt-cache file format or the library layout, see `prompt_cache.py`.
- To change the composite, the color match, or the mask morphology, see `imaging.py`.
- To change the memory forecast or the pre-load guard, see `memory.py` (and never split the
  arithmetic in two).
- To change a placement, a loader, the residency probe, the action matrix, or the decode recovery,
  see `pipeline.py`.
- To change the lease protocol, the run order, or a request method's response shape, see
  `service.py` — and read the lease-protocol section of `../MODULE_README.md` first.
- To add a declaration that other modules or the tests must see, add it to the matching module AND
  to the re-export block of `__init__.py`.
