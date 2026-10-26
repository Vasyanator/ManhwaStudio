# Module: modules/ai_backend/runtime

## Purpose
Process-wide runtime plumbing shared by every AI backend service: where the program lives on disk,
whether Torch is usable, which device the user picked, how many models may stay resident, and the
ROCm-specific workarounds that must be configured before any inference happens.

Nothing here is a service. Nothing here knows about OCR, detection, inpainting or translation; those
domains (`ocr/`, `detection/`, `inpaint/`, `reline/`, `translate/`) depend on this package, never the
other way round.

## Architecture
`paths.py` sits at the bottom and is stdlib-only. `torch_support.py` is likewise dependency-free, and
`torch_flavour.py` builds only on it at import time (torch and `torch_device.py` lazily, inside its
health-fact calls);
`model_manager.py` builds only on `residency/` (which builds on `memory_budget.py` and `torch_support.py`). `device_service.py` reads/writes user config and probes runtimes.
`rocm_runtime.py` and `rocm_mmap_transfer.py` are the two ROCm/HIP workarounds: the first is applied
once at backend startup, the second is applied per weight transfer by the Torch services.
`safetensors_stream.py` and `encoder_truncation.py` are stdlib-only (torch lazily, per read);
`gguf_stream.py` builds on `safetensors_stream.py` (stdlib, `gguf.constants` lazily) and
`tensor_container.py` on both; `memory_probe.py` depends only on `torch_support.py`. They are the
shared, model-agnostic checkpoint/memory utilities model packages build on (first consumer:
`inpaint/flux2_klein/`).

The only outbound dependency of this package is `rocm_runtime._resolve_cache_root()`, which lazily
imports `..engines.paddle_onnx` to reuse the ONNX model-root resolution and falls back to
`paths.program_root()` when that heavy import is unavailable. Keep that import lazy: it is what stops
a ROCm startup tweak from pulling cv2/onnxruntime into a Torch-only install.

## Files and submodules
- `paths.py`: `program_root()` — the installation/repo root that contains `config.py`, `modules/`
  and `ManhwaStudio_AI_Models/` — and `side_models_root()` —
  `ManhwaStudio_AI_Models/side_models` under it. Stdlib only; imported freely by any layer.
- `test_paths.py`: pins every side-model consumer directory (FLUX.1-Fill, FLUX.2 klein variants,
  watermark removal, Reline) to `side_models_root()` and to the root `config.py` constants.
- `torch_support.py`: `is_torch_available()` cached import probe, a debug override that simulates a
  missing Torch (`configure_torch_support(simulate_disabled=...)`), and the stable user-facing
  message for Torch-gated endpoints.
- `torch_flavour.py`: the one owner of "which torch build is this" — `TorchFlavour` (`CPU` / `CUDA` /
  `ROCM`), `flavour_of(torch_module)` and `is_rocm_build(torch_module)` over an already imported
  torch (the caller keeps its own import gate) — and of the torch facts of the health snapshot:
  `TorchBuildFacts` / `torch_build_facts()`, `gpu_device_facts()` (index, name, base `gcn_arch`,
  `integrated` via `torch_device`), `installed_rocm_device_archs()` (the `amd-torch-device-gfxN`
  wheels from `importlib.metadata`) and `torch_health_payload()`, the snapshot's `torch` object.
  Importing it never imports torch (a test enforces it). `test_torch_flavour.py` holds its table,
  the fact tests and the site-agreement tests of every consumer (including `modules/ai_device.py`).
- `model_manager.py`: `SmallModelManager` — the small-model pool over the residency arbiter:
  the service-facing lease API (`begin_model_use(key, unload_callback, *, owner=, hint=)` ->
  `ModelUsageLease`, `mark_unloaded`), its reclaim policy (drop whole idle models, LRU) and the
  secondary count cap `General.ai_max_loaded_models` as slots (`set_max_loaded_models` evicts
  live); `health()` keeps the four small-pool counters. It never loads models itself; services
  own their runtime objects.
- `residency/`: `ResidencyArbiter` — the one ledger and reclaim arbiter both model pools (small
  and large) take their leases from as peers: lease state machine, byte reservations, reclaim
  order, owner rule, footprint measurement. See its `MODULE_README.md`.
