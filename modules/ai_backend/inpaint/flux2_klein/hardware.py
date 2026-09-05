"""
File: modules/ai_backend/inpaint/flux2_klein/hardware.py

Purpose:
The accelerator this service runs on and what is free on it: resolving the
user's configured device to a real runtime device, reading a host/device memory
snapshot, and dropping the torch caching allocator's blocks.

Main responsibilities:
- `memory_snapshot` - `{vram_total, vram_free, ram_total, ram_free}`, with `0`
  meaning "unknown" rather than "none";
- `_resolve_selected_backend_device` - `General.ai_device` (`not-selected`
  included) resolved against the devices actually present;
- `_clear_torch_cache` - best-effort release of cached device blocks.

Notes:
- `memory_snapshot`, `_clear_torch_cache` and `_resolve_selected_backend_device`
  are all replaced by the test suite through this module object, so every
  consumer in the package reaches them as `hardware.<name>(...)`.
- torch is imported lazily; every function here is safe to call without it.
"""

from __future__ import annotations

import logging

try:
    from ai_device import AIDevice
except Exception:  # pragma: no cover - one of the two import roots always works
    from modules.ai_device import AIDevice

from ...runtime.torch_support import is_torch_available

try:
    from config import UserConfig
except Exception:  # pragma: no cover - config is always importable in-app
    UserConfig = None

log = logging.getLogger(__name__)

# =====================================================================
#  Memory reporting
# =====================================================================
def memory_snapshot(device: str | None = None) -> dict[str, int]:
    """Total/free host and device memory in bytes; `0` where unknown.

    `device` is the torch device string this service would use. When it names a
    concrete CUDA index (`cuda:1`) the VRAM figures come from THAT card instead
    of the process's current one, so a forecast on a two-accelerator host is not
    compared against the wrong card's free memory. `None`, `"cuda"` and CPU-like
    names leave the current device.

    Deliberately tolerant: `status` must answer on a machine with no Torch, no
    GPU and no psutil, and a missing figure is reported as `0` rather than
    failing the whole call.
    """
    ram_total = 0
    ram_free = 0
    try:
        import psutil

        virtual = psutil.virtual_memory()
        ram_total = int(virtual.total)
        ram_free = int(virtual.available)
    except Exception as exc:  # noqa: BLE001 - psutil is optional at runtime
        log.debug("FLUX.2 klein: host memory unavailable (%s)", exc)

    vram_total = 0
    vram_free = 0
    if is_torch_available():
        try:
            import torch

            if torch.cuda.is_available():
                free, total = torch.cuda.mem_get_info(_cuda_device_index(device))
                vram_free = int(free)
                vram_total = int(total)
        except Exception as exc:  # noqa: BLE001 - no GPU / driver mismatch
            log.debug("FLUX.2 klein: device memory unavailable (%s)", exc)

    return {
        "vram_total": vram_total,
        "vram_free": vram_free,
        "ram_total": ram_total,
        "ram_free": ram_free,
    }


def _cuda_device_index(device: str | None) -> int | None:
    """Explicit CUDA ordinal in `device`, or `None` for the current device.

    `"cuda:1"` -> `1`; `"cuda"`, `"cpu"`, `None` and anything unparsable -> `None`,
    which every `torch.cuda` query reads as "the current device".
    """
    text = str(device or "").strip().lower()
    prefix = "cuda:"
    if not text.startswith(prefix):
        return None
    try:
        return int(text[len(prefix) :])
    except ValueError:
        return None


def _clear_torch_cache() -> None:
    import gc

    gc.collect()
    try:
        import torch

        if torch.cuda.is_available():
            torch.cuda.empty_cache()
            if hasattr(torch.cuda, "ipc_collect"):
                torch.cuda.ipc_collect()
    except Exception:  # noqa: BLE001 - no torch / no accelerator is fine here
        pass


# =====================================================================
#  Device selection (mirrors the other inpaint services)
# =====================================================================
def _resolve_selected_backend_device(fallback: str) -> str:
    """Resolve `General.ai_device` into a concrete torch device string.

    Unlike `flux_fill.py`, which pins itself to the discrete GPU, this service
    honours the user's device choice: a 9B model is the case where a user with
    two accelerators most needs to say which one to use.
    """
    fallback_norm = _normalize_backend_device(fallback, "cpu")
    configured = _read_configured_device()
    if configured is None:
        configured = fallback_norm

    normalized = _normalize_backend_device(configured, fallback_norm)
    available = _safe_available_devices()

    if normalized in available:
        return normalized
    if normalized.startswith("cuda") and "cuda" in available:
        return "cuda"
    if fallback_norm in available:
        return fallback_norm
    if "cuda" in available:
        return "cuda"
    return "cpu"


def _read_configured_device() -> str | None:
    """`General.ai_device` from the user config, `None` when unset."""
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
    if value == "not-selected":
        return None
    return value or None


def _safe_available_devices() -> set[str]:
    try:
        return set(AIDevice.detect_available_devices())
    except Exception:  # noqa: BLE001 - detection must never break a request
        return {"cpu"}


def _normalize_backend_device(raw: str, fallback: str) -> str:
    value = str(raw or "").strip().lower()
    if value in {"cpu", "mps", "cuda"}:
        return value
    if value.startswith("cuda:"):
        return value
    return str(fallback or "cpu").strip().lower() or "cpu"
