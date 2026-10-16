# Module: modules/ai_backend/ocr

## Purpose
Backend service adapters that turn raw image bytes into recognized text. Each file wraps one OCR
engine behind the same small API (`health()`, `recognize_image_bytes()`, and `warmup()` except for
Baberu and PaddleOCR-VL, whose model files only arrive with a request), hides the
engine's package/weight/device particulars, and returns the uniform JSON-friendly payload
`{"lines": [str, ...], "text": str}`.

## Architecture
The Rust side never talks to these classes directly. The call chain is
`ipc/handlers/ocr.py` -> `HandlerContext.state.<service>` -> the service in this package; the
handler layer only decodes header fields and the request blob and never constructs a service or
imports an engine module itself. Instances are built once in `server.py` and stored on `AppState`
(`manga_ocr`, `easy_ocr`, `paddle_ocr`, `paddle_vl_ocr`, `surya_ocr`, `baberu_ocr`).

Every service is lazy: importing the module must not import its engine, and the model is loaded on
first recognition. Resident models are leased from `runtime/model_manager.py`
(`begin_model_use` -> `mark_loaded`/`mark_load_failed` -> `release`), so the shared manager can evict
them; an eviction callback that does not own the requested key returns `False`. Every lease passes
`owner=` (the module's `_LEASE_OWNER`) and `hint=` (the residency footprint hint from the files the
load reads: ONNX files x1.25 on the EP's device, Baberu's int8 decoders on the host; MangaOCR's
PyTorch weights sit in the Hugging Face cache and get a device-only hint). A non-absolute directory
is never sized (an empty one would sum the working directory).

Device selection is not decided here. Torch services resolve `General.ai_device` through
`runtime/torch_device.py` (`resolve_selected_backend_device`; `easy.py` binds it with
`accept_mps=False`) and release the allocator cache through its `clear_torch_cache`; the ONNX services take provider/device from
`runtime/device_service.py` or `engines/paddle_onnx.py` helpers. Baberu applies the selection to
its vision graph only and keeps both decoders on the CPU provider (see its bullet).

