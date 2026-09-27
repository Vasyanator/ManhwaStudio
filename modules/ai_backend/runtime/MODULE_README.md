# Module: modules/ai_backend/runtime

## Purpose
Process-wide runtime plumbing shared by every AI backend service: where the program lives on disk,
whether Torch is usable, which device the user picked, how many models may stay resident, and the
ROCm-specific workarounds that must be configured before any inference happens.

Nothing here is a service. Nothing here knows about OCR, detection, inpainting or translation; those
domains (`ocr/`, `detection/`, `inpaint/`, `reline/`, `translate/`) depend on this package, never the
other way round.

## Architecture
`paths.py` sits at the bottom and is stdlib-only. `torch_support.py` and `model_manager.py` are
likewise dependency-free. `device_service.py` reads/writes user config and probes runtimes.
`rocm_runtime.py` and `rocm_mmap_transfer.py` are the two ROCm/HIP workarounds: the first is applied
once at backend startup, the second is applied per weight transfer by the Torch services.

The only outbound dependency of this package is `rocm_runtime._resolve_cache_root()`, which lazily
imports `..engines.paddle_onnx` to reuse the ONNX model-root resolution and falls back to
`paths.program_root()` when that heavy import is unavailable. Keep that import lazy: it is what stops
a ROCm startup tweak from pulling cv2/onnxruntime into a Torch-only install.

## Files and submodules
- `paths.py`: `program_root()` — the installation/repo root that contains `config.py`, `modules/`
  and `ManhwaStudio_AI_Models/`. Stdlib only; imported freely by any layer.
- `torch_support.py`: `is_torch_available()` cached import probe, a debug override that simulates a
  missing Torch (`configure_torch_support(simulate_disabled=...)`), and the stable user-facing
  message for Torch-gated endpoints.
- `model_manager.py`: `LoadedModelManager` / `ModelUsageLease` — resident-model bookkeeping,
  LRU eviction of idle models under a configurable cap, and leases that keep an in-use model from
  being unloaded. It never loads models itself; services own their runtime objects.
- `device_service.py`: selected Torch device and ONNX provider/device-id state, accelerated
  defaults, human-readable device-name probing (CUDA / DirectML / MiGraphX), CUDA-ROCm diagnostics
  for the Rust settings tab, and persistence into `UserConfig`. Every write is
  `UserConfig.update(mutator)` touching only its own keys (a serialized re-read + write of the
  on-disk `user_config.json`/`.db` via the root `docstore.py`); automatic fallbacks and the
  model-limit normalization at startup are best-effort (logged, never abort a query) and the
  limit is written only when the stored value differs.
- `rocm_runtime.py`: `configure_rocm_runtime()` — MIOpen tuning and the allocator override, applied
  once at backend startup. It is also what tells `error_text` whether this process is on ROCm.
- `error_text.py`: `sanitize_torch_error()` — strips Torch's `expandable_segments:True` out-of-memory
  advice from a message on its way to the user, and `configure_error_text()` /
  `rocm_advice_rewrite_enabled()`, the ROCm gate that decides whether it strips anything at all.
  Stdlib only, like `paths.py`, because the torch-free `ipc/` layer imports it — which is exactly
  why the ROCm fact is pushed IN rather than probed here.
- `rocm_mmap_transfer.py`: the ROCm host->device weight-transfer workaround
  (`mmap_staging_required()`, `stage_cpu_tensor()`, `move_module_to()`, `patched_module_to()`,
  `invalidate_maps_cache()`).
- `test_device_service.py`, `test_rocm_runtime.py`, `test_rocm_mmap_transfer.py`,
  `test_error_text.py`: unit tests for the contracts below. None of them needs a GPU; ROCm decisions
  are forced via monkeypatch.

## Contracts and invariants
- **`paths.program_root()` is the single owner of the package's directory-depth assumption.** No
  other module in `modules/ai_backend` may compute a root with `Path(__file__).resolve().parents[N]`.
  Moving a file between package directories must not require a matching constant edit anywhere else.
- `__init__.py` re-exports nothing and imports no submodule. The package must stay import-cheap so
  the torch-free `ipc/` layer and its tests never drag in the AI stack — the same reason the parent
  `modules/ai_backend/__init__.py` resolves `run_server` lazily via PEP 562.
- `General.ai_device`, `General.ai_onnx_provider` and `General.ai_onnx_device_id` use `not-selected`
  as the config sentinel. A stale `*_configured` flag must never make the sentinel look resolved, and
  an automatic runtime fallback (CPU, or the single available provider) must not be persisted as an
  explicit user choice. Services resolve the sentinel to a real runtime default before constructing a
  Torch device or ONNX provider settings.
