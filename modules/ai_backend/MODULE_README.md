# Module: modules/ai_backend

## Purpose
Python AI backend runtime called by the Rust application over a framed, multiplexed, bidirectional
IPC protocol (AF_UNIX socket by default, token-authenticated WebSocket as fallback). It hosts OCR,
text detection, inpainting, line restoration, machine translation, device selection, shared
resident-model management, and the in-process web-scraping browser session.

This file is the **map and boundary document** for the package. Per-domain detail lives in the
`MODULE_README.md` of each sub-package; do not duplicate it here.

## Architecture

The package is layered. Every arrow points downwards; there are no cycles.

```
ai_backend.py (repo root)          — process entrypoint, puts <repo>/modules on sys.path
        │
        ▼
   server.py                       — COMPOSITION ROOT: constructs services, fills AppState,
        │                            starts the framed IPC server
        ├── ipc/                   — transport, dispatcher, event bus, handlers (torch-free)
        ├── browser/               — in-process Selenium / CloakBrowser scraping session
        │
        ├── ocr/  detection/  inpaint/  watermark/  reline/  translate/  — service domains
        │            │
        │            ▼
        ├── engines/               — model-family runtime shared by MORE THAN ONE domain
        │            │
        │            ▼
        └── runtime/               — device selection, resident models, ROCm workarounds, paths
```

- **`server.py` is the only place that knows every domain.** It builds the services and publishes
  them as named `AppState` fields. Handlers reach services exclusively through
  `HandlerContext.state.<field>`, so those field names are a cross-layer contract: renaming one
  silently breaks an IPC method.
- **Dependency direction is one-way**: domains may import `engines/` and `runtime/`; `engines/` may
  import `runtime/`; `runtime/` imports nothing from the layers above it. The single permitted
  reverse edge is a lazy, `try`-guarded `from ..engines.paddle_onnx import resolve_compiled_cache_root`
  inside `runtime/rocm_runtime.py` — it must stay lazy so a Torch-only install never loads
  onnxruntime just to tune MIOpen.
- **A module belongs in `engines/` only if two or more domains use it** (`paddle_onnx` serves
  `ocr/paddle.py` and `detection/paddle.py`; `surya_checkpoints` serves `ocr/surya.py` and
  `detection/surya.py`; `model_download` serves `inpaint/flux_fill.py` and `watermark/service.py`).
  A runtime used by exactly one domain belongs inside that domain.

## Files and submodules
- `server.py`: composition root — service construction, `AppState` wiring, framed IPC startup.
- `__init__.py`: PEP 562 lazy re-export of `run_server`, so importing a light submodule (e.g.
  `ipc.framing`) never drags in the AI stack.
- `test_health_snapshot.py`: covers `server._build_health_snapshot`; lives here because `server.py`
  does.
- `runtime/`: program root (`paths.program_root()`), Torch availability, device/provider selection,
  `LoadedModelManager`, and the two ROCm workarounds. See `runtime/MODULE_README.md`.
- `engines/`: cross-domain model-family runtime and model acquisition (`paddle_onnx`,
  `surya_checkpoints`, `model_download`). See `engines/MODULE_README.md`.
- `ocr/`: OCR services (`ocr.manga` / `.easy` / `.paddle` / `.paddle_vl` / `.surya`) and the
  PaddleOCR-VL script constraint. See `ocr/MODULE_README.md`.
- `detection/`: text detectors (`textdetector.ctd` / `.paddle` / `.surya`) plus the vendored
  ComicTextDetector implementation in `detection/textdetector/`. See `detection/MODULE_README.md`.
- `inpaint/`: inpainting backends (`inpaint.lama_v2` / `.lama_mpe` / `.aot` / `.sdxl` /
  `.flux_fill`), the standalone LaMa V2 runtime module, and the vendored `lama_runtime_bundle/`.
  See `inpaint/MODULE_README.md`.
- `watermark/`: visible-watermark removal (`watermark.detect` / `.remove` / `.status` / `.unload`).
  A domain of its own, not a sixth inpainter: it PREDICTS a mask instead of consuming one, and its
  weights plus the network code are fetched on demand into `side_models/WatermarkRemoval/`
  (`config.WATERMARK_DIR`). See `watermark/MODULE_README.md`.
- `reline/`: Reline pipeline adapter and catalog-backed downloader (`reline.models`,
  `reline.process`). See `reline/MODULE_README.md`.
- `translate/`: machine translation (`translate.deep`). See `translate/MODULE_README.md`.
- `ipc/`: framed transport, dispatcher, event bus, handlers. See `ipc/MODULE_README.md`, the method
  surface in `ipc/handlers/MODULE_README.md`, and the authoritative wire spec `ipc/PROTOCOL.md`.
