# Module: modules/ai_backend/inpaint

## Purpose
Inpainting backends of the Python AI backend: five independent service adapters (LaMa V2, LaMa MPE,
AOT, SDXL, FLUX.1-Fill-dev) that take an image + mask as PNG bytes and return PNG bytes. They are
constructed in `server.py`, stored on `AppState`, and reached from Rust through the `inpaint.*` IPC
methods implemented in `ipc/handlers/`.

## Architecture
Every service follows the same shape and is safe to copy from when adding a sixth backend:

- construction is cheap; torch / diffusers / cv2 are imported lazily inside the methods that need
  them, so importing one inpainter never pulls in another one's dependencies;
- one resident model at a time, guarded by an `RLock`, keyed by a service-specific `model_key`
  (`lama_v2:<device>:<ckpt>`, `aot:<device>`, `sdxl:<mode>:<device>:<path>`, `flux_fill:<quant>`, …);
- the key is leased from the shared `runtime.model_manager.LoadedModelManager`
  (`begin_model_use` → `mark_loaded` / `mark_load_failed` → `release`), which is what lets an idle
  model be evicted by another service;
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
- `test_sdxl.py`, `test_flux_fill.py`: pure-Python unit tests (no torch, no diffusers, no weights, no
  GPU — fake `torch`/`diffusers` modules are injected into `sys.modules`).

## Contracts and invariants

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
  repo under `components/`. Downloads are streamed to `.part` files and renamed atomically.
- `progress_callback(phase, step, total, label)` has two phases: `download` (byte-level) and
  `generate` (step-level). `ipc/handlers/flux_fill.py` streams both as `progress` frames with header
  `phase`/`step`/`total`/`label` and no preview blob.
- The pipeline is pinned to the DISCRETE GPU; `_select_discrete_device()` excludes the Ryzen iGPU and
  falls back to CPU rather than using it.

### ROCm staging obligation
On a ROCm Torch build, a host→device copy out of a safetensors file mapping stalls in amdkfd (~1-2 s
per tensor ≥1 MiB). Any weight move here must route through `runtime/rocm_mmap_transfer.py`:
- `sdxl.py` and the plain FLUX path wrap `pipe.to(device)` in `with patched_module_to():`;
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
  with `SDXL_SAMPLERS` in `src/tabs/cleaning/tools/sdxl.rs`.
- To change the FLUX quant catalog, download layout, or device pinning, see `flux_fill.py`
  (`AVAILABLE_QUANTS`, `_build_download_plan`, `_select_discrete_device`).
- To add a new inpaint backend, copy the service shape above, wire it in `server.py` + `AppState`,
  add an `ipc/handlers/` method and a `METHOD_INPAINT_*` constant in `ipc/protocol.py`, and route any
  GPU weight move through `runtime/rocm_mmap_transfer.py`.