- Which default is chosen, and when the user must be asked instead:
  - Torch: CPU is only a temporary fallback. `device.get` reports
    `torch_device_needs_selection` (`AIDevice.needs_manual_selection`) as soon as an accelerator
    such as CUDA becomes available while the user's choice is still unresolved, so the Rust settings
    tab can ask rather than silently staying on CPU.
  - ONNX: on Windows, `DmlExecutionProvider` is preferred whenever it is among the available
    providers (`_default_provider_from_available`); otherwise the default provider is used, and
    failing that the first available one.
  - A DirectML selection with no configured device id is reported through `device.get` as needing
    manual confirmation (`_device_needs_manual_selection`) and must not be persisted until the user
    picks an adapter.
- Torch and ONNX model roots are separate trees (`ManhwaStudio_AI_Models/Torch` vs `.../ONNX`);
  device selection here is per-runtime for that reason and the two selections never alias.
- `LoadedModelManager` eviction callbacks always run outside the manager lock, so a service-local
  lock can be taken inside them without deadlock.
- **`ModelUsageLease.mark_load_failed()` means the LOAD failed, nothing else.** It reaches
  `abort_load`, which clears the entry's `resident` flag and drops its unload callback, so calling
  it for a failure that happened after the model was already loaded makes the manager under-count
  residency and leaves a model that still occupies VRAM permanently non-evictable. A service must
  therefore scope its load in its own `try` and call `mark_loaded()` as soon as that load returns —
  before running inference — while `release()` runs in `finally` on every path. The inpaint and
  watermark services are the reference shape (`inpaint/MODULE_README.md`, covered by
  `inpaint/test_lease_protocol.py`); `ocr/` and `detection/` still wrap load and inference in one
  `try` and are due the same treatment.
- On a ROCm Torch build, `configure_rocm_runtime()` runs once at process startup before any
  inference: it defaults `MIOPEN_FIND_MODE=FAST` (immediate mode, no per-input-shape kernel
  auto-tuning), forces `expandable_segments:False` into the allocator-config variable Torch will
  actually read, disables cudnn/MIOpen benchmark, and pins the MIOpen user/kernel cache under
  `ManhwaStudio_AI_Models/.cache/miopen`. It is a strict no-op on CUDA/CPU/MPS/absent-Torch installs
  and never raises. Every env default here uses `setdefault` so an explicit user override wins —
  with ONE deliberate exception, `expandable_segments`, described in the next bullet. Torch services
  must not depend on a specific MIOpen Find mode and must not re-enable cudnn benchmark.
- **`expandable_segments` must stay off on ROCm, no code may advertise turning it on, and this is
  the one setting a user override does NOT win.** On the ROCm stack this project targets (torch
  2.12.0+rocm7.2, HIP 7.2, gfx1201) the `True` mode corrupts computation — NaN from a varying module
  above 256 px, `HSA_STATUS_ERROR_ILLEGAL_INSTRUCTION` and a wedged HSA queue at 512-1536 px — and
  the user sees a fully black image with normal timings. That is a silent WRONG RESULT, not an
  error, which is why it is treated as a correctness hazard rather than a preference: an inherited
  or explicitly set `True` is reversed, and the reversal is logged at WARNING with the symptom and
  the way out. `MS_ALLOW_EXPANDABLE_SEGMENTS=1` is that way out — it leaves the user's value exactly
  as found, also at WARNING. Two mechanics make the override actually take effect:
  - **Which variable.** Torch reads `PYTORCH_CUDA_ALLOC_CONF`, then `PYTORCH_HIP_ALLOC_CONF`, then
    `PYTORCH_ALLOC_CONF`, and stops at the FIRST that is present — an empty value counts as present
    (`c10/core/AllocatorConfig.cpp`, `c10/util/env.cpp::get_env`). Writing the HIP spelling while
    the environment carries the CUDA one is silently inert, so `_effective_alloc_conf_var()` picks
    the variable Torch will really consult and writes only that one.
  - **Which option.** Only `expandable_segments` is rewritten; every other option in the value
    (`max_split_size_mb`, `garbage_collection_threshold`, …) is preserved verbatim and in order, and
    a value that already says `False` comes back byte-identical.
  Timing is not a concern: Torch's allocator config is a lazily initialized singleton read at the
  FIRST GPU allocation, not at `import torch`, and `configure_rocm_runtime()` runs before anything
  touches the GPU.
