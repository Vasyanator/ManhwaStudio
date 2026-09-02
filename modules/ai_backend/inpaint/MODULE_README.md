# Module: modules/ai_backend/inpaint

## Purpose
Inpainting backends of the Python AI backend: six independent service adapters (LaMa V2, LaMa MPE,
AOT, SDXL, FLUX.1-Fill-dev, FLUX.2 klein) that take an image + mask as PNG bytes and return PNG
bytes. They are constructed in `server.py`, stored on `AppState`, and reached from Rust through the
`inpaint.*` IPC methods implemented in `ipc/handlers/`.

## Architecture
Every service follows the same shape and is safe to copy from when adding a seventh backend:

- construction is cheap; torch / diffusers / cv2 are imported lazily inside the methods that need
  them, so importing one inpainter never pulls in another one's dependencies;
- one resident model at a time, guarded by an `RLock`, keyed by a service-specific `model_key`
  (`lama_v2:<device>:<ckpt>`, `aot:<device>`, `sdxl:<mode>:<device>:<path>`, `flux_fill:<quant>`,
  `flux2_klein:<dtype>|<placement>|…|<paths>`, …);
- the key is leased from the shared `runtime.model_manager.LoadedModelManager`
  (`begin_model_use` → `mark_loaded` / `mark_load_failed` → `release`), which is what lets an idle
  model be evicted by another service. **Load and inference are two separate `try` scopes**: see
  the lease-protocol section below;
- the target device comes from `General.ai_device` through the module-local
  `_resolve_selected_backend_device()` (`not-selected` resolves to a real runtime default). The one
  exception is `flux_fill.py`, which ignores the user's device choice and pins itself to the
  discrete GPU via `_select_discrete_device()` — see the FLUX section;
- `health()` / `unload()` are part of every service's contract, and `unload()` reports the dropped
  key back to the model manager.

## Files and submodules
- `lama.py`: `LamaInpaintService` — LaMa V2 (`inpaint.lama_v2`). Discovers `.ckpt`/`.pt` checkpoints
  under `ManhwaStudio_AI_Models/Torch/LaMa/models`, normalizes the `refine` parameters, and loads the
  runtime by FILE PATH (see the dynamic-load section below). Also used as a building block by
  `sdxl.py`, so `server.py` shares ONE instance between the two.
- `lama_v2_runtime_inpainter.py`: `InpainterV2` — the standalone LaMa runtime wrapper (standard +
  refine modes). **Not a package member at runtime**; see below.
- `lama_runtime_bundle/`: vendored `saicinpainting` (training modules, evaluation, refinement) that
  `InpainterV2` imports. Loaded via `sys.path`, never as a subpackage of `modules.ai_backend`. See
  `lama_runtime_bundle/MODULE_README.md`.
- `lama_mpe.py`: `LamaMpeInpaintService` — LaMa MPE (`inpaint.lama_mpe`). Downloads and SHA256-checks
  `inpainting_lama_mpe.ckpt` into `ManhwaStudio_AI_Models/Torch/LaMa_MPE`, and requires a
  `lama_modernised/` checkout in the program root (`_validate_runtime_layout_locked`).
- `aot.py`: `AotInpaintService` — AOT-GAN (`inpaint.aot`), including the ported generator
  (`AOTGenerator` and its scaled-weight-standardized conv blocks). Weights:
  `ManhwaStudio_AI_Models/Torch/AOT/inpainting.ckpt`.
- `sdxl.py`: `SdxlInpaintService` — SDXL (`inpaint.sdxl`, streaming). See the SDXL section.
- `flux_fill.py`: `FluxFillInpaintService` — FLUX.1-Fill-dev (`inpaint.flux_fill`, `.unload`,
  `.status`, streaming). See the FLUX section.
- `flux2_klein.py`: `Flux2KleinInpaintService` — FLUX.2 klein 9B region editing
  (`inpaint.flux2_klein`, `.status`, `.estimate`, `.unload` and the `.prompt_cache.*` family,
  streaming). See the FLUX.2 klein section.
- `test_sdxl.py`, `test_flux_fill.py`, `test_flux2_klein.py`, `test_lease_protocol.py`: pure-Python
  unit tests (no torch, no diffusers, no weights, no GPU — fake `torch`/`diffusers`/`transformers`
  modules are injected into `sys.modules`, and `test_lease_protocol.py` stubs each service's load and
  inference step instead).

## Contracts and invariants

### The lease protocol: a failed inference is not a failed load
All six services take the lease BEFORE `self._lock` (`begin_model_use` can block while another
thread's eviction callback waits for this service's lock; the reverse order deadlocks), and then run
two distinct `try` scopes inside it:

- **load scope** — `_ensure_*_locked(...)` only. A failure here, and only here, reports
  `lease.mark_load_failed()`.
- **run scope** — everything after `lease.mark_loaded(...)`, which is called as soon as the load
  returned. A failure here records `_last_error` and re-raises without touching the lease.

`mark_load_failed()` reaches `LoadedModelManager.abort_load`, which clears the entry's `resident`
flag and drops its unload callback. Calling it after the load already succeeded — while the service
still holds the model in `_net`/`_model`/`_pipe` with `_active_key` set and its weights still occupy
VRAM — makes the manager under-count residency and makes that model permanently non-evictable until
the same key is used again. Wrapping load and inference in one `try` is therefore a defect, not a
style choice; `test_lease_protocol.py` pins it for all six services at once.