- `test_model_manager.py`, `test_model_manager_race.py`: the lease report calls; the S-058 race
  (two threads, two keys, a stub service in the reference lease shape).
  `test_model_manager_pool.py`: the count cap as slots (LRU, live lowering, kept in-use and
  callback-less models, unchanged refusal text).
- `device_service.py`: selected Torch device and ONNX provider/device-id state, accelerated
  defaults, human-readable device-name probing (CUDA / DirectML / MiGraphX), CUDA-ROCm diagnostics
  for the Rust settings tab, and persistence into `UserConfig`. Every write is
  `UserConfig.update(mutator)` touching only its own keys (a serialized re-read + write of the
  on-disk `user_config.json`/`.db` via the root `docstore.py`); automatic fallbacks and the
  model-limit normalization at startup are best-effort (logged, never abort a query) and the
  limit is written only when the stored value differs. It is also the large-model pool's
  `LimitsStore` (`UserConfigLimitsStore`, built by `large_models_limits_store(floors)`): the two
  hard caps `General.large_models_{vram,ram}_cap_bytes` (a `None` removes the key; a retired
  `General.large_models_keep_policy` is ignored with one INFO log and left in place), floors
  delegated to the cache-side store; `general_section()` hands `General` to the startup OOM
  backstop.
- `torch_device.py`: the one owner of `General.ai_device` -> concrete Torch device resolution
  (`resolve_selected_backend_device`, with `read_configured_device`, `normalize_backend_device`,
  `safe_available_devices`) and of the allocator-cache release after a model drop
  (`clear_torch_cache`), and of the "never the iGPU" rule (`is_integrated_gpu`,
  `resolve_discrete_backend_device`). Each Torch service binds them once at module level under the
  private names `_resolve_selected_backend_device` / `_clear_torch_cache` (the names the tests
  patch), with its policy as the binding's arguments. `test_torch_device.py` covers the resolution
  order, both device policies, the iGPU rule table, the discrete resolution and the cache release.
- `rocm_runtime.py`: `configure_rocm_runtime()` — MIOpen tuning and the allocator override, applied
  once at backend startup. It is also what tells `error_text` whether this process is on ROCm. On
  ROCm it removes a truthy `DIFFUSERS_GGUF_CUDA_KERNELS` (diffusers would fetch a CUDA-only kernel).
- `error_text.py`: `sanitize_torch_error()` — strips Torch's `expandable_segments:True` out-of-memory
  advice from a message on its way to the user, and `configure_error_text()` /
  `rocm_advice_rewrite_enabled()`, the ROCm gate that decides whether it strips anything at all.
  Stdlib only, like `paths.py`, because the torch-free `ipc/` layer imports it — which is exactly
  why the ROCm fact is pushed IN rather than probed here.
- `health_snapshot.py`: `HealthSnapshot` — the copy-on-write health fields every model service
  publishes so `health()` never takes the service's work lock. Stdlib only.
- `test_health_snapshot.py`: merge / copy semantics of `HealthSnapshot`.
- `rocm_mmap_transfer.py`: the ROCm host->device weight-transfer workaround
  (`mmap_staging_required()`, `stage_cpu_tensor()`, `move_module_to()`, `patched_module_to()`,
  `invalidate_maps_cache()`).
- `safetensors_stream.py`: model-agnostic checkpoint I/O — the one safetensors header parser
  (`read_safetensors_header_and_data_start` / `read_safetensors_header`, ceiling
  `MAX_SAFETENSORS_HEADER_BYTES`), the per-tensor contract (`TensorSpan` incl. `quant_type`,
  `parse_tensor_spans`: mappable dtype, length == shape x itemsize, no overlap, file order), the
  span-layout rule of both containers (`check_span_layout`), the container-agnostic
  tensor-at-a-time reader (`StreamingTensorReader`) and its safetensors subclass
  (`StreamingSafetensorsReader`; a tensor is read in `READ_CHUNK_BYTES` chunks straight into
  its own buffer), rate-limited byte progress (`ByteProgress`, `ByteProgressCb`,
  `PROGRESS_MIN_INTERVAL_SECONDS`), the opt-in `SAFETENSORS_DTYPES_WITH_F8` table and the backend's ONE
  meta-module filler (`fill_module_from_safetensors` -> `FillReport`, `MissingWeightsError`,
  `LEAVE_ON_META`, `TensorWrite`). Stdlib only; torch is imported lazily by `read_tensor` and the
  filler.
