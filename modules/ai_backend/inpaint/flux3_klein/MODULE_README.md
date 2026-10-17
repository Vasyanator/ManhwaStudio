# Module: modules/ai_backend/inpaint/flux3_klein

## Purpose
FLUX.3 Image Klein (Black Forest Labs, 5.6B distilled flow-map DiT) for the Python AI backend: a
caption plus up to ten reference images becomes one image. It runs from the official weight package
(`<root>/model.safetensors` bf16 or `variants/{fp8r,nvfp4}/`, `ae.safetensors`, `text_encoder/` =
Qwen3-VL-4B) through `Flux3KleinService`, driven by the large-model manager
(`../../runtime/large_models/`): the user picks the weight FORMAT (bf16 / fp8r / nvfp4) and the
two resource steps «Видеопамять» x «Оперативная память» (`vram_step`, `ram_step`, «Авто» each by
default); where every byte of the DiT, the AE and the encoder lives
during each phase is the manager's plan, never a per-component option of this package.

**Integration boundary.** `server.py` constructs `Flux3KleinService(large_models)` as
`AppState.flux3_klein` (health key `inpaint.flux3_klein`) and lists it as the
`large_models.profiles` adapter `flux3_klein`. The IPC surface is `ipc/handlers/flux3_klein.py`:
`inpaint.flux3_klein` (the cleaning tab's region editor, through `region.py`), `.status`,
`.unload` and the two `.download.*` methods (`../flux3_download.py`, the gated repository
`black-forest-labs/flux-3-image-klein`; the user accepts the FLUX Kommunity License on Hugging Face
and sends a token). The wire is `ipc/PROTOCOL.md` "FLUX.3 klein"; `wire.py` is its one mapping onto
`Flux3Options`. The calibration tool `tools/large_model_calibration/` (its `flux3` driver) calls the
service directly.

Out of scope: the CFG "base" model (refused by `config.py` and `loading.require_flow_map`), the
prompt upsampler, the safety checker, bounding boxes, a prompt cache, and prompt translation.

## Architecture
One-directional layering; a module imports only from modules on lines above it and from
`../../runtime/`:

```
config, params, phases          (torch-free; params -> runtime.large_models.tiers for VramStep / RamStep / Steps)
wire                            (torch-free; the IPC params mapping and the region frame rule)
large_model                     (torch-free at import; the LargeModelSpecs)
quant, packing                  (torch)
dit, autoencoder, sampling      (torch)
loading, text_encoder           (torch; transformers / accelerate lazily)
service                         (torch-free at import; imports the torch modules inside methods)
region                          (numpy / PIL; ../region_imaging; the region editor over the service)
__init__                        (re-export only; the torch-side names lazily, PEP 562)
```

Run order of one `generate` (each line is a `phases.Phase` = one manager step):

1. before the admission, no lock: package files + `config.json`, `normalize_request`, the
   VL_REFS-needs-references refusal, reference preparation, output size, device, the concrete fp8
   path (`quant.resolve_fp8_compute`; an explicit `SCALED_MM` that cannot run fails here), then
   `LargeModelManager.acquire(model, Flux3ModelOptions, RequestShape, format=variant,
   steps=options.steps)` (refused with the shortest resource and the formats that would fit);
2. load scope: `DIT_LOAD`, `AE_LOAD` = `Session.load(component)` for a component that holds
   nothing (read straight into its place for the first phase), else nothing (`resident`);
3. run scope: `DIT_OFFLOAD` = `Session.load(dit, ae)` (between-runs layout -> encode placement),
   `ENCODER_LOAD` = `transition(None, encode)`, `ENCODE` = `run_phase(encode)`,
   `ENCODER_RELEASE` = `transition(encode, ref_encode)`, `REF_ENCODE` = `run_phase(ref_encode)`,
   `DIT_RESTORE` = `transition(ref_encode, denoise)`, `DENOISE` = `run_phase(denoise)`,
   `DIT_OFFLOAD_DECODE` = `transition(denoise, decode)`, `DECODE` = `run_phase(decode)`; then
   `Session.finish` (shrinks toward the step pair's between-runs state: the card only under
   «Максимум скорости», RAM only under «Держать модели в памяти»; it never loads).

A transition moves EVERY component the two placements differ in (the step is named after the move
the measurement table cares about): e.g. `ENCODER_RELEASE` also reads the AE when the step pair
keeps nothing before its first use.

## Files and submodules
- `config.py`: reference constants, `read_package_config` -> `PackageConfig`, weight-format
  detection from a header, `resolve_model_files`, `variant_availability`, the variant-independent
  paths, the encoder's shard list and checkpoint key maps (torch-free, so the spec sizes from them).
- `large_model.py`: the `LargeModelSpec`s (`build_spec`, `model_specs`, `model_name`),
  `Flux3ModelOptions`, `residency_key`, `formats`, `compute_path`, the request-shape bounds
  (`request_shape`, `caption_token_bound`, `reference_pixel_bound`).
- `params.py`: `EncoderMode`, `Fp8Compute`, `AeDtype`, `Flux3Options` (per load; `variant` = format,
  `vram_step` / `ram_step`, `steps`),
  `Flux3Request` (per call), `normalize_request`, `default_seed`.
- `phases.py`: `Phase`, `PhaseObserver`, `observed` (observer exceptions propagate).
- `quant.py`: `Fp8RowwiseLinear`, `Nvfp4Linear`, dequantization, `probe_scaled_mm`,
  `resolve_fp8_compute`, `set_fp8_compute`, `swap_quantized_linears`.
- `packing.py`: `Packed` / `MODES` (the DiT input contract), reference preparation, `to_tensor` /
  `to_pil`, `scaled_ids`, `reference_tokens`, `initial_noise`, `pack`.
- `dit.py`: `Flux3Dit`.
- `autoencoder.py`: `AutoEncoder`, `latent_ids`, `latent_shape`, `build_autoencoder_meta`,
  `ae_tensor_cast`, `load_autoencoder` (standalone).
- `sampling.py`: `flow_map_schedule`, `denoise_flow_map` (one sample, no CFG).
- `loading.py`: `build_dit_meta` (header checks, quantized swap and dtype check, fp8 path),
  `convert_dit_tensor` (the spec's conversion), `load_dit` -> `LoadedDit` (standalone: parity
  script, tests).
- `text_encoder.py`: the truncated Qwen3-VL encoder (33 of 36 layers): `build_text_encoder_meta`,
  `encoder_key_map`, `load_text_encoder` / `load_text_encoder_model` (standalone),
  `load_text_processor`, `encode_text`, `encode_with_references`.
- `wire.py`: `normalize_flux3_params` -> `Flux3WireParams` (the one wire -> options mapping;
  unknown keys, wrong types and out-of-range values refused by name), `validate_region_size`
  (multiple of 16, side >= 128, 4 MP, aspect 4:1), `MAX_EXTRA_REFERENCES` (9), `FIELDS`.
- `region.py`: `edit_region_bytes` — the region editor: decode and check region / mask /
  references, the seed (`null` = random, echoed), `Flux3Request(region as reference #0, explicit
  size)`, the progress frames (`progress_stages()`, stable `stage` ids), cancellation through the
  observer and the step callback, the size check, the color match (global, then local CIELAB
  unless `color_match_local_pct` is 0; both against the painted mask — rules in
  `../MODULE_README.md` "The color match is two steps") and the inward-feathered composite
  (`../region_imaging.py`), the answer dict.
- `service.py`: `Flux3KleinService` (`generate`, `status`, the profiles adapter
  `large_model_name` / `large_model_inputs`, `health`, `unload`), `Flux3Result`, `Flux3Applied`,
  `Flux3Canceled`, `_model_key`.
- `_service_test_fixtures.py`: the service test harness (tiny real safetensors of stand-in
  modules, the real arbiter + manager over fake probes, stubs at the meta-build seams).
- `LICENSE`: the reference's Apache-2.0 licence (the ported files name it in their headers).
- `test_*.py`: unit tests next to each module (tiny CPU models, stubs; no weights, no GPU);
  `test_large_model.py` runs the spec over a real tiny package through the real manager (a
  juggled DiT equals the resident one in every format); `test_characterization.py` holds the
  observer / health / lease CONTRACT and the per-step-pair GOLDEN table;
  `test_wire.py` / `test_region.py` pin the wire mapping and the region editor (byte identity
  outside the mask, seed echo, stages, cancel);
  `test_real_headers_*.py` and `test_real_processor.py` read the real package and skip unless
  `MS_FLUX3_DIR` names its root. Run: `venv/bin/python -m pytest modules/ai_backend/inpaint/flux3_klein -q`.

## Contracts and invariants

### Attribution
`dit.py`, `autoencoder.py`, `packing.py`, `sampling.py`, `quant.py`, `loading.py` and the encoder
parts of `text_encoder.py` are ported from black-forest-labs/flux-3-image-klein (commit e53f4ea,
Apache-2.0). Each header names its source file and lists "Modified:"; keep both current when
editing. The repository `NOTICE` carries the matching entry.

### Reference parity (kept because they change the output)
- bf16 timesteps; float32 image state across the Euler steps with bf16 `dt * v` products;
- noise from a CPU generator seeded with the request seed, rounded to bf16 (identical on every
  device); default seed = sha256 of the caption mod 2**32;
- text: chat template, 4096-token cap (the `config.json` `max_tokens: 8192` is ignored and logged),
  right padding to a multiple of 80, padding tokens ATTENDED by the DiT, position ids
  `cumsum - 1` with pads = 1, hidden states 4..32;
- references: single reference fitted and reflect-padded to its canvas (≤ 4 MP, cropped back when
  no size was given; an explicit output size does NOT shrink it), several references each under
  1 MP on the 16 grid; AE latent mean, ids l = -(1 + i). `Flux3Options.reference_max_pixels`
  (default `None` = exactly this rule) lowers both caps — a low-VRAM deviation, see below;
- only the distilled flow-map sampler (`denoise_flow_map`).

### Deliberate deviations
- DiT attention uses default SDPA, not the forced cuDNN backend (fails on gfx1201).
- NVFP4 runs by per-forward dequantization to bf16 (no FlashInfer kernel); weights stay quantized
  in device memory.
- `EncoderMode.VL_REFS` is this port's experiment, absent from the reference: the SAME prepared
  references the AE encodes are also shown to Qwen3-VL's vision tower, with Qwen3-VL's own mRoPE
  ids. It requires at least one reference (refused before the admission otherwise, no fallback).
- Weights are streamed tensor by tensor (`runtime.safetensors_stream`), never loaded whole.
- `reference_max_pixels` (opt-in): every prepared reference is downscaled under that area (aspect
  kept, sides on the 16 grid, never upscaled; single: the canvas incl. padding, and the crop box
  follows) BEFORE both the AE and, in VL_REFS, the encoder see it. Fewer reference tokens shorten
  the denoise sequence (kitchen photo at a 512² cap: 3750 -> 1014 tokens) at the cost of reference
  detail. Not in the residency key; minimum `MIN_REFERENCE_PIXELS` (64²).

### fp8 compute is never chosen silently
`Fp8Compute.AUTO` probes rowwise `_scaled_mm` per device (a definitive answer is cached for the
process; an out-of-memory failure is `transient` and probed again next time) (`probe_scaled_mm`, result compared
with the dequant path) and falls back to `DEQUANT` with a WARNING naming the reason; an explicit
`SCALED_MM` that fails the probe is a `RuntimeError` before any weight is read. The applied path is
reported in `Flux3Result.applied.fp8_compute` and `health()["fp8_compute"]`.

### Large-model spec (`large_model.py`)
- One registered model per (encoder mode, AE dtype) — `flux3_klein.<text|vl_refs>.ae_<dtype>`,
  family `flux3_klein` (the calibration key). Why: the manager caches inventories per (model,
  format, files) and the spec's `convert` / `device_itemsize` see no options, while the mode
  changes the encoder's state keys and the AE dtype its device bytes. Switching either makes
  another handle; the idle one stays reclaimable by the residency arbiter (parked or dropped
  before any degrade); `unload()` drops all of them.
- Components: `dit` (run device only; blocks in FORWARD order: per depth
  `content_mode_blocks.image.d`, `content_mode_blocks.image_cond.d`, `txt_mode_blocks.d`, then
  `single_blocks.*` — the hooks prefetch the next of that order; floats bf16, quantized payloads as
  stored), `encoder` (run device or CPU, blocks = decoder layers, bf16), `ae` (monolithic, the
  model's AE dtype, BatchNorm statistics float32).
- Phases: `encode`, `ref_encode`, `denoise`, `decode`. Forecast inputs that need the tokenizer or
  the reference preparation are UPPER bounds (`caption_token_bound`, `reference_pixel_bound`).
- `formats`: bf16 (native), fp8r (`scaled_mm` or `dequant`, the concrete path resolved before the
  admission), nvfp4 (dequant) with availability, DiT file size, quality rank 3 / 2 / 1. The
  manager never changes the format; a refusal names the formats that would fit.
- No model-specific degrade rung: `reference_max_pixels` and the AE dtype are the user's quality
  choices, the port has no tiled AE, encoder-on-CPU is a step placement (`tiers.template_for`) or
  the host pin below.
- «Энкодер на CPU» (`Flux3Options.encoder_on_cpu`; dataclass default `False`, WIRE default
  `true`): the spec's `host_pin_of` pins `encoder` to host execution
  (`runtime/large_models` "Host pin"): CPU from RAM at every VRAM step, placed by the RAM step alone.
- Residency key `flux3_klein:{format}|{device}|{fp8 requested}|{ae dtype}|{encoder mode}|{root}`
  (`residency_key`), plus `|host_pin=encoder` under «Энкодер на CPU» (added by
  `LargeModelSpec.key`; `service._model_key` is that key): everything that changes the loaded
  bytes, modules or placement; `reference_max_pixels` and the resource steps are not in it.

### Service
- Defaults: `EncoderMode.TEXT`, `Fp8Compute.AUTO`, AE float32, `reference_max_pixels=None`,
  `vram_step=VramStep.AUTO`, `ram_step=RamStep.AUTO`. Where each component lives and runs is
  decided by the step pair (`tiers.template_for`; e.g. «Минимум»
  VRAM keeps no DiT block on the card and runs the encoder on the CPU unless RAM is also
  «Минимум», which streams both from the file; «Только текущий этап» parks the idle components in
  RAM, or drops them under RAM «Минимум»). Under ample budgets «Не больше половины» plans like
  «Только текущий этап».
- The service never moves a weight: every placement change is a manager step. A phase function
  fetches its module from the `PhaseContext` on every call and is restartable from its inputs
  (the manager may retry a phase on a gentler plan after a memory failure: the run then reports
  `applied.degraded`, `applied.steps_effective`; a device failure lowers only the VRAM step, a host
  failure only the RAM step; `applied.steps_requested` and the options are never rewritten).
- Failures: a refusal before or at the admission touches nothing (no phase, nothing held;
  `last_error` set for an admission refusal). A failed load scope registers nothing: the session
  finishes and the model's idle handles are unloaded. A CANCEL (`Flux3Canceled`, raised by the
  caller's observer at a phase begin or by its `step_callback` after a denoise step) is not a
  failure: no degrade retry (`degrade.classify_failure` does not know it), nothing is unloaded
  (the session's `finish` shrinks to the between-runs state as after any non-memory error), it is
  logged at INFO and does not touch `last_error`; the next run starts normally.
- `generate(..., step_callback=)`: `(step, total)` after every flow-map step (repeats steps after a
  degrade retry; exceptions propagate).
- `status(params)` (the `.status` answer: file checks through `large_model.formats` with a
  placeholder fp8 path, never a probe) and the profiles adapter (`large_model_name`,
  `large_model_inputs`: the fp8 path is probed once per device, cached) read the wire through
  `wire.normalize_flux3_params`. A failed run keeps the between-runs state
  (a memory failure makes the manager drop everything); every failure of `generate` is logged with
  its traceback, then `_release_failed_run_memory` clears the frames' locals and releases the
  allocator cache.
- `health()` never takes the lock. Published at the end of a run / unload: `device`,
  `model_key`, `variant`, `fp8_compute` (applied), `compute_path`, `vram_step` / `ram_step` (the
  effective pair), `degraded`, `last_error`. Live, from the manager's ledger
  (`LargeModelManager.placements`: no probe, never waits for a session), so a reclaim park or drop
  between runs is seen at once: `loaded`, `ready` (DiT + AE held), `dit_device` /
  `encoder_resident` (the run device while any block is on the card, `cpu` when parked in RAM or
  run on the CPU, `None` when not held). The service keeps no placement bookkeeping of its own:
  the observer actions also read the "before" side from the ledger (`Session.placements`), only
  the predicted "after" from the plan's target state.
- The text processor (tokenizer + image processor) is a small host object cached per encoder
  directory outside the manager, dropped by `unload()`.
- Device: `options.device`, else `_resolve_selected_backend_device("cuda")`
  (`runtime.torch_device.resolve_discrete_backend_device`, never an integrated GPU), normalized
  (`cuda` -> `cuda:0`) before the key is built.

### Observer phases and `info` keys
Every `Phase` is reported exactly once per call as a `begin` / `end` pair (`end` repeats `begin`'s
info and adds `failed`), no-op steps included, so a measurement table is uniform. A failure ends
its phase with `failed=True` and no later phase follows. The mapping onto manager steps is the run
order above.
- every phase: `variant`, `encoder_mode`, `fp8_compute` (requested), `ae_dtype`, `device`,
  `reference_max_pixels`, `n_refs`, `width`, `height` (the generated canvas), `steps`, `seed`,
  `image_tokens`, `ref_tokens`, `vram_step` / `ram_step` (requested, may be `auto`), `compute_path`,
  `vram_step_effective` / `ram_step_effective` (the plan in force at the step's begin), and the manager's forecast of the plan in force: `forecast_device_peak` /
  `forecast_host_peak` of the step (the phase, or the transition it performs; the load scope and
  `DIT_OFFLOAD` are parts of `idle->encode`), `forecast_run_device_peak` /
  `forecast_run_host_peak` of the whole run, `forecast_calibrated` (host peaks are absolute
  anonymous RSS as the manager saw it);
- `DIT_LOAD`, `AE_LOAD`: `action` = `loaded` | `resident` (nothing read: already held, or the
  first phase keeps it nowhere);
- `DIT_OFFLOAD`, `DIT_OFFLOAD_DECODE`: `action` = `offloaded` (the DiT leaves the device) |
  `on_host` (stays in RAM) | `skipped`; `DIT_RESTORE`: `restored` | `resident`;
- `ENCODER_LOAD`: `action` = `loaded` (read, also after an exec-device change) | `restored` (from
  RAM to the device) | `resident`;
- `ENCODER_RELEASE`, `REF_ENCODE`, `DIT_RESTORE`, `DENOISE`, `DIT_OFFLOAD_DECODE`, `DECODE`:
  `text_tokens` (padded DiT text length), `text_real_tokens`.

### Import cost and patch points
Importing the package, `service.py` or `large_model.py` does not import torch. Tests patch on the
DEFINING module: `service._resolve_selected_backend_device`, `service._clear_torch_cache`,
`loading.build_dit_meta`, `autoencoder.build_autoencoder_meta`,
`text_encoder.{build_text_encoder_meta, load_text_processor, encode_text, encode_with_references}`,
`quant.{resolve_fp8_compute, probe_scaled_mm}`; the code looks them up as `<module>.<name>` at call
time. Patching the package re-export is a silent no-op.

## Editing map
- Package layout, `config.json` rules, weight-format detection, encoder checkpoint layout:
  `config.py`.
- Request validation, defaults, option enums: `params.py`; the IPC params, their defaults and
  the region frame rule: `wire.py` (keep `PROTOCOL.md` "FLUX.3 klein" and the Rust engine's
  settings / frame rule in step).
- The region editor (composite, seed, progress stages, cancel points): `region.py`; the pixel
  helpers it shares with FLUX.2: `../region_imaging.py`; the download: `../flux3_download.py`.
- Components, blocks, phases, formats, residency key, forecast inputs: `large_model.py` (the
  per-phase activation and time constants are the calibration tables of `runtime/large_models`).
- Run order, the step <-> observer-phase mapping, `info` keys, health: `service.py` (update the
  phase list above and the VRAM script together).
- fp8 / nvfp4 math or the probe: `quant.py`; checkpoint checks and cast rule: `loading.py`.
- Reference preparation, token packing, noise: `packing.py` (keep `large_model.reference_pixel_bound`
  an upper bound of it); the sampler: `sampling.py`.
- Encoder building, tokenization, the VL experiment: `text_encoder.py`.
