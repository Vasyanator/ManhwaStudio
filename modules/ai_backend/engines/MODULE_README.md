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
- `model_download` is used by the FLUX.1-Fill service (`inpaint/flux_fill.py`), the FLUX.2 / FLUX.3 klein
  downloads (`inpaint/hf_gated_download.py`), the watermark-removal domain (`watermark/service.py`
  weights, `watermark/code_fetch.py` pinned source files) **and** Reline (`reline/service.py`).

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
  reuses sessions across backend requests through `runtime/model_manager.py` (owner = the
  per-session key, hint = the `.onnx` file x1.25 on `ProviderSettings.footprint_device()`,
  the one EP -> residency-device rule the ONNX OCR services share), and configures the
  compiled-kernel cache directory used by MiGraphX. Key entry points: `resolve_model_paths()`,
  `resolve_det_model_path()`, `resolve_models_root()`, `resolve_compiled_cache_root()`,
  `provider_attempts()`, `RuntimeFactory`, `PaddleOnnxRuntime`.
- `surya_checkpoints.py`: presence check of the Rust-downloaded Surya checkpoint directories and the
  one place that points Surya's process-global checkpoint settings at them — `checkpoint_ready()`,
  `require_local_checkpoint()`, `point_settings_at()`. It never downloads. The Surya package is
  imported lazily inside each function.
- `model_download.py`: the staged-download envelope every self-downloading service shares —
  `download_to_path()` (per-destination lock, re-check under it, process-private
  `<name>.<pid>.part` staging, caller-supplied `verify` gate, atomic `os.replace`). The one
  retrying HTTP transfer is owned here, as `download_http_to_path()` — streaming GET, retry with
  backoff, `Range` resume, size / sha256 gate, cancellation, staged publish — with three seams for
  transport specifics: `headers` (Reline's User-Agent), `follow` (Google Drive's confirm form,
  parsed in `watermark/service.py`) and `get` (the Drive caller passes a `requests.Session.get`
  carrying the confirm cookie), plus a caller `verify` gate (the checkpoint magic bytes). It is the
  Python reference for `PROJECT_RULES.md` "Network downloads"; a private download loop in a caller
  is what it exists to prevent. `download_bearer_to_path()` is the same transfer with an
  `Authorization: Bearer` header, for the two Hugging Face callers. `retry_progress_label()` owns
  the one user-facing retry text. `fetch_small_body()` applies the same network rules (timeouts,
  transient classification, backoff, cancel) to a small in-memory document with a size ceiling and
  no staging — Reline's catalog. The token is an ARGUMENT, never read from the environment: one
  caller reads `HF_TOKEN`, the other receives the token as a request field, and this layer must not
  decide which.
- `test_paddle_onnx_lease.py`: `footprint_device` and the owner/hint `RuntimeFactory` leases with.
- `test_surya_checkpoints.py`, `test_model_download.py`: unit tests for the contracts above. A fake
  `surya.common.s3` (whose `download_directory` raises) is injected into `sys.modules` for the former; the `download_http_to_path` /
  `download_bearer_to_path` cases of the latter inject a scripted `get` and a recording `sleep`. Neither touches the network.

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
- **Surya checkpoints are Rust's.** Rust downloads them (pinned external catalog: `manifest.json`
  plus every listed file, surya-ocr 0.17.1 layout) and sends absolute directories per request. A
  Surya service calls `require_local_checkpoint()` (empty / relative path: `ValueError`; missing
  directory or incomplete manifest: `FileNotFoundError` "not downloaded") and then
  `point_settings_at()` under its service lock, and passes the directory as `checkpoint=`: a local
  path skips Surya's `s3://` download branch, and the settings cover library code that reads its
  default instead. The check runs before `runtime/rocm_mmap_transfer.patched_module_to()` is
  entered, which keeps only the weight transfer inside the patch.
- **`download_to_path`'s resume is OPT-IN (`resumable=True`) and changes only what happens to the
  bytes of a FAILED transfer.** By default they are unlinked. With `resumable=True` a failure renames them to a stable `<name>.part` and the next run
  renames that back into its own pid-scoped staging file. The RENAME is the claim, and it is what
  preserves the pid scoping: a parked partial has exactly one owner at a time, and of two processes
  racing for it one wins and the other simply starts from zero. A missing parked file makes the
  claim REMOVE any pid-scoped leftover rather than resume it: such a file can only come from a
  crashed process that shared our pid. Its `present` predicate (default: non-empty) decides
  whether a destination counts as already downloaded.
- **`download_http_to_path` (and `download_bearer_to_path` over it) always resumes and is the
  network contract.** Transient = `requests`
  `ConnectionError` / `Timeout` / `ChunkedEncodingError`, a body ending before the expected length,
  HTTP 408 / 429 / 5xx; everything else (other 4xx via `raise_for_status`, disk errors, the size and
  hash gates) is permanent and never retried — a paid or rate-limited host must not be hammered.
  `RetryPolicy` (defaults mirror the Rust engine: 5 retries without progress, 2..30 s, `Retry-After`
  up to 60 s) counts failures IN A ROW: an attempt that staged bytes beyond the FURTHEST length ever
  staged in this run resets the budget (measured against that high-water mark, not the previous
  attempt, so a server that ignores `Range` cannot reset it forever). Every GET
  has `(15, 60)` connect / read timeouts. The budget running out raises `RetriesExhaustedError`
  with the bytes KEPT for the next run.
