# Module: modules/ai_backend/watermark

## Purpose
Visible-watermark detection and removal for the Python AI backend. Three upstream networks are
supported — SLBR (default), WDNet and SplitNet — reached from Rust through the `watermark.*` IPC
methods. The service is constructed in `server.py` and stored on `AppState`, like every other
service domain.

The primary product of this domain is a **mask**, not a cleaned image: the networks localize a
watermark adequately but reconstruct comic line art badly, so the intended flow is
`watermark.detect` → mask editor → one of the project's existing inpainters. `watermark.remove`
(the direct network pass) is the secondary, explicitly experimental path.

## Architecture
```
service.py  WatermarkRemovalService  — params, weights download, residency, detect/remove
     │
     ▼
code_fetch.py                        — pinned manifest, SHA-256 download, sandboxed import,
     │                                 network construction (no weights)
     ▼
../runtime/                          — paths, LoadedModelManager, rocm_mmap_transfer
```
`code_fetch.py` never touches weights and never imports `service.py`. `service.py` is the only
module that imports torch (lazily, inside the methods that need it).

On-disk layout, under `ManhwaStudio_AI_Models/side_models/WatermarkRemoval/`:
```
slbr/     model_best.pth.tar          src/src/networks/{resunet,blocks}.py, src/src/utils/model_init.py
wdnet/    WDNet_G.pkl                 src/{WDNet,unet_parts}.py
splitnet/ 27kpng_model_best.pth.tar   src/scripts/models/{sa_resunet,blocks,rasc,unet}.py,
                                      src/scripts/utils/model_init.py
```
`<model>/src` is the checkout root; upstream repository-relative paths are reproduced verbatim
underneath it (which is where SLBR's doubled `src` segment comes from). The closures use
package-absolute imports and do not import from a flat dump.

## Files and submodules
- `__init__.py`: docstring only, re-exports nothing (domain contract).
- `code_fetch.py`: the pinned manifest (commit SHA + per-file size and SHA-256), the verified
  download, the import machinery, and `build_network()`. Edit this to add a model, change a pin,
  or change how the closures are imported.
- `service.py`: `WatermarkRemovalService`, `normalize_detect_params`, `normalize_remove_params`,
  the Google Drive weight download, and the detect/remove algorithms. Edit this for parameters,
  tiling, device handling or residency.
- `test_code_fetch.py`, `test_service.py`: unit tests. Neither needs torch, weights, a GPU, or
  the network.

## Contracts and invariants

### The network code is downloaded, never vendored
None of the three upstream repositories has a LICENSE file, so their source must not enter this
repository or the distribution. It is fetched onto the user's machine on first use. The rules
this imposes are not negotiable:

- **Commit SHAs are pinned**, never branch names — a moved branch silently changes what the user
  executes.
- **Every downloaded file is SHA-256 verified and a mismatch is fatal.** This is executable code
  arriving over the network; the check is not a warning and has no override.
- **The bytes that are hashed are the bytes that run.** `_load_module` reads each manifest file
  ONCE into memory, hashes that buffer, and `compile()`s + `exec()`s that same buffer. The import
  system is never handed a path, so there is no window in which a file could be swapped between
  verification and execution. `is_code_ready()` exists for `watermark.status` and for deciding
  whether to offer a download; it is explicitly **not** the gate the loader trusts.
- **A downloaded file is never modified.** Adaptation happens around it: pinned-`__path__` package
  stubs, `types.ModuleType` stubs for training-only imports, and a fixed execution order. An
  in-place patch of someone else's file is fragile and is a derivative work.
- **The upstream `__init__.py` files are never downloaded.** They pull `progress.bar`,
  `tensorboardX`, `pytorch_ssim`, `pytorch_iou`, `scipy.misc.imread` and
  `skimage.measure.compare_psnr`, none of which exist in this environment.
- **The manifest is in dependency order and the loader follows it** (`model_init` → `blocks` →
  `rasc` → `unet` → `sa_resunet`, `unet_parts` → `WDNet`, `model_init` → `blocks` → `resunet`).
  Every module is registered before the one that imports it, so each `from <sibling> import ...`
  resolves out of `sys.modules` and never reaches a finder. All ten files import only at module
  level — verified — which is why nothing has to stay registered afterwards.
- Zero `sys.path` entries are added, and a pinned single-entry `__path__` keeps this repository's
  own Rust `src/` directory from being merged into the `src` package as a namespace portion.
- **`sys.modules` is left exactly as it was found.** `_OWNED_NAMESPACES` declares the roots the
  loader shadows per model (`src`, `scripts`, and WDNet's `unet_parts` / `ms_watermark_wdnet_net` /
  `dataloader` / `tensorboardX` / `vgg`); the whole owned subtree is snapshotted before the import
  and restored after it, under `_lock`. This matters concretely: the program root is on `sys.path`
  once LaMa V2 has been used (`inpaint/lama.py::_prepare_runtime_paths`) and the repository root
  contains a directory named `src/`. A pre-existing entry under an owned name is **shadowed, never
  reused** — trusting whatever sits under a name as generic as `unet_parts` or `vgg` would defeat
  the verified-code closure.
- `class WDNet` (the training driver) and `SLBR.multi_gpu()` hardcode `.cuda()`. They are never
  called; only `generator`, `SLBR` and `UnetVMS2AMv4` are.

### Constructor arguments are load-bearing
`build_network()` sets every argument explicitly. SLBR's `options.py` defaults `k_center=1` while
the released checkpoint's own `scripts/test.sh` overrides it to `2` — a 43 459-parameter
difference that fails `load_state_dict`. `use_refine=True`, `k_refine=3` and `k_skip_stage=3` are
equally mandatory. Verified parameter counts: SLBR 21 390 953, WDNet 21 043 848,
SplitNet 32 606 396.

### Weight integrity is only partially verifiable today
The three checkpoints come from their authors' original Google Drive locations (they are not
re-hosted — that would be republication of unlicensed weights). **Their SHA-256 digests are not
known yet**: Drive was unreachable when the manifest was captured, so `_WEIGHT_SHA256` holds
`None` for all three, meaning "unverified", and the digest comparison is skipped with a log line
saying so. Fill an entry in once a real download has been hashed; never invent a value.