`lease.release()` runs in `finally` in every path. One consequence to keep in mind: because a model
that survived a failed inference now correctly counts toward `max_loaded_models`, a user cap of `1`
makes SDXL's `four_channel` mode (which takes a SECOND lease for the shared LaMa prefill) fail with
the manager's explicit "лимит загруженных моделей" error instead of silently exceeding the cap.

### The dynamic LaMa load chain (the most fragile thing here)
`lama.py` → `lama_v2_runtime_inpainter.py` → `lama_runtime_bundle/` is wired by **file path, not by
import**, and static analysis cannot see it. Getting it wrong produces no import error at all — only
a `FileNotFoundError` the first time a user runs LaMa V2 inpainting.

- `LamaInpaintService._resolve_inpainter_source_path()` returns
  `Path(__file__).resolve().parent / "lama_v2_runtime_inpainter.py"` and
  `_prepare_runtime_paths()` prepends `Path(__file__).resolve().parent / "lama_runtime_bundle"`.
  Both are derived from **this package's own location**, never from the program root, so that moving
  the package moves the whole chain as one unit. Keep it that way.
- `_load_inpainter_class_locked()` then executes the file via
  `importlib.util.spec_from_file_location` under the module name
  `mf_lama_inpainter_v2_runtime`, cached in `sys.modules`.
- **`lama_v2_runtime_inpainter.py` must stay self-contained.** Loaded that way it has no package
  context (`__package__` is empty), so ANY relative import in it (`from ..runtime.paths import
  program_root`) raises `ImportError` at load time. It counts its own depth
  (`_MODULE_DIR.parents[2]` = program root) instead of calling `runtime.paths.program_root()`, and
  that is deliberate — the file header says so. Do not "clean it up".
- `_prepare_runtime_paths()` also puts the program root on `sys.path` so the standalone module's
  `from config import LAMA_DIR` can resolve.

### SDXL
- Two modes, `nine_channel` and `four_channel`, and the loaded UNet's `in_channels` must match:
  `_validate_mode_channels()` raises an explicit error on a mismatch. **Never** fall back silently to
  the other mode — the wrong channel count produces garbage, not a degraded result.
- `four_channel` prefills the hole with the shared `LamaInpaintService` (so the text is gone from the
  context) before a moderate-denoise latent-blend pass; `normalize_sdxl_params` therefore caps
  `denoise_strength` below 1.0 in that mode, because strength 1.0 re-noises the hole to pure noise
  and throws the prefill away.
- Weights are user-supplied (a ckpt/safetensors file, a local diffusers folder, or an HF repo id), so
  this service owns no fixed model directory.
- Progress streaming: when a `progress_callback` is given, a diffusers `callback_on_step_end` emits a
  cheap linear latent→RGB preview per step (`_latent_preview_rgb`, no VAE decode); `ipc/handlers/
  sdxl.py` turns those into `progress` frames carrying a latent-preview PNG blob, followed by a
  terminal `response` (see `ipc/PROTOCOL.md §5.4`).
- `_encode_png_bytes_rgb` is imported **directly** by `ipc/handlers/sdxl.py` to encode those preview
  blobs. It is the one handler→service import that bypasses `HandlerContext.state`; do not rename or
  move it.
- fp16 runs set `pipe.vae.config.force_upcast` rather than calling `upcast_vae()`, which would break
  the fp16 masked-image encode.

### FLUX.1-Fill-dev
- All weights live under `ManhwaStudio_AI_Models/side_models/FLUX.1-Fill-dev-GGUF/`, **not** the
  Hugging Face cache: the chosen GGUF quant from `YarvixPA/FLUX.1-Fill-dev-GGUF`, plus the diffusers
  components (VAE / CLIP-L / T5-XXL / scheduler / tokenizers) from the open `ostris/Flex.1-alpha`
  repo under `components/`. Downloads go through `../engines/model_download.py` (shared with
  `watermark/service.py`): serialized per destination file, staged into a process-private
  `<name>.<pid>.part` and published with an atomic `os.replace`. `ensure_model()` deliberately runs
  outside `self._lock` and the IPC layer dispatches onto a thread pool, so two first uses of the
  same quant do reach it at once; the loser of each file's lock re-checks and skips instead of
  refetching. `flux_fill.py` keeps only the transport (the HF bearer header).
- `progress_callback(phase, step, total, label)` has two phases: `download` (byte-level) and
  `generate` (step-level). `ipc/handlers/flux_fill.py` streams both as `progress` frames with header
  `phase`/`step`/`total`/`label` and no preview blob.
- The pipeline is pinned to the DISCRETE GPU; `_select_discrete_device()` excludes the Ryzen iGPU and
  falls back to CPU rather than using it.

### FLUX.2 klein
- **The mask is a PERMISSION TO CHANGE, not a hole.** Every pixel outside it must come back
  byte-identical, and `_composite_over_region` guarantees that: the feather is applied INWARDS
  so the blend weight is exactly zero outside, and a final `np.where` copies the original pixels
  back. Do not replace it with a plain Gaussian feather.
