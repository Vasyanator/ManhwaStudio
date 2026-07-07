"""
Package: modules/ai_backend

Python AI backend runtime for the Rust application. The heavy service stack
(OCR / text detection / inpaint / translation) lives in `server.py` and pulls
in torch and the model libraries; the framed IPC transport in `ipc/` is
intentionally torch-free.

`run_server` is re-exported lazily (PEP 562 `__getattr__`) so that importing a
light submodule such as `modules.ai_backend.ipc.framing` does NOT drag in the
whole AI stack. Only an actual access of `modules.ai_backend.run_server` (or
`from modules.ai_backend import run_server`) triggers importing `server.py`.
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
