# Module: modules/ai_backend/engines

## Purpose
Model-family runtime and model-acquisition plumbing shared by **more than one service domain**. A
module belongs here only when at least two of `ocr/`, `detection/`, `inpaint/`, `watermark/`,
`reline/`, `translate/` depend on it; anything used by a single domain belongs inside that domain's
package instead.

All three current members earn their place:
- `paddle_onnx` is used by the PaddleOCR recognizer (`ocr/paddle.py`) **and** the PaddleOCR text
  detector (`detection/paddle.py`);
- `surya_checkpoints` is used by the Surya OCR service (`ocr/surya.py`) **and** the Surya text
  detector (`detection/surya.py`);
- `model_download` is used by the FLUX.1-Fill service (`inpaint/flux_fill.py`), the FLUX.2 klein
  downloader (`inpaint/flux2_download.py`) **and** the watermark-removal service
  (`watermark/service.py`).

That is why none of them lives under `ocr/`, `detection/` or `inpaint/`: putting one in a single
domain would make the other domain import across a sibling boundary for shared machinery.

## Architecture
Engines sit between `runtime/` (device selection, resident-model leases, program root) and the
service domains. They own the model-family specifics — file layout, session construction, checkpoint
fetching — and expose a small typed API that the domain adapters call.

Dependency direction: `engines/` may import from `runtime/`; it must never import from `ocr/`,
`detection/`, `inpaint/`, `reline/`, `translate/`, `ipc/`, or `server.py`. `runtime/rocm_runtime.py`
holds the one permitted reverse edge, a lazy `from ..engines.paddle_onnx import
resolve_compiled_cache_root` inside a `try` — it must stay lazy so a Torch-only install never pays
for cv2/onnxruntime at startup.

## Files and submodules
- `paddle_onnx.py`: shared ONNX Runtime engine for PaddleOCR. Resolves the model files under
  `ManhwaStudio_AI_Models/ONNX/PaddleOCR`, builds ORT sessions for the selected Execution Provider
  and device id, runs the PP-OCR recognition pipeline and the forward-only detection pass
  (`PaddleOnnxRuntime.forward_det`; `normalize_det_rgb` is the one owner of the detection input
  normalization) without any Paddle dependency,
  reuses sessions across backend requests through `runtime/model_manager.py`, and configures the
  compiled-kernel cache directory used by MiGraphX. Key entry points: `resolve_model_paths()`,
  `resolve_det_model_path()`, `resolve_models_root()`, `resolve_compiled_cache_root()`,
  `provider_attempts()`, `RuntimeFactory`, `PaddleOnnxRuntime`.
- `surya_checkpoints.py`: presence check and **eager** download of the `s3://` checkpoints the Surya
  services use — `checkpoint_local_dir()`, `checkpoint_ready()`, `ensure_checkpoint_downloaded()`.
  The Surya package is imported lazily inside each function.
- `model_download.py`: the staged-download envelope every self-downloading service shares —
  `download_to_path()` (per-destination lock, re-check under it, process-private
  `<name>.<pid>.part` staging, caller-supplied `verify` gate, atomic `os.replace`) and
  `stream_response_to_file()` (response body → file with cumulative byte progress). It owns no
  GENERIC transport: a caller passes a `fetch(staging)` callable, because Google Drive needs a
  `requests.Session` carrying the confirm cookie. The Hugging Face composition of the two IS owned
  here, as `download_bearer_to_path()` — bearer header + streaming GET + staged publish — because
  both Hugging Face callers (`inpaint/flux_fill.py` and `inpaint/flux2_download.py`) need exactly
  it and a second private copy is what that function exists to prevent. It takes the token as an
  ARGUMENT and never reads the environment: one caller reads `HF_TOKEN`, the other receives the
  token as a request field, and this layer must not decide which.
- `test_surya_checkpoints.py`, `test_model_download.py`: unit tests for the contracts above. A fake
  `surya.common.s3` is injected into `sys.modules` for the former, and a fake `requests` module for
  the `download_bearer_to_path` cases of the latter; neither touches the network.

## Contracts and invariants
- `__init__.py` re-exports nothing and imports no submodule. `paddle_onnx` pulls cv2/numpy/
  onnxruntime at import time, so a re-export would make every `engines.*` import pay for the AI
  stack; keeping the package import-cheap is what lets the torch-free `ipc/` layer and its tests stay
  importable — the same reason the parent `modules/ai_backend/__init__.py` resolves `run_server`
  lazily via PEP 562.
- Model roots come from `runtime/paths.program_root()`. No module here may re-derive the
  installation root with `Path(__file__).resolve().parents[N]`; `paths.py` is its single owner.
- Torch and ONNX model roots are separate trees. `paddle_onnx` owns
  `ManhwaStudio_AI_Models/ONNX/PaddleOCR` only, and must never read Torch checkpoints or write ONNX
  weights outside `ONNX/`.
- `resolve_models_root()` probes a fixed candidate order — current working directory, program root,
  then the parent of each — and returns the first that exists, falling back to the first candidate so
  a missing model is reported against the expected in-CWD location. Changing that order changes which
  installation a running backend picks up; treat it as a contract, not an implementation detail.
