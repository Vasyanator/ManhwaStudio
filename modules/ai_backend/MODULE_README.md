# Module: modules/ai_backend

## Purpose
Python AI backend runtime called by the Rust application over a framed, multiplexed, bidirectional
IPC protocol on a single AF_UNIX domain socket.

## Architecture
`server.py` wires OCR, text detection, inpainting, translation, device selection, version health
metadata, shared loaded-model management, and the in-process web-scraping browser session. The
server uses the `ipc/` framing layer (see `ipc/PROTOCOL.md`) — a length-prefixed frame protocol over
an AF_UNIX stream socket — not HTTP.

The advanced web-scraping downloaders (Selenium / CloakBrowser) used to run as separate stdio
processes under `modules/new_project/`; they are now folded into this backend. `browser/service.py`
(`BrowserService`, on `AppState.browser`) drives the existing daemon classes in-process and is
exposed through the single IPC method `browser.command` (`ipc/handlers/browser.py`). Downloaded
images are still handed to the launcher as an on-disk directory path + count — no image bytes travel
over IPC for downloads. Selenium/Playwright are imported lazily on first browser use so AI-only
startup never loads them.
Torch-backed services load weights from `ManhwaStudio_AI_Models/Torch`; ONNX-backed services load
weights from `ManhwaStudio_AI_Models/ONNX`. Library-managed model downloads and caches, such as
EasyOCR and Surya, must remain under those libraries' default paths unless a service explicitly owns
the download contract.

## Files and submodules
- `browser/service.py`: `BrowserService` — hosts the Selenium/CloakBrowser scraping session
  in-process (driving the daemon classes from `modules/new_project/`), behind the `browser.command`
  IPC method (`ipc/handlers/browser.py`).
- `server.py`: service construction, `AppState` wiring, and framed IPC server startup (delegates to
  `ipc/frame_server.py`). The legacy HTTP server has been removed; all IPC goes through the `ipc/`
  package. See `ipc/MODULE_README.md` and `ipc/PROTOCOL.md` for the transport contract.
- `ipc/`: framed, multiplexed, bidirectional IPC layer. See `ipc/MODULE_README.md`.
- `model_manager.py`: shared resident-model lease and unload manager.
- `device_service.py`: selected Torch/ONNX device state, accelerated defaults, and manual
  selection flags when backend devices need a user choice.
- `test_device_service.py`: unit tests for backend device selection sentinel and fallback
  contracts.
- `test_reline_service.py`: unit tests for Reline catalog-name, archive-name, and direct-URL
  model resolution.
- `*_service.py`: backend service adapters for OCR, detectors, inpaint, and MT.
- `paddle_onnx_runtime.py`: shared ONNX Runtime helpers for PaddleOCR.
- `paddle_vl_ocr_service.py`: PaddleOCR-VL OCR backend (IPC method `ocr.paddle_vl`). PyTorch/Transformers-only
  vision-language OCR loaded with `trust_remote_code=True`; needs no text detection and no language
  selection (fixed `OCR:` prompt). Weights are fetched into the Hugging Face hub cache on first use,
  not the app model tree. The model's remote code was saved with transformers 4.55; before
  `from_pretrained` the service installs signature-guarded compat shims (`_ensure_transformers_compat`)
  for the `create_causal_mask` keyword rename and the `check_model_inputs` factory change so it runs on
  the app's transformers 4.57.x without a global downgrade (no-ops when the API already matches).
  An optional `script` (`korean`/`chinese`/`japanese`) hard-restricts decoding to one writing system
  via `script_constraint.py` to curb hallucination on messy/handwritten text; constrained mode caps
  `max_new_tokens` to avoid non-terminating rambles. `test_paddle_vl_ocr_service.py` covers the text
  post-processing contract.
- `script_constraint.py`: stateful UTF-8 `prefix_allowed_tokens_fn` for PaddleOCR-VL. The SentencePiece
  tokenizer uses byte_fallback (CJK comes out as script-agnostic `<0xNN>` byte tokens), so a plain
  token allowlist cannot express "one script"; this reconstructs the decoded byte stream and only
  allows byte continuations whose completed codepoints fall in the target Unicode ranges (plus
  whitespace/digits/punctuation, and EOS only on a character boundary). `test_script_constraint.py`
  covers it.
- `rocm_runtime.py`: ROCm/HIP MIOpen runtime tuning. `configure_rocm_runtime()` runs once at
  backend startup; on a ROCm Torch build (`torch.version.hip` set) it switches MIOpen to immediate
  mode (`MIOPEN_FIND_MODE=FAST`) to avoid per-input-shape kernel auto-tuning/compilation, disables
  cudnn/MIOpen benchmark, and pins the MIOpen user/kernel cache under
  `ManhwaStudio_AI_Models/.cache/miopen`. No-op on CUDA/CPU/MPS/absent-Torch installs; env defaults
  use `setdefault` so explicit user overrides win.
- `test_rocm_runtime.py`: unit tests for `configure_rocm_runtime` no-op and ROCm-path behavior.
- `reline_service.py`: Reline pipeline adapter, catalog-backed model downloader, and
  `reline.process` IPC method handler backend.
