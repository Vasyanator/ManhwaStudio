"""
File: modules/ai_backend/runtime/torch_device.py

Purpose:
The one owner of two decisions every Torch service in the backend makes: which
concrete Torch device the user's `General.ai_device` choice resolves to right
now, and how the accelerator allocator cache is released after a model drop.

Key functions:
- read_configured_device() - `General.ai_device`, `None` for unset / sentinel
- normalize_backend_device() - coerce a device string to a Torch device name
- safe_available_devices() - `AIDevice.detect_available_devices()`, `{"cpu"}` on failure
- resolve_selected_backend_device() - the full resolution rule
- clear_torch_cache() - `gc.collect()` plus the CUDA (and optionally MPS) cache release

Notes:
Services differ in exactly two device policies and one cache policy, and each
service states its policy once, at its module-level binding (a
`functools.partial` named `_resolve_selected_backend_device` /
`_clear_torch_cache`, the names the test suite patches):
- `accept_mps=False` (EasyOCR, ComicTextDetector): `mps` is not a device this
  service runs on, so a configured `mps` falls back like an unknown value;
- `mps_last_resort=True` (LaMa, LaMa MPE, AOT, SDXL, watermark): when nothing
  else matches, an available `mps` is preferred over `cpu`;
- `include_mps=False` in `clear_torch_cache` (FLUX.1-Fill, FLUX.2 klein,
  watermark): only the CUDA cache is released.
torch is imported lazily inside each call, so importing this module stays cheap.
"""

from __future__ import annotations

import gc
import logging

try:
    from ai_device import AIDevice
except Exception:  # pragma: no cover - one of the two import roots always works
    from modules.ai_device import AIDevice

try:
    from config import UserConfig
except Exception:  # pragma: no cover - config is always importable in-app
    UserConfig = None

log = logging.getLogger(__name__)

#: Config sentinel for "the user has not chosen a device yet".
_NOT_SELECTED = "not-selected"


def read_configured_device() -> str | None:
    """`General.ai_device` lower-cased and stripped; `None` when unset, empty or `not-selected`."""
    config_root = getattr(UserConfig, "config", None)
    if not isinstance(config_root, dict):
        return None
    general = config_root.get("General")
    if not isinstance(general, dict):
        return None
    value = general.get("ai_device")
    if not isinstance(value, str):
        return None
    value = value.strip().lower()
    if value == _NOT_SELECTED:
        return None
    return value or None


def normalize_backend_device(raw: str | None, fallback: str, *, accept_mps: bool = True) -> str:
    """Coerce `raw` to `cpu` / `cuda` / `cuda:<n>` (and `mps` when `accept_mps`).

    Anything else (unknown text, empty, `None`, `mps` with `accept_mps=False`)
    yields `fallback` lower-cased and stripped, or `cpu` when that is empty.
    The fallback itself is not validated: callers pass an already normalized one.
    """
    value = str(raw or "").strip().lower()
    if value in {"cpu", "cuda"} or value.startswith("cuda:"):
        return value
    if accept_mps and value == "mps":
        return value
    return str(fallback or "cpu").strip().lower() or "cpu"


def safe_available_devices() -> set[str]:
    """Devices Torch can use right now; `{"cpu"}` when probing raises (logged at debug)."""
    try:
        return set(AIDevice.detect_available_devices())
    except Exception:
        log.debug("torch device probing failed, assuming CPU only", exc_info=True)
        return {"cpu"}


def resolve_selected_backend_device(fallback: str, *, accept_mps: bool = True, mps_last_resort: bool = False) -> str:
    """Resolve `General.ai_device` to a device string that is available right now.

    Order: the configured device (the normalized `fallback` when the sentinel or
    nothing is configured) if available; plain `cuda` for an unavailable
    `cuda:<n>`; the normalized `fallback`; `cuda`; `mps` when `mps_last_resort`;
    finally `cpu`. Never raises: a failing device probe counts as CPU only.
    `accept_mps` and `mps_last_resort` are the per-service policies documented in
    the file header.
    """
    fallback_norm = normalize_backend_device(fallback, "cpu", accept_mps=accept_mps)
    configured = read_configured_device()
    if configured is None:
        configured = fallback_norm

    normalized = normalize_backend_device(configured, fallback_norm, accept_mps=accept_mps)
    available = safe_available_devices()

    if normalized in available:
        return normalized
    if normalized.startswith("cuda") and "cuda" in available:
        return "cuda"
    if fallback_norm in available:
        return fallback_norm
    if "cuda" in available:
        return "cuda"
    if mps_last_resort and "mps" in available:
        return "mps"
    return "cpu"


def clear_torch_cache(*, owner: str, include_mps: bool = True) -> None:
    """Run `gc.collect()`, then release the CUDA allocator cache (and MPS when `include_mps`).

    Best-effort and never raises: a missing torch only skips the release, and a
    failing release is logged at WARNING naming `owner` (the service, for the
    log line), because a cache that is not released keeps VRAM occupied.
    """
    gc.collect()
    try:
        import torch  # type: ignore
    except Exception:
        # No torch means no torch allocator to release; nothing was loaded on it.
        return

    try:
        if hasattr(torch, "cuda") and torch.cuda.is_available():
            torch.cuda.empty_cache()
            if hasattr(torch.cuda, "ipc_collect"):
                torch.cuda.ipc_collect()
    except Exception as exc:
        log.warning("%s: could not release the torch CUDA cache: %s", owner, exc, exc_info=True)
    if not include_mps:
        return
    try:
        if hasattr(torch, "mps") and hasattr(torch.mps, "empty_cache"):
            torch.mps.empty_cache()
    except Exception as exc:
        log.warning("%s: could not release the torch MPS cache: %s", owner, exc, exc_info=True)