- No model auto-download in `paddle_onnx`: missing weights are an explicit `FileNotFoundError` naming
  the expected path. The selected Execution Provider is always used directly and an initialization
  error is surfaced, never silently downgraded to CPU.
- `surya_checkpoints` exists so the (potentially minutes-long, network-bound) `s3://` download runs
  **before** `runtime/rocm_mmap_transfer.patched_module_to()` is entered and never inside it — that
  helper's contract forbids holding its process-global `torch.nn.Module.to` patch around network I/O.
  A Surya service must call `ensure_checkpoint_downloaded()` first and keep only the weight transfer
  inside the patch.
- **Resume is OPT-IN (`resumable=True`) and changes only what happens to the bytes of a FAILED
  transfer.** By default they are unlinked, which is what `inpaint/flux_fill.py` has always relied
  on and what its tests now pin explicitly. With `resumable=True` a failure renames them to a
  stable `<name>.part` and the next run renames that back into its own pid-scoped staging file.
  The RENAME is the claim, and it is what preserves the pid scoping: a parked partial has exactly
  one owner at a time, and of two processes racing for it one wins and the other simply starts from
  zero. Two processes failing on the same file at once can leave the shorter of two partials — a
  bounded re-transfer, never a correctness problem, because the publish-time gate is what
  guarantees the result. A missing parked file makes the claim REMOVE any pid-scoped leftover
  rather than resume it: such a file can only come from a crashed process that shared our pid.
- **`Range` handling is the caller's, and every legal answer must be covered.** `_bearer_attempt`
  sends `Range: bytes=<n>-` only when bytes were claimed, and then: `206` appends; **`200` means
  the server IGNORED the range, so the file is truncated and rewritten from zero** — appending a
  whole body to existing bytes is how a silently double-length file gets produced and reported as a
  success; `416` means the partial is at or past the file length, so it is discarded and the file
  refetched; anything else takes the existing `raise_for_status` path.
- **`stream_response_to_file`'s `mode` and `initial_done` must agree.** `mode="ab"` without
  `initial_done` reports a resumed file restarting at zero (a backwards-jumping bar and a nonsense
  transfer rate for any consumer deriving speed from consecutive frames); `initial_done` without
  `mode="ab"` truncates the very bytes it meant to keep. `expected` is the WHOLE file, because a
  range response announces only the length of its range.
- **`download_bearer_to_path` compares nothing itself.** Neither it nor `stream_response_to_file`
  checks the received bytes against `Content-Length`, so a body that ends CLEANLY but short reaches
  the publish as a success and is atomically renamed to the final name, where it is
  indistinguishable from a complete file. A caller that knows the expected length MUST pass a
  `verify` (`inpaint/flux2_download.py` passes a length gate built from the hub listing); the
  primitive runs it on the staged bytes before the rename and deletes them when it raises.
- `download_bearer_to_path`'s `on_chunk` **may raise to abort the transfer**, and that is the
  supported cancellation mechanism: `stream_response_to_file` has no cancel hook, so an exception
  from the chunk callback propagates out of `fetch` and `download_to_path`'s `finally` removes the
  staging file. Nothing partial is ever published. Do not add a cancel parameter here instead — the
  caller already owns the signal, and a second mechanism would give two answers to one question.
- `model_download`'s lock is a **download lock, not a service lock**: it is held across network I/O
  and must never be nested inside a service-local lock, because a multi-GiB transfer must not block
  `health()`, `unload()` or a `LoadedModelManager` eviction callback. Its per-destination locks are
  created on demand and never removed. A destination only ever appears complete on disk: `verify`
  runs on the staging file and a failed or rejected transfer removes it and leaves any previous
  destination untouched.
- `checkpoint_local_dir()` returns `""` and `checkpoint_ready()` returns `False` when the Surya
  package is not importable. Both mean "cannot tell" and callers must read them as "download it",
  never as an optimistic yes or a guessed path. Surya owns its own cache layout; do not relocate it.
- Every missing-package path raises with context instead of guessing.

## Editing map
- To change the PaddleOCR ONNX file layout or model-root resolution, edit `paddle_onnx.py`
  (`resolve_models_root` / `resolve_model_paths`).
- To change ORT provider selection, session options, or the MiGraphX compiled-kernel cache, edit
  `paddle_onnx.py` (`provider_attempts`, `RuntimeFactory`, `resolve_compiled_cache_root`).
- To change how Surya checkpoints are located or fetched, edit `surya_checkpoints.py`; both Surya
  services share it, so update `test_surya_checkpoints.py` in the same change.
- To change the staging name, the serialization granularity or the publish step of an on-demand
  weight download, edit `model_download.py`; FLUX and watermark removal both ride on it, so update
  `test_model_download.py` in the same change. The transport and the integrity gate stay in the
  calling service.
- To add a new engine here, first confirm two or more service domains will use it — otherwise it
  belongs in the single domain package that needs it.