- **`whole_region` is the "no mask" mode, and the flag is VERIFIED, not trusted.** The client still
  sends a mask — a solid one — so the request format does not fork, and `_require_solid_mask`
  refuses a mask with any empty pixel, naming the count. A flag that disagrees with the data would
  otherwise surface as a partial edit while the UI said no mask was needed. The mode settles two
  other params in `normalize_flux2_klein_params` (`_whole_region_overrides`, which logs whenever it
  overrides something the caller asked for): `mask_dilate_px` → 0, because a full mask has nothing
  to grow into, and `color_match` → `False`, because `_match_color_outside_mask` takes its
  statistics from the ring OUTSIDE the mask and here that ring is empty — computing it from the
  changed pixels would force the edit's own mean and standard deviation back onto the original's,
  i.e. undo the edit, and computing it from an empty sample is a division by zero. `mask_feather_px`
  is deliberately NOT switched off: it is what joins the regenerated region to the page.
- **The region's border is a mask contour.** `_mask_distance_inside` pads the mask with a ring of
  zeros before measuring, because the region is a WINDOW onto a larger page and the pixels past its
  edge belong to that page, which the request may not change. Without the ring neither backend sees
  a contour there — `cv2.distanceTransform` only measures to zeros that exist inside the array, and
  `cv2.erode` / `ImageFilter.MinFilter` extend the border rather than eating into it — so a mask
  painted up to the region edge met the untouched page with a hard step, and under `whole_region`,
  where the mask covers everything, the feather was a complete no-op.
- **`mask_feather_px` is the ramp WIDTH, and the blend rounds.** `_feather_mask_inwards` builds
  the alpha as a smoothstep of `_mask_distance_inside`, so the weight is 0 on the contour and 1
  at exactly `feather_px` px inside; a mask thinner than that compresses the ramp to its own
  half-width instead of losing the edit. Two failure modes this replaced, both measured on a real
  page and both visible as the "seam" users reported: an erode-then-Gaussian-blur ramp whose
  nominal 6 px spanned ~22 px and whose weight peaked at 0.13 for `feather_px=32` (silently
  discarding 87% of the edit), and a truncating `astype(np.uint8)` that biased every partially
  blended pixel one level DARKER — a mask-shaped dark patch with a hard contour. The blend must
  stay an exact identity when `generated == original`; there is a test for it.
- **The seam is bounded by the composite, not by `mask_dilate_px`.** Measured across dilate
  0/8/16/32/64 with the corrected feather, the excess gradient on the mask contour stays within
  0.3-0.8% — the dilate exists to give a thin mask a full latent cell (the pipeline bilinearly
  downsamples the mask to a 16 px grid), not to control the edge. What the composite CANNOT fix
  is that a 4-step distilled denoise returns the masked area ~18% less textured than the original
  (Laplacian variance, ring 8-24 px inside the mask); that is the model, and no parameter here
  changes it.
- **The mask is accepted only as L8.** `_decode_mask` checks `img.mode == "L"` and raises on
  anything else. An RGB/RGBA mask used to be converted by taking the alpha or the per-channel
  maximum, which silently promoted stray colored pixels to "edit this" — a client bug turning
  into an edit of the wrong area. There is no conversion path; the wire contract is the check.
- **The transformer path is one contract for both input shapes.** `_load_transformer` forces
  `guidance_embeds=False` and refuses fp8_scaled weights BEFORE it branches on file vs.
  directory: klein has no `guidance_in` block whatever config a diffusers folder ships, and a
  folder of fp8 shards must be refused on the header (`component_safetensors_shards` walks the
  `*.index.json` weight map plus the `*.safetensors` glob) instead of failing multiple GiB into
  `from_pretrained`.
- **A single-file transformer REQUIRES its own `config.json` on disk, and there is no substitute
  for it.** diffusers 0.39 recognizes every FLUX.2 checkpoint as `flux-2-dev`
  (`single_file_utils.py`, `CHECKPOINT_KEY_NAMES["flux2"]`) and, given no `config`, resolves the
  gated `black-forest-labs/FLUX.2-dev` repo over the network — the config of a DIFFERENT model.
  `_load_transformer` therefore searches `<checkpoint dir>/transformer`, `<checkpoint dir>`,
  `<parent>/transformer`, `<parent>` (`find_transformer_config_dir`, order shared with the error
  message through `component_probe_order`), passes the directory it finds as `config=` and adds
  `local_files_only=True`; when it finds none it raises naming the exact file to provide. Two
  things must not be "fixed" here:
  - **Do not reconstruct the config from the checkpoint.** Most parameters do follow from tensor
    shapes (`num_layers`, `num_single_layers`, `in_channels`, `joint_attention_dim`,
    `attention_head_dim`, `num_attention_heads`, `mlp_ratio`, `guidance_embeds`), but `rope_theta`
    — 2000 for klein, not the usual 10000 — plus `eps` and `patch_size` do not, and a model built
    with the wrong `rope_theta` has EXACTLY the same tensor shapes. A shape check on `meta` passes,
    the load succeeds, and the user gets quietly wrong images. That is the silent fallback this
    package forbids, so the missing config is an error instead.
  - **Do not let the Hub fallback back in.** It is not a convenience: it substitutes another
    model's architecture while the user believes their own checkpoint is loading.
  `validate_transformer_config_dir` additionally refuses a discovered `config.json` whose
  `_class_name` is not `Flux2Transformer2DModel`, because the search also probes the checkpoint's
  own directory, where a VAE or text-encoder config could otherwise be picked up.
