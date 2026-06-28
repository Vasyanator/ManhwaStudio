"""
File: modules/ai_backend/rocm_runtime.py

Purpose:
Configure MIOpen convolution behavior when the active PyTorch build targets
ROCm (AMD HIP), so Torch-backed services (LaMa, AOT, CTD, SDXL prefill) do not
pay repeated per-input-shape kernel auto-tuning/compilation cost.

Main responsibilities:
- detect a ROCm/HIP PyTorch build via `torch.version.hip`;
- switch MIOpen into immediate mode (no exhaustive Find / no per-shape JIT
  tuning) by defaulting `MIOPEN_FIND_MODE=FAST` when the user has not set it;
- pin MIOpen's user/kernel cache to the app cache root so the small amount of
  kernel state that is still produced survives backend restarts;
- disable cuDNN/MIOpen benchmark auto-tuning explicitly.

Key functions:
- `configure_rocm_runtime()`

Notes:
- A no-op on CPU-only, CUDA, MPS, or absent-Torch installs: it only acts when
  `torch.version.hip` is a non-empty string.
- All environment defaults use `setdefault` semantics so an explicit user/env
  override always wins.
- MIOpen reads `MIOPEN_FIND_MODE` at convolution Find time and the cache paths
  at handle creation (first GPU convolution), so configuring this before the
  first inference request is sufficient.
"""

from __future__ import annotations

import logging
import os
from pathlib import Path

log = logging.getLogger(__name__)

# MIOpen FIND_MODE=2 (FAST): use Immediate Mode and a heuristically chosen
# precompiled kernel instead of the lengthy exhaustive Find that compiles and
# benchmarks many kernel candidates for every new convolution input shape.
_MIOPEN_FIND_MODE_FAST = "2"


def _resolve_cache_root() -> Path:
    """Resolve the app cache root (`ManhwaStudio_AI_Models/.cache`).

    Falls back to a path relative to this file when the ONNX runtime helper is
    unavailable, so MIOpen cache pinning still targets a stable directory.
    """
    try:
        from .paddle_onnx_runtime import resolve_compiled_cache_root

        return resolve_compiled_cache_root()
    except Exception:
        return Path(__file__).resolve().parents[2] / "ManhwaStudio_AI_Models" / ".cache"


def configure_rocm_runtime() -> bool:
    """Apply MIOpen immediate-mode + persistent-cache settings for ROCm Torch.

    Returns `True` when the active Torch build is a ROCm/HIP build and the
    settings were applied, `False` otherwise (CPU/CUDA/MPS build, Torch missing,
    or any detection failure). Never raises: configuration is best-effort and a
    failure must not break backend startup.
    """
    try:
        import torch  # type: ignore
    except Exception:
        # Torch absent (ONNX-only install): nothing MIOpen-related to configure.
        return False

    hip_version = getattr(getattr(torch, "version", None), "hip", None)
    if not isinstance(hip_version, str) or not hip_version.strip():
        # CUDA / CPU / MPS Torch build: MIOpen is not used.
        return False

    # Immediate mode: skip per-shape exhaustive Find/compile. setdefault keeps an
    # explicit user override intact.
    os.environ.setdefault("MIOPEN_FIND_MODE", _MIOPEN_FIND_MODE_FAST)

    # Pin MIOpen user perf-db and compiled-kernel cache to the app cache root so
    # any kernel state that is still produced is reused across backend restarts.
    cache_root = _resolve_cache_root() / "miopen"
    user_db = cache_root / "user_db"
    kernel_cache = cache_root / "kernels"
    try:
        user_db.mkdir(parents=True, exist_ok=True)
        kernel_cache.mkdir(parents=True, exist_ok=True)
        os.environ.setdefault("MIOPEN_USER_DB_PATH", str(user_db))
        os.environ.setdefault("MIOPEN_CUSTOM_CACHE_DIR", str(kernel_cache))
    except OSError as exc:
        # Cache pinning is an optimization; fall back to MIOpen defaults on a
        # filesystem error instead of failing backend startup.
        log.warning(
            "MIOpen cache directory could not be prepared at %s: %s. "
            "Using MIOpen default cache location.",
            cache_root,
            exc,
        )

    # Disable cuDNN/MIOpen benchmark auto-tuning explicitly (defensive: another
    # module could have enabled it). benchmark=True re-runs Find per new shape.
    try:
        torch.backends.cudnn.benchmark = False
    except Exception as exc:
        log.warning("Could not disable cudnn/MIOpen benchmark: %s", exc)

    log.info(
        "ROCm Torch build detected (hip=%s); MIOpen immediate mode enabled "
        "(MIOPEN_FIND_MODE=%s), benchmark disabled, cache pinned to %s.",
        hip_version.strip(),
        os.environ.get("MIOPEN_FIND_MODE"),
        cache_root,
    )
    return True
