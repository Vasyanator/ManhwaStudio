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
  included) resolved against the devices actually present (binding of
  `runtime.torch_device.resolve_selected_backend_device`, default policy);
- `_clear_torch_cache` - best-effort release of cached CUDA blocks (binding of
  `runtime.torch_device.clear_torch_cache`, MPS cache left alone).

Notes:
- `memory_snapshot`, `_clear_torch_cache` and `_resolve_selected_backend_device`
  are all replaced by the test suite through this module object, so every
  consumer in the package reaches them as `hardware.<name>(...)`.
- torch is imported lazily; every function here is safe to call without it.
"""

from __future__ import annotations

import functools
import logging

from ...runtime.torch_support import is_torch_available
from ...runtime.torch_device import clear_torch_cache, resolve_selected_backend_device

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


#: `General.ai_device` resolution with this service's device policy (owner:
#: `runtime.torch_device`); patched by name in the test suite. Unlike
#: `flux_fill.py`, which pins itself to the discrete GPU, this service honours the
#: user's device choice: a 9B model is the case where a user with two
#: accelerators most needs to say which one to use.
_resolve_selected_backend_device = resolve_selected_backend_device

#: Allocator-cache release with this service's policy (owner: `runtime.torch_device`);
#: patched by name in the test suite.
_clear_torch_cache = functools.partial(clear_torch_cache, owner="FLUX.2 klein", include_mps=False)