## Files and submodules
- `manga.py`: MangaOCR (`ocr.manga`). Two runtimes behind one service. The ONNX variants read
  encoder/decoder exports from `ManhwaStudio_AI_Models/ONNX/MangaOCR/{base,2025}` (selected by the
  request's `manga_model`; unknown values fall back to `base_onnx`) and run beam search here rather
  than in transformers. The `base_torch` variant lazily imports the original `manga_ocr` PyTorch
  package and its locally cached `kha-white/manga-ocr-base` weights and never downloads.
- `easy.py`: EasyOCR (`ocr.easy`). Loads `easyocr.Reader(model_storage_directory=<request
  easyocr_model_dir>, download_enabled=False)` and owns the GPU/CPU retry ladder (a missing or
  md5-mismatched weight file is "not downloaded", never retried on the CPU). The language list is
  used verbatim: Rust owns code canonicalization and the script choice. The reader identity (lease
  key) is languages + device + directory. Image decoding happens outside the service lock.
- `paddle.py`: PaddleOCR ONNX (`ocr.paddle`, and `ocr.paddle_onnx` which is served by the same
  service). A thin adapter: all model resolution and session handling lives in
  `engines/paddle_onnx.py`. The request field `paddle_lang` carries a model key such as
  `korean_v5`, not a language.
- `paddle_vl.py`: PaddleOCR-VL (`ocr.paddle_vl`). PyTorch/Transformers vision-language OCR built
  from the vendored classes of `paddle_vl_vendor/` (no `trust_remote_code`); it needs no text
  detection and no language choice (fixed `OCR:` prompt). Each request names the variant
  (`paddle_vl_model`, an opaque `^[a-z0-9_]{1,64}$` label) and its absolute checkpoint directory
  (`paddle_vl_model_dir`); the resident identity is `(model, model_dir, device)` under the lease key
  `paddlevlocr:<model>:<device>`, and `health()` echoes the resident `model`. A request reads its
  device-resolution fallback (the resident or in-flight device) under a short private lock, never
  under the instance lock, so it does not wait for another request's load to pick a device. Holds the
  transformers compat shims, the loading-info guard and the optional `script` restriction.
- `paddle_vl_vendor/`: unmodified, sha256-pinned PaddleOCR-VL-1.6 model code (Apache-2.0) that
  serves every variant; see its `MODULE_README.md`. Never edited.
- `baberu.py`: Baberu OCR (`ocr.baberu`), the backend fallback of the native Rust engine. Runs the
  upstream ONNX export (vision fp16 + int8 decoder prefill/step) with a port of upstream
  `onnx_infer.py` (Pillow BICUBIC 224x224, greedy decode with repetition penalty and run caps).
  **Vision on the selected EP, decoders on CPU** (why: int8 decoder ops fall back to CPU on GPU
  EPs; benchmark 2026-10-06): `vision_provider_settings` maps the ONNX selection (TensorRT ->
  CUDA on the same device, no engine cache), `_build_vision_session` builds it and falls back to
  `CPUExecutionProvider` with a WARN when the provider is unavailable, the build fails, or
  `get_providers()[0]` shows onnxruntime silently dropped it; prefill/step always use
  `CPUExecutionProvider`. Lease key `baberuocr:<provider>:<device>`; a selection change rebuilds
  under the new key and `mark_unloaded`s the old one. Health `device` is the effective vision
  device (`None` when nothing is resident). The decode constants must stay identical to
  `ms_onnx`'s port, and so must the placement rule (`ms_onnx::baberu_ocr::graph_placement`).
  Files arrive as four absolute paths (`baberu_model_files`). Reads Japanese/Chinese/English
  bubbles only, not Korean.
- `result_format.py`: `format_recognition_lines`, the single owner of the `{"lines", "text"}`
  shape for engines that return one raw text block (`paddle_vl.py`, `baberu.py`).
- `surya.py`: Surya OCR (`ocr.surya`). Foundation + recognition predictors from the request's
  `surya_recognition_dir`, plus an optional detection predictor from `surya_detection_dir` for the
  `ocr_with_boxes` task; each is leased under its own model key, and the directory is part of the
  resident identity next to the device.
- `script_constraint.py`: stateful UTF-8 `prefix_allowed_tokens_fn` used only by `paddle_vl.py`.
- `test_lease_hints.py`: the MangaOCR, EasyOCR and Surya hint builders (Baberu's split hint and its
  lease spy are in `test_baberu.py` `LeaseHintTests`).
- `test_manga.py`, `test_paddle_vl.py`, `test_surya.py`, `test_easy.py`, `test_script_constraint.py`,
  `test_baberu.py`, `test_paddle_vl_vendor.py`: unit tests for the contracts below. They fake
  `torch`/`transformers`/`surya`/`easyocr`/`manga_ocr` in `sys.modules` or stub the runtime, so no
  weights, GPU, or heavy packages are needed; the fake download functions raise, so a download
  attempt fails the test. `test_paddle_vl_vendor.py` pins the vendored bytes. `test_easy.py` also
  holds the guard that Rust's EasyOCR table (`crates/ms-sysprobe/fixtures/easyocr_scripts.json`)
  names the file names and md5 sums of the installed `easyocr.config` (skipped when either is
  absent).
- Opt-in real-weight checks, skipped unless their variable is set: `test_baberu.py`
  `RealWeightsTests` (`MS_BABERU_MODEL_DIR`), `test_paddle_vl_real.py` (`MS_PADDLE_VL_DIR`,
  optional `MS_PADDLE_VL_DEVICE`: K/V-cache parity and timing), `test_easy.py` `RealWeightsTests`
  (`MS_EASYOCR_MODEL_DIR`) and `test_surya.py` `RealCheckpointTests` (`MS_SURYA_RECOGNITION_DIR` +
  `MS_SURYA_DETECTION_DIR`); the last two patch the library download function to fail.

## Contracts and invariants
- `__init__.py` must stay a docstring only. Re-exporting any service would make importing one engine
  drag in the dependencies of all the others (and pull the AI stack into torch-free consumers such
  as `ipc/`). Import the concrete module.
- Missing packages, missing weights, and unsupported requests surface as explicit errors with the
  offending path/package in the message. No silent fallback to another engine or another model.
- Model roots: MangaOCR ONNX weights live under `ManhwaStudio_AI_Models/ONNX/MangaOCR`, PaddleOCR
  ONNX weights under `ManhwaStudio_AI_Models/ONNX/PaddleOCR`.
- PaddleOCR-VL (`side_models/PaddleOCR-VL/<variant>/`), Baberu (`side_models/BaberuOCR/`), EasyOCR
  and Surya are downloaded and verified by RUST (`ms_sysprobe::ai_models::external`: pinned
  source, sha256 per file, completion marker); Rust is the only owner of their sources, file lists
  and directories. The backend receives absolute paths per request, loads them offline
  (`local_files_only=True`, plain ORT sessions, `download_enabled=False`, a local Surya
  `checkpoint=`), NEVER downloads, never reads a library or Hugging Face cache for them, and
  answers a missing directory or file with an explicit error naming it ("not downloaded" for a
  missing directory or an incomplete Surya manifest). The weights are only valid for the pinned
  library versions (`easyocr==1.7.2`, `surya-ocr==0.17.1`, `requirements/torch.txt`), whose file
  layout the catalog mirrors. No network-downloaded Python runs for them: PaddleOCR-VL uses
  `paddle_vl_vendor/`.
- Surya: before constructing a predictor the service proves the directory complete
  (`engines/surya_checkpoints.require_local_checkpoint`), points Surya's process-global checkpoint
  settings at it (`point_settings_at`) AND passes it as `checkpoint=`, so no library code can fall
  back to an `s3://` default and download it.
- PaddleOCR-VL refuses a checkpoint whose loading info lists missing, unexpected or mismatched
  keys: transformers would otherwise random-initialize them and return garbage instead of an error.
  Generation always passes `use_cache=True` (the shipped generation configs disable the cache).
- Path depth is not computed here. Anything rooted at the installation directory goes through
  `runtime/paths.py::program_root()`; no `parents[N]` counting in this package.
- ROCm staging obligation (see `runtime/rocm_mmap_transfer.py` for the amdkfd stall):
  - `manga.py` and `paddle_vl.py` own the `nn.Module` and move it with `move_module_to(...)`;
  - `surya.py` cannot reach the transfer (the surya loader moves the weights itself), so predictor
    construction — and nothing else — runs inside `with patched_module_to():`.
  The patch is process-global: the Surya checkpoint presence check stays outside the block, and
  inference must never run inside it. Both helpers are strict no-ops off ROCm.
- MangaOCR's `MangaOcrModel.from_pretrained` and PaddleOCR-VL's `from_pretrained(dtype=<checkpoint
  dtype>)` intentionally request no host-side cast; that is what makes the weights mmap-backed and
  the staging helper necessary. Do not "fix" this by adding a cast.
- The vendored PaddleOCR-VL code was written against transformers 4.55.
  `_ensure_transformers_compat()` must run before `_load_vendored_classes()` imports the modeling
  module and installs signature-guarded, idempotent shims for the `create_causal_mask` keyword
  rename and the `check_model_inputs` decorator-factory change. Both are no-ops when the installed
  API already matches; remove them when the vendored code is moved to an upstream revision that no
  longer needs them.
- Lease protocol (`runtime/MODULE_README.md`): `paddle_vl.py` and `baberu.py` take the lease before
  the instance lock, scope the load in its own `try`, call `mark_loaded()` as soon as the load
  returns (before the script-constraint build and inference) and `release()` in `finally`. Replacing
  a resident model under a DIFFERENT key reports the old key `mark_unloaded`; a failed reload under
  the SAME key whose lease found it resident reports it unloaded too, so the manager never counts a
  model nobody holds.
- `script` (`korean`/`chinese`/`japanese`, `None`/auto to disable) hard-restricts PaddleOCR-VL
  decoding. It cannot be a token allowlist: the SentencePiece tokenizer uses byte_fallback, so CJK
  arrives as script-agnostic `<0xNN>` byte tokens. `script_constraint.py` therefore reconstructs the
  decoded UTF-8 byte stream and allows only continuations whose completed codepoints fall in the
  target ranges (plus whitespace, digits, common punctuation), with EOS allowed only on a complete
  character boundary. Constrained mode also caps `max_new_tokens`, because a hard restriction on
  mismatched input can produce a non-terminating ramble.
- Output shape is fixed: `lines` are trimmed and non-empty, `text` joins them with `\n` (or spaces
  when `join_newlines=False`), and `reflect_strings=True` reverses line order for right-to-left
  column reading.
- Services are shared across dispatcher threads: guard mutable state with the instance lock, and do
  not hold that lock across a download, a network wait, or inference.
- `health()` reads only the service's `runtime.health_snapshot.HealthSnapshot` (plus filesystem
  probes), never the instance lock that a load holds; every change of a reported field is
  republished through `_publish_health_locked()` (contract: `runtime/MODULE_README.md`).
  `test_baberu.py` `HealthDuringLoadTests` pins it.

