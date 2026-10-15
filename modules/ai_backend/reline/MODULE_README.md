# Module: modules/ai_backend/reline

## Purpose
Adapter around the third-party `reline` image-processing/super-resolution package. It owns Reline
model discovery, download, and extraction, and turns the Rust-side `RelineOptions` into a Reline
pipeline JSON. It backs the `reline.models` and `reline.process` IPC methods.

## Architecture
`server.py` (the composition root) constructs one `RelineService` and stores it as
`AppState.reline`; `ipc/handlers/reline.py` reaches it through `HandlerContext.state.reline` and
never imports this package directly.

Reline works entirely on on-disk paths — no image bytes cross the IPC socket. `process_image_file`
builds a node list (`file_reader` → processing nodes → writer), resolves the checkpoint to a LOCAL
path, and hands the JSON to `reline.Pipeline`. The Torch-backed pipeline is imported lazily inside
that call, so nothing here is imported at backend startup beyond the stdlib-only `runtime.paths`.

Model resolution order: an existing local file under `MODEL_DIR` → the remote catalog
(`CATALOG_URL`) → the built-in `EXTRA_MODELS` list. Downloads go through the shared retrying
transfer `../engines/model_download.download_http_to_path` (retry with backoff, `Range` resume,
process-private staging, atomic publish) into `MODEL_DIR/.download`; `.tar.xz`/`.txz` archives are
extracted to a stable `<archive stem>.pth` cache name so a later catalog-name lookup finds them.

### Upstream model sources
The third-party stack ships no weights and no downloader of its own: `reline` runs the pipeline,
`resselt` detects the architecture and loads the state dict, `resr` does tiling/inference (versions
observed in `venv`: 1.4.4 / 1.4.1 / 1.1.0). Everything a user can pick therefore comes from one of
two places:

- `CATALOG_URL` (`https://mdb.yor.ovh/v1/files`, `service.py`) — the same catalog endpoint the
  upstream GUIs (`rewaifu/reline-web`, `breadyk/reline-local-GUI`) read; entries carry `filename`
  and `url`, and the published artifacts are Torch checkpoints or `.tar.xz` archives of one. That
  observation dates from the 2026-05 upstream survey and has not been re-verified since.
- `EXTRA_MODELS` (`service.py`) — models the catalog does not publish, each with a `source` URL
  for manual placement when no direct `url` exists.

## Files and submodules
- `__init__.py`: docstring only. Deliberately re-exports nothing so importing the package stays
  cheap; import `service` explicitly.
- `service.py`: `RelineService` (`health` / `list_models` / `process_image_file`), the catalog
  fetch/match helpers, the downloader/extractor, and the params → pipeline-JSON mapping. Edit here
  for anything Reline.
- `test_service.py`: unit tests for catalog-name matching, compound archive suffixes, direct-URL
  filenames, extracted-checkpoint reuse, the `EXTRA_MODELS` manual-download hint, and the download
  (retry + resume, archive extraction, corrupt-archive removal, permanent status, suffix refusal,
  cancel), the catalog fetch, and that construction creates no directory.
  Runs with no network and no Torch (the transport and clock are injected, helpers patched).

## Contracts and invariants
- Reline checkpoints are Torch files kept under `ManhwaStudio_AI_Models/side_models/Reline`
  (`MODEL_DIR`, derived from `runtime.paths.side_models_root()`, the backend's single owner of
  that root).
- TORCH CHECKPOINTS ONLY — this is a hard upstream constraint, not a policy of ours. The `upscale`
  node is a thin wrapper over `resselt.load_from_file` (`reline/nodes/upscale/node.py`), and that
  function dispatches on the file extension: `.pt` / `.pth` / `.ckpt` / `.safetensors`, and raises
  `ValueError("Unsupported model file extension …")` for anything else
  (`resselt/registry.py`, `load_from_file`). ONNX super-resolution models therefore CANNOT be routed
  through Reline — adding `.onnx` to `MODEL_SUFFIXES` would only move the failure from resolution to
  load time. An ONNX SR path needs its own node/service (the project's ONNX runtime lives elsewhere,
  see `crates/ms-onnx`), not a wider filter here.
- The third-party `reline` package receives LOCAL model paths only; it never downloads. Catalog
  resolution and archive extraction are owned by `service.py`; the transfer itself is the shared
  `download_http_to_path` (never a private loop here).
- Catalog models are user-chosen and NOT pinned: a download passes `expected_size=0`, so the
  server's `Content-Length` is the only size check (a short body is still detected and resumed).
  A filename without a supported model/archive suffix is refused before any request; an archive
  that cannot be extracted is deleted so the next request refetches it. Every GET sends
  `USER_AGENT`.
- `reline.process` has no progress channel; retries are logged. Both handlers pass their cancel
  event as `should_cancel`, which reaches the catalog fetch and the model download (per chunk and
  during retry waits); a cancelled download keeps its bytes for a resume. The pipeline itself is
  not interruptible.
- The catalog is fetched with `../engines/model_download.fetch_small_body` (timeouts, retry with
  backoff, cancel, size ceiling); a non-array response is a `RuntimeError`.
- Constructing `RelineService` has no side effect: `MODEL_DIR` / `DOWNLOAD_DIR` are created only
  by a download, and a missing `MODEL_DIR` means "nothing downloaded". Tests point both at a
  temporary directory.
- Diagnostics go through the module logger (`log`), never `print`.
- `EXTRA_MODELS` is the escape hatch for models the remote catalog does not publish. An entry with
  an empty `url` must be placed manually under `MODEL_DIR`; resolution otherwise raises
  `FileNotFoundError` naming the model and its `source` URL. No silent fallback.
- `from reline import Pipeline` inside `service.py` is the THIRD-PARTY top-level package, not this
  sub-package: Python 3 imports are absolute, so the identical names do not collide. Keep all
  intra-package imports relative.
- A missing input image raises `FileNotFoundError`; a pipeline that produces no output file raises
  `RuntimeError`. Failures are logged with a traceback and re-raised — the IPC layer maps them to
  `response{status:"error"}`.
- Enum-like option values (`READER_MODES`, `TILERS`, `DTYPES`, resize filters, …) are validated
  against their allowed sets; an unknown value is an error, never a silently substituted default.

## Editing map
- Catalog resolution, download, or archive extraction: `service.py` (`_fetch_catalog`,
  `_find_catalog_entry`, `_find_existing_model`, `_download_model`,
  `_extract_first_model_from_archive`). Retry/resume behaviour: `../engines/model_download.py`.
- A new pipeline node or option: `RelineService._build_pipeline_json` plus the matching allowed-value
  set; mirror the field in the Rust `RelineOptions`.
- A model missing from the remote catalog: add an `EXTRA_MODELS` entry.
- Request/response shape of `reline.models` / `reline.process`: `../ipc/handlers/reline.py` and
  `../ipc/PROTOCOL.md`.
