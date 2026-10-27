# Module: modules/ai_backend/inpaint

## Purpose
Inpainting and image-generation backends of the Python AI backend: seven services (LaMa V2,
LaMa MPE, AOT, SDXL, FLUX.1-Fill-dev, FLUX.2 klein, FLUX.3 Image Klein). Each takes an image (a
region for the two klein editors) + mask as PNG bytes and returns PNG bytes; they are constructed
in `server.py`, stored on `AppState`, and reached from Rust through the `inpaint.*` IPC methods
implemented in `ipc/handlers/`. FLUX.3 klein is a caption + references -> image generator
(`flux3_klein/`) that serves region editing through `flux3_klein/region.py` (the region is
reference #0; the RAW result is answered and the client composites it inside the mask).

## Architecture
Every service follows the same shape and is safe to copy from when adding another backend:

- construction is cheap; torch / diffusers / cv2 are imported lazily inside the methods that need
  them, so importing one inpainter never pulls in another one's dependencies;
- one resident model at a time, guarded by an `RLock`, keyed by a service-specific `model_key`
  (`lama_v2:<device>:<ckpt>`, `aot:<device>`, `sdxl:<mode>:<device>:<source>`, `flux_fill:<quant>`,
  …); the large-model services (FLUX.2 klein, FLUX.3 klein) are handles of the large-model
  manager instead (`runtime.large_models`), keyed by their spec;
- the key is leased from the shared `runtime.model_manager.SmallModelManager`
  (`begin_model_use` → `mark_loaded` / `mark_load_failed` → `release`), which is what lets an idle
  model be evicted by another service. **Load and inference are two separate `try` scopes**: see
  the lease-protocol section below. The small-pool services (LaMa V2, LaMa MPE, AOT, SDXL, FLUX
  Fill) pass `owner=` (a module constant `_LEASE_OWNER`) and `hint=` (the residency footprint
  hint from the very checkpoint files the load reads) to `begin_model_use`, so their bytes count in
  the shared budget (`runtime/residency/MODULE_README.md`);
- the target device comes from `General.ai_device` through the module-level binding
  `_resolve_selected_backend_device()` of `runtime.torch_device.resolve_selected_backend_device`
  (the one owner; each service's policy is the binding's arguments, and the allocator cache is
  released the same way through `_clear_torch_cache` -> `runtime.torch_device.clear_torch_cache`;
  `not-selected` resolves to a real runtime default). The large-model services (FLUX.2 klein,
  FLUX.3 klein) bind `runtime.torch_device.resolve_discrete_backend_device` instead, which honours
  the choice but never answers an integrated GPU. `flux_fill.py` ignores the user's device choice
  and pins itself to the discrete GPU via `_select_discrete_device()` — see the FLUX section;
- `health()` / `unload()` are part of every service's contract, and `unload()` reports the dropped
  key back to the model manager. `health()` never takes the service lock (which spans load AND
  inference here): it reads the service's `HealthSnapshot`, republished by `_publish_health_locked()`
  at every reported-state change (`runtime/MODULE_README.md`; pinned for AOT by
  `test_health_during_load.py`).

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
- `lama_mpe.py`: `LamaMpeInpaintService` — LaMa MPE (`inpaint.lama_mpe`). SHA-256-checks
  `inpainting_lama_mpe.ckpt` in `ManhwaStudio_AI_Models/Torch/LaMa_MPE` before every load and never
  downloads it: the Rust side (`ms_sysprobe::ai_models::ensure_lama_mpe`) owns the fetch, and a
  missing file is a `FileNotFoundError` naming it. Requires a `lama_modernised/` checkout in the
  program root (`_validate_runtime_layout_locked`). Tests: `test_lama_mpe.py`.
- `aot.py`: `AotInpaintService` — AOT-GAN (`inpaint.aot`), including the ported generator
  (`AOTGenerator` and its scaled-weight-standardized conv blocks). Weights:
  `ManhwaStudio_AI_Models/Torch/AOT/inpainting.ckpt`.
- `sdxl.py`: `SdxlInpaintService` — SDXL (`inpaint.sdxl`, streaming). See the SDXL section.
- `flux_fill.py`: `FluxFillInpaintService` — FLUX.1-Fill-dev (`inpaint.flux_fill`, `.unload`,
  `.status`, streaming). See the FLUX section.
- `flux2_klein/`: `Flux2KleinInpaintService` — FLUX.2 klein region editing
  (`inpaint.flux2_klein`, `.status`, `.unload`, `.component_action` and the
  `.prompt_cache.*` family, streaming) on the large-model manager. Serves BOTH variants (9B and
  4B) without knowing which:
  it is driven by the three user paths, and the large-model manager keeps one pipeline handle and
  one encoder handle per path set (`large_model.py`), so there is no variant field on the
  inference path — only
  `require_encoder_transformer_compatible`, which refuses a MIXED pair.
  A PACKAGE (`params`, `progress`, `conversion`, `components`, `prompt_cache`, `imaging`,
  `hardware`, `pipeline`, `large_model`, `memory`, `service`); `__init__.py` re-exports its public
  surface, so `from .inpaint.flux2_klein import Flux2KleinInpaintService` and every
  `flux2_klein.<name>` reference resolve against the package. See
  `flux2_klein/MODULE_README.md` for the layout and the FLUX.2 klein sections below for the
  contracts.
- `flux3_klein/`: `Flux3KleinService` — FLUX.3 Image Klein generation from the official weight
  package (bf16 / fp8r / nvfp4 or GGUF DiT, AE, bf16 or GGUF Qwen3-VL encoder), with a `PhaseObserver` per phase;
  `AppState.flux3_klein` (`inpaint.flux3_klein*`, the `large_models.profiles` adapter
  `flux3_klein`); the region editor `region.py` and its wire `wire.py`. Also driven by the
  measurement tool `tools/large_model_calibration/`. See the FLUX.3 klein section below and
  `flux3_klein/MODULE_README.md`.
- `region_imaging.py`: the region editors' wire codecs (FLUX.2 klein, FLUX.3 klein): strict L8 mask
  and RGB decode, the solid-mask check, the RGB PNG answer. Tests: `test_region_imaging.py`.
- `test_lease_hints.py`: the owner and the file-size hint each small-pool service hands
  `begin_model_use` (reuses `test_lease_protocol.py`'s case builders, temp checkpoint files).
- `test_health_during_load.py`: `health()` answers while AOT's lock section loads and infers, and
  reflects loading -> ready and a load error (stubbed load and inference, no torch).
- `test_lama_decode.py`: LaMa's cv2 decode failure is logged and falls back to PIL (stub cv2).
- `test_sdxl.py`, `test_flux_fill.py`, `test_lease_protocol.py`
  (`flux2_klein/`'s and `flux3_klein/`'s tests run real torch on the CPU, see their readmes):
  pure-Python
  unit tests (no torch, no diffusers, no weights, no GPU — fake `torch`/`diffusers`/`transformers`
  modules are injected into `sys.modules`, and `test_lease_protocol.py` stubs each service's load and
  inference step instead).

## Contracts and invariants

### The lease protocol: a failed inference is not a failed load
Every small-pool service takes the lease BEFORE `self._lock` (FLUX.2 klein is a large-model manager
handle instead: its sessions, see the FLUX.2 klein section) (`begin_model_use` can block while another
thread's eviction callback waits for this service's lock; the reverse order deadlocks), and then run
two distinct `try` scopes inside it:

- **load scope** — `_ensure_*_locked(...)` only. A failure here, and only here, reports
  `lease.mark_load_failed()`.
- **run scope** — everything after `lease.mark_loaded(...)`, which is called as soon as the load
  returned, still under `self._lock` and UNCONDITIONALLY (never gated on `lease.needs_load`): a
  lease that found the key resident can see the model switched or unloaded by another request
  before it reaches the lock, and the reload that follows must be registered too
  (`runtime/MODULE_README.md`, STATE S-058). A failure here records `_last_error` and re-raises
  without touching the lease.

`mark_load_failed()` reaches `SmallModelManager.abort_load`, which clears the entry's `resident`
flag and drops its unload callback. Calling it after the load already succeeded — while the service
still holds the model in `_net`/`_model`/`_pipe` with `_active_key` set and its weights still occupy
VRAM — makes the manager under-count residency and makes that model permanently non-evictable until
the same key is used again. Wrapping load and inference in one `try` is therefore a defect, not a
style choice; `test_lease_protocol.py` pins it for all seven services at once, together with the
stale-lease reload (`StaleLeaseReloadTests`).

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
- **Never downloads; every load is `local_files_only=True`.** `resolve_model_source()` turns the
  user's `model_path` into an `SdxlModelSource` before any load: a ckpt/safetensors file loads via
  `from_single_file(path, config=sdxl_config_dir)` — the pinned diffusers config repo Rust
  downloaded for the mode, checked to hold `model_index.json` first, because a `config` that is not
  an existing directory makes diffusers fetch the configs from the Hub even with
  `local_files_only=True`; a local diffusers folder loads via `from_pretrained`; the one curated id
  `CURATED_INPAINT_REPO_ID` loads from Rust's download (`sdxl_model_dir`) with `variant="fp16"`
  (the variant is decided by that id, never by sniffing file names). Any other repo id is a
  `ValueError`. The resolved source (path, config dir, variant) is part of the lease key.
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
  repo under `components/`. Downloads go through `../engines/model_download.download_bearer_to_path`:
  serialized per destination file, staged into a process-private
  `<name>.<pid>.part`, retried with backoff and resumed with `Range` on transient failures (bytes of
  a failed call are parked for the next one), checked against the size the hub listing announced,
  and published with an atomic `os.replace`. The listing is mandatory: `_build_download_plan`
  raises when a listing fails or the quant is missing from it, so no file is ever sized `0` by
  accident; a file on disk at another size is re-downloaded. The resolved plan is cached per quant
  (`flux1-fill-dev-<quant>.listing.json` in the GGUF directory, destinations relative to it) and the
  listing is fetched only when the cache is absent or a planned file is missing, so a complete model
  runs offline. Without a cache (a model from an older build) a failed listing is tolerated, with a
  WARNING, only when the quant and `_components_present()` are on disk: every published file passed
  its gate. A malformed cache, or one whose destination leaves the directory, is ignored. `ensure_model()` deliberately runs
  outside `self._lock` and the IPC layer dispatches onto a thread pool, so two first uses of the
  same quant do reach it at once; the loser of each file's lock re-checks and skips instead of
  refetching. `_download_file_streaming` is a thin call into the shared helper, which takes the
  token as an argument, so this module keeps only the DECISION that its token comes from
  `HF_TOKEN` / `HUGGING_FACE_HUB_TOKEN` in the process environment.
- Cancellation: `inpaint_image_bytes` / `ensure_model` take `should_cancel` (the handler passes
  `cancel_event.is_set`); the helper polls it per chunk and per 100 ms of a retry wait and raises
  `DownloadCancelled`. Generation itself is not interruptible.
- `progress_callback(phase, step, total, label)` has two phases: `download` (byte-level) and
  `generate` (step-level). `ipc/handlers/flux_fill.py` streams both as `progress` frames with header
  `phase`/`step`/`total`/`label` and no preview blob. A retry wait is an ordinary `download` frame
  whose `label` is `engines.model_download.retry_progress_label` — no extra field. A failing
  progress callback is logged once per call and never aborts the download or the generation.
- The pipeline is pinned to the DISCRETE GPU; `_select_discrete_device()` delegates to
  `runtime.torch_device.resolve_discrete_backend_device(..., honour_configured=False)` (the one owner
  of the iGPU rule: `is_integrated`, name markers, exact AMD iGPU arch set), takes the discrete
  device with the most memory, makes it current, and falls back to CPU rather than use an iGPU.
  `General.ai_device` is ignored here by design.

### FLUX.2 klein
- **The backend answers the RAW decode; the client post-processes it.** The painted mask is a
  PERMISSION TO CHANGE: every pixel outside it must end up byte-identical. Since protocol 17 that
  is the Rust host's job (`crates/ms-region-postprocess`: the colour match, the inward-feathered
  composite, the latent-mask dilation), so a settings change re-composites without a new run. The
  wire mask of `inpaint.flux2_klein` is therefore the LATENT mask, already dilated by the client
  (`mask_dilate_px` there, 0 under `whole_region`); the service uses it unchanged as the pipeline's
  `mask_image` and as the `text_attention_in_mask` target, and answers `decoded.convert("RGB")`.
  Outside the mask that answer is a VAE round trip of the region, NOT byte-identical to it. The
  former post fields (`mask_dilate_px`, `mask_feather_px`, `color_match`, `color_match_local_pct`,
  `color_match_scale_px`) are refused by name (`params.RETIRED_FIELDS`).
- **`whole_region` is the "no mask" mode, and the flag is VERIFIED, not trusted.** The client still
  sends a mask — a solid one — so the request format does not fork, and `region_imaging.require_solid_mask`
  refuses a mask with any empty pixel, naming the count. A flag that disagrees with the data would
  otherwise surface as a partial edit while the UI said no mask was needed. The mode settles one
  other param in `normalize_flux2_klein_params` (`_whole_region_overrides`, which logs whenever it
  overrides something the caller asked for): `text_attention_in_mask` → `False`: every token is
  inside a solid mask, so the attention mask would block nothing and still cost its `S²` bytes
  (see the next bullet). What the mode means for the post-processing is the client's rule.
- **`text_attention_in_mask` confines the PROMPT to the mask** (opt-in, default `False`;
  `flux2_klein/attention.py`). Why it exists: the pipeline always conditions on a clean copy of
  the WHOLE region, so an instruction can be grounded on a matching object OUTSIDE the mask; the
  model edits it there, the per-step latent blend and the client's composite throw that edit
  away, and the user sees nothing change under the mask. The contract:
  - ONE additive `(S, S)` mask per run over diffusers 0.39's joint sequence `[text L | noisy N |
    region clean copy N | reference R]` (verified in the installed pipeline and transformer),
    passed as the pipeline's `attention_kwargs={"attention_mask": M}`, which diffusers forwards to
    every double- and single-stream attention of the conditional AND the unconditional pass.
  - "Inside" is the 16 px token grid of the latent (client-dilated) mask the pipeline receives as `mask_image`,
    with a token inside when ANY of its pixels is set — one token wider than the pipeline's own
    bilinear latent mask, so the text reaches the partially masked band too.
  - Blocked: image queries outside the mask -> text keys, and text queries -> outside image
    keys, for the noisy tokens, the clean copy AND a region-aligned reference. The marks
    reference is a copy of the region with marks drawn on it; when its token grid equals the
    region's it follows the same inside/outside grid, because open outside reference tokens
    would hand the prompt a second, spatially aligned path back to the outside object. A
    reference of another size (no alignment can be assumed) stays fully open to the text, with
    one log line per run; the service only accepts region-sized references, so a run always
    takes the aligned branch. Open: text <-> text, text <-> inside tokens, every image <-> image
    pair (context and harmonization are untouched). Every row keeps an open key; the blocked value is
    `finfo(dtype).min`, not `-inf`, so a fully blocked row could never turn into `NaN`.
  - `L` is read from the real `prompt_embeds`; all `L` positions count, padding included (the
    pipeline never hands the tokenizer's mask to the transformer, and padding states carry prompt
    content). `R` is derived from the reference's own size (`condition_tokens`), never assumed.
    A negative embedding of another length, or a mask with no inside token, is a `ValueError` —
    never a silently unmasked or everything-blocked run. The empty mask is refused BEFORE the
    lease, load and prompt encode (`attention.require_text_attention_target` on the wire mask,
    which is the latent mask itself); the planner's own refusal stays as a last-resort contract check.
  - The mask lives for the denoise only. diffusers keeps `pipe._attention_kwargs` after the call,
    so `service._denoise` clears it (and its own reference) in a `finally` around the pipeline
    call: the mask never survives into the VAE decode (whose forecast excludes it), a failed
    denoise, or an idle resident pipeline.
  - The mask is in the transformer's dtype (SDPA wants an additive mask to match the queries),
    on the transformer's compute device (where `prompt_embeds` go), and is a `(S, S)` VIEW of a
    buffer whose rows are padded to a multiple of 8: SDPA's memory-efficient kernel otherwise
    copies an unaligned mask on EVERY attention call (measured: 281 MB vs 136 MB extra peak at
    S = 8706). A mask also rules out the flash kernel; attention runs ~1.5x slower.
  - The layout is pinned to diffusers, not to our own assumption, by
    `flux2_klein/test_attention.py::RealTransformerLayoutTests` (a tiny real
    `Flux2Transformer2DModel` on the CPU): SDPA's shape check catches only a sequence-LENGTH
    change, never a reorder at the same `S`.
  - Not removable: second-order leakage through image <-> image attention in later layers
    (outside tokens read inside tokens that read the text). Its pixels are discarded by the
    latent blend and the composite anyway.
  - The effective value is returned as the top-level `text_attention_in_mask`, deliberately NOT
    in `applied`: the client persists `applied` into the user's settings, and a `whole_region`
    run must not switch the user's choice off for good.
- **A marks reference is a CONDITION image, never an edit target.** The optional third blob segment
  (`reference_len`, PROTOCOL.md §5.4) is the user's marks composited over a copy of the region. The
  service decodes it to RGB, refuses any size other than the region's (only then do diffusers'
  identical reference/region preprocessing rules — the 1 MP cap and the floor to 16 — keep it
  pixel-aligned with the region, and for a valid region both are no-ops), and passes it as
  `image_reference`: ONE PIL image, because diffusers treats a list as a batch sized from its first
  element. The pipeline already conditions on the region itself, so the reference is the second
  condition image. It never enters the client's composite or colour match; without one the
  pipeline call carries no `image_reference` key at all. The memory guard counts its condition
  tokens in the denoise (the spec's denoise work: the region's token count again), and so does
  `large_models.profiles` when the request's `shape.references` names it.
- **The latent-mask dilation gives a thin mask a full latent cell** (the pipeline bilinearly
  downsamples the mask to a 16 px grid); it does not control the seam, which is the client's
  composite. What no composite can fix is that a 4-step distilled denoise returns the masked area
  ~18% less textured than the original (Laplacian variance, ring 8-24 px inside the mask); that is
  the model.
- **The mask is accepted only as L8.** `region_imaging.decode_mask` checks `img.mode == "L"` and raises on
  anything else. An RGB/RGBA mask used to be converted by taking the alpha or the per-channel
  maximum, which silently promoted stray colored pixels to "edit this" — a client bug turning
  into an edit of the wrong area. There is no conversion path; the wire contract is the check.
- **The transformer path is one contract for both input shapes.** A single `.safetensors` FILE
  (BFL or diffusers layout) or a diffusers FOLDER (its shards, `component_safetensors_shards`) are
  inventoried and loaded by the large-model manager the same way; `large_model.require_loadable`
  refuses an fp8_scaled checkpoint (header, sharded folders included) and a
  `model.diffusion_model.`-prefixed one BEFORE anything is admitted, and the skeleton is built from
  the checkpoint's own config with `guidance_embeds=False` forced (klein has no `guidance_in`).
- **A single-file transformer REQUIRES its own `config.json` on disk, and there is no substitute
  for it.** diffusers 0.39 recognizes every FLUX.2 checkpoint as `flux-2-dev`
  (`single_file_utils.py`, `CHECKPOINT_KEY_NAMES["flux2"]`) and, given no `config`, resolves the
  gated `black-forest-labs/FLUX.2-dev` repo over the network — the config of a DIFFERENT model.
  `large_model.transformer_config_dir` therefore searches `<checkpoint dir>/transformer`, `<checkpoint dir>`,
  `<parent>/transformer`, `<parent>` (`find_transformer_config_dir`, order shared with the error
  message through `component_probe_order`); the skeleton is built from the
  directory it finds (`local_files_only=True`); when it finds none the run is refused naming the
  exact file to provide. Two
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
- **Every component is read ONE TENSOR AT A TIME, straight into its tier.** The large-model
  manager's loader (`runtime.large_models.loader`, over the one filler
  `runtime.safetensors_stream.fill_module_from_safetensors`) builds each component on `meta` and
  fills it per its plan: device units on the device, parked units in packed host buffers, disk units
  not at all; the host peak is one tensor. diffusers' `from_single_file` / `from_pretrained` are not
  used any more (the old streaming loader and its eligibility rule are gone). The BFL -> diffusers
  key rule is `conversion.py` (diffusers' own per-key converter), applied per tensor. Every
  component read from disk reports its bytes as the wire's SECOND progress level (`file_step` /
  `file_total` = the whole component, `file_label` = the file name, or the folder name for a
  diffusers / transformers folder, `file_component` = `progress.FILE_COMPONENT_*`) on the CURRENT
  step frame: a load rides on its own step (the transformer on `LOAD_STEP_TRANSFORMER`, the text
  encoder — run, cache build, «load» action — on `LOAD_STEP_TEXT_ENCODER`); a read the manager
  makes inside a phase (a DISK-tier component, a degrade retry's reload, a re-read of DISK /
  DROPPED units) rides on the step current then — the encode step, or a `generate` frame during
  the denoise. The VAE reports no bytes.
- **The text encoder and the transformer must be the SAME variant, and that is checked before any
  weight is read** (`require_encoder_transformer_compatible`, `components.py`). The pipeline
  concatenates the encoder's hidden states at layers `(9, 18, 27)`
  (`TEXT_ENCODER_OUT_LAYER_INDICES`) and hands the result to
  `context_embedder = nn.Linear(joint_attention_dim, …)`, so the contract is
  `TEXT_ENCODER_OUT_LAYERS * hidden_size == joint_attention_dim`: 3·4096 = 12288 for klein 9B,
  3·2560 = 7680 for klein 4B. Those indices are a **diffusers default we do not own** — the sibling
  `Flux2Pipeline` uses `(10, 20, 30)` — so `_encode_prompt_phase` passes them EXPLICITLY as
  `text_encoder_out_layers` instead of inheriting the default, and `TEXT_ENCODER_OUT_LAYERS` is
  derived from the tuple rather than written out a second time. Nothing else compares the two — a 4B transformer beside a 9B encoder
  passes every component check, passes the memory guard, and dies as a bare torch matmul error
  inside the denoise, ~34 GB into the read. The guard reads the two `config.json` files only and
  fires from `inpaint_image_bytes` and from the transformer/VAE `load` action, before any session is
  admitted. It is deliberately SILENT when it cannot PROVE a mismatch — an empty
  path, a component not on disk, a config without the field — because each of those is a
  different fault with a better diagnosis further down; adding a "missing config" error here
  would replace those messages with a worse one.
- **Cancellation happens at call boundaries only.** The handler checks `cancel_event` before it
  calls the service and after the service returns; a running diffusion step is not interrupted.
  This is the shared contract of all six wired inpaint services (`ipc/handlers/sdxl.py`,
  `ipc/handlers/flux_fill.py` do exactly the same) — do not make this one the exception.
- **The service itself downloads nothing.** The user supplies three paths (Qwen3 text encoder,
  transformer, VAE); the pipeline needs five components, so the tokenizer and the scheduler are
  DISCOVERED next to those paths (`component_search_roots` / `discover_component_dir`). A component
  that is not found raises an error naming what to put where — there is deliberately no built-in
  scheduler config, because an invented one produces plausible-looking garbage. Putting those files
  on disk in the first place is the downloader's job — the app's Rust model catalog
  (`ms_sysprobe::ai_models`) fetches every FLUX.2 klein file, 4B and 9B, every format; the backend
  downloads none — and the two never meet: the downloader knows nothing about the pipeline and the
  service knows nothing about Hugging Face.
- **The transformer and the text encoder may each be a GGUF file** (protocol 21; the unsloth
  FLUX.2 klein 4B / 9B quants and Qwen3 4B / 8B GGUF encoders, all from the Rust catalog;
  `../runtime/qwen3_gguf.py` bridges the llama.cpp tensor names to HF Qwen3, the GGUF weights
  stay quantized in memory and are dequantized per forward by `runtime.gguf_layers`). A GGUF file
  carries no `config.json`, tokenizer or scheduler: its config is probed beside the file first,
  then in EVERY installed klein package (`components.installed_klein_package_roots`, the
  `KLEIN_PACKAGE_DIR_NAMES` folders under `<side_models>`, 4B then 9B), and a candidate is
  accepted only when its config matches the file's header — a package only under the STRICT match
  (it must declare every compared field and agree on all of them), which is what tells a 9B file
  from a 4B one; the package roots also join the tokenizer / scheduler search, but only while a
  component is a GGUF file, so bf16 discovery is unchanged. A GGUF component with no matching
  config beside it or in a package is refused by name.
  `text_encoder_fp8: true` with a GGUF encoder is refused naming both fields. The per-component
  weight FORMATS (`transformer_format(s)` / `text_encoder_format(s)`, `[{id, path}]` candidates the
  client sends) give one format row each; `large_models.profiles` answers the encoder's rows as the
  additive `encoder_formats` beside the pipeline's `formats`. Detail: `flux2_klein/MODULE_README.md`.
- **Every text-encoder config load goes through `components.text_encoder_config_kwargs`** (the layer
  truncation plus the RoPE normalization `text_encoder_rope_kwargs`): a transformers-v5 config that
  keeps its RoPE base only in `rope_parameters` (the uncensored 9B encoder's) gets the base as a
  `rope_theta` kwarg, because the pinned transformers reads only the top-level field and would
  silently use its default; such an encoder's prompt-cache fingerprint is salted with that base
  (`prompt_cache.py`), and the v1 migration deletes entries recorded under the unsalted identity.
- **A weights FILE may stand in for its component folder** (`component_dir_for_path`). Neither
  `AutoencoderKLFlux2` nor the Qwen3 encoder has a single-file loader in diffusers 0.39, and a user
  picking a model naturally selects `diffusion_pytorch_model.safetensors` rather than the directory
  above it — which produced the loader's own "compatible with StableCascadeUNet, ..." class list as
  a user-facing error. A file whose directory holds a `config.json` is therefore normalized to that
  directory; a file without that sibling is left alone so the caller still diagnoses it. This is a
  path normalization, not a fallback: the folder containing the file IS the component, and its
  `config.json` describes that very file. The transformer is exempt — the manager reads a single
  file directly, so a file path there means a file.
- **The region is validated, never fixed up** (`validate_region_size`): sides a multiple of 16 and
  at least 128 px, area at most 1 MP, aspect ratio at most 8:1. The pipeline would otherwise resize
  above 1 MP and crop to a multiple of 16, returning a window of a different size than the mask.
- **A run is TWO HANDLES of the large-model manager, admitted in this ORDER** (a memory contract):
  the PIPELINE handle (`transformer` + `vae`, phases `denoise` / `decode`) is admitted, loaded and
  placed into its denoise state FIRST; the TEXT ENCODER handle (Qwen3, phase `encode`) is admitted
  second, so it plans on what the pipeline's reservation left — on the GPU when the step pair and
  the free device memory allow it beside the pipeline, else juggled, else on the CPU from RAM (the
  encoder alternatives of `tiers.template_for`) — or, under «Энкодер на CPU» (`encoder_on_cpu`,
  default `true`: `EncoderOptions.on_cpu`, the encoder spec's host pin), always on the CPU from
  RAM, kept / released / re-read by the RAM step alone. It is admitted under a device ceiling
  (`device_cap_bytes`): the combined device peak of the nearest fitting HIGHER «Видеопамять» step
  minus this step's pipeline weights, so a lower step never forecasts or uses more device memory
  than a higher one for the two handles together (without it, a step whose pipeline reserves less
  would hand the encoder more room; `flux2_klein/memory.py` module notes). It encodes what the prompt cache misses into a few
  MB of embeddings and goes to its between-runs state; then the denoise and the decode run as
  manager phases. Sessions are acquired under the service lock, pipeline before encoder — the
  stated exception of the manager's "acquire before the service lock" rule for a multi-handle
  service with no small-pool lease (why it is deadlock-free: the `service.py` header; pinned by
  `flux2_klein/test_service.py::LockOrderTests`). The encoder being its own handle is what lets a prompt-cache build touch
  the encoder alone and a run with a cached prompt need no encoder installed.
- **The warm-up belongs to a COLD load.** When the pipeline handle held no byte at the start of the
  run (first run, after an unload or an eviction, after steps that drop everything between runs), steps 1-6
  are reported (transformer, tokenizer, VAE, scheduler, placement = the transition into the denoise
  state, warm-up = one 64x64 VAE decode that primes the allocator and MIOpen). A hot run reports
  steps 0 and 9 only; whatever the manager parked between runs is restored when the denoise phase
  starts. The manager's loader and moves are synchronous, so "placed" is "materialized".
- **Denoise and decode are two manager phases.** The pipeline is called with `output_type="latent"`
  and the latents are kept on the host, so a decode retry never repeats the denoise. A memory failure
  in a phase is the manager's (`Session.run_phase`): rung 0 reclaims idle holdings of other pools,
  then the degrade ladder re-plans. **The ladder's own order: (1) the tiling
  fallback (`large_model.FALLBACK_VAE_TILING`; tiling engages only above the VAE's threshold, 1024
  output px on the shipped VAE), (2) the lowered tile (`FALLBACK_VAE_TILE_LOWERED`: both thresholds
  together to 64 latent cells / 512 px for that decode, restored afterwards), (3) the next «Видеопамять»
  step (same RAM step), whose decode keeps the transformer off the device** — so the transformer
  is parked only after both tiling rungs failed. At most `degrade.MAX_RETRIES` (3) retries per run (rung 0 included); then the
  run fails with `LargeModelRunFailed`. `applied.vae_tiling` claims tiling only where it engaged;
  thresholds never appear in `applied`, which is `{vae_tiling}` alone; whether the transformer
  was off the device during the decode is the effective step pair (`vram_step_effective`).
  - The decode owns `torch.no_grad()` (it is not inside `pipeline.__call__`) and decodes on the
    VAE's own device; `_require_execution_device` checks the denoise call computes where the
    transformer was placed.
- **The prompt-embedding cache is always on in memory** (`runtime/prompt_cache_service.EmbeddingMemory`,
  LRU, `PROMPT_EMBED_CACHE_ENTRIES`, no byte cap; a 9B entry is ~12.6 MB). Its key is (library, text,
  `max_sequence_length`, encoder fingerprint, precision id, origin file or `encoded`); its
  provenance (`MemoryEmbedding`) sits in a copy-on-write index the lock-free plan reads. Not keyed by
  where the encoder ran. Entries live on the HOST and `unload()` keeps them.
- **The prompt LIBRARY (protocol 23)** is the model-agnostic `runtime/prompt_library.py`, one library
  per MODEL: `<program root>/prompt_cache/<library id>/` with `params.prompt_library` =
  `flux2_klein_9b` / `flux2_klein_4b` (Rust `Flux2Variant::prompt_library_id`, directory names,
  frozen). Any encoder variant of the model may serve its entries. The ORCHESTRATION (memory, the
  plan, `.status` keys, build / list / export / import) is the model-agnostic
  `runtime/prompt_cache_service.py` (`PromptCacheOrchestrator`); FLUX.2 plugs in through
  `prompt_cache.Flux2PromptCacheAdapter` (its texts, preference, width filter, encoder identity,
  v1 migration and v1 import) and keeps its encoder session, progress steps, answer keys and
  refusals in `service.py`. FLUX.3 klein is the second model: one library `flux3_klein` (every
  encoder format), TEXT encoder mode only, through `flux3_klein/prompt_cache.Flux3PromptCacheAdapter`
  (no migration, no legacy import; the memory is also byte-capped); its rules are
  `flux3_klein/MODULE_README.md` "Prompt cache".
  - **One owner of "is this text served": `service._prompt_plan`** (`PromptCacheOrchestrator.plan`).
    Per text the run needs (the prompt, plus `""` under effective classifier-free guidance —
    `Flux2PromptCacheAdapter.texts_needed`) the candidates are
    memory and library entries with the EXACT text (the backend-normalized `strip()`ed text, case and
    inner whitespace significant) and length, minus every candidate whose width the transformer does
    not accept (`joint_attention_dim`, `prompt_cache.transformer_text_width`), ranked by
    `prompt_library.select_entry`: the request's `text_encoder_source` first, then its
    `prompt_cache_precision_order` (Rust-owned; `fp8` = an fp8-quantized encode), memory before a
    file, newer, file name. The run (planned once under the lock, after the migration),
    `.status` (`prompt_cached` = every needed text served, `prompt_cache_entry` = the prompt's
    choice as `EntryInfo`), `_encoder_need` and the forecast all derive from it, so status and run
    cannot diverge (the guidance `""` text included).
  - **The run reads a chosen file lazily** (`PromptCacheOrchestrator.serve_locked`: «Промпт взят из кэша»; the tensor then
    stays in memory). A file that vanished or changed since the plan (`LibraryEntryGone`) re-plans
    once; a text still unserved is encoded when an encoder is available, else the run is refused
    naming the file.
  - **The text encoder is OPTIONAL when every text is served.** `text_encoder_path` is the ONE path
    `normalize_flux2_klein_params` does not require; an empty path and one not on disk mean the same
    (`text_encoder_available`). The absence is an error only where an encode is unavoidable
    (`_encode_prompts_locked`, plus early copies in `inpaint_image_bytes` and `prompt_cache_build`,
    before 18 GB of transformer are read). `status.available` is not false over a missing encoder
    when `prompt_cached`; the Qwen tokenizer stays required (the pipeline is built with one). An
    encoder that EXISTS but cannot be fingerprinted is a hard error.
  - **When the forecast counts the encoder** has one owner, `service._encoder_need`
    (`EncoderNeed.counted`): left out ONLY when its handle holds no byte AND the plan serves every
    text; neither reading waits for the service lock.
  - **Presets.** A preset is the library's entries of one exact text; its NAME lives in every
    entry's metadata (display name = newest non-empty). `.prompt_cache.build {params, name}`
    («Кэшировать») writes, for every text the run needs, an entry of the CURRENT encoder (same
    fingerprint and fp8) unless one exists, reusing an embedding this process encoded with that
    encoder, else encoding through the encoder handle alone (no pipeline; the encoder is released
    afterwards unless it was resident already). An empty `name` keeps the preset's name (a new
    preset gets the first ~40 characters of the text); a different name RENAMES the preset — a
    torch-free header rewrite of all its entries, no encode (there is no separate rename method).
    It needs the encoder (an entry must name the encoder that made it). Generation encodes stay
    memory-only.
  - **`.list`** (lenient: no transformer / VAE needed) answers the presets newest first, `""` never
    listed, each with its entries at the request's length ranked by the same rule and the first as
    `selected`; every `EntryInfo` carries `current_encoder`, `encoder_source`, `encoder_format`,
    `encoder_fp8`, so the client decides «Кэшировать» (disabled when a more precise entry of the same
    source or an entry of the current encoder exists) without asking again. Also `skipped` files and
    the `legacy` migration report.
  - **`.export {params, file, path}`** copies one entry atomically; **`.import {params, path}`**
    files a v2 file under ITS OWN library (`library_matches` says whether it is the requested one;
    a non-FLUX.2 library is refused) and converts a v1 file by the migration rules (a pre-fix or
    unprovable one is refused by name). Writes are deterministic (`entry_file_name`), so build,
    import and migration are idempotent (`already_present`).
  - **Container v2** (`manhwastudio.prompt_cache`, version 2): one `prompt_embeds` bf16 tensor
    (`text_encoder_dtype_name`) and `__metadata__` = library, name, prompt, length, dtype, encoder
    `source` / `format` / `fp8` / `id` / `label`, `created_at`, `origin`
    (`built` / `imported` / `migrated_v1` / `migrated_v1_unverified`), plus informational
    `text_encoder_path` / `migrated_from`. The file is the truth; `index.json` is a rebuildable
    cache; a corrupt or foreign file is `skipped`, never fatal and never deleted.
  - **Encoder identity** (`prompt_cache.text_encoder_fingerprint`): SHA-256 of `config.json` plus
    the sorted `(file name, size)` of the weight files (a GGUF file: config + `gguf` marker + name +
    size), salted with the RoPE base when the loader normalizes it — metadata only, never a hash of
    the weights. `text_encoder_path` in a file is informational, never a compatibility input.
  - **v1 migration** (`prompt_cache.migrate_legacy_families`, once per process, triggered by
    `.list` / `.build` / `.import` / a run that names a library — never by `.status` or the
    forecast). v1 family directories (`<name>-<8 hex>`) are classified against every locally
    findable encoder (the installed klein packages' `text_encoder*` folders and GGUF files, the
    request's encoder, the recorded paths), salted AND unsalted: an entry whose id is the UNSALTED
    identity of a RoPE-normalized encoder (made before the fix) and an unmatched entry of an
    uncensored encoder (post-fix unprovable) are DELETED; the others become v2 entries of the
    library their tensor width names (`LEGACY_LIBRARY_BY_WIDTH`), each written and re-verified
    before its v1 file is deleted; an emptied family directory is removed; a float16, unknown-width
    or unreadable file stays where it is (`not_migrated`). **Containment:** the root, every family
    and every file must be a real directory / regular file strictly inside the root
    (`prompt_library.require_confined`, `lstat`, no symbolic link or reparse point); a linked root
    migrates nothing, a linked family or file is reported in `not_migrated` and never followed —
    the same rule guards every library rewrite, replace and the export source. **Durability:** the
    v2 publish fsyncs its directory, the library directory and the root are fsynced again, and only
    then is the v1 file unlinked and its family directory fsynced (`prompt_library.sync_directory`,
    a no-op off POSIX like `ms_atomic_file::sync_directory`); a failed fsync keeps the v1 file.
- **Resources are the large-model manager's** (`runtime/large_models/MODULE_README.md`): where every
  byte of both handles lives in every phase, the forecast, the admission guard (no host OOM, device
  never above `total - reserve`), the degrade ladder and the between-runs state. FLUX.2 contributes
  only its specs (`large_model.py`). The calibration tables measure the `flux2_klein` phases per
  device class (`../runtime/large_models/calibration/`, fitted by `tools/large_model_calibration/`),
  so those forecasts are `calibrated: true` and their activations bind the admission. A phase
  without a measured entry (a borrowed table of an unknown device class) is `calibrated: false`:
  its activations choose the plan, the admission is checked on the exact weight bytes and strictly
  on the host side, and a device OOM is the ladder's (`planner` module contract).
- **The request's step pair (`memory.requested_steps`)** is `params.vram_step` /
  `params.ram_step` (`tiers.VramStep` / `RamStep` ids, `auto` included); an absent or `null` axis
  is «Авто». The retired placement wire (`placement`, `profile`, `low_cpu_mem_usage`,
  `vae_slicing`, `unload_transformer_before_vae`, `unload_text_encoder_after_encode`) is REFUSED
  by name (`params.RETIRED_FIELDS`), never mapped: the migration of a client's saved settings is
  the client's (Rust `legacy_memory_steps`, which owns that table). The text encoder computes
  wherever the plan of the pair puts it — on the GPU under MAX / PHASE when it fits beside the
  pipeline, else on the CPU from RAM — unless `encoder_on_cpu` pins it to the CPU (`params.encoder_on_cpu`,
  the one owner of its default); there is one encoder spec per precision
  (`flux2_klein_text_encoder` / `.fp8`). `vae_tiling` is the decode's request; `text_encoder_fp8`
  selects the fp8 encoder spec. A run's answer carries the four step keys
  (`memory.STEP_FIELD_KEYS`: `vram_step`, `ram_step` requested, `vram_step_effective`,
  `ram_step_effective` what ran), `degraded`, `degrade_reason` at the top level and `applied` =
  `{vae_tiling}`. Forecasts are `large_models.profiles`, whose adapter is
  `large_model_name(params)` (the dtype's registered pipeline model) plus
  `large_model_inputs(params)` (options, device) — the single-handle fallback — and
  `large_model_profiles(params, shape, format=, steps=)`, which the handler prefers: the pipeline
  planned alone, the encoder planned with that plan's reservation claimed, then the pipeline
  planned again with the encoder's reservation claimed (`LargeModelManager.profiles(claimed_device_bytes=,
  claimed_host_bytes=)`); the answer is that report (`selected` = the PIPELINE's plan, the encoder
  only claimed in it; the client sums `selected` + `encoder`, minus the one `forecast.host_baseline`
  both carry, `service._with_host_baseline`) plus `encoder` (the encoder's
  `selected`, `null` when the encoder is not needed AND not resident, `EncoderNeed.counted`), and an
  encoder that does not fit refuses `selected`. The reservation of a plan is its device peak and its host peak above the baseline
  RSS (`service._reservation_of`). `memory.forecast_memory` is the same two-handle admission as
  one forecast dict (pipeline first, encoder on what is left, under the combined ceiling); its
  consumers are the calibration tool (`tools/large_model_calibration/flux2.py`) and the golden
  test.
- **A host run device (`cpu` / `mps`)** is planned on the host alone, exactly as the manager plans
  it (`Budgets.host_device`; any non-`cuda*` device, `memory.is_host_run_device`): no device
  budget, every byte in RAM, the RAM step alone picks the plan. `memory.forecast_memory` then
  answers `vram_bytes: 0`, `ram_bytes` = the host growth and `fits` = the host plan's, so it agrees
  with what a run's admission will do (the unknown-VRAM-fits rule is a CUDA-only rule).
- **A failed COLD pipeline load drops only its own handle** (`LargeModelManager.unload_key` of the
  session's key): a warm handle of another checkpoint or device under the same model name keeps
  its bytes; an admission refusal left nothing to drop.
- **A progress callback that raises aborts the run**: `_denoise`'s per-step callback lets the
  exception propagate out of the pipeline call, so the session finishes (reservation, run lock and
  lease returned) and the caller gets the error. Whether a dead peer is tolerated is the caller's
  policy: the IPC handler's forwarder never raises (it logs the first failed frame of a request at
  WARNING and keeps going), so **a client disconnect does not cancel a FLUX.2 run**; mid-run
  cancellation is a separate, future feature.
- **The encoder's between-runs state follows the RAM step**: it stays as the session's plan keeps
  it (on the GPU under «Максимум скорости» when it ran there, in RAM under «Держать модели в
  памяти», dropped otherwise). The only explicit release is the prompt-cache build's own
  (`_encode_prompts_locked(release=True)`).
- **`text_encoder_fp8`** (default `False`) is weight-only fp8 for the encoder's decoder-layer
  linears (`large_model.encoder_spec(True)`): the conversion quantizes each weight to
  `float8_e4m3fn` with per-output-row scales WHILE it is read (so the load peak is one bf16 tensor,
  and the resident encoder is ~half), dequantized to bfloat16 in `forward`; measured relative max
  error ~3.4% per tensor. A torch without `float8_e4m3fn` is refused, never ignored. The embedding
  and the norms stay bfloat16.
- The Qwen3 encoder is built as a truncated `Qwen3Model` (`components.ENCODER_KEEP_LAYERS` layers,
  `lm_head` and deeper layers never built nor read); its checkpoint keys lose their `model.`
  prefix in the spec.
- **Models on disk (local check, 2026-10-16):** the klein 9B checkpoint (`flux-2-klein-9b`) and the
  uncensored 9B encoder; the 4B path is exercised by the tiny real models of the tests (and by the
  opt-in real GGUF header tests when their env vars name files). One spec family serves both variants (block and layer counts come from the files).
- **`is_distilled` is READ FROM THE CHECKPOINT, and it decides whether guidance can happen at all.**
  `components.checkpoint_is_distilled` answers a TRI-STATE from the `model_index.json` beside the
  weights — `True` / `False` as declared, `None` when there is no manifest, it cannot be read, or it
  carries no boolean `is_distilled`. The pipeline uses the flag for nothing but
  `do_classifier_free_guidance` (`guidance_scale > 1 and not is_distilled`), so getting it wrong on
  a distilled checkpoint — which every shipped klein build is — costs TWICE the compute per denoise
  step for a guidance that was distilled away, plus a whole wasted pass over the 16 GB text encoder
  for a negative prompt the denoise never reads.
  - **Unknown means today's behaviour, logged.** `None` builds the pipeline with `is_distilled=False`
    and leaves `guidance_scale` in force, and `service._pipeline_for` says so at INFO. This is a
    documented choice, not a silent fallback: a hand-assembled folder of components carries no
    manifest, and refusing it would be worse than running it the way this service ran every
    checkpoint before the flag was read.
  - **A distilled checkpoint skips the negative encode entirely.** Every decision that depends on
    the scale asks `service._effective_guidance_scale`, which returns 1.0 there and announces the
    ignored slider once per `(checkpoint, requested scale)`. So `_prompts_to_encode` never asks for
    the empty prompt, `_prompt_embeds_locked` returns `negative=None`, and `_denoise` sends
    `guidance_scale=1.0` down to diffusers. The filesystem is consulted only when the user actually
    raised the slider, so the default costs nothing.
  - **The flag needs no field in the pipeline key.** `components.model_index_search_roots` reads
    only the transformer's and the VAE's roots — the two paths the pipeline handle's key carries —
    so a checkpoint that would answer differently is already a different handle. The encoder is
    another handle, which is why its path is deliberately NOT a search root here.
  - **`status.guidance_supported`** is the same answer told to the client: `False` exactly when the
    checkpoint declares `is_distilled: true`, `True` otherwise — the undeclared case included,
    because that is what its run really does. A plain boolean, not the tri-state (`ipc/PROTOCOL.md`
    §5.4).
- Unlike `flux_fill.py`, this service honours the user's `General.ai_device` choice through the
  module-level `_resolve_selected_backend_device()` binding (`hardware.py`, bound to
  `runtime.torch_device.resolve_discrete_backend_device`): a configured discrete `cuda:<n>` or `cpu`
  is used as is, a configured integrated GPU is overridden (WARNING) by the largest discrete device,
  and an unset / plain `cuda` choice takes that device too (CPU when there is none).
- **The reported device is never a placeholder.** `status`, `health` and the generation answer take
  it from `_device_label()`: the loaded `self._device` when a pipeline is resident, otherwise the
  device `_resolve_selected_backend_device("cuda")` WILL pick — the same call the pipeline build
  makes, so the two cannot disagree. Answering `"cpu"` because nothing is loaded yet told the user
  that a run costing tens of minutes would happen on the CPU while it would in fact run on the
  configured GPU; `loaded` / `ready` are what distinguish a fact from a plan. For the same reason
  `large_models.profiles` plans against the free VRAM of THAT accelerator (`cuda:1` means card 1).

### FLUX.2 klein: per-component residency and actions
The wire contract is `dev-docs/flux2_component_residency.md`; `ipc/PROTOCOL.md §5.4` carries the
client-facing half. What this module owns:

- **Residency comes from the large pool's ledger parts** (`pipeline.residency_of` over the
  arbiter's per-component `(device bytes, host bytes)` of the pipeline handle of the request's paths
  and dtype, and of the encoder handle of its path and precision): `not_loaded` / `ram` / `gpu` /
  `mixed` (a juggled component parked in part — reported, never rounded). `offloaded` stays in the
  vocabulary but is never answered (no accelerate hooks any more).
- **`_component_actions` is the AUTHORITY, and it lives only here.** The text encoder offers `load`
  / `unload` only (where it runs is the steps' decision, never a button); `load` / `unload`
  appear on both the transformer and the VAE (one handle); the transformer offers `to_ram` while on
  the device and `to_gpu` while in RAM, never on a `mixed` one; the VAE offers `warmup` while on the
  device and no move.
- **Every action is a manager operation**: `load` (transformer/VAE) a pipeline session that loads,
  places and warms up (steps 0-6) and ends in the steps' between-runs state; the encoder's `load`
  an encoder session (steps that would drop it at once — VRAM not MAX and RAM not KEEP / «Авто» —
  are replaced by (PHASE, KEEP): kept in RAM);
  `unload` drops the idle handles (`unload()` = every FLUX.2 handle; the transformer/VAE «unload»
  the pipeline only, the encoder's the encoder only); `to_ram` a (PHASE, KEEP) session whose
  `finish` parks the pipeline in RAM; `to_gpu` a (MAX, KEEP) session that calls `Session.settle()`
  (the manager's `finish` only shrinks, so moving back onto the device is explicit) — the VAE
  moves with the transformer; `warmup` one 64x64 decode. Every admission is the guard; the old
  per-button headroom checks are gone. The encoder KNOWN GAP (a user-loaded encoder outside the
  lease accounting) is closed: the encoder is a large-pool handle like the pipeline.
- **`status` never blocks, and that is load-bearing.** A generation holds `self._lock` for its
  entire run, so the residency probe takes it with `acquire(blocking=False)` and reports
  `components_busy: true` with the `residency`/`actions` keys ABSENT when it cannot. Everything else
  `status` reports is read without the lock.
- **Refusal before admission**: `component_action` takes the lock WITHOUT waiting (busy refusal) and
  re-checks the action against the current list BEFORE any session is acquired — a session that is
  finished unused moves the model to its between-runs state, which would perform the refused action.

### FLUX.3 klein (`flux3_klein/`)
- Wired since protocol v15: `AppState.flux3_klein`, `inpaint.flux3_klein` (region editing:
  `flux3_klein/region.py`; mid-run cancel through the observer and the denoise step callback,
  `Flux3Canceled`), `.status`, `.unload`, the prompt library `.prompt_cache.{build,list,export,
  import}` (TEXT mode; a served caption runs without the encoder handle, whose weights may then be
  absent) and `large_models.profiles` with `model: "flux3_klein"`.
  Wire: `ipc/PROTOCOL.md` "FLUX.3 klein".
- The backend downloads no FLUX.2 or FLUX.3 klein weight: the application's Rust model catalog
  owns every one (pinned files, gated-repository access preflight, resume, adoption by hash) and
  sends the package root as `source_root`; the GGUF candidates arrive as absolute paths
  (protocol 24, `dit_formats` / `text_encoder_formats`).
- «Энкодер на CPU» (`encoder_on_cpu`, wire default `true`, shared with FLUX.2): the text encoder
  is pinned to host execution through the spec's host pin (`runtime/large_models` "Host pin").
- Large-model manager handles, not a small-pool service: no small-pool lease. Two handles per run,
  the pipeline (DiT + AE) and the text encoder (only when the run encodes), both admitted by
  `LargeModelManager.acquire(steps=...)` UNDER the service lock, pipeline before encoder (the
  stated exception of the manager's "acquire before the service lock" rule: FLUX.3 registers no
  reclaim callback that takes its lock; `flux3_klein/test_service.py::LockOrderTests`); the
  encoder is admitted beside the pipeline under the shared two-handle rule
  (`runtime/large_models/paired.py`). Every load and move is a manager `Session` step under the
  lock, driven by the user's two resource steps (`Flux3Options.vram_step` x `ram_step`). `health()` reads the
  published fields from a `HealthSnapshot` and where each component is from the manager's ledger
  (`LargeModelManager.placements`). The lease contract is pinned by `flux3_klein/test_characterization.py`
  (`LeaseContract`) and `flux3_klein/test_service.py`, not by `test_lease_protocol.py`.
- Device: `options.device` or `_resolve_selected_backend_device` (the FLUX.2 klein policy:
  `runtime.torch_device.resolve_discrete_backend_device`, never an integrated GPU unless
  `options.device` names one explicitly).
- Every contract of the port (reference parity, deviations, fp8 probe, the step pair -> placement
  mapping, observer phases and their `info` keys): `flux3_klein/MODULE_README.md`.

### ROCm staging obligation
On a ROCm Torch build, a host→device copy out of a safetensors file mapping stalls in amdkfd (~1-2 s
per tensor ≥1 MiB). Any weight move here must route through `runtime/rocm_mmap_transfer.py`:
- `sdxl.py` and the plain FLUX path wrap their move in `with patched_module_to():` (FLUX.2 klein
  moves nothing itself: the large-model manager's loader reads into anonymous or registered
  buffers and never copies a file-mapped tensor to the GPU);
- the FLUX CPU-offload path cannot (the patch is process-global and must never wrap inference), so
  `_materialize_components_for_offload()` re-homes the safetensors components in anonymous host
  memory up front instead, clearing the allocator cache between components.
The block under `patched_module_to()` must contain the weight move and nothing else — no download,
no network wait, no inference. Everything is a strict no-op off ROCm.

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
  with `SDXL_SAMPLERS` in `crates/ms-tab-cleaning/src/tools/ai_editor/engines/sdxl/mod.rs`.
- To change which SDXL model sources are accepted or how they load, see `resolve_model_source` /
  `_build_sdxl_inpaint_pipeline` in `sdxl.py`; the config and curated files themselves are the Rust
  catalog's (`crates/ms-sysprobe/src/ai_models/`), and the wire fields are `ipc/PROTOCOL.md §5.4`.
- To change the FLUX quant catalog, download layout, or device pinning, see `flux_fill.py`
  (`AVAILABLE_QUANTS`, `_build_download_plan`, `_select_discrete_device`); the staging /
  serialization / atomic-publish envelope itself lives in `../engines/model_download.py` and is
  shared with `watermark/`.
- To change how a FLUX.2 component is described to the large-model manager — its files, key
  mapping, blocks, dtype, the fp8 encoder, the phases' work, the decode fallbacks, the pre-admission
  refusals — see `flux2_klein/large_model.py` (BFL keys: `flux2_klein/conversion.py`). How a
  component is READ (tensor by tensor) is `../runtime/large_models/loader.py` over
  `../runtime/safetensors_stream.py`; the fp8-scaled refusal stays in `flux2_klein/`. The
  byte-level progress of a component read is `progress.LoadFrames`, built per request by
  `service._load_frames` and registered as the load-progress sink of every session
  (`service._acquire`, `Session.set_load_progress`); it must never move `LOAD_STEP_*` /
  `LOAD_PHASE_STEPS`.
- To change FLUX.2 klein params, region limits, the retired-field refusals or the step pair of a
  request, see `flux2_klein/` (`params.normalize_flux2_klein_params`, `params.RETIRED_FIELDS`,
  `params.validate_region_size`, `memory.requested_steps`, `memory.forecast_memory`); the
  steps, the forecast arithmetic and the degrade ladder are `../runtime/large_models/`. Keep
  the param set in sync with `ipc/PROTOCOL.md §5.4`. The wire contract carries one `applied` flag,
  `vae_tiling`, and the four step keys; the Rust side matches those names exactly; do not rename
  one side alone.
- To change what "tiling" means — the threshold, the forecast predicate or the decode's last rung —
  see `flux2_klein/components.py::vae_tile_threshold_pixels` (the threshold, read from the VAE
  config), `large_model._decode_work` / `tiling_engages` (the forecast) and
  `pipeline._vae_tiling_engages` / `_lowered_vae_tile_thresholds` (the decode). Never hard-code the
  1024/128 numbers in any of the three, and never move one of the two tile thresholds alone.
- The memory gate is the large-model manager's admission. Never add a second forecast:
  `large_model_profiles` and `memory.forecast_memory` plan through the manager's planner and
  forecast.
- To change what `status` reports per component, or which buttons the FLUX.2 panel offers, see
  `pipeline.residency_of` (the literal of the ledger parts), `_component_actions` (the
  authoritative matrix — it exists only there) and `Flux2KleinInpaintService._components_locked` /
  `_components_residency_nowait`. To change what an action DOES, see `component_action` /
  `_component_action_locked`: every action is a manager session or `unload` — never a hand-made
  move. Keep the wire literals in step with `dev-docs/flux2_component_residency.md` and
  `ipc/PROTOCOL.md §5.4`.
- To change the prompt phase, see `_prompt_embeds_locked` / `_encode_prompts_locked`
  (the encoder session) / `_encode_prompt_phase`, and `_prompt_plan` for which embedding serves a
  text (both over `../runtime/prompt_cache_service.py`: `plan`, `serve_locked`, `remember_locked`).
- To change the prompt LIBRARY, see the service's `prompt_cache_build` / `_list` / `_export` /
  `_import` (FLUX.2's frames, refusals and answer keys) over the shared
  `../runtime/prompt_cache_service.py` (what is served, built, listed, exported, imported);
  FLUX.2's library ids, encoder provenance, width filter, the v1 migration and its adapter are
  `flux2_klein/prompt_cache.py`; the container, index, file names and the selection rule are
  `../runtime/prompt_library.py` (model-agnostic: a FLUX.3 library reuses both with its own id and
  adapter). To
  change what identifies an encoder, see `text_encoder_fingerprint` — and keep it metadata-only. To
  change what an ABSENT encoder makes possible, see `text_encoder_available` /
  `require_text_encoder`.
- To change the load ORDER or the warm-up, see `_run_locked` (pipeline session, then the encoder
  session, then the denoise and decode phases) and `_load_pipeline_locked` (the cold-load steps 1-6
  and `_warmup_locked`). The `phase:"load"` step numbers live in the `LOAD_STEP_*` constants and
  `LOAD_PHASE_STEPS`; they are the order the user sees, so they must stay strictly increasing
  along that sequence, and the first `LOAD_STEP_PREPARE` frame is emitted BEFORE the encoder check
  and the pre-admission refusals, which touch the filesystem.
- To change how the Qwen tokenizer (or the scheduler, `_ensure_scheduler_locked`) is obtained, see
  `_ensure_tokenizer_locked` — the one place `Qwen2TokenizerFast` is constructed. It is cached by the discovered directory, shared between the
  pipeline component and every prompt encode (encoding is read-only and the pipeline's copy is never
  called), and dropped by `unload()`. Do not add a second read: it used to happen on every encode,
  which is real disk I/O on an otherwise hot pipeline.
- To change how the marks reference reaches the pipeline, see `_split_request_blob` in
  `../ipc/handlers/flux2_klein.py` (the wire split), `inpaint_image_bytes` (decode and size check)
  and `_denoise` (`image_reference`) in `flux2_klein/service.py`, and the reference term of the
  denoise work in `flux2_klein/large_model.py`.
- To change the "no mask" mode, see `normalize_flux2_klein_params` / `_whole_region_overrides` (the
  params it settles) and `region_imaging.require_solid_mask` (the check). `_denoise` needs no special
  case and must not grow one — the mode is expressed entirely as normalized parameters.
- To change where the FLUX.2 klein transformer config is looked for, see
  `transformer_config_roots` / `find_transformer_config_dir` / `validate_transformer_config_dir` in
  `flux2_klein/components.py`; the "not found" text is `_missing_transformer_config_message` and lists the
  probed directories through the same `component_probe_order`, so search and message stay in step.
- To change the encoder<->transformer compatibility rule, see `TEXT_ENCODER_OUT_LAYERS` /
  `transformer_config_dir` / `require_encoder_transformer_compatible` in
  `flux2_klein/components.py`, and its two call sites in `service.py` (`inpaint_image_bytes`,
  `_component_action_locked`). `transformer_config_dir` must keep resolving to the SAME
  directory `large_model.transformer_config_dir` builds the skeleton from, or the guard and the
  loader would judge different files.
- To change how a service reports load vs. inference failures to the model manager, change all the
  small-pool services at once and extend `test_lease_protocol.py` — the protocol is a cross-service
  contract (FLUX.2 klein is a large-model manager handle: its sessions follow the manager's).
- To change FLUX.3 klein (run order, its model spec, observer phases, loaders, the region editor
  and its wire), see `flux3_klein/MODULE_README.md`; which files make up its package: the Rust
  catalog (`ms_sysprobe::ai_models::external_catalog::flux3_klein`).
- To add a new inpaint backend, copy the service shape above, wire it in `server.py` + `AppState`,
  add an `ipc/handlers/` method and a `METHOD_INPAINT_*` constant in `ipc/protocol.py`, and route any
  GPU weight move through `runtime/rocm_mmap_transfer.py`.