- **A failed transformer restore invalidates the pipeline.** Moving the parked 9B transformer
  back onto the device is itself a full host->device copy that can OOM. That failure never
  reaches the caller — it must mask neither a decode that succeeded nor one that failed for a
  better reason — so `_decode_locked` logs it (with the decode error still in the exception
  chain) and calls `_invalidate_pipeline_locked`: `_pipe`/`_active_key` are cleared and the key
  is reported to the model manager, because a cache hit skips placement and would otherwise run
  a pipeline whose transformer sits on the host.
- **Cancellation happens at call boundaries only.** The handler checks `cancel_event` before it
  calls the service and after the service returns; a running diffusion step is not interrupted.
  This is the shared contract of all six inpaint services (`ipc/handlers/sdxl.py`,
  `ipc/handlers/flux_fill.py` do exactly the same) — do not make this one the exception.
- **Nothing is downloaded.** The user supplies three paths (Qwen3 text encoder, transformer, VAE);
  the pipeline needs five components, so the tokenizer and the scheduler are DISCOVERED next to
  those paths (`component_search_roots` / `discover_component_dir`). A component that is not found
  raises an error naming what to put where — there is deliberately no built-in scheduler config,
  because an invented one produces plausible-looking garbage.
- **A weights FILE may stand in for its component folder** (`component_dir_for_path`). Neither
  `AutoencoderKLFlux2` nor the Qwen3 encoder has a single-file loader in diffusers 0.39, and a user
  picking a model naturally selects `diffusion_pytorch_model.safetensors` rather than the directory
  above it — which produced the loader's own "compatible with StableCascadeUNet, ..." class list as
  a user-facing error. A file whose directory holds a `config.json` is therefore normalized to that
  directory; a file without that sibling is left alone so the caller still diagnoses it. This is a
  path normalization, not a fallback: the folder containing the file IS the component, and its
  `config.json` describes that very file. The transformer is exempt — it has a real single-file
  loader, so a file path there means a file.
- **The region is validated, never fixed up** (`validate_region_size`): sides a multiple of 16 and
  at least 128 px, area at most 1 MP, aspect ratio at most 8:1. The pipeline would otherwise resize
  above 1 MP and crop to a multiple of 16, returning a window of a different size than the mask.
- **The standalone decode owns two things `pipeline.__call__` would have given it for free.**
  Pulling the VAE decode out of the pipeline means it is no longer under diffusers'
  `@torch.no_grad()` and no longer gets the pipeline's device bookkeeping, and both bit us on real
  weights:
  - `_decode_once` (and `_encode_prompt_phase`) open `torch.no_grad()` themselves. Without it the
    VAE builds an autograd graph and `image_processor.postprocess` dies on *"Can't call numpy() on
    Tensor that requires grad"*.
  - `_vae_input_device` decides where the latents go. With NO accelerate hook it is the VAE's own
    device — never `_execution_device`, because `unload_transformer_before_vae` may have parked the
    transformer on the host by then and the probe would answer `cpu`. UNDER a hook
    (`model_cpu_offload`, `sequential_cpu_offload`) it is `_execution_device`, because diffusers'
    `@apply_forward_hook` calls `pre_forward(self)` WITHOUT the arguments: the hook moves the
    weights to the accelerator and leaves the latents behind (`cpu` under model offload, `meta`
    under sequential).
- **Denoise and decode are two steps.** The pipeline is called with `output_type="latent"` (which
  already returns unpacked, batch-norm-denormalized latents) and the VAE decode runs separately, so
  the transformer can be parked on the host first — its residency plus the decode peak is the most
  common OOM here. An OOM in the decode is recovered from without repeating the denoise: a host copy
  of the latents is kept, the transformer is parked, then tiling/slicing is enabled. The settings
  actually in force come back to Rust as `applied` so the next run starts on the cheap path.
- **A run is TWO PHASES, and their ORDER is a memory contract, not an optimization.** The text
  encoder is needed for exactly one forward pass per prompt: the four denoising steps and the VAE
  decode never look at it. Holding 8B of Qwen3 resident next to the 9B transformer is what made
  every profile cost ~32 GiB — more than the 31.9 GiB this project's reference card reports free,
  so `full_gpu` could not work at all, and more than the host had free, so the load once got the
  user's editor killed by the OOM killer. Therefore:
  - **phase 1** (`_ensure_pipeline_locked`) builds the pipeline with `text_encoder=None` and
    places the transformer + VAE. `Flux2KleinInpaintPipeline` accepts the `None`: `pipe.components`
    validates only the KEY set, `device` / `_execution_device` skip non-modules, and `encode_prompt`
    is never reached because `prompt_embeds` is supplied.
  - **the warm-up** (`_warmup_pipeline_locked`) then makes "placed" mean "materialized": it refuses
    a component still on the host or on `meta` (`_require_components_materialized`), runs one 64x64
    VAE decode (`_warmup_vae_decode`) to retire the copies and prime the allocator / MIOpen, and
    synchronizes. It is a `phase:"load"` step and deliberately never a `generate` one. Skipped
    under the two accelerate offload placements, where the weights are SUPPOSED to stay in host
    memory between forwards.
  - **phase 2** (`_prompt_embeds_locked` → `_encode_prompts_locked` → `_encode_prompt_phase`) loads
    the encoder, encodes the prompt (and the empty negative prompt when `guidance_scale > 1`) into
    a few MB of embeddings, and by default keeps it.
  - **Why the encoder comes SECOND.** In the reverse order — the one this replaced — the encoder's
    16 GB host peak and the transformer's 18 GB host peak were both host peaks, so keeping the
    encoder meant `max` became a sum (~34.7 GiB) and only `unload_text_encoder_after_encode=True`
    made a run fit. Loading the pipeline first moves 18 GB to the accelerator BEFORE the encoder is
    read, so the encoder arrives into a host that is free and can stay there. The peak is
    `max(encoder-in-host, transformer + VAE-on-card)` on two different resources instead of one.
  - **The encoder therefore encodes in HOST memory in every placement**, `full_gpu` included. It
    used to go on the accelerator there, which was safe only while it ran before the transformer
    existed; now the transformer is already resident and 18.3 + 16.4 GB does not fit on a 34.2 GB
    card. Anything that puts the encoder back onto the device, or back into the resident pipeline,
    undoes this.