- `browser/`: `BrowserService`, the in-process Selenium/CloakBrowser session behind the single
  `browser.command` IPC method. All browser work is pinned to one owner thread (Playwright's sync
  API is greenlet-bound); downloaded images are handed to the launcher as an on-disk directory path
  + count. See `browser/MODULE_README.md`.

Tests live next to the code they cover (`runtime/test_*.py`, `ipc/handlers/test_*.py`, …), matching
the convention `browser/` already used.

## Contracts and invariants
- **Two import roots, both must keep working.** The running backend imports this package as
  top-level `ai_backend.*` (repo-root `ai_backend.py` puts `<repo>/modules` on `sys.path`); the test
  suite imports it as `modules.ai_backend.*`. Intra-package imports must therefore always be
  package-relative — an absolute `import modules.ai_backend.x` breaks the runtime root, and an
  absolute `import ai_backend.x` breaks the tests.
- **Never re-derive the program root by counting `Path(__file__).resolve().parents[N]`.** Call
  `runtime.paths.program_root()`. The depth constant exists in exactly one file; a module that
  counts levels itself silently points at the wrong directory the next time it moves. The one
  unavoidable exception is `inpaint/lama_v2_runtime_inpainter.py`, which is loaded standalone via
  `spec_from_file_location` and therefore cannot use a relative import — it is documented in place.
- **Service-domain `__init__.py` files are docstring-only.** `runtime/`, `engines/`, `ocr/`,
  `detection/`, `inpaint/`, `reline/`, `translate/` and `ipc/` re-export nothing and import no
  submodule: importing one engine must never pull another domain's heavy dependencies (torch,
  diffusers, transformers, onnxruntime) into the process, and the `ipc/` layer must stay torch-free.
  `browser/__init__.py` is the single exception — it re-exports `BrowserService`, which is safe only
  because `browser/service.py` imports Selenium/Playwright lazily (see `browser/MODULE_README.md`).
  The vendored subtrees (`detection/textdetector/`, `inpaint/lama_runtime_bundle/`) keep their
  upstream `__init__.py` contents and are import-gated by their callers instead.
- Service initialization is lazy and must surface missing packages or weights as explicit errors —
  never a silent fallback to a different model, device, or channel mode.
- Torch and ONNX model roots are separate: `ManhwaStudio_AI_Models/Torch` vs `.../ONNX`. Do not write
  ONNX weights under `Torch/` or Torch checkpoints under `ONNX/`. Library-managed caches (EasyOCR,
  Surya, PaddleOCR-VL via the Hugging Face hub) stay under those libraries' own default paths unless
  a service explicitly owns the download contract.
- `General.ai_device`, `General.ai_onnx_provider` and `General.ai_onnx_device_id` use `not-selected`
  as the config sentinel; a service must resolve it to a real runtime default before constructing a
  Torch device or ONNX provider settings, and must never persist that automatic resolution as an
  explicit user choice. Which default is picked, and when the user is asked to confirm one, is
  `runtime/MODULE_README.md`'s contract.
- The `health` IPC method and the `TOPIC_HEALTH` event push must include `backend_version` from root
  `config.VERSION`; Rust compares it with its own version and warns on a mismatch.
- Long-running inference never runs on the Rust GUI thread — it is always a backend request.
- On ROCm Torch builds, MIOpen tuning is configured once at startup by
  `runtime.rocm_runtime.configure_rocm_runtime()`, and any service moving checkpoint weights to the
  GPU must route the move through `runtime.rocm_mmap_transfer` or pay a multi-minute amdkfd stall.
  The staging contract (`move_module_to` vs `patched_module_to`) is in `runtime/MODULE_README.md`.
- The backend serves the framed IPC transport over one of two byte transports selected by
  `run_server(transport=...)` (`ai_backend.py --transport`, default `unix`); there is no HTTP server.
  Transport details, socket path rules, single-instance enforcement and the WS token handshake are
  documented in `ipc/MODULE_README.md`.

## Editing map
- To add a whole new service domain: create the sub-package (docstring-only `__init__.py` +
  `MODULE_README.md`), construct the service in `server.py`, add its `AppState` field, and add a
  handler module under `ipc/handlers/` with one import line in `ipc/handlers/__init__.py`.
- To add a service to an existing domain: edit that domain's package and its `MODULE_README.md`.
- To add a runtime shared by two domains: put it in `engines/`, not in one of the domains.
- To change device/provider selection, resident-model limits or the ROCm workarounds: `runtime/`.
- To change where the program root or model roots resolve: `runtime/paths.py` and root `config.py`.
- To change the wire protocol or a handler: `ipc/` (`PROTOCOL.md` first).
