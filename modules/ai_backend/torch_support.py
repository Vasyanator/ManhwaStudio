"""
FILE OVERVIEW: modules/ai_backend/torch_support.py
Shared Torch capability helpers for optional backend runtime.

Main responsibilities:
- detect whether `torch` can be imported in the current Python environment;
- provide a debug override that simulates missing Torch even when it is installed;
- expose a stable user-facing error message for Torch-gated endpoints.
"""

from __future__ import annotations

import threading

_STATE_LOCK = threading.Lock()
_SIMULATE_DISABLED = False
_TORCH_IMPORT_CHECKED = False
_TORCH_AVAILABLE = False

TORCH_UNAVAILABLE_ERROR = "PyTorch не установлен"


def configure_torch_support(*, simulate_disabled: bool) -> None:
    global _SIMULATE_DISABLED
    with _STATE_LOCK:
        _SIMULATE_DISABLED = simulate_disabled


def is_torch_available() -> bool:
    with _STATE_LOCK:
        if _SIMULATE_DISABLED:
            return False

        global _TORCH_IMPORT_CHECKED, _TORCH_AVAILABLE
        if _TORCH_IMPORT_CHECKED:
            return _TORCH_AVAILABLE

        try:
            import torch  # type: ignore  # noqa: F401
        except Exception:
            _TORCH_AVAILABLE = False
        else:
            _TORCH_AVAILABLE = True
        _TORCH_IMPORT_CHECKED = True
        return _TORCH_AVAILABLE


def torch_unavailable_error() -> str:
    return TORCH_UNAVAILABLE_ERROR