- **The prompt-embedding cache is always on** (`_prompt_cache`, LRU, `PROMPT_EMBED_CACHE_ENTRIES`).
  It is keyed by encoder path + text + `max_sequence_length` + dtype + fp8 — not by placement, since
  the same encoder produces the same embedding wherever it ran. A mask edit, a new seed or a
  repeated prompt therefore skips the prompt phase entirely; a miss costs a full 16 GB read from
  disk, which is why that phase reports its own `phase:"load"` progress steps (7-9). Entries
  live on the HOST so the cache never pins device memory, and `unload()` keeps them: they are
  megabytes of embeddings, not weights.
- **The prompt cache is also a LIBRARY on disk** (`prompt_cache.build` / `.list` / `.save` /
  `.load` / `.export` / `.import`). The point is to stop holding 16 GB of encoder for a prompt
  the user always types anyway: `build` encodes a prompt into the in-memory LRU **without
  generating anything** and lets the encoder go again, `save` writes that entry to disk, and
  `load` puts it back into the same LRU on a machine where the encoder was never read at all.
  - **One key, one cache.** Generation, `build`, `load` and `status.prompt_cached` all go through
    `_prompt_cache_key` and the same `_prompt_cache`. A second key would report hits the run then
    misses.
  - **The text encoder is OPTIONAL when the prompt is cached.** That is the point of the file
    format: a `.msprompt` carried to a machine where the 16 GB Qwen3 was never downloaded must
    still generate, and physically it can — the four denoising steps and the VAE decode never
    look at the encoder, and `_prompts_to_encode()` returns an empty list on a full hit.
    Therefore `text_encoder_path` is the ONE path `normalize_flux2_klein_params` does not
    require, an empty path and one that is not on disk mean the same thing
    (`text_encoder_available` — the second is what a settings file carried from another machine
    looks like), and the absence becomes an error only where an encode is unavoidable:
    `_encode_prompts_locked` (the invariant, reachable from every encoding path) plus an early
    copy of the same check in `inpaint_image_bytes` and `prompt_cache_build`, so a doomed run
    does not first read 18 GB of transformer. The refusal (`require_text_encoder`) names BOTH
    ways out — configure an encoder, or load a ready cache for this prompt. A path that EXISTS
    but cannot be fingerprinted stays a hard error: degrading a broken checkout into "no
    encoder" would hide it behind a silently weaker check.
    - **What is still checked without an encoder.** Everything that does not need one: the
      format marker, the format version, `max_sequence_length`, the dtype (metadata AND the
      tensor's own dtype token) and the fp8 flag. Only the fingerprint comparison is skipped,
      it is logged, and the answer carries `encoder_verified: false` — "checked" and "taken on
      trust" must never reach the client as the same thing.
    - **The library without a current family.** `encoder_family_name` needs the encoder, so with
      none installed there is no current family: `prompt_cache.list` then lists EVERY family
      (each entry names its own `family`, top-level `family` is empty = none active, `directory`
      is the library root), and `load`/`export` resolve a name across families
      (`find_prompt_cache_entry`). A name that exists in two families is REFUSED naming both,
      never resolved by an arbitrary rule. No second naming scheme was introduced — the family
      is the one already recorded in each file. `save` is the exception that still REQUIRES the
      encoder: the entry it writes must name the encoder that produced it.
    - **`status`** reports `text_encoder_available` beside `available`; `available` is not false
      merely because the encoder is missing when `prompt_cached` is true. Only the ENCODER is
      waived that way: the Qwen tokenizer is a real pipeline component (`_ensure_pipeline_locked`
      builds the pipeline with one and `_require_component_dir` raises without it), so it stays
      required in every case — measured on the reference host, where the encoder-less run still
      emitted the "Загрузка токенизатора" load step.
    - **The forecast follows.** With no encoder installed `forecast_memory` zeroes the `encode`
      and `encode_standalone` phases (neither can run there), so `estimate` and the guard get a
      lower number from the same single calculation.
  - **Layout:** `<program root>/prompt_cache/<encoder family>/<name>.msprompt`, the root taken
    from `runtime.paths.program_root()`. The family level exists because an embedding is only
    valid for the encoder that produced it; two encoders must never share a listing. A family
    name is `<encoder directory name>-<first 8 hex of the fingerprint>` — readable so a human can
    see whose folder it is, hashed so that the two `text_encoder/` folders every checkout ships
    cannot merge. Both the family and the entry name pass `sanitize_name_component`, an
    ALLOW-list, so nothing a user or a file can say composes a path outside the library.
  - **File format `.msprompt`:** a safetensors container with one `prompt_embeds` tensor and a
    `__metadata__` map (format marker + version, the ORIGINAL prompt text, `max_sequence_length`,
    dtype, fp8 flag, `text_encoder_id`, `text_encoder_family`, `text_encoder_path` (informational)
    and `created_at`). safetensors because the dependency is already here, third-party tools can
    read it, and its header can be parsed without materializing a tensor — which is what makes
    every refusal below torch-free and free of allocation (`read_prompt_file_header`).
  - **Encoder identity is checked, and checked CHEAPLY.** `text_encoder_fingerprint` is a SHA-256
    of the encoder's `config.json` plus the sorted `(file name, size)` list of its weight files —
    never a hash of the 16 GB itself, which would cost more than re-encoding the prompt. It
    catches another model, another precision, another shard layout and a partial download; it
    does not catch a fine-tune with an identical config and identical file sizes. Embeddings of a
    foreign encoder load without an error and denoise into something else entirely, so
    `validate_prompt_file_metadata` refuses a mismatch by name — encoder, sequence length, dtype,
    fp8, and the tensor's own dtype token against the declared one.
  - **`import` files an entry under the family recorded IN THE FILE**, not under the selected
    encoder: filing it under the current one would hide it from the encoder it actually works
    with. A family mismatch is therefore not an import error — the answer carries
    `family_matches: false` for the client to warn about. `load` of a foreign entry stays a hard
    refusal: importing files a cache away, loading feeds a generation.
  - **A name collision is a decision.** `save`/`import` refuse an existing name unless
    `overwrite` was passed, because rebuilding a lost entry costs the encoder read it was saved
    to avoid. Every write is atomic (`publish_bytes_atomically`: `<name>.<pid>.part` +
    `fsync` + `os.replace`), the same recipe as `../engines/model_download.py`.
  - **A `build` loads ONLY the encoder, and the guard knows it.** `forecast_memory` grows one
    phase, `encode_standalone` (no pipeline on the card, no pipeline in the host), which is the
    only phase `_require_encode_headroom_locked` checks. Its VRAM is 0 and its RAM never exceeds
    the `encode` phase's, so adding it cannot move `estimate`, `vram_bytes`/`ram_bytes` or
    `_preset_advice` — there is still exactly ONE forecast.
  - `build` releases the encoder afterwards (that is the point of the button), EXCEPT when one
    was already resident under the same key when the call arrived — that one belongs to the
    previous run and the user's own `unload_text_encoder_after_encode` decides its fate.
  - A corrupt or foreign file in a family directory does not break `list`: it is reported in
    `skipped` with its reason. Users can and do drop files there by hand.
- **`unload_text_encoder_after_encode`** (default: `False` in EVERY placement) keeps the encoder
  between runs for instant prompt changes. The default flipped with the load reorder: the encoder
  now arrives after the transformer has left host memory, so keeping it costs RAM that nothing else
  in the run wants back, while dropping it costs a full 16 GB re-read on the next cache miss. When
  it is off, the encoder IS resident, so it joins `_model_key` — the key must never claim less than
  what the service holds. **The default is duplicated on the Rust side** (`MemoryPreset::values` and
  `Flux2KleinSettings` in `src/tabs/cleaning/tools/flux2_klein.rs`); changing one side alone makes
  the UI and the backend disagree about what a preset means.
  - The one combination the reorder does NOT make cheaper is a resident encoder together with
    `unload_transformer_before_vae`: parking the 9B transformer for the decode copies it back into
    host memory, where the kept encoder already sits, and those two genuinely coexist (~34.5 GiB).
    `forecast_memory` sums them for the `decode` phase, and `_preset_advice` offers the encoder
    unload as its own lever precisely for that case.
- **`text_encoder_fp8`** (default `False` everywhere, presets included) is weight-only fp8 for the
  encoder's linear layers, done with torch alone (`_quantize_text_encoder_fp8`; torchao,
  bitsandbytes and quanto are not installed and none was added). Per-output-row scales, dequantized
  to the compute dtype in `forward`; measured relative max error ~3.4% per weight tensor on gfx1201
  / torch 2.12.0+rocm7.2. **It does not lower the load peak** — the bf16 weights must exist before
  they can be quantized — so it only pays off together with
  `unload_text_encoder_after_encode=False`. A torch build without `float8_e4m3fn` raises; the flag
  is never silently ignored.