- **Staged bytes are deleted only when proven wrong**: a body delivering more than `expected_size`,
  a `206` whose `Content-Range` total differs from it, or a sha256 mismatch (`SizeMismatchError` /
  `HashMismatchError`); leftovers of an earlier run already longer than `expected_size` are dropped
  and the file refetched. A cancel, a transient failure, a short body or a raising progress callback
  PARK the bytes. A caller `verify` that raises on complete bytes also proves them wrong: they are
  deleted, never parked.
- **An interstitial is followed INSIDE each attempt.** `follow(response, hop)` runs on every
  non-error response before its body is read and may hand back a URL to GET instead (same headers,
  same `Range`); a retry therefore repeats Drive's confirm step against a fresh form. More than
  `_MAX_FOLLOW_HOPS` (3) followed URLs in one attempt is a `DownloadError`, not a retry; a `follow`
  may itself raise a permanent refusal (Drive's second HTML page).
- **Nothing that cannot be the file is ever staged.** Before the staging file is opened, a 2xx
  answer is checked: a `Content-Length` other than `expected_size` (for a `206`, one that does not
  end the file at `expected_size`), a `200` to a range request without the full length, or an
  HTML page (`text/html`, unless a `follow` hook owns pages or the length proves the file) is an
  `_UnusableAnswer` — retried like a network failure, staging untouched. This is what stops a
  captive-portal / proxy page from becoming a staged prefix that a later `Range` resume completes
  into a right-sized corrupt file. Callers with a digest (FLUX via the listing's LFS `sha256`,
  the watermark pins) are additionally hash-verified; Reline (no pin, `expected_size=0`) relies
  on the HTML and length checks alone.
- **Every legal answer to `Range` is covered**: `206` is appended only after its `Content-Range`
  start equals the staged length (otherwise truncate and restart from zero); `200` means the
  server IGNORED the range, so the body is written to a side file (`<staging>.restart`) that
  replaces the partial only once it is LONGER than it — a failing restart never costs the bytes
  already staged (appending would produce a silently double-length file). A hard crash in that
  window can leave the pid-scoped side file behind; it is never read. `416` truncates and
  restarts. Restarts are immediate and not counted
  against the budget; they cannot loop because a restart sends no `Range`.
- **`expected_size` is the integrity gate; `0` means unknown.** With a size, a destination at
  another size is not "present" and is atomically replaced; without one, only a body shorter than
  its own `Content-Length` is detected. Pinned files (FLUX via the hub listing, the watermark
  weights and source files) always pass their size; only Reline's user-chosen catalog models pass
  `0`, because nothing pins them.
- **Cancellation**: `should_cancel()` is polled at every chunk and every `cancel_poll_s` (100 ms) of
  a backoff wait and raises `DownloadCancelled` — deliberately not a `RuntimeError`, because the
  IPC handlers forward `RuntimeError` text to the user as a readable refusal (every
  `DownloadError` message is such text and never carries the token). A caller's `on_progress` may
  still raise its own cancellation type at a chunk boundary. **Bound:** the open response is not
  closed from another thread, so on a socket that stalls mid-body the cancel is observed when the
  read times out — at most `READ_TIMEOUT_S` (60 s) later; on a flowing transfer it is observed at
  the next chunk, during a backoff wait within 100 ms. This is the one place where the
  `PROJECT_RULES.md` "cancel interrupts the transfer" rule holds only up to that bound.
- **User-facing texts name the model file** (the destination's name), never the pid-scoped
  staging file.
- **Retry status reaches the UI only through the existing progress `label`** (`on_retry` ->
  `retry_progress_label`), never as a new IPC field: a typed field would be a Rust <-> Python
  contract change and bump `PROTOCOL_VERSION`.
- `download_to_path` compares nothing against a length; a direct caller that knows the expected
  length must pass a `present` predicate and a `verify`.
- `model_download`'s lock is a **download lock, not a service lock**: it is held across network I/O
  and must never be nested inside a service-local lock, because a multi-GiB transfer must not block
  `health()`, `unload()` or a `SmallModelManager` eviction callback. Its per-destination locks are
  created on demand and never removed. A destination only ever appears complete on disk: `verify`
  runs on the staging file, and a failed or rejected transfer removes (or, resumable, parks) it and
  leaves any previous destination untouched.
- `checkpoint_ready()` returns `False` when the Surya package is not importable ("cannot tell",
  never an optimistic yes); `require_local_checkpoint()` raises `RuntimeError` then.
- Every missing-package path raises with context instead of guessing.

## Editing map
- To change the PaddleOCR ONNX file layout or model-root resolution, edit `paddle_onnx.py`
  (`resolve_models_root` / `resolve_model_paths`).
- To change ORT provider selection, session options, or the MiGraphX compiled-kernel cache, edit
  `paddle_onnx.py` (`provider_attempts`, `RuntimeFactory`, `resolve_compiled_cache_root`).
- To change how a Surya checkpoint directory is checked or handed to the library, edit
  `surya_checkpoints.py`; both Surya services share it, so update `test_surya_checkpoints.py` in the
  same change. Which files a checkpoint holds is the Rust catalog's.
- To change the staging name, the serialization granularity or the publish step of an on-demand
  download, edit `model_download.py`; FLUX, watermark removal and Reline all ride on it, so update
  `test_model_download.py` in the same change. Pins (size, sha256), confirm-page parsing and
  headers stay in the calling service.
- To change retry classification, backoff, timeouts, resume, the interstitial hop limit or the
  size / hash gate of any download, edit `download_http_to_path` / `_http_attempt` /
  `RetryPolicy` in `model_download.py` — never a private loop in a caller.
- To add a new engine here, first confirm two or more service domains will use it — otherwise it
  belongs in the single domain package that needs it.