## Editing map
- To change how an engine is invoked or how its result is normalized, edit that engine's file here.
- To change PaddleOCR ONNX model layout, provider selection, or session/caching, edit
  `../engines/paddle_onnx.py`, not `paddle.py`.
- To change how a Surya checkpoint directory is checked or handed to the library, edit
  `../engines/surya_checkpoints.py`; the detector service under `../detection/` shares it.
- To change the request/response wire shape or add an OCR IPC method, edit `../ipc/handlers/ocr.py`
  (and the protocol constants next to it), then add the matching service method here.
- To change the Baberu decode or preprocessing, edit `baberu.py` AND the native port in
  `crates/ms-onnx/src/baberu_ocr/` together; `test_baberu.py` mirrors the Rust decode cases.
- To change which PaddleOCR-VL, Baberu, EasyOCR or Surya files exist or where they live, edit the
  Rust catalog (`crates/ms-sysprobe/src/ai_models/`), not this package; a library version bump
  needs the catalog, the pins in `requirements/torch.txt` and the installer list together.
- To change where the installation root is, edit `../runtime/paths.py`.
- To change model residency, eviction, or lease behavior, edit `../runtime/model_manager.py`.
- To add or widen a supported writing system for PaddleOCR-VL, edit `script_constraint.py`
  (`_SCRIPT_RANGES` and the alias table) and extend `test_script_constraint.py`.
