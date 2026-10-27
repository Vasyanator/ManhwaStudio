# Module: modules/ai_backend/inpaint/flux3_klein

## Purpose
FLUX.3 Image Klein (Black Forest Labs, 5.6B distilled flow-map DiT) for the Python AI backend: a
caption plus up to ten reference images becomes one image. It runs from the official weight package
(`<root>/model.safetensors` bf16 or `variants/{fp8r,nvfp4}/`, `ae.safetensors`, `text_encoder/` =
Qwen3-VL-4B), optionally with a GGUF DiT file and / or a GGUF file of the encoder's text half
(see "GGUF DiT and GGUF encoder"), through `Flux3KleinService`, driven by the large-model manager
(`../../runtime/large_models/`): the user picks the weight FORMAT (bf16 / fp8r / nvfp4 / a GGUF id),
the text-encoder format (`bf16` / a GGUF id) and the
two resource steps «Видеопамять» x «Оперативная память» (`vram_step`, `ram_step`, «Авто» each by
default); where every byte of the DiT, the AE and the encoder lives
during each phase is the manager's plan, never a per-component option of this package. One run
holds TWO manager handles: the pipeline (DiT + AE) and the text encoder, composed by the shared
two-handle rule (`../../runtime/large_models/paired.py`, phases `large_model.FLUX3_PAIR`).

**Integration boundary.** `server.py` constructs `Flux3KleinService(large_models)` as
`AppState.flux3_klein` (health key `inpaint.flux3_klein`) and lists it as the
`large_models.profiles` adapter `flux3_klein` (it composes both handles, `large_model_profiles`). The IPC surface is `ipc/handlers/flux3_klein.py`:
`inpaint.flux3_klein` (the cleaning tab's region editor, through `region.py`), `.status`,
`.unload` and the prompt library `.prompt_cache.{build,list,export,import}` (TEXT mode only; see
"Prompt cache"). The files come from the app's model catalog (Rust, `ms_sysprobe` external catalog
`flux3_klein`): the backend downloads nothing for FLUX.3 and never derives a GGUF path — the client
sends every offered candidate with its absolute path (`dit_formats`, `text_encoder_formats`). The
wire is `ipc/PROTOCOL.md` "FLUX.3 klein"; `wire.py` is its one mapping onto `Flux3Options`. The calibration tool `tools/large_model_calibration/` (its `flux3` driver) calls the
service directly.

Out of scope: the CFG "base" model (refused by `config.py` and `loading.require_flow_map`), the
prompt upsampler, the safety checker, bounding boxes, caching `vl_refs` encodes (D11), prompt
translation, and VL_REFS with a GGUF encoder (the GGUF holds no vision tower; the `mmproj` file is not read).

## Architecture
One-directional layering; a module imports only from modules on lines above it and from
`../../runtime/`:

```
config, params, phases          (torch-free; params -> runtime.large_models.tiers for VramStep / RamStep / Steps)
prompt_cache                    (torch-free; runtime.prompt_library / prompt_cache_service; the library side)
wire                            (torch-free; the IPC params mapping and the region frame rule)
large_model                     (torch-free at import; the LargeModelSpecs)
quant, packing                  (torch)
dit, autoencoder, sampling      (torch)
loading, text_encoder           (torch; transformers / accelerate lazily)
service                         (torch-free at import; imports the torch modules inside methods)
region                          (numpy / PIL; ../region_imaging; the region editor over the service)
__init__                        (re-export only; the torch-side names lazily, PEP 562)
```

Run order of one `generate` (P = the pipeline session, E = the text-encoder session; each line
from 2 on is a `phases.Phase` = one manager step):

