"""
File: modules/ai_backend/inpaint/flux2_klein/hardware.py

Purpose:
The accelerator this service runs on and what is free on it: resolving the
user's configured device to a real runtime device, reading a host/device memory
snapshot, and dropping the torch caching allocator's blocks.

Main responsibilities:
- `memory_snapshot` - `{vram_total, vram_free, ram_total, ram_free}`, with `0`
  meaning "unknown" rather than "none" (wrapper over
  `runtime.memory_probe.memory_snapshot` carrying this service's wording, logger
  and `is_torch_available` binding);
- `_resolve_selected_backend_device` - `General.ai_device` (`not-selected`
  included) resolved against the devices actually present, never an
  integrated GPU (binding of `runtime.torch_device.resolve_discrete_backend_device`,
  default policy);
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

from ...runtime.memory_probe import cuda_device_index
from ...runtime.memory_probe import memory_snapshot as probe_memory_snapshot
from ...runtime.torch_support import is_torch_available
from ...runtime.torch_device import clear_torch_cache, resolve_discrete_backend_device

log = logging.getLogger(__name__)

# =====================================================================
#  Memory reporting
# =====================================================================
def memory_snapshot(device: str | None = None) -> dict[str, int]:
    """Total/free host and device memory in bytes; `0` where unknown.

    `runtime.memory_probe.memory_snapshot` with this service's wording
    ("FLUX.2 klein: ... memory unavailable") on this module's logger, and with
    THIS module's `is_torch_available` binding (looked up per call, so patching
    `hardware.is_torch_available` keeps reaching the probe). `device` selects
    the CUDA card the VRAM figures come from (see the runtime contract). A wrapper
    `def` rather than a re-bound import on purpose: it is the package's patch
    point, replaced by the test suite through this module object. Never raises.
    """
    return probe_memory_snapshot(
        device, owner="FLUX.2 klein", logger=log, torch_available=is_torch_available
    )


#: Explicit CUDA ordinal in a device string (owner: `runtime.memory_probe`).
_cuda_device_index = cuda_device_index


#: `General.ai_device` resolution with this service's device policy (owner:
#: `runtime.torch_device`); patched by name in the test suite. Unlike
#: `flux_fill.py`, which ignores the setting and pins itself to the discrete GPU,
#: this service honours the user's device choice (a 9B model is the case where a
#: user with two accelerators most needs to say which one to use) — except an
#: integrated GPU, which is overridden with a warning.
_resolve_selected_backend_device = resolve_discrete_backend_device

#: Allocator-cache release with this service's policy (owner: `runtime.torch_device`);
#: patched by name in the test suite.
_clear_torch_cache = functools.partial(clear_torch_cache, owner="FLUX.2 klein", include_mps=False)
