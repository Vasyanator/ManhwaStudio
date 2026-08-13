# Module: modules/ai_backend/engines

## Purpose
Model-family runtime that is shared by **more than one service domain**. A module belongs here only
when at least two of `ocr/`, `detection/`, `inpaint/`, `reline/`, `translate/` depend on it; a model
runtime used by a single domain belongs inside that domain's package instead.

Both current members earn their place:
- `paddle_onnx` is used by the PaddleOCR recognizer (`ocr/paddle.py`) **and** the PaddleOCR text
  detector (`detection/paddle.py`);
- `surya_checkpoints` is used by the Surya OCR service (`ocr/surya.py`) **and** the Surya text
  detector (`detection/surya.py`).

That is why neither lives under `ocr/` or `detection/`: putting it in one of them would make the
other domain import across a sibling boundary for a shared engine.

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
  and device id, runs the PP-OCR detection and recognition pipelines without any Paddle dependency,
  reuses sessions across backend requests through `runtime/model_manager.py`, and configures the
  compiled-kernel cache directory used by MiGraphX. Key entry points: `resolve_model_paths()`,
  `resolve_det_model_path()`, `resolve_models_root()`, `resolve_compiled_cache_root()`,
  `provider_attempts()`, `RuntimeFactory`, `PaddleOnnxRuntime`.
- `surya_checkpoints.py`: presence check and **eager** download of the `s3://` checkpoints the Surya
  services use — `checkpoint_local_dir()`, `checkpoint_ready()`, `ensure_checkpoint_downloaded()`.
  The Surya package is imported lazily inside each function.
- `test_surya_checkpoints.py`: unit tests for the checkpoint contract; a fake `surya.common.s3` is
  injected into `sys.modules`, so neither the Surya package nor network access is needed.

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
- To add a new engine here, first confirm two or more service domains will use it — otherwise it
  belongs in the single domain package that needs it.