- The user-facing matching half is `error_text.sanitize_torch_error()`: Torch's own OOM message ends
  with advice to set `PYTORCH_{CUDA,HIP}_ALLOC_CONF=expandable_segments:True`, so any message shown
  to the user must be passed through it first (`ipc/dispatcher.py` does this for the terminal error
  frame). **The rewrite is ROCm-gated and defaults to OFF**: on NVIDIA/CUDA that advice is correct
  and the replacement note would assert a ROCm-specific claim that is false there, so a process that
  never called `configure_rocm_runtime()` — a unit test, a CPU or ONNX-only install — forwards
  Torch's text byte for byte. `configure_rocm_runtime()` is the only writer of that flag
  (`configure_error_text(rocm_runtime=…)`, set on every one of its exits); `error_text` never probes
  torch itself, because it must stay stdlib-only for `ipc/`. When the gate is on, the transform is
  targeted, idempotent, never raises, and leaves the rest of the Torch diagnostic byte-identical —
  the UNTOUCHED text always stays in the process log. The match is anchored on Torch's own advice
  wording (`try setting …`), NOT on surrounding sentence text: a Torch OOM line whose numbers carry
  no sentence-ending period offers nothing for a `[^.\n]*?` prefix to stop at, and the diagnostic
  the user has to forward gets eaten. Services storing `_last_error` do NOT sanitize yet:
  `dev-docs/known_gaps.md` KG-007.
- On a ROCm Torch build, a Torch service that moves checkpoint weights to the GPU must route the move
  through `rocm_mmap_transfer` or it pays a multi-minute amdkfd stall (a copy from a writable private
  file mapping, exactly how safetensors hands out weights, breaks copy-on-write over the mapped
  range). Which form to use:
  - the service owns the `nn.Module` and calls `.to()` itself -> `move_module_to(module, device)`;
  - a third-party loader moves the model internally (surya predictors, `DiffusionPipeline.to`)
    -> wrap that call in `with patched_module_to():`.
  `patched_module_to()` replaces the process-global `torch.nn.Module.to` class attribute (only the
  entering thread takes the staging path), so the block must contain the weight move and **nothing
  else** — never a download, a network wait, or inference. This is why the Surya checkpoint download
  lives in `../engines/surya_checkpoints.py` and runs before the patch is entered.
  The workaround is self-gating (ROCm + posix + `cuda` target + CPU source + >=1 MiB + genuinely
  file-backed pages) and a strict no-op everywhere else, so wiring it in never changes
  CPU / CUDA-NVIDIA / MPS / Windows behavior. `MS_ROCM_MMAP_STAGING=0` disables it entirely. A
  transfer that also changes dtype is not affected by the pathology (Torch casts on the host into
  anonymous memory first) and needs no staging.
  The `/proc/self/maps` snapshot lives for exactly one load session (one `move_module_to` call or one
  `patched_module_to` block) and is thread-local, so services carry no invalidation obligation;
  `invalidate_maps_cache()` exists only as a manual reset.
- Every module here must surface a missing package or an unusable device as an explicit error. No
  silent fallback to CPU, to a different provider, or to a guessed path.

## Editing map
- To change how the installation root is found, edit `paths.py` — and only `paths.py`.
- To change device selection, sentinel handling, or device-name probing, edit `device_service.py`.
- To change the resident-model cap or eviction policy, edit `model_manager.py`.
- To change MIOpen tuning, the MIOpen cache location, the allocator override, or an environment
  default applied at ROCm startup, edit `rocm_runtime.py`; add the case to `test_rocm_runtime.py`
  and add the env key to that file's save/restore list or the value leaks into the next test — for
  the three allocator variables a leak also changes WHICH of them the function decides Torch reads.
- To change which upstream error text is rewritten before the user sees it, edit `error_text.py` and
  `test_error_text.py`. Keep it stdlib-only (`ipc/` imports it), keep every rewrite targeted rather
  than a truncation, anchor each pattern on the upstream wording it removes rather than on
  neighbouring sentence text, and keep the result free of any string the rewrite itself matches,
  which is what makes it idempotent. A rewrite that is only correct on one runtime belongs behind
  the same gate as the ROCm one, never applied unconditionally.
- To change the staging gate, the `/proc/self/maps` classifier, or the `Module.to` patch, edit
  `rocm_mmap_transfer.py`; add the corresponding case to `test_rocm_mmap_transfer.py`.
- To wire a new Torch service onto the GPU, do not edit this package: pick the
  `move_module_to` / `patched_module_to` form described above from the service side.