1. before the admissions, no lock: package files + `config.json`, a GGUF DiT's header
   (`_require_gguf_dit`: `loading.require_gguf_dit`), `normalize_request`, the
   VL_REFS-needs-references refusal, reference preparation, output size, device, the concrete fp8
   path (`quant.resolve_fp8_compute`; an explicit `SCALED_MM` that cannot run fails here); then,
   UNDER the service lock: the prompt decision (`_prompt_locked`: TEXT mode serves the caption
   from memory or the library BEFORE any admission — a read file is remembered in memory; else,
   and always in `vl_refs` mode, the run encodes and `_require_encoder` refuses missing encoder
   weights or a bad GGUF encoder header, naming `text_encoder_format`); then (`_admit_locked`)
   only when the run encodes the encoder's combined VRAM-axis ceilings (`paired.live_ceilings`,
   planned before P holds a reservation), P = `acquire(flux3_klein.ae_<dtype>, format=<DiT id>)`,
   then, only when the run encodes, E = `acquire(flux3_klein_text_encoder.<mode>, format=<encoder
   id>, device_cap_bytes=paired.encoder_cap_beside(...))`; each refused with the shortest resource
   and the formats that would fit; a refused E finishes P with nothing read;
2. load scope: `DIT_LOAD`, `AE_LOAD` = `P.load(component)` for a component that holds nothing
   (read straight into its place for P's first phase, `ref_encode`), else nothing (`resident`);
3. run scope: `DIT_OFFLOAD` = `P.load(dit, ae)` (between-runs layout -> the `ref_encode`
   placement, where DiT and AE sit while the encoder runs), `ENCODER_LOAD` =
   `E.transition(None, encode)`, `ENCODE` = `E.run_phase(encode)` (a TEXT result remembered in
   memory), `ENCODER_RELEASE` = `E.finish()` (the encoder shrinks to its between-runs state now
   and its reservation is released) — for a served caption the three are no-ops on no handle
   (`ENCODER_LOAD` `action` `cached`, ENCODE builds the `EncodedText` from the served tensor), `REF_ENCODE` = `P.run_phase(ref_encode)`, `DIT_RESTORE` =
   `P.transition(ref_encode, denoise)`, `DENOISE` = `P.run_phase(denoise)`, `DIT_OFFLOAD_DECODE` =
   `P.transition(denoise, decode)`, `DECODE` = `P.run_phase(decode)`; then `P.finish` (shrinks
   toward the step pair's between-runs state: the card only under «Максимум скорости», RAM only
   under «Держать модели в памяти»; it never loads).

A transition moves EVERY component of its handle the two placements differ in (the step is named
after the move the measurement table cares about).

## Files and submodules
- `config.py`: reference constants, `read_package_config` -> `PackageConfig`, weight-format
  detection from a header, the request's sources (`DitSource` = `WeightFormat` | `GgufDit`,
  `EncoderSource`, `NATIVE_DIT_FORMATS`, `FORMAT_ID_PATTERN`), `resolve_model_files`,
  `variant_availability`, the variant-independent paths, the encoder's shard list, its files and
  presence per source (`text_encoder_files`, `text_encoder_availability`) and checkpoint key maps
  (torch-free, so the spec sizes from them).
- `large_model.py`: the two handles' `LargeModelSpec`s (`pipeline_spec` / `pipeline_model_name`,
  `encoder_spec` / `encoder_model_name`, `model_specs`), `FLUX3_PAIR`, `Flux3ModelOptions`,
  `dit_source` / `encoder_source`, `pipeline_key` / `encoder_key`, `formats` (DiT rows) /
  `encoder_formats` (encoder rows), `compute_path`, the encoder name-space dispatch (shard keys /
  GGUF names), the request-shape bounds (`request_shape`, `caption_token_bound`,
  `reference_pixel_bound`).