- **`placement` is an enum, not a boolean**: `full_gpu`, `encoder_cpu`, `model_cpu_offload`,
  `sequential_cpu_offload`. Since the load reorder it governs ONE thing: how the transformer + VAE
  are placed. It no longer decides where the prompt is encoded — that is always host memory now,
  because the transformer is already on the card by then.
- **Placement never trusts the loader kwargs.** `_apply_placement` moves the GPU-resident components
  unconditionally, even when they were asked to load straight onto the device: `nn.Module.to` is a
  no-op for a tensor already there, and a loader that silently ignores a placement kwarg must not be
  able to leave a component behind. diffusers 0.39's `from_single_file` is exactly such a loader — it
  accepts `device_map`, discards it, and honours only its own `device` kwarg (`_single_file_device`
  does the translation) — which is how a single-file transformer used to stay in host RAM under
  «Минимум RAM» while the VAE was on the GPU, making `_execution_device` answer `cpu` and killing the
  run inside the VAE's first `conv2d`. Before the pipeline is called under `encoder_cpu`,
  `_require_execution_device` checks the probe against the placement TARGET (the service's
  `_device`, not a component's own device) and raises a named error instead of letting torch fail.
- The Qwen3 encoder is the one **transformers** loader here, so it takes `dtype=`; the diffusers
  loaders take `torch_dtype=` and nothing else. Do not unify the two spellings.
- **The load is GATED on free memory, per phase** (`_require_memory_headroom`, called from
  `_require_headroom_locked` before anything is read). A host-side shortfall is not an exception:
  the kernel OOM killer picks a victim among everything running, and it has already closed a user's
  editor with unsaved work. The forecast is `forecast_memory` — the SAME function `estimate` reports
  to the UI, because a guard that disagrees with the number on screen is worse than no guard — and
  each phase is checked separately, since they run in sequence. Only phases that will actually load
  something are checked: a cached prompt skips `encode`, a resident pipeline skips
  `denoise`/`decode`. Reserves (`HOST_MEMORY_RESERVE_BYTES` 2 GiB, `DEVICE_MEMORY_RESERVE_BYTES`
  512 MiB) exist because occupying the last byte is what makes the OOM killer fire before our own
  exception can. A free-memory figure of `0` means UNKNOWN (no psutil, no accelerator) and never
  refuses a run.
  - **What the service already HOLDS is discounted, and that is not optional.** `forecast_memory`
    answers for a run starting from nothing, which is what `estimate` must show; the guard runs
    against a machine where the placed pipeline and the kept encoder are already allocated and
    therefore already missing from the free figures. `forecast_memory` publishes those two costs
    as `resident` (`pipeline_device` / `text_encoder_host`) and `_require_memory_headroom`
    subtracts whichever `_require_headroom_locked` reports as held — so there is still exactly one
    calculation, and the numbers in the refusal are the ones that were compared. Measured on the
    reference host the first time a second prompt reached a resident pipeline: the guard demanded
    17.6 GiB of VRAM that the very pipeline it was about to reuse was occupying, and refused a run
    whose memory was already in place. The refusal message names the short resource, the phase, the numbers, and the
  settings that DO fit right now — computed from `_MEMORY_PRESETS`, which must stay in sync with
  `MemoryPreset::values` in `src/tabs/cleaning/tools/flux2_klein.rs`.
- **`is_distilled` is left at `False`** and `guidance_scale` (default 1.0) decides: the pipeline uses
  the flag for nothing but `do_classifier_free_guidance`, so at 1.0 the run is identical to a
  distilled configuration while the slider stays meaningful. Nothing is assumed about the user's
  checkpoint.
- Unlike `flux_fill.py`, this service honours the user's `General.ai_device` choice through the
  module-local `_resolve_selected_backend_device()`.
- **The reported device is never a placeholder.** `status`, `health` and the generation answer take
  it from `_device_label()`: the loaded `self._device` when a pipeline is resident, otherwise the
  device `_resolve_selected_backend_device("cuda")` WILL pick — the same call the pipeline build
  makes, so the two cannot disagree. Answering `"cpu"` because nothing is loaded yet told the user
  that a run costing tens of minutes would happen on the CPU while it would in fact run on the
  configured GPU; `loaded` / `ready` are what distinguish a fact from a plan. For the same reason
  `memory_snapshot(device)` reads the free VRAM of THAT accelerator (`cuda:1` means card 1), so the
  `estimate` forecast is not compared against another card's memory.
- `_materialize_components_for_offload` is a deliberate copy of `flux_fill.py`'s (this package
  forbids importing a sibling service); it differs in listing the transformer too — here it is
  safetensors-backed, not GGUF — and last, so a failed round trip of the largest component still
  leaves the smaller ones re-homed.

