# Module: modules/ai_backend/ocr

## Purpose
Backend service adapters that turn raw image bytes into recognized text. Each file wraps one OCR
engine behind the same small API (`health()`, `warmup()`, `recognize_image_bytes()`), hides the
engine's package/weight/device particulars, and returns the uniform JSON-friendly payload
`{"lines": [str, ...], "text": str}`.

## Architecture
The Rust side never talks to these classes directly. The call chain is
`ipc/handlers/ocr.py` -> `HandlerContext.state.<service>` -> the service in this package; the
handler layer only decodes header fields and the request blob and never constructs a service or
imports an engine module itself. Instances are built once in `server.py` and stored on `AppState`
(`manga_ocr`, `easy_ocr`, `paddle_ocr`, `paddle_vl_ocr`, `surya_ocr`).

Every service is lazy: importing the module must not import its engine, and the model is loaded on
first recognition. Resident models are leased from `runtime/model_manager.py`
(`begin_model_use` -> `mark_loaded`/`mark_load_failed` -> `release`), so the shared manager can evict
them; an eviction callback that does not own the requested key returns `False`.

Device selection is not decided here. Torch services resolve `General.ai_device` through
`AIDevice.detect_available_devices()`; the ONNX services take provider/device from
`runtime/device_service.py` or `engines/paddle_onnx.py` helpers.

## Files and submodules
- `manga.py`: MangaOCR (`ocr.manga`). Two runtimes behind one service. The ONNX variants read
  encoder/decoder exports from `ManhwaStudio_AI_Models/ONNX/MangaOCR/{base,2025}` (selected by the
  request's `manga_model`; unknown values fall back to `base_onnx`) and run beam search here rather
  than in transformers. The `base_torch` variant lazily imports the original `manga_ocr` PyTorch
  package and its locally cached `kha-white/manga-ocr-base` weights and never downloads.
- `easy.py`: EasyOCR (`ocr.easy`). Owns language-code normalization, the GPU/CPU retry ladder, and
  the Windows-standalone SSL fallbacks (certifi CA bundle, then optional unverified HTTPS gated by
  `MF_EASYOCR_INSECURE_SSL_FALLBACK`). Image decoding happens outside the service lock.
- `paddle.py`: PaddleOCR ONNX (`ocr.paddle`, and `ocr.paddle_onnx` which is served by the same
  service). A thin adapter: all model resolution and session handling lives in
  `engines/paddle_onnx.py`. The request field `paddle_lang` carries a model key such as
  `korean_v5`, not a language.
- `paddle_vl.py`: PaddleOCR-VL (`ocr.paddle_vl`). PyTorch/Transformers vision-language OCR loaded
  with `trust_remote_code=True`; it needs no text detection and no language choice (fixed `OCR:`
  prompt). Holds the transformers compat shims and the optional `script` restriction.
- `surya.py`: Surya OCR (`ocr.surya`). Foundation + recognition predictors, plus an optional
  detection predictor for the `ocr_with_boxes` task; each is leased under its own model key.
- `script_constraint.py`: stateful UTF-8 `prefix_allowed_tokens_fn` used only by `paddle_vl.py`.
- `test_manga.py`, `test_paddle_vl.py`, `test_surya.py`, `test_script_constraint.py`: unit tests for
  the contracts below. They fake `torch`/`transformers`/`surya`/`manga_ocr` in `sys.modules`, so no
  weights, GPU, or heavy packages are needed.

## Contracts and invariants
- `__init__.py` must stay a docstring only. Re-exporting any service would make importing one engine
  drag in the dependencies of all the others (and pull the AI stack into torch-free consumers such
  as `ipc/`). Import the concrete module.
- Missing packages, missing weights, and unsupported requests surface as explicit errors with the
  offending path/package in the message. No silent fallback to another engine or another model.
- Model roots: MangaOCR ONNX weights live under `ManhwaStudio_AI_Models/ONNX/MangaOCR`, PaddleOCR
  ONNX weights under `ManhwaStudio_AI_Models/ONNX/PaddleOCR`. EasyOCR, Surya, and PaddleOCR-VL
  deliberately use their own library / Hugging Face caches, because those packages own the download
  behavior — do not redirect them into the app model tree.
- Path depth is not computed here. Anything rooted at the installation directory goes through
  `runtime/paths.py::program_root()`; no `parents[N]` counting in this package.
- ROCm staging obligation (see `runtime/rocm_mmap_transfer.py` for the amdkfd stall):
  - `manga.py` and `paddle_vl.py` own the `nn.Module` and move it with `move_module_to(...)`;
  - `surya.py` cannot reach the transfer (the surya loader moves the weights itself), so predictor
    construction — and nothing else — runs inside `with patched_module_to():`.
  The patch is process-global: the Surya checkpoint download
  (`engines/surya_checkpoints.ensure_checkpoint_downloaded`) must stay outside the block, and
  inference must never run inside it. Both helpers are strict no-ops off ROCm.
- MangaOCR's `MangaOcrModel.from_pretrained` and PaddleOCR-VL's `from_pretrained(dtype=<checkpoint
  dtype>)` intentionally request no host-side cast; that is what makes the weights mmap-backed and
  the staging helper necessary. Do not "fix" this by adding a cast.
- PaddleOCR-VL runs remote code saved with transformers 4.55. `_ensure_transformers_compat()` must
  run before `from_pretrained` and installs signature-guarded, idempotent shims for the
  `create_causal_mask` keyword rename and the `check_model_inputs` decorator-factory change. Both are
  no-ops when the installed API already matches; remove them once the upstream remote code catches
  up.
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

## Editing map
- To change how an engine is invoked or how its result is normalized, edit that engine's file here.
- To change PaddleOCR ONNX model layout, provider selection, or session/caching, edit
  `../engines/paddle_onnx.py`, not `paddle.py`.
- To change Surya checkpoint location or download behavior, edit `../engines/surya_checkpoints.py`;
  the detector service under `../detection/` shares it.
- To change the request/response wire shape or add an OCR IPC method, edit `../ipc/handlers/ocr.py`
  (and the protocol constants next to it), then add the matching service method here.
- To change where the installation root is, edit `../runtime/paths.py`.
- To change model residency, eviction, or lease behavior, edit `../runtime/model_manager.py`.
- To add or widen a supported writing system for PaddleOCR-VL, edit `script_constraint.py`
  (`_SCRIPT_RANGES` and the alias table) and extend `test_script_constraint.py`.
