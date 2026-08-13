"""
Package: modules/ai_backend

Python AI backend runtime for the Rust application, split into domain
sub-packages:

    runtime/    — process-wide runtime plumbing: model manager, device service,
                  torch availability, ROCm/MIOpen setup, program paths.
    engines/    — shared inference engine adapters (ONNX runtime factory, Surya
                  checkpoint fetching) used by several services.
    ocr/        — one module per OCR engine (easy, manga, paddle, paddle_vl,
                  surya) plus the script-constraint helper.
    detection/  — text detector services (ctd, paddle, surya) and the vendored
                  `textdetector/` model code.
    inpaint/    — inpaint services (aot, lama, lama_mpe, sdxl, flux_fill) and
                  the vendored LaMa runtime bundle.
    reline/     — Reline super-resolution adapter and its model catalog.
    translate/  — machine-translation adapters.
    browser/    — headless browser service.
    ipc/        — framed, multiplexed IPC protocol + handlers (torch-free).
    server.py   — composition root: builds every service into the shared
                  `AppState` and serves the IPC protocol.

`server.py` is the only module that imports across all of these; the framed IPC
transport in `ipc/` is intentionally torch-free and reaches services solely
through `HandlerContext.state`.

`run_server` is re-exported lazily (PEP 562 `__getattr__`) so that importing a
light submodule such as `modules.ai_backend.ipc.framing` does NOT drag in the
whole AI stack. Only an actual access of `modules.ai_backend.run_server` (or
`from modules.ai_backend import run_server`) triggers importing `server.py`.
The sub-packages follow the same rule: their `__init__.py` files re-export
nothing, so importing one never pulls in its heavy dependencies.

Two import roots are live for the same code: the backend process imports it as
`ai_backend.*` (the entrypoint puts `<repo>/modules` on `sys.path`) while the
test suite imports it as `modules.ai_backend.*`. Intra-package imports must
therefore always be package-relative.
"""

from __future__ import annotations

from typing import Any

__all__ = ["run_server"]


def __getattr__(name: str) -> Any:
    """Lazily resolve `run_server` from `.server` on first access (PEP 562).

    Keeps the package import cheap: the torch-backed service stack in
    `server.py` is only imported when `run_server` is actually requested,
    so the framed IPC layer (`ipc/`) and its tests stay importable without
    the AI model dependencies.
    """
    if name == "run_server":
        from .server import run_server

        return run_server
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