### ROCm staging obligation
On a ROCm Torch build, a host→device copy out of a safetensors file mapping stalls in amdkfd (~1-2 s
per tensor ≥1 MiB). Any weight move here must route through `runtime/rocm_mmap_transfer.py`:
- `sdxl.py`, the plain FLUX path and the non-offload FLUX.2 klein placements wrap their move in
  `with patched_module_to():`;
- the FLUX CPU-offload path cannot (the patch is process-global and must never wrap inference), so
  `_materialize_components_for_offload()` re-homes the safetensors components in anonymous host
  memory up front instead, clearing the allocator cache between components.
The block under `patched_module_to()` must contain the weight move and nothing else — no download,
no network wait, no inference. Everything is a strict no-op off ROCm.

One documented exception: FLUX.2 klein's `low_cpu_mem_usage` path hands accelerate a GPU
`device_map`, which never calls `nn.Module.to` and therefore cannot be staged. **Measured**
2026-09-02 on this project's ROCm host (AMD Radeon AI PRO R9700 / gfx1201, torch 2.12.0+rocm7.2,
diffusers 0.39.0, accelerate 1.12.0), klein VAE, 168 MB, 250 BF16 tensors of which 42 are ≥ 1 MiB,
page cache dropped before each run, two runs per variant: `device_map` 1.35 s / 1.12 s, CPU load +
staged `.to()` 1.38 s / 1.13 s, CPU load + UNSTAGED `.to()` 1.46 s / 1.27 s. All 42 large tensors
report `tensor_needs_staging() == True`, yet the unstaged move costs 0.10 s rather than the 42-84 s
an amdkfd stall would imply: the pathology does not reproduce through this loader on this driver,
so the `device_map` shortcut costs nothing measurable and stays. This is a measurement of one host,
not a repeal of the rule — re-measure before extending the exception anywhere else.