- `sdxl_inpaint_service.py`: SDXL inpaint backend (IPC method `inpaint.sdxl`). Lazily builds a
  `StableDiffusionXLInpaintPipeline` from a local ckpt/safetensors or a HF repo id and caches it
  through the shared model manager. `nine_channel` mode requires a 9-channel inpaint UNet (full
  denoise); `four_channel` mode requires a 4-channel UNet and prefills the hole with the shared
  `LamaInpaintService` before a moderate-denoise latent-blend pass. The mode/channel mismatch is an
  explicit error. Normalizes generation params, maps sampler names to diffusers schedulers, dilates
  and blurs the mask, and composites the output over the original outside the mask. When a
  `progress_callback` is supplied, a diffusers `callback_on_step_end` emits a cheap linear
  latent->RGB preview each step; the `inpaint.sdxl` IPC handler streams these as `progress` frames
  (each with an optional latent preview PNG blob) followed by a terminal `response` instead of a
  single JSON response. See `ipc/handlers/sdxl.py` and `ipc/PROTOCOL.md §5.4`.
- `test_sdxl_inpaint_service.py`: pure-Python unit tests for SDXL param normalization and sampler
  mapping (no torch/diffusers required).
- `flux_fill_inpaint_service.py`: FLUX.1-Fill-dev inpaint/object-removal backend (IPC methods
  `inpaint.flux_fill` streaming, `.unload`, `.status`). Downloads on demand into
  `ManhwaStudio_AI_Models/side_models/FLUX.1-Fill-dev-GGUF/` (NOT the HF cache): the chosen GGUF
  quant from `YarvixPA/FLUX.1-Fill-dev-GGUF` plus diffusers components (VAE/CLIP/T5/scheduler) from
  the open `ostris/Flex.1-alpha` repo, with byte-level download progress. Builds a `FluxFillPipeline`
  from the local GGUF transformer + components, pinned to the DISCRETE GPU (the Ryzen iGPU is
  excluded) with MIOpen immediate mode. Mask dilation + Poisson (`cv2.seamlessClone`) tone matching
  remove the dark-patch seam. `progress_callback(phase, step, total, label)` distinguishes the
  `download` and `generate` phases; the `inpaint.flux_fill` handler streams these as `progress`
  frames (header `phase`/`step`/`total`/`label`, no preview blob). See `ipc/handlers/flux_fill.py`.
- `textdetector/`: ComicTextDetector implementation used by CTD service.

## Contracts and invariants
- Service initialization is lazy and must surface missing packages or weights as explicit errors.
- Torch and ONNX model roots are separate; do not write ONNX weights under `Torch/` or Torch
  checkpoints under `ONNX/`.
- When no device config exists, CPU is only a temporary fallback. PyTorch should report an
  unresolved device choice as soon as CUDA is available, and ONNX should prefer DirectML on Windows
  when that provider exists. Unconfigured DirectML selection is reported via the `device.get` IPC
  method as needing manual confirmation before it is persisted.
- `General.ai_device`, `General.ai_onnx_provider`, and `General.ai_onnx_device_id` use
  `not-selected` as the default config sentinel. Backend services must resolve it to a real runtime
  default before constructing Torch devices or ONNX provider settings.
- EasyOCR, Surya, and PaddleOCR-VL should use their own library/Hugging Face caches because those
  packages own the download behavior.
- SDXL inpaint requires `diffusers`/`transformers`; they are imported lazily and a missing package,
  missing weights, or a mode/UNet channel mismatch surfaces as an explicit error. The service must
  not silently fall back to the wrong channel mode. SDXL weights are user-supplied (arbitrary path
  or HF repo), so the service owns no fixed model directory.
- Reline checkpoints are Torch files kept under `ManhwaStudio_AI_Models/side_models/Reline`; the
  `reline` package receives local model paths only, so catalog downloads are owned by
  `reline_service.py`. Models missing from the remote catalog can be exposed via the built-in
  `EXTRA_MODELS` list; entries without a direct `url` must be placed manually under that directory.
- Long-running model inference runs outside the Rust GUI thread through backend requests.
- On ROCm Torch builds, MIOpen runtime tuning is configured once at process startup by
  `rocm_runtime.configure_rocm_runtime()` before any inference; Torch services must not depend on a
  specific MIOpen Find mode and must not re-enable cudnn benchmark.
- The `health` IPC method and `TOPIC_HEALTH` event push must include `backend_version` from root
  `config.VERSION`; Rust uses it to warn when the backend package and Studio binary versions do not
  match.
- The backend listens on a single AF_UNIX socket (the framed IPC transport; no HTTP server). The
  default path is per-platform (`/tmp/manhwastudio_backend_socket` on posix,
  `tempfile.gettempdir()/manhwastudio_backend_socket` on Windows) and must match the Rust side
  byte-for-byte; `--socket PATH` overrides it and is optional. A single live instance is enforced
  by stale-socket detection: a live peer on the path raises `FrameBackendInstanceError` (in
  `ipc/frame_server.py`), a stale socket file is unlinked before bind. On posix the socket file is
  `chmod 0o600` and it is unlinked on shutdown. AF_UNIX is required; a Python build without it
  fails with a clear error (Windows 10 1803+ and a modern CPython are required).

## Editing map
- To change model root resolution, edit `config.py` and the affected service resolver.
- To change PaddleOCR ONNX layout, edit `paddle_onnx_runtime.py`.
- To change inpaint checkpoint handling, edit the corresponding inpaint service.
- To change Reline model catalog resolution, download/extract behavior, or pipeline JSON mapping,
  edit `reline_service.py`.