- `params.py`: `EncoderMode`, `Fp8Compute`, `AeDtype`, `Flux3Options` (per load; `variant` = format,
  `dit_formats` = the offered formats, `encoder` = the encoder source, `encoder_formats` = the
  offered encoder sources, `vram_step` / `ram_step`, `steps`),
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
- `loading.py`: `build_dit_meta` (header checks, quantized swap and dtype check, fp8 path; a
  `GgufDit`: `GgufLinear` swap), `gguf_dit_layout` / `require_gguf_dit` (the GGUF DiT header
  against the meta model), `convert_dit_tensor` (the spec's conversion, GGUF included),
  `load_dit` -> `LoadedDit` (standalone, safetensors exports only: parity script, tests).
- `text_encoder.py`: the truncated Qwen3-VL encoder (33 of 36 layers): `build_text_encoder_meta`,
  `encoder_key_map`, `load_text_encoder` / `load_text_encoder_model` (standalone),
  `load_text_processor`, `encode_text`, `encode_with_references`; the GGUF encoder
  (`gguf_encoder_mismatches`, `require_gguf_text_encoder`, `build_gguf_text_encoder_meta`).
- `prompt_cache.py`: the library side of the prompt cache — `LIBRARY_ID` (`flux3_klein`), the
  entry tensor `text_tokens`, `text_encoder_fingerprint` / `encoder_provenance` (D11 identity),
  `text_width`, `library`, the memory caps, `Flux3PromptCacheRequest` (the wire fields),
  `Flux3CacheQuery` and `Flux3PromptCacheAdapter` (the shared orchestration's adapter).
- `wire.py`: `normalize_flux3_params` -> `Flux3WireParams` (the one wire -> options mapping;
  unknown keys, wrong types, out-of-range values and malformed format candidates refused by name;
  `text_encoder_formats` carries the encoder candidates for `.status`; `prompt_cache` the
  prompt-library fields), `normalize_flux3_library_params` (the library methods' lenient reading), `validate_region_size`
  (multiple of 16, side >= 128, 4 MP, aspect 4:1), `MAX_EXTRA_REFERENCES` (9), `FIELDS`.
- `region.py`: `edit_region_bytes` — the region editor: decode and check region / mask /
  references, the seed (`null` = random, echoed), `Flux3Request(region as reference #0, explicit
  size)`, the progress frames (`progress_stages()`, stable `stage` ids) and the disk-read frames
  (`file_component_id`, `file_label`; see "Progress frames" below), cancellation through the
  observer and the step callback, the size check and the answer dict; `progress_sinks` is the
  one frame builder, also used by `build_prompt_cache` (the IPC build). The answer is the RAW
  generated region (`packing.to_pil`, rounded): the colour match and the composite inside the
  painted mask are the Rust host's (`crates/ms-region-postprocess`); the mask travels for
  validation only (non-empty, or solid under `whole_region`).
- `service.py`: `Flux3KleinService` (`generate`, `status`, the profiles adapter
  `large_model_profiles` over `plan_profiles` / `plan_pair` -> `paired.PairedReport`, `health`,
  `unload`, the prompt cache `prompt_cache_build` / `_list` / `_export` / `_import`),
  `Flux3Result`, `Flux3Applied`, `Flux3Canceled`, `_model_key` / `_encoder_handle_key`.
- `_service_test_fixtures.py`: the service test harness (tiny real safetensors of stand-in
  modules, the real arbiter + manager over fake probes, stubs at the meta-build seams).
- `LICENSE`: the reference's Apache-2.0 licence (the ported files name it in their headers).
- `test_*.py`: unit tests next to each module (tiny CPU models, stubs; no weights, no GPU);
  `test_large_model.py` runs the specs over a real tiny package through the real manager (a
  juggled DiT equals the resident one in every format); `test_characterization.py` holds the
  observer / health / lease CONTRACT and the per-step-pair GOLDEN table; `test_service.py` also
  pins the two admissions (order, the encoder's ceiling, a refused encoder) and the lock order
  against a concurrent reclaim (`LockOrderTests`);
  `test_wire.py` / `test_region.py` pin the wire mapping and the region editor (the raw
  answer, seed echo, stages, cancel);
  `test_prompt_cache.py` pins the encoder identity and the cache paths over the real service
  (a cached TEXT run with the encoder weights DELETED opens no encoder handle and forecasts none,
  memory / library hits, the byte cap, a gone file, `vl_refs` never cached, the library surface);
  `test_gguf.py` runs a tiny GGUF DiT and a tiny `qwen3vl` GGUF encoder through the real manager
  in every step pair against their bf16 twins (exact), the header refusals, the D9 key and the
  RECORDED real headers (`fixtures/flux3_gguf_headers.json`: names, GGUF dims, ggml types,
  metadata and the two package configs, no tensor data);
  `test_real_headers_*.py`, `test_real_gguf_headers.py` (every GGUF header plus Q8_0-vs-bf16 rows
  read by byte range) and `test_real_processor.py` read the real package and skip unless
  `MS_FLUX3_DIR` names its root. Run: `venv/bin/python -m pytest modules/ai_backend/inpaint/flux3_klein -q`.
- `fixtures/flux3_gguf_headers.json`: the recorded GGUF header facts (above); re-record only from
  the catalog-pinned files.

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

### Large-model specs (`large_model.py`): two handles
- PIPELINE: one registered model per AE dtype — `flux3_klein.ae_<dtype>`; components `dit` (run
  device only; blocks in FORWARD order: per depth `content_mode_blocks.image.d`,
  `content_mode_blocks.image_cond.d`, `txt_mode_blocks.d`, then `single_blocks.*` — the hooks
  prefetch the next of that order; floats bf16, quantized payloads as stored) and `ae`
  (monolithic, the model's AE dtype, BatchNorm statistics float32); phases `ref_encode`,
  `denoise`, `decode`; no host pin.
- TEXT ENCODER: one registered model per encoder mode — `flux3_klein_text_encoder.<text|vl_refs>`;
  component `encoder` (run device or CPU, blocks = decoder layers, bf16); phase `encode`. Its
  files and meta follow the PLANNED format (`encoder_source(options, format)`), so every encoder
  row is planned with its own files.
- Both are family `flux3_klein` (the calibration key: the encode entries keep matching). Why one
  model per AE dtype / mode: the manager caches inventories per (model, format, files) and a
  spec's `convert` / `device_itemsize` see no options, while the mode changes the encoder's state
  keys and the AE dtype its device bytes. Switching either makes another handle; the idle one
  stays reclaimable by the residency arbiter (parked or dropped before any degrade); `unload()`
  drops all four models.
- Why two handles: a run that needs no encode (a served caption, "Prompt cache") must not
  acquire, read or forecast the encoder; and the encoder is the second admission of the two-handle rule
  (`paired`): admitted beside the pipeline's reservation under the combined VRAM-axis ceiling,
  with `FLUX3_PAIR` = pipeline phases (ref_encode, denoise, decode), rest `ref_encode` (the
  pipeline's state while the encoder runs), encode `encode`.
- Forecast inputs that need the tokenizer or the reference preparation are UPPER bounds
  (`caption_token_bound`, `reference_pixel_bound`). The encode work's variant follows the
  SELECTED encoder (a `PhaseWork` callable sees no format), so another encoder row is planned with
  the selected one's activation cost (display only).
- `formats` (pipeline): one row per OFFERED DiT format (`Flux3Options.dit_formats`, the client's
  order = quality order, `quality_rank` = count - index; default the natives bf16 / fp8r / nvfp4 =
  3 / 2 / 1): bf16 (native), fp8r (`scaled_mm` or `dequant`, the concrete path resolved before the
  admission), nvfp4 (dequant), a GGUF id (`gguf`), with availability, DiT file size.
  `encoder_formats` (encoder): one row per offered encoder (`Flux3Options.encoder_formats`: `bf16`
  first, then the GGUF candidates), availability per `config.text_encoder_availability`, in
  VL_REFS a GGUF row unavailable (D6). The manager never changes a format; a refusal names the
  formats that would fit.
- No model-specific degrade rung: `reference_max_pixels` and the AE dtype are the user's quality
  choices, the port has no tiled AE, encoder-on-CPU is a step placement (`tiers.template_for`) or
  the host pin below.
- «Энкодер на CPU» (`Flux3Options.encoder_on_cpu`; dataclass default `False`, WIRE default
  `true`): the ENCODER spec's `host_pin_of` pins `encoder` to host execution
  (`runtime/large_models` "Host pin"): CPU from RAM at every VRAM step, placed by the RAM step alone.
- Residency keys: pipeline `flux3_klein:{dit format}|{device}|{fp8 requested}|{ae dtype}|{root}`
  (`pipeline_key`), then `|dit=<file>` for a GGUF DiT (two files may share an id); encoder
  `flux3_klein_text_encoder:{encoder format}|{device}|{encoder mode}|{root}` (`encoder_key`), then
  `|enc=<file>` for a GGUF encoder, plus `|host_pin=encoder` under «Энкодер на CPU» (added by
  `LargeModelSpec.key`; `service._model_key` / `_encoder_handle_key` are those keys): everything
  that changes a handle's loaded bytes, modules or placement; `reference_max_pixels` and the
  resource steps are in neither. The split renamed every FLUX.3 model, so learned floors of the
  former single-handle models stay in the floors file unused until «Забыть».

### GGUF DiT and GGUF encoder
- Sources: `GgufDit(id, path)` (`value` = id, like `WeightFormat.value`) and
  `EncoderSource(id, path | None)` (`bf16` = the `text_encoder/` shards). Ids are the Rust
  catalog's (`FORMAT_ID_PATTERN`), opaque here except the three native DiT ids and `bf16`; paths
  are absolute and come from the client (the catalog's relative paths joined onto the root).
- GGUF DiT (unsloth's export of `model.safetensors`): the names ARE the BFL state keys, norm
  `.scale` F32, a few plain BF16 tensors, the rest block-quantized. It reads the ROOT
  `config.json` (no `quantization` rule). `require_gguf_dit` checks flow map, key set, shapes and
  dequantizable types against the meta `Flux3Dit`; the quantized linears become
  `runtime.gguf_layers.GgufLinear` (compute bf16, `uint8` rows on every tier); plain tensors are
  cast to bf16 by `convert_dit_tensor`. Compute path `gguf`; no calibration entry yet (the
  forecast says `calibrated: false`).
- GGUF encoder (Qwen's `Qwen3-VL-4B-Instruct-GGUF`, `general.architecture` `qwen3vl`, text half
  only): TEXT mode only (D6: `Flux3Options` and `wire.py` refuse VL_REFS with it by name). Its
  names are bridged by `runtime.qwen3_gguf` onto the `Qwen3VLTextModel` keys (blocks >= 33 never
  read; no Q/K permutation); config, tokenizer and processor stay `text_encoder/`'s, and
  `require_gguf_text_encoder` checks the header against that config (block count >= 33, widths,
  head counts, vocabulary) before the admission. The embedding and the linears become
  `GgufEmbedding` / `GgufLinear`; the F32 norms are cast to bf16. The encode work is variant
  `text_gguf` (never a borrowed bf16 measurement).
- Every GGUF refusal names the format id and the wire field (`format` / `text_encoder_format`).

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
- Failures: a refusal before or at the admissions touches nothing (no phase, nothing read;
  `last_error` set for an admission refusal; a refused encoder finishes the admitted pipeline
  session, which only shrinks toward its between-runs state). A failed load scope registers
  nothing: both sessions finish and the run's pipeline handle (its key alone) is unloaded. A CANCEL (`Flux3Canceled`, raised by the
  caller's observer at a phase begin or by its `step_callback` after a denoise step) is not a
  failure: no degrade retry (`degrade.classify_failure` does not know it), nothing is unloaded
  (the session's `finish` shrinks to the between-runs state as after any non-memory error), it is
  logged at INFO and does not touch `last_error`; the next run starts normally.
- `generate(..., step_callback=)`: `(step, total)` after every flow-map step (repeats steps after a
  degrade retry; exceptions propagate).
- `generate(..., load_progress=)`: registered as both sessions' disk-read sink
  (`Session.set_load_progress`) right after the admissions, so every checkpoint read of the run —
  load scope, transitions, reloads after a degrade retry, DISK / DROPPED re-reads — reaches it as
  `loader.LoadProgress` on the run's thread. The service adds no throttle (the manager's
  `ByteProgress`, 0.2 s per stream + one 100% frame) and a raising sink cannot cancel a read.

### Progress frames (`region.py`, wire `inpaint.flux3_klein`)
- Step level: `phase:"load"` per observer phase begin (`step` = its index in `progress_stages()`,
  `total` = the last index, stable `stage` id), `phase:"generate"` per denoise step.
- Byte level: each `LoadProgress` re-emits the `load` frame of the stage IN FORCE (the observer
  phase whose step performs the read; a degrade reload during the denoise is a `load` frame of
  stage `denoise`) plus `file_step` / `file_total` (bytes of this read of the component; a re-read
  counts only the units it brings back), `file_label` (the file name; the folder name for a
  component of several files in one folder) and `file_component` (`transformer` = DiT,
  `text_encoder` = encoder, `vae` = AE). The shared Rust bar (`engines/shared/stream_progress*`)
  draws it as the second bar with a client-side read speed; it localizes `transformer` and
  `text_encoder` and shows the file name alone for `vae`.
- `status(params)` (the `.status` answer: file checks through `large_model.formats` with a
  placeholder fp8 path, never a probe; `formats` per offered DiT id, `text_encoder_formats`
  `{id: {available, reason, bytes}}` for `bf16` and every offered GGUF encoder,
  `text_encoder_present` = the selected encoder format) and the profiles adapter
  (`large_model_profiles`: `paired.plan_pair` over both handles, FLUX.2's answer shape — `selected`
  the pipeline's plan, `encoder` the encoder's or `null` exactly when `EncoderNeed.counted` is
  false (`_encoder_need`: a TEXT caption served by memory or the library, or no installed
  encoder, while the encoder handle holds no byte), `encoder_formats` its rows; the fp8 path is probed once per
  device, cached) read the wire through `wire.normalize_flux3_params`. A failed run keeps the between-runs state
  (a memory failure makes the manager drop everything); every failure of `generate` is logged with
  its traceback, then `_release_failed_run_memory` clears the frames' locals and releases the
  allocator cache.
- `health()` never takes the lock. Published at the end of a run / unload: `device`,
  `model_key` (the pipeline handle's), `variant`, `fp8_compute` (applied), `compute_path`,
  `vram_step` / `ram_step` (the pipeline's effective pair), `degraded`, `last_error`. Live, from
  the manager's ledger of BOTH handle keys (`LargeModelManager.placements`: no probe, never waits
  for a session), so a reclaim park or drop between runs is seen at once: `loaded` (either
  handle), `ready` (DiT + AE held), `dit_device` / `encoder_resident` (the run device while any
  block is on the card, `cpu` when parked in RAM or run on the CPU, `None` when not held). Between
  runs the large pool holds two resident FLUX.3 handles where it held one.
- `Flux3Applied.steps_effective` is the pipeline's effective pair; `degraded` is true when either
  handle degraded (`degrade_reason` the pipeline's, else the encoder's). The service keeps no placement bookkeeping of its own:
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
- every phase: `variant` (the DiT format id), `text_encoder_format` (the encoder format id),
  `encoder_mode`, `fp8_compute` (requested), `ae_dtype`, `device`,
  `reference_max_pixels`, `n_refs`, `width`, `height` (the generated canvas), `steps`, `seed`,
  `image_tokens`, `ref_tokens`, `vram_step` / `ram_step` (requested, may be `auto`), `compute_path`,
  `vram_step_effective` / `ram_step_effective` (the plan in force at the step's begin), and the manager's forecast of the plan in force: `forecast_device_peak` /
  `forecast_host_peak` of the step on ITS handle's forecast (the phase, or the transition it
  performs; the load scope and `DIT_OFFLOAD` are parts of the pipeline's `idle->ref_encode`,
  `ENCODER_LOAD` the encoder's `idle->encode`, `ENCODE` its `encode`; `None` for
  `ENCODER_RELEASE`, the encoder's `finish`), `forecast_run_device_peak` of the whole run under
  the two-handle rule (`paired.combined_device_peak` over `FLUX3_PAIR`) and
  `forecast_run_host_peak` (the pipeline's host peak plus the encoder's growth above the baseline
  RSS, an upper bound), `forecast_calibrated` (both handles; host peaks are the anonymous RSS
  the manager saw with every large-pool holding taken out, plus the handle's own peak); `vram_step_effective` / `ram_step_effective` are the plan in force of
  the step's handle;
- `DIT_LOAD`, `AE_LOAD`: `action` = `loaded` | `resident` (nothing read: already held, or the
  first phase keeps it nowhere);
- `DIT_OFFLOAD`, `DIT_OFFLOAD_DECODE`: `action` = `offloaded` (the DiT leaves the device) |
  `on_host` (stays in RAM) | `skipped`; `DIT_RESTORE`: `restored` | `resident`;
- `ENCODER_LOAD`: `action` = `loaded` (read, also after an exec-device change) | `restored` (from
  RAM to the device) | `resident` | `cached` (a served caption: no encoder handle; the three
  encoder phases then carry `None` forecast keys);
- `ENCODER_RELEASE`, `REF_ENCODE`, `DIT_RESTORE`, `DENOISE`, `DIT_OFFLOAD_DECODE`, `DECODE`:
  `text_tokens` (padded DiT text length), `text_real_tokens`.

### Prompt cache (TEXT mode only; D11, D12, E5-E8)
- One library, `<program root>/prompt_cache/flux3_klein/` (`prompt_cache.LIBRARY_ID`, every
  encoder format), container v2 of `runtime/prompt_library.py`; an entry is the caption's
  `EncodedText.tokens` `[L, 20480]` bf16 under the tensor `text_tokens`, keyed by the exact
  (wire-stripped) caption, `max_sequence_length` 4096 (`TEXT_MAX_TOKENS`) and the encoder
  provenance (source always `stock`, format = encoder format id, fp8 false). The width filter is
  the DiT's `in_channels.text`. Selection, memory, `.status`, list / export / import and the
  build's name rules are the shared orchestration's (`runtime/prompt_cache_service.py`) over
  `Flux3PromptCacheAdapter`; the client's precision order ranks the formats (bf16 > q8_0 >
  q4_k_m).
- Encoder identity: sha256 of `text_encoder/config.json`, `chat_template.json` and the sorted
  (name, size) of the bf16 shards, or the GGUF file's (name, size). An encoder that cannot be
  identified still runs; its encode is just not remembered (the cache never fails a run).
- Memory: always on, at most `PROMPT_EMBED_CACHE_ENTRIES` (8) entries and
  `PROMPT_EMBED_CACHE_BYTES` (512 MiB; the newest entry is kept even above it), per library id
  (`""` = a request without `prompt_library`: the calibration tool, tests); kept by `unload()`.
  Every fresh TEXT encode is remembered with the provenance computed before the load; a library
  read stays in memory, so a file is read once.
- THE user's rule: a served TEXT caption opens no encoder handle — nothing read, placed or
  forecast, and the encoder may be ABSENT on disk, the whole `text_encoder/` directory included
  (`config.resolve_model_files` requires only the DiT files and the AE, so the DiT format rows
  of `.status` / the forecast never depend on it; nothing — not even the tokenizer — is read for a
  served run); a
  resident encoder over a served caption is counted by the forecast but not touched. Only an
  encode requires the weights (`_require_encoder`, refusal names `text_encoder_format`).
- A library file that vanished or changed between the plan and the read (`LibraryEntryGone`)
  re-plans once (before any admission); a caption still unserved is encoded, or refused naming
  the file when no encoder is installed.
- `vl_refs` never reads or writes the library or the memory (the encoder sees the references);
  `.status` answers `prompt_cached: false` there and the build is refused by `encoder_mode`.
- `prompt_cache_build` («Кэшировать»): the encoder handle ALONE (no pipeline, no ceiling, the
  request's steps, a minimal shape), observer phases `ENCODER_LOAD` / `ENCODE` /
  `ENCODER_RELEASE` (IPC stages `prepare`, `encoder_load`, `encode`, `encoder_release` through
  `region.build_prompt_cache`); a handle that held nothing before is unloaded afterwards. Requires
  the selected encoder even when memory already holds the embedding (the entry records its
  provenance). A served caption's `real_length` is its stored (padded) length: no tokenizer is
  read (informational: the denoise reads tokens and ids only).

### Import cost and patch points
Importing the package, `service.py` or `large_model.py` does not import torch. Tests patch on the
DEFINING module: `service._resolve_selected_backend_device`, `service._clear_torch_cache`,
`loading.{build_dit_meta, gguf_dit_layout, require_gguf_dit}`, `autoencoder.build_autoencoder_meta`,
`text_encoder.{build_text_encoder_meta, build_gguf_text_encoder_meta, require_gguf_text_encoder,
load_text_processor, encode_text, encode_with_references}`,
`quant.{resolve_fp8_compute, probe_scaled_mm}`, `prompt_cache.program_root` (the library root); the code looks them up as `<module>.<name>` at call
time. Patching the package re-export is a silent no-op.

## Editing map
- Package layout, `config.json` rules, weight-format detection, DiT / encoder sources, encoder
  checkpoint layout: `config.py`.
- GGUF DiT checks and build: `loading.py`; GGUF encoder checks and build: `text_encoder.py`; the
  name bridge is `../../runtime/qwen3_gguf.py` (shared with FLUX.2), the quantized layers
  `../../runtime/gguf_layers.py`.
- Request validation, defaults, option enums: `params.py`; the IPC params, their defaults and
  the region frame rule, the format candidates: `wire.py` (keep `PROTOCOL.md` "FLUX.3 klein" and
  the Rust engine's settings / frame rule / candidate list in step).
- The region editor (raw answer, seed, progress stages, cancel points): `region.py`; the wire
  codecs it shares with FLUX.2: `../region_imaging.py`.
- Components, blocks, phases, formats, residency key, forecast inputs: `large_model.py` (the
  per-phase activation and time constants are the calibration tables of `runtime/large_models`).
- Run order, the two admissions and their lock order, the step <-> observer-phase mapping,
  `info` keys, health: `service.py` (update the phase list above and the VRAM script together);
  the two-handle memory rule itself: `../../runtime/large_models/paired.py` (shared with FLUX.2).
- fp8 / nvfp4 math or the probe: `quant.py`; checkpoint checks and cast rule: `loading.py`.
- Reference preparation, token packing, noise: `packing.py` (keep `large_model.reference_pixel_bound`
  an upper bound of it); the sampler: `sampling.py`.
- Encoder building, tokenization, the VL experiment: `text_encoder.py`.
- Prompt cache: the library id, the encoder identity, the width read, the adapter in
  `prompt_cache.py`; the run's serve / remember, the build and the library methods in
  `service.py`; the wire fields in `wire.py`; the shared selection and library surface in
  `../../runtime/prompt_cache_service.py` (shared with FLUX.2; keep `PROTOCOL.md` "FLUX.3 klein"
  and the Rust FLUX.3 engine in step).