- `prompt_library.py`: the model-agnostic prompt-embedding LIBRARY — one directory per model
  (`<root>/<library id>/`, id `^[a-z0-9_]{1,40}$`), one `.msprompt` per embedding (container v2:
  marker `manhwastudio.prompt_cache`, the file's `__metadata__` is the truth: library, preset `name`,
  prompt, length, dtype, encoder provenance `source`/`format`/`fp8`/`id`/`label`, `created_at`,
  `origin`), deterministic file names (`entry_file_name`), a rebuildable `index.json` (files win: a
  lost/foreign/newer index is rebuilt, a corrupt file is `skipped`, never deleted), THE selection
  rule (`select_entry`: preferred source, then precision order, memory, newest, name),
  `PromptLibrary` (lazy load reconciling on a directory-mtime change, an immutable lock-free
  `LibrarySnapshot`, `write_entry` / `add_payload` / `rename_text`, `read_tensor` raising
  `LibraryEntryGone`), the per-directory registry `library_for`, `publish_bytes_atomically` and the
  export/import path checks. THE preset-name rule is the explicit `NAME_REMOVED_RANGES` table
  (no Unicode category lookup), mirrored by the Rust client and pinned for both by
  `fixtures/preset_name_normalization.json`. THE containment rule of every rewrite, replace and
  delete is `require_confined` (`UnsafeLibraryPath`: no symbolic link / reparse point on the way,
  strictly inside the real root; a linked `.msprompt` is `skipped`, a linked library or root
  directory is not listed and not written); `remove_confined_file` /
  `remove_confined_empty_directory` delete under it. Durability: every publish fsyncs its
  directory after the replace and every removal after the unlink (`sync_directory`: the POSIX-only
  policy of `ms_atomic_file::sync_directory`, a no-op on Windows). Stdlib + `safetensors_stream`; `safetensors.torch` lazily
  (`serialize_tensor` / `load_tensor`, the tests' patch points). `test_prompt_library.py`: its tests.
  Model specifics (library ids, encoder identity, width checks, v1 migration) stay in the model
  package (`inpaint/flux2_klein/prompt_cache.py`).
- `gguf_stream.py`: model-agnostic GGUF header parsing — `read_gguf_header` -> `GgufHeader`
  (version 2/3, alignment, data start, spans in file order, scalar/string metadata, names of
  zero-element tensors), `is_gguf_file`, `ggml_type_name`, ceiling `MAX_GGUF_HEADER_BYTES`; every
  count field is preflighted (cardinality cap + bytes left) before its loop, so a hostile count
  is refused at once, never walked. Turns
  each tensor info into a `TensorSpan`: dims reversed into torch order; plain types map to their
  token in the caller's `dtypes`; every block-quantized type becomes `U8` rows
  `(*rows, bytes_per_row)` with `quant_type` = the ggml type id.
- `tensor_container.py`: THE container dispatch — `container_kind` (GGUF magic sniff, else
  safetensors), `read_container_layout` -> `ContainerLayout` (a header view: spans, data start,
  file size, the GGUF header) and `open_tensor_reader` (a `StreamingTensorReader`).
- `_gguf_test_fixtures.py`: TEST-ONLY writer of tiny real GGUF files (`gguf.GGUFWriter`) shared by
  every GGUF test: `make_tensor` (F32 / F16 / BF16 / Q8_0 / Q4_0 / Q5_0 via `gguf.quants`;
  Q4_K / Q5_K / Q6_K as random blocks with finite scales), `FixtureTensor.reference()` (the
  `gguf.quants` dequantization), `write_gguf`, and BFL- / llama.cpp-style name sets
  (`bfl_style_tensors`, `llama_style_tensors` with a quantized `token_embd.weight`,
  `mixed_fixture_tensors`).
- `test_gguf_stream.py`, `test_tensor_container.py`: the parser (round trip against
  `gguf.GGUFReader`, every malformed-header refusal) and the dispatch (both kinds, exact bytes,
  no torch import).
- `gguf_layers.py`: the GGUF dequantizing layers — `GgufLinear` / `GgufEmbedding` (persistent
  uint8 `weight` buffer in the file's row layout, quant type on the module, so it survives
  `TensorSlot.set`, packed views, meta and `state_dict`; the embedding dequantizes only looked-up
  rows), `dequantize` / `bytes_per_row` / `require_dequantizable` over diffusers' pure-torch block
  functions only (never its CUDA `ops`), and `swap_gguf_modules` (header quant map -> replaced
  `nn.Linear` / `nn.Embedding`, validated before any swap). Imports torch and diffusers at top:
  import it lazily from torch-free modules. `test_gguf_layers.py`: bit-exact parity with diffusers,
  forward parity, survival of slot moves, refusals.
- `memory_probe.py`: `memory_snapshot` — the tolerant `{vram,ram}_{total,free}` snapshot, `0` =
  unknown, never raises — and `cuda_device_index`.
- `encoder_truncation.py`: `keep_layers_for_hidden_states` — how many decoder layers a text
  encoder keeps when a pipeline reads only some hidden states (`max(indices) + 1`, and why the
  `+ 1` is exact).
- `memory_budget.py`: the single owner of "usable host/device memory now" for every model pool.
  Reads MemAvailable, the cgroup v2 limit (`memory.max` / `memory.high`, tightest ancestor, only
  `inactive_file` counted as reclaimable) and `RssAnon + RssShmem` (psutil on Windows/macOS),
  plus device total/free/reserved after one warm-up matmul (`HostBudget`, `DeviceBudget`,
  `read_host_budget`, `read_device_budget`). Reserves: 2 GiB host, 512 MiB ROCm / 768 MiB CUDA;
  headroom never exceeds `total - reserve`; unknown = 0 = refuse. Also `HostWatch` (live check
  during a load -> `HostMemoryShort`) and the Windows sysmem-spill heuristic. Tests:
  `test_memory_budget.py`.
- `large_models/`: the model-agnostic large-model core (spec, header inventory, tier templates of
  the two resource axes «Видеопамять» x «Оперативная память», planner, calibration tables,
  forecast, mechanics, manager). See its `MODULE_README.md`.
- `test_safetensors_stream.py`, `test_memory_probe.py`, `test_encoder_truncation.py`: their
  contracts; the truncation test builds tiny random `Qwen3Model`s on the CPU (skips without
  torch/transformers).
- `test_device_service_large_limits.py`: the large-model limits store over an in-memory settings
  document (only its two keys written, invalid values read as defaults, the retired key ignored,
  floors delegated, a failed write leaves the manager unchanged).
- `test_device_service.py`, `test_rocm_runtime.py`, `test_rocm_mmap_transfer.py`,
  `test_error_text.py`: unit tests for the contracts below. None of them needs a GPU; ROCm decisions
  are forced via monkeypatch.

## Contracts and invariants
- **`paths.program_root()` is the single owner of the package's directory-depth assumption.** No
  other module in `modules/ai_backend` may compute a root with `Path(__file__).resolve().parents[N]`.
  Moving a file between package directories must not require a matching constant edit anywhere else.
- **`paths.side_models_root()` is the single backend owner of the side-model root.** Every backend
  consumer of an on-demand side model derives its directory as `side_models_root() / <name>`; none
  reads root `config` (`SIDE_MODELS_DIR`, `FLUX_FILL_DIR`, `WATERMARK_DIR`, `MODELS_DIR`) or joins
  `ManhwaStudio_AI_Models/side_models` itself. Root `config.py` still lists the same directories for
  its folder-creation pass; `test_paths.py` asserts both spellings agree.
- **`torch_flavour` is the single backend owner of "which torch build is this".** No backend module
  reads `torch.version.hip` / `torch.version.cuda` to branch on the build; it calls
  `is_rocm_build` / `flavour_of`. Only a non-blank string version counts, so `None`, `""`,
  whitespace and a torch without `version` are never ROCm. A new consumer gets a row in
  `test_torch_flavour.py`'s site-agreement section.
- **The health snapshot's `torch` facts are computed once per process and never raise.** The health
  worker builds a snapshot every second, so `torch_build_facts` (after the first success),
  `gpu_device_facts` and `installed_rocm_device_archs` (also when empty) are cached; the only device
  query, `torch.cuda.get_device_properties` per device, runs on the first snapshot. It initializes
  torch's CUDA/HIP runtime state but allocates no VRAM (measured on gfx1201: 0 MiB, the first tensor
  +202 MiB), and the app's first `device.get` after connect already does the same
  (`device_service.py` `_detect_cuda_device_names_locked`, `torch.cuda.get_device_name`). Without torch
  (including the simulate-missing override) `torch_health_payload()` is `None` and the device probe
  is never called; any failing fact also yields `None` (logged WARNING once, then DEBUG). The key's
  shape is an IPC contract (`PROTOCOL_VERSION`).
