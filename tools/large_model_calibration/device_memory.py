"""
File: tools/large_model_calibration/device_memory.py

Purpose:
Device memory OUTSIDE torch's allocator accounting for the VRAM script: the
`--vram-cap-gib` cap (`torch.cuda.set_per_process_memory_fraction`), the size of
the HIP/CUDA context this process creates, and the device-wide used counter the
phase recorder turns into a whole-process footprint.

Key declarations:
- `CapReport` - what `apply_vram_cap` measured and set (JSON-ready)
- `sysfs_vram_used()` - amdgpu `mem_info_vram_used` per PCI address
- `nvidia_vram_used()` - `nvidia-smi` used memory per PCI address
- `device_vram_used()` - both, merged (the baseline the tool takes)
- `pci_address()` - the PCI address of a torch device
- `apply_vram_cap()` - baseline -> context init -> cap -> context size

Notes:
- torch's cap counts only memory its caching allocator reserves. The context,
  kernel code objects (rocBLAS / hipBLASLt / MIOpen load them lazily) and other
  driver allocations are outside it, yet a real 8 GiB card must hold them too:
  `CapReport.context_bytes` (after one tiny allocation) and the recorder's
  per-phase `non_torch_end` (device used - other processes' baseline - torch
  reserved) measure them.
- The baseline comes from a device-wide counter read BEFORE this process
  touches the device (torch's `mem_get_info` would create the context it is
  meant to measure): the amdgpu sysfs counter, or `nvidia-smi` on NVIDIA
  (`device_vram_used`). Without either the context size and the non-torch
  fields are `None`. On NVIDIA the later readings come from `mem_get_info`
  (total - free), which may count driver-reserved memory `nvidia-smi` leaves
  out; that offset lands in the context size (a conservative error).
- The counters are device-wide: another process using the GPU during the run
  makes them wrong. Run on an otherwise idle GPU.
"""

from __future__ import annotations

import logging
import shutil
import subprocess
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

log = logging.getLogger(__name__)

#: Bytes per GiB.
GIB = 1024**3

#: amdgpu exposes one DRM card directory per GPU here.
_DRM_ROOT = Path("/sys/class/drm")


@dataclass(frozen=True)
class CapReport:
    """Device-memory facts of the run (bytes; `None` where not measurable).

    `cap_bytes` / `fraction` are `None` without `--vram-cap-gib`.
    `baseline_used` is the device-wide used VRAM before this process touched the
    device (other processes, the desktop); `context_bytes` what this process's
    context + one 2 MiB-class allocation added, minus torch's reserved bytes.
    """

    device: str
    total_bytes: int
    pci_address: str | None
    baseline_used: int | None
    context_bytes: int | None
    cap_bytes: int | None
    fraction: float | None

    def as_dict(self) -> dict[str, Any]:
        """JSON form, with GiB fields for reading."""
        data: dict[str, Any] = asdict(self)
        for key in ("total_bytes", "baseline_used", "context_bytes", "cap_bytes"):
            value = data[key]
            data[f"{key}_gib"] = None if value is None else round(value / GIB, 3)
        return data


def sysfs_vram_used() -> dict[str, int]:
    """`{pci address: used VRAM bytes}` from amdgpu's `mem_info_vram_used`; empty when unavailable.

    The address is the basename of the card's resolved `device` link
    (`0000:03:00.0`). A card whose counter cannot be read is logged and left out.
    """
    used: dict[str, int] = {}
    if not _DRM_ROOT.is_dir():
        return used
    for card in sorted(_DRM_ROOT.glob("card[0-9]*")):
        counter = card / "device" / "mem_info_vram_used"
        if "-" in card.name or not counter.is_file():
            continue
        try:
            used[(card / "device").resolve().name] = int(counter.read_text(encoding="ascii").strip())
        except (OSError, ValueError) as exc:
            log.warning("cannot read %s: %s", counter, exc)
    return used