### Package boundaries
- `__init__.py` exposes nothing on purpose: importing one inpainter must not drag in another's
  dependency stack (diffusers for SDXL/FLUX, torch for AOT/LaMa MPE). Import submodules directly.
- Depth-sensitive paths go through `runtime.paths.program_root()`, never a local
  `Path(__file__).resolve().parents[N]`. The single exception is
  `lama_v2_runtime_inpainter.py` (standalone, cannot import it).
- Allowed dependencies: `..runtime.*` and, inside this package, `sdxl.py` → `lama.py`. This package
  must not import `ipc/`, `server.py`, or a sibling service package.
- Errors are explicit: a missing package, missing weights, a mode/channel mismatch, or a bad mask
  size raises. No silent fallbacks.

## Editing map
- To change LaMa checkpoint discovery or refine defaults, see `lama.py`.
- To change where the LaMa V2 runtime or its `saicinpainting` bundle is found, see
  `_resolve_inpainter_source_path` / `_prepare_runtime_paths` in `lama.py` — and re-verify by loading
  the class, not by reading the code.
- To change SDXL param validation, sampler mapping, or the latent preview, see `sdxl.py`
  (`normalize_sdxl_params`, `SAMPLER_CONFIGS`, `_latent_preview_rgb`); keep `SAMPLER_CONFIGS` in sync
  with `SDXL_SAMPLERS` in `src/tabs/cleaning/tools/sdxl.rs`.
- To change the FLUX quant catalog, download layout, or device pinning, see `flux_fill.py`
  (`AVAILABLE_QUANTS`, `_build_download_plan`, `_select_discrete_device`); the staging /
  serialization / atomic-publish envelope itself lives in `../engines/model_download.py` and is
  shared with `watermark/`.
- To change FLUX.2 klein params, region limits, placement modes, the memory forecast, or the OOM
  recovery ladder, see `flux2_klein.py` (`normalize_flux2_klein_params`, `validate_region_size`,
  `_apply_placement`, `forecast_memory`, `_decode_region_latents`); keep the param set in sync with
  `ipc/PROTOCOL.md §5.4`. The wire contract carries five `applied` flags —
  `unload_transformer_before_vae`, `vae_tiling`, `vae_slicing`,
  `unload_text_encoder_after_encode`, `text_encoder_fp8` — and the `estimate` breakdown carries
  three peaks, `peak_encode` / `peak_denoise` / `peak_decode`, in pipeline order. The Rust side
  matches those names exactly; do not rename one side alone.
- To change the memory gate or its advice, see `forecast_memory`, `_require_memory_headroom` and
  `_preset_advice`. Never add a second forecast: `estimate` and the gate must be the same
  arithmetic.
- To change the prompt phase, see `_prompt_embeds_locked` / `_encode_prompts_locked` /
  `_encode_prompt_phase`, and `_prompt_cache_key` for what invalidates a cached embedding.
- To change the prompt-cache LIBRARY, see the `prompt_cache_*` methods of the service
  (`prompt_cache_build` / `_list` / `_save` / `_load` / `_export` / `_import`) and, one layer
  below them, `prompt_cache_root` / `encoder_family_name` / `sanitize_name_component` /
  `prompt_cache_entry_path` / `list_prompt_cache_entries`. To change the FILE, see
  `prompt_file_metadata`, `read_prompt_file_header`, `validate_prompt_file_metadata` and
  `write_prompt_file`; bump `PROMPT_CACHE_VERSION` when an existing field changes meaning, and
  never widen `validate_prompt_file_metadata` into a "load it and see" path. To change what
  identifies an encoder, see `text_encoder_fingerprint` — and keep it metadata-only. To change
  what an ABSENT encoder makes possible, see `text_encoder_available` / `require_text_encoder` /
  `local_encoder_identity` and `find_prompt_cache_entry`; `text_encoder_path` inside a file's
  metadata stays informational and must never become a compatibility input — on another machine
  it points nowhere.
- To change the load ORDER or the warm-up, see `inpaint_image_bytes` (the sequence and the lease
  boundary — `mark_loaded` runs before the warm-up, because everything after
  `_ensure_pipeline_locked` fails with the pipeline already resident) and
  `_warmup_pipeline_locked` / `_require_components_materialized` / `_warmup_vae_decode`. The
  `phase:"load"` step numbers live in the `LOAD_STEP_*` constants and `LOAD_PHASE_STEPS`; they are
  the order the user sees, so they must stay strictly increasing along that sequence.
- To change the "no mask" mode, see `normalize_flux2_klein_params` / `_whole_region_overrides` (the
  params it settles) and `_require_solid_mask` (the check). `_generate_locked` needs no special
  case and must not grow one — the mode is expressed entirely as normalized parameters.
- To change where the FLUX.2 klein transformer config is looked for, see
  `transformer_config_roots` / `find_transformer_config_dir` / `validate_transformer_config_dir` in
  `flux2_klein.py`; the "not found" text is `_missing_transformer_config_message` and lists the
  probed directories through the same `component_probe_order`, so search and message stay in step.
- To change how a service reports load vs. inference failures to the model manager, change all six
  at once and extend `test_lease_protocol.py` — the protocol is a cross-service contract.
- To add a new inpaint backend, copy the service shape above, wire it in `server.py` + `AppState`,
  add an `ipc/handlers/` method and a `METHOD_INPAINT_*` constant in `ipc/protocol.py`, and route any
  GPU weight move through `runtime/rocm_mmap_transfer.py`.