Independently of that, `_validate_checkpoint_magic` runs on **every** download and **every** load
and rejects anything whose first bytes are neither a zip container nor a protocol≥2 pickle stream.
That is what catches a Google Drive HTML interstitial silently saved as a `.pth.tar`, which is the
realistic failure mode here.

Checkpoints load with `map_location="cpu"` (their tensors are tagged `cuda:0`) and an **explicit**
`weights_only=True` — not inherited from the installed torch's default, which only flipped in
torch 2.6 — read `checkpoint["state_dict"]` when present, and strip a `module.` prefix
defensively. `load_state_dict` is strict: a partially loaded network produces garbage, not a
degraded result. Torch's restricted unpickler is **never** bypassed — its refusal is turned into
an explicit error instead.

Checkpoint downloads go through `../engines/model_download.py`, shared with `inpaint/flux_fill.py`:
serialized per destination path, staged into a **process-private** `<name>.<pid>.part`, gated by
`verify` (the magic-byte check plus the optional SHA-256) and only then published with `os.replace`.
`ensure_model_assets` deliberately runs outside the service lock (a download must never block
`health()`, `unload()` or an eviction) and the IPC layer dispatches onto a thread pool, so without
that serialization two concurrent first-use requests would stream into one destination. The waiter
re-checks after acquiring the lock rather than refetching 80–130 MiB. `service.py` still owns the
transport (the `requests.Session` and Drive's confirm interstitial) and the integrity gate.

### Resolution strategy
- **`detect` never tiles.** The image is downscaled so its long side is `downscale_to`
  (256/512/768, default 512), reflect-padded to a square multiple of 16, run once, cropped,
  upscaled back bilinearly, thresholded and dilated. The mask branch has to see the whole
  watermark; a tile of a webtoon page usually cannot. The returned mask is at the SOURCE
  resolution. Small images are not upscaled.
- **`remove` tiles** with square tiles that are multiples of 16 (`tile`, default 512) overlapping
  by `overlap` px (default 64), blended with a raised-cosine feather and normalized by the
  accumulated weight so the blend is an exact partition of unity at the borders too. Each tile is
  composed as `out = pred * mask + input * (1 - mask)`; **skipping that composition is a known
  upstream defect** (colored artifacts outside the mask — the loss never constrains that region).
  One `generate` progress frame per tile.
- **One padding policy covers all three models**: reflect-pad to a square multiple of 16. SLBR
  needs square *and* `% 16 == 0` (three `size=x.shape[2:][::-1]` calls swap H and W upstream),
  SplitNet needs `% 16 == 0`, WDNet needs nothing. Padding all three costs a little wasted compute
  and removes every special case, so the upstream H/W-swap bug is never hit and no source edit is
  needed.
- Inputs are fed as `[0, 1]` RGB with no mean/std normalization — what all three upstream
  inference paths do, and what their `clamp(0, 1)` and long-skip additions assume.
- Output tuples differ per model and are unpacked in `_select_outputs`: SLBR
  `([refined, coarse], [mask, …], [watermark])`, SplitNet `([refined, coarse], mask, watermark)`,
  WDNet `(image, mask, alpha, watermark, intermediate)`.

### Service shape
- Construction is cheap; torch is imported lazily inside the methods that need it.
- One resident network at a time under an `RLock`, key `watermark:<model>:<device>`, leased from
  `LoadedModelManager` with the full four-call protocol (`begin_model_use` → `mark_loaded` /
  `mark_load_failed` → `release`). `_unload_key` refuses a foreign key; `unload()` reports the
  dropped key with `mark_unloaded`.
- **`mark_load_failed()` is reserved for a failed LOAD.** `_lease_and_run` wraps
  `_ensure_model_locked` in its own `try` and calls `mark_loaded()` as soon as it returns, before
  `run(net)`; a failure inside `run` only records `_last_error` and re-raises. Reporting a failed
  forward as a failed load reaches `abort_load`, which clears `resident` and drops the entry's
  unload callback while the weights still occupy the device — the manager would then under-count
  residency and could never evict that network again. `release()` still runs in `finally` either
  way. Same shape in all five inpaint services.
- **The lease is taken before `self._lock`, never inside it.** `begin_model_use` can block while
  another thread's eviction callback waits for this service's lock; the reverse order deadlocks.
- The device comes from `General.ai_device` through the module-local
  `_resolve_selected_backend_device()` (`not-selected` resolves to a runtime default). Unlike
  `flux_fill.py` this domain does **not** pin itself to a discrete GPU: the networks are small and
  must stay usable on CPU.
- Weight moves use `runtime.rocm_mmap_transfer.move_module_to` (Form 1). The download always
  completes first — the staging context must never wrap a download.
- An unknown model id raises; it is never silently replaced by the default. An absent or empty
  `model` parameter means `DEFAULT_MODEL`.
- Progress is `progress_callback(phase, step, total, label)` with `phase` in
  `{"download", "generate"}`, matching the FLUX contract. A raising callback is logged at debug
  level and never aborts the job.
- User-facing message strings are Russian; code, comments and this document are English.

## Editing map
- To change the pinned upstream commits, the file manifest or the import/stub machinery, see
  `code_fetch.py` (`_REPOS`, `_NETWORK_MODULE`, `_PACKAGE_STUBS`, `_MODULE_STUBS`,
  `_OWNED_NAMESPACES`).
- To add a fourth model: add a `_RepoSpec` **in dependency order** with a `module` name per file,
  plus `_NETWORK_MODULE`, `_OWNED_NAMESPACES`, `_PACKAGE_STUBS`, `_MODULE_STUBS` entries and a
  `build_network` branch in `code_fetch.py`; a `_WeightSpec` and a `_WEIGHT_SHA256` entry in
  `service.py`; and an output case in `_select_outputs`.
- To record a checkpoint digest once one is known: `_WEIGHT_SHA256` in `service.py`.
- To change parameters, clamping, tiling geometry or the feather: `normalize_*_params`,
  `_plan_tiles`, `_axis_offsets`, `_feather_window` in `service.py`.
- To change the Drive download or the confirm flow: `_download_from_google_drive` /
  `_build_confirm_url` in `service.py`. To change the staging/serialization/atomic-publish envelope
  itself, edit `../engines/model_download.py` — `inpaint/flux_fill.py` shares it.
- The IPC surface (`watermark.*` handlers, method constants) lives in `../ipc/`, not here.