def nvidia_vram_used() -> dict[str, int]:
    """`{pci address: used VRAM bytes}` from `nvidia-smi`; empty without it.

    `nvidia-smi` prints an 8-digit PCI domain (`00000000:01:00.0`); the key is
    normalized to the 4-digit lowercase form `pci_address` produces. A failing
    query is logged and yields nothing (the context then stays unmeasured).
    """
    binary = shutil.which("nvidia-smi")
    if binary is None:
        return {}
    try:
        output = subprocess.run([binary, "--query-gpu=pci.bus_id,memory.used", "--format=csv,noheader,nounits"], capture_output=True, text=True, check=True, timeout=30).stdout
    except (OSError, subprocess.SubprocessError) as exc:
        log.warning("nvidia-smi query failed: %s", exc)
        return {}
    used: dict[str, int] = {}
    for line in output.splitlines():
        address, sep, mib = line.partition(",")
        if not sep:
            continue
        domain, _, rest = address.strip().lower().partition(":")
        try:
            used[f"{domain[-4:]}:{rest}"] = int(mib.strip()) * 1024**2
        except ValueError:
            log.warning("nvidia-smi: unreadable line %r", line)
    return used


def device_vram_used() -> dict[str, int]:
    """`sysfs_vram_used()` merged with `nvidia_vram_used()` (disjoint vendors)."""
    return {**nvidia_vram_used(), **sysfs_vram_used()}


def pci_address(device: str) -> str | None:
    """`dddd:bb:dd.` prefix of the PCI address of torch `device` (function digit left out), or `None`."""
    import torch

    props = torch.cuda.get_device_properties(torch.device(device))
    domain, bus, slot = (getattr(props, name, None) for name in ("pci_domain_id", "pci_bus_id", "pci_device_id"))
    if domain is None or bus is None or slot is None:
        return None
    return f"{domain:04x}:{bus:02x}:{slot:02x}."


def is_amd_sysfs(address: str | None) -> bool:
    """Whether `address` has an amdgpu sysfs counter (else readings come from `mem_get_info`)."""
    return _match(address, sysfs_vram_used()) is not None


def _match(address: str | None, counters: dict[str, int]) -> int | None:
    """The counter whose address starts with `address` (`None` when there is no single match)."""
    if address is None:
        return None
    hits = [value for key, value in counters.items() if key.startswith(address)]
    return hits[0] if len(hits) == 1 else None


def apply_vram_cap(device: str, cap_gib: float | None, baseline: dict[str, int]) -> CapReport:
    """Cap torch on `device` at `cap_gib` GiB (when given) and measure the context this process created.

    `baseline` is `device_vram_used()` taken before the process touched the
    device. The cap is set before the first allocation, then one tiny tensor
    is allocated so the context exists, and the device-wide counter is read
    again.

    # Raises
    `ValueError` for a cap that is not positive or exceeds the device's total
    memory.
    """
    import torch

    target = torch.device(device)
    total = int(torch.cuda.get_device_properties(target).total_memory)
    cap_bytes: int | None = None
    fraction: float | None = None
    if cap_gib is not None:
        cap_bytes = round(cap_gib * GIB)
        if cap_bytes <= 0 or cap_bytes > total:
            raise ValueError(f"--vram-cap-gib {cap_gib}: must be > 0 and at most the device's {total / GIB:.2f} GiB")
        fraction = cap_bytes / total
        torch.cuda.set_per_process_memory_fraction(fraction, target)
        log.info("VRAM cap: torch may reserve at most %.2f GiB of %.2f GiB (fraction %.4f)", cap_gib, total / GIB, fraction)
    probe = torch.empty(1, device=target)
    torch.cuda.synchronize(target)
    reserved = torch.cuda.memory_reserved(target)
    address = pci_address(device)
    before = _match(address, baseline)
    if is_amd_sysfs(address):
        after = _match(address, sysfs_vram_used())
    else:
        free, device_total = torch.cuda.mem_get_info(target)
        after = device_total - free
    del probe
    context = None if before is None or after is None else after - before - reserved
    if context is None:
        log.warning("HIP/CUDA context size not measurable (no device-wide counter for %s, PCI %s)", device, address)
    else:
        log.info(
            "device baseline used %.2f GiB (other processes), this process's context %.2f GiB (outside torch's cap)",
            before / GIB,
            context / GIB,
        )
        if cap_bytes is not None:
            log.info(
                "a real card of %.2f GiB would leave torch about %.2f GiB after this context alone",
                cap_bytes / GIB,
                (cap_bytes - context) / GIB,
            )
    return CapReport(
        device=device,
        total_bytes=total,
        pci_address=address,
        baseline_used=before,
        context_bytes=context,
        cap_bytes=cap_bytes,
        fraction=fraction,
    )