- **No in-process WMI or COM call before `import torch` in the backend process (TheRock #9305).**
  On Windows the ROCm runtime's `amd_comgr.dll` crashes in `DllMain` after any in-process COM/WMI
  query, so `import torch` fails.
  GPU listings go through `wmic` / `powershell` subprocesses only (`device_service.py`,
  `modules/ai_device.py`); no backend module may import `wmi`, `comtypes`, `win32com` /
  `pythoncom` or call `CoInitialize`. Python >= 3.12 implements `platform.win32_ver()` (and so
  `platform.uname()` / `platform.system()` / `platform.platform()`) with the in-process `_wmi`
  module: the backend runs on 3.11 today, and a Python bump must first move every pre-torch
  `platform.*` call to `sys.platform` / `os.name`.
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
- **Torch device resolution and the cache release have one owner, `torch_device.py`.** A service
  never re-implements them; it states its policy at its binding: `accept_mps=False` (the service
  does not run on MPS: EasyOCR, ComicTextDetector), `mps_last_resort=True` (prefer an available
  `mps` over `cpu` when nothing else matches: LaMa, LaMa MPE, AOT, SDXL, watermark),
  `include_mps=False` in `clear_torch_cache` (release only the CUDA cache: FLUX.1-Fill, FLUX.2
  klein, watermark). Resolution never raises (a failing probe counts as CPU only); a failing cache
  release is logged at WARNING with the service name, never raised.
- **A large model never runs on an integrated GPU; the rule has one owner,
  `torch_device.resolve_discrete_backend_device`.** iGPU = `props.is_integrated` truthy, OR an iGPU
  name marker, OR an EXACT AMD iGPU arch (`_IGPU_ARCHS`, compared on the `gcnArchName` base; never a
  prefix — `gfx103*` also names discrete RX 6000 cards). Among the rest the device with the most
  `total_memory` wins. FLUX.2 klein / FLUX.3 klein bind it with `honour_configured=True` (a
  configured `cpu`, available `mps` or discrete `cuda:<n>` is kept; a configured iGPU is overridden
  with a WARNING); FLUX.1-Fill uses `honour_configured=False` (the setting is ignored). With no
  discrete device the answer is CPU (or `mps` per `mps_last_resort`), never the iGPU.
- Torch and ONNX model roots are separate trees (`ManhwaStudio_AI_Models/Torch` vs `.../ONNX`);
  device selection here is per-runtime for that reason and the two selections never alias.
- **`health()` never takes a service's work lock.** A service's instance lock is held across a
  model load (and, in most services, inference) for seconds to minutes, and `server.py`'s health
  worker calls every `health()` in turn, so one locking `health()` froze the whole pushed health
  snapshot that Rust gates AI actions on. Every service with such a lock keeps its state-derived
  health fields in a `HealthSnapshot` and republishes them (`_publish_health_locked()`, under its
  own lock) at EVERY change of a field it reports: model resident / dropped, device, model id,
  `last_error`. `health()` reads the snapshot plus lock-free filesystem probes only. Lock order is
  service lock -> snapshot lock, never the reverse. While a load runs, `ready` is `False` (the old
  model was dropped first) and `last_error` keeps its previous value; no extra state field is sent,
  because the health payload is part of the IPC contract (`PROTOCOL_VERSION`).
- Eviction callbacks always run outside every manager lock (the arbiter lock is a leaf), so a
  service-local lock can be taken inside them without deadlock. **A service never holds its lock
  across `begin_model_use` without holding a lease of its own** (the owner-rule premise in
  `residency/MODULE_README.md`; SDXL's nested LaMa lease is the allowed shape: SDXL holds its own
  lease). `owner=` / `hint=` are optional: without them a lease reserves only its slot and its
  owner is the key, which is the pre-arbiter behaviour; with a hint
  (`residency.footprint.hint_from_files`) a load first reclaims idle models of either pool until
  its bytes fit and is refused with `ModelMemoryShortError` (a `RuntimeError`) otherwise.
- **`ModelUsageLease.mark_load_failed()` means the LOAD failed, nothing else.** It reaches
  `abort_load`, which clears the entry's `resident` flag and drops its unload callback, so calling
  it for a failure that happened after the model was already loaded makes the manager under-count
  residency and leaves a model that still occupies VRAM permanently non-evictable. A service must
  therefore scope its load in its own `try` and call `mark_loaded()` as soon as that load returns —
  before running inference — while `release()` runs in `finally` on every path. The inpaint and
  watermark services are the reference shape (`inpaint/MODULE_README.md`, covered by
  `inpaint/test_lease_protocol.py`); `ocr/paddle_vl.py` and `ocr/baberu.py` follow it too. The other
  `ocr/` services and `detection/` still wrap load and inference in one `try`, which is harmless only
  because their `mark_loaded()` precedes the inference (a later `mark_load_failed()` is a no-op).
- **`mark_loaded()` is called UNCONDITIONALLY, under the service lock, whenever the service holds
  the model after its load step** — never gated on `lease.needs_load`. `needs_load` is only the
  manager's expectation at lease time: between the lease and the service lock another request can
  switch or unload the model (`mark_unloaded` keeps the entry, not resident, while a lease uses it),
  and the service then reloads it under a lease that said `needs_load=False`. Gated, that reload is
  never registered: the model sits outside `max_loaded_models` and is never evictable (STATE
  S-058). Called outside the lock, the opposite race registers a model another thread already
  dropped. `finish_load` is idempotent, so a call for a model that was resident all along only
  refreshes its LRU time. Such a race reload bypasses the eviction that precedes a reserved load, so
  the cap can be exceeded by one until the next reserved load evicts; evicting from `mark_loaded`
  would run foreign unload callbacks under this service's lock (deadlock order). Pinned by
  `test_model_manager_race.py` and `inpaint/test_lease_protocol.py` (`StaleLeaseReloadTests`).
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
  silent fallback to CPU, to a different provider, or to a guessed path. (`memory_probe` is the
  deliberate exception for a REPORT: a missing figure is `0` = unknown, never a guessed value.)
- **`tensor_container` is the ONE place that decides which container a checkpoint file is.** The
  large-model inventory, the residency hooks' DISK data start and the meta-module filler all go
  through it (`read_container_layout` / `open_tensor_reader`); no other site sniffs a magic,
  branches on a `.gguf` suffix or calls a container parser to read tensors. Both containers
  produce the same `TensorSpan` (offsets relative to the data start, file order) under one layout
  rule, `check_span_layout`: no overlap, and — at reader construction only — inside the file. A
  layout is a HEADER VIEW (sizing works on header-only files); the readers refuse a truncated
  file before the first read.
- **GGUF is parsed and read without `mmap`.** The header parser uses plain `read()`; tensor bytes
  go through `StreamingTensorReader` into a fresh `bytearray` (anonymous memory, one tensor at a
  time), so no ROCm staging is needed and the host peak is one tensor. A block-quantized GGUF
  tensor enters the span contract as `U8` rows `(*rows, bytes_per_row)` with
  `TensorSpan.quant_type`; the large-model planner and residency treat it as ordinary bytes.
  The container is policy-free: IQ / TQ types parse, zero-element tensors get no span (listed
  in `GgufHeader.empty_tensors`, so a model expecting one fails as a missing weight), more than
  4 dims are accepted (ComfyUI-GGUF 5-D weights). Whether a type can be dequantized is the
  consumer's decision. A missing `gguf` package is a `RuntimeError` naming it.
- **The meta-module filler contract** (`fill_module_from_safetensors`): files in the given order,
  each in file order, ONE tensor at a time — when a file tensor is read the filler references no
  earlier one (pinned by `test_safetensors_stream.py` and FLUX.2's characterization tests). Key
  conversion is exactly one form: 1:1 `key_map` (+ `cast`) or 1:N `convert(file_key, tensor) ->
  [(state_key, tensor)]`, optionally with `expected_keys_of(file_key)`. Whenever the keys are known
  from the headers (1:1, or `expected_keys_of`), unexpected / duplicate / missing keys are refused
  before the first read; otherwise per tensor, missing after the last read
  (`MissingWeightsError`, a `ValueError` carrying `missing`). `targets(state_key)` places each key
  (device, or `LEAVE_ON_META`: not written, and a tensor yielding only such keys is not read). The
  default writer checks the shape and copies with `.to(device)` into
  `nn.Parameter(requires_grad=False)` / the buffer; a `write` callback replaces that step per file
  tensor. A CUDA/HIP device is synchronized after every file tensor (measured HIP pile-up of host
  staging buffers otherwise). Byte progress: bytes are counted per read chunk (16 MiB), so one
  large tensor reports several times; reports at most every 0.2 s (five frames per second: the
  UI derives the read speed from consecutive frames), plus exactly one 100% frame after the last
  write. The interval is for checkpoint LOADS only; downloads pace their own frames. On any
  refusal the module is partially filled and must be discarded.
- **`safetensors_stream`, `gguf_stream`, `tensor_container`, `memory_probe` and
  `encoder_truncation` are model-agnostic.** They carry
  model-agnostic checkpoint I/O, memory probes and the hidden-state truncation rule: no
  model-family names and no refusal policy (FLUX.2 klein's fp8-scaled refusal stays in
  `inpaint/flux2_klein/`). The default `SAFETENSORS_DTYPES` deliberately has no 8-bit float
  tokens — a caller that reads them passes `SAFETENSORS_DTYPES_WITH_F8` (or its own table) through
  `dtypes`. Log wording and the
  logger belong to the caller (`owner` / `logger` parameters of `ByteProgress` and
  `memory_snapshot`), so each service's records keep its prefix and land in its own logger tree;
  a service whose tests patch its own `is_torch_available` passes that binding as
  `torch_available`.

## Editing map
- To change how the installation root is found, edit `paths.py` — and only `paths.py`.
- To place a new on-demand side model, use `side_models_root() / "<dir>"` and add its consumer to
  `test_paths.py`.
- To change device selection, sentinel handling, or device-name probing, edit `device_service.py`.
- To change how a torch build is classified (CPU / CUDA / ROCm), edit `torch_flavour.py` and
  `test_torch_flavour.py` — and only there.
- To change how a Torch service turns `General.ai_device` into a device, or how it releases the
  allocator cache, edit `torch_device.py` and `test_torch_device.py`; a per-service difference is a
  keyword argument of the owner, never a local copy.
- To change the small pool's cap or eviction policy, edit `model_manager.py`; to change the
  shared ledger, reclaim order, owner rule or footprint rules, edit `residency/`.
- To add a health field to a service, publish it from that service's `_publish_health_locked()`
  (never read guarded state in `health()`); a new or changed field is an IPC contract change.
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
- To change which safetensors dtypes are mappable by default, what makes a header or tensor entry
  malformed, how a tensor is read (chunk size), or the byte-progress rate limit, edit `safetensors_stream.py`
  and `test_safetensors_stream.py` (and re-run `inpaint/flux2_klein/` tests: FLUX.2 re-binds these
  names). A model-specific refusal belongs in that model's package, not here.
- To change how a GGUF header is parsed, which ggml types become `U8` rows, or how a file's
  container is recognized, edit `gguf_stream.py` / `tensor_container.py` and their tests (and
  `_gguf_test_fixtures.py` when a new type must be writable); re-run `large_models/` tests. A new
  container is a new parser plus one branch in `tensor_container`, never a check at a call site.
- To change how a `meta`-built module is filled from checkpoint files, edit
  `fill_module_from_safetensors` and re-run `inpaint/flux2_klein/` and `inpaint/flux3_klein/`
  tests. It is the one direct-to-device filler: FLUX.3 klein's standalone loaders (`load_dit`,
  `load_autoencoder`, `load_text_encoder*`) use its 1:1 form, while the FLUX.3 service loads through
  the large-model manager (`large_models/loader.py`, the spec's conversion per tensor); FLUX.2 klein
  loads through the manager too, its spec's conversion using the 1:N form (one fused BFL tensor ->
  several diffusers keys).
  A model-specific conversion, writer or refusal wording is a callback or a caller-side re-raise,
  never a branch inside the filler.
- To change the memory snapshot, edit `memory_probe.py`; to change the truncation rule, edit
  `encoder_truncation.py`.
- To wire a new Torch service onto the GPU, do not edit this package: pick the
  `move_module_to` / `patched_module_to` form described above from the service side.
