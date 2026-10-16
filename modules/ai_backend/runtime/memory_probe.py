"""
File: modules/ai_backend/runtime/memory_probe.py

Purpose:
A tolerant host/device memory snapshot that any Torch service can report or
plan against, and the parse of an explicit CUDA ordinal out of a device string.

Main responsibilities:
- `memory_snapshot` - `{vram_total, vram_free, ram_total, ram_free}` in bytes,
  with `0` meaning "unknown" rather than "none"; never raises;
- `cuda_device_index` - `"cuda:1"` -> `1`, anything else -> `None`.

Notes:
- The log wording and the logger belong to the caller (`owner`, `logger`), so a
  service's "memory unavailable" records keep its own prefix and land in its own
  logger tree. Whether torch counts as available is the caller's too
  (`torch_available`): a service whose tests patch its own `is_torch_available`
  binding passes that binding in, and the patch keeps reaching the probe.
- psutil and torch are imported lazily inside `memory_snapshot`; this module is
  safe to import and call without either.
"""

from __future__ import annotations

import logging
from collections.abc import Callable

from .torch_support import is_torch_available

log = logging.getLogger(__name__)


def memory_snapshot(
    device: str | None = None,
    *,
    owner: str,
    logger: logging.Logger | None = None,
    torch_available: Callable[[], bool] = is_torch_available,
) -> dict[str, int]:
    """Total/free host and device memory in bytes; `0` where unknown.

    `device` is the torch device string the caller would use. When it names a
    concrete CUDA index (`cuda:1`) the VRAM figures come from THAT card instead
    of the process's current one, so a forecast on a two-accelerator host is not
    compared against the wrong card's free memory. `None`, `"cuda"` and CPU-like
    names leave the current device.

    `owner` prefixes the debug records written when a figure is unavailable;
    `logger` receives them (this module's logger when `None`). `torch_available`
    decides whether the device half is attempted at all.

    Deliberately tolerant: a status call must answer on a machine with no Torch,
    no GPU and no psutil, and a missing figure is reported as `0` rather than
    failing the whole call. Never raises.
    """
    sink = logger if logger is not None else log
    ram_total = 0
    ram_free = 0
    try:
        import psutil

        virtual = psutil.virtual_memory()
        ram_total = int(virtual.total)
        ram_free = int(virtual.available)
    except Exception as exc:  # noqa: BLE001 - psutil is optional at runtime
        sink.debug("%s: host memory unavailable (%s)", owner, exc)

    vram_total = 0
    vram_free = 0
    if torch_available():
        try:
            import torch

            if torch.cuda.is_available():
                free, total = torch.cuda.mem_get_info(cuda_device_index(device))
                vram_free = int(free)
                vram_total = int(total)
        except Exception as exc:  # noqa: BLE001 - no GPU / driver mismatch
            sink.debug("%s: device memory unavailable (%s)", owner, exc)

    return {
        "vram_total": vram_total,
        "vram_free": vram_free,
        "ram_total": ram_total,
        "ram_free": ram_free,
    }


def cuda_device_index(device: str | None) -> int | None:
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
