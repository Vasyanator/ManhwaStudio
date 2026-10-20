"""
File: modules/ai_backend/runtime/torch_flavour.py

Purpose:
The one owner of "which torch build is this": CPU-only, CUDA or ROCm (HIP),
decided from an already imported torch module's `torch.version`. Every backend
site that branches on the build asks here instead of reading
`torch.version.hip` itself, so the sites cannot drift apart. It also owns the
build facts the backend publishes in its health snapshot (`torch` key): build
flavour and versions, the visible GPU devices, the installed AMD per-arch
torch wheels and whether the AMD split-kernel (whl-next) layout is installed.

Key structures:
- `TorchFlavour`
- `TorchBuildFacts`

Key functions:
- `flavour_of(torch_module)`
- `is_rocm_build(torch_module)`
- `torch_build_facts()` - flavour + versions of this process's torch, cached
- `gpu_device_facts()` - visible CUDA/ROCm devices, cached
- `installed_rocm_device_archs()` - gfx archs of installed `amd-torch-device-*` wheels, cached
- `rocm_split_kernels_installed()` - whether `rocm-sdk-core` is installed (kernels ship as
  per-arch device wheels), same cached scan
- `torch_health_payload()` - the health snapshot's `torch` object; never raises

Notes:
- Importing this module never imports torch (a test enforces it): `flavour_of`
  takes the module the caller already imported, so the caller keeps its own
  import gate. The fact functions import torch and `torch_device` lazily,
  inside the call, and only after `is_torch_available()` said yes.
- A ROCm wheel carries a CUDA-shaped API (`torch.cuda`) and may also report a
  `torch.version.cuda`, so `hip` is checked first.
- Only a non-blank string counts as a version: `None`, `""`, whitespace, a
  non-string value, or a module without `version` all mean "not this build".
- The health worker builds a snapshot every second, so every fact is computed
  once per process and cached (device facts and the wheel scan also when
  empty): torch and its wheels do not change under a running backend. Each
  cache has a single initializer: its lock is held through the first
  population, so concurrent first callers wait and every probe runs once. The one
  device query (`torch.cuda.get_device_properties`) runs on the first snapshot;
  it initializes torch's CUDA/HIP runtime state without allocating VRAM, as
  the app's first `device.get` already does (runtime MODULE_README).
"""

from __future__ import annotations

import enum
import importlib.metadata as importlib_metadata
import logging
import re
import threading
from dataclasses import dataclass

from .torch_support import is_torch_available

log = logging.getLogger(__name__)


class TorchFlavour(str, enum.Enum):
    """The build flavour of a torch installation; the value is a stable lowercase id."""

    CPU = "cpu"
    CUDA = "cuda"
    ROCM = "rocm"


def _is_version_string(value: object) -> bool:
    """Whether `value` is a usable version: a string with at least one non-blank character."""
    return isinstance(value, str) and bool(value.strip())


def flavour_of(torch_module: object) -> TorchFlavour:
    """Flavour of `torch_module` (an imported torch, or any stand-in for one).

    A non-blank `torch.version.hip` -> `ROCM`; else a non-blank
    `torch.version.cuda` -> `CUDA`; else `CPU`. A module without `version` (a
    partial or stand-in torch) is `CPU`. Never raises.
    """
    version = getattr(torch_module, "version", None)
    if _is_version_string(getattr(version, "hip", None)):
        return TorchFlavour.ROCM
    if _is_version_string(getattr(version, "cuda", None)):
        return TorchFlavour.CUDA
    return TorchFlavour.CPU


def is_rocm_build(torch_module: object) -> bool:
    """Whether `torch_module` is a ROCm/HIP build (`flavour_of(...) is TorchFlavour.ROCM`). Never raises."""
    return flavour_of(torch_module) is TorchFlavour.ROCM


@dataclass(frozen=True)
class TorchBuildFacts:
    """What this process's torch build is: flavour plus its version strings.

    `version` is `torch.__version__` (e.g. `2.13.0+rocm10.1.0`); `hip_version` /
    `cuda_version` are the stripped `torch.version.hip` / `.cuda`, `None` when
    not a non-blank string (the same rule `flavour_of` applies).
    """

    flavour: TorchFlavour
    version: str
    hip_version: str | None
    cuda_version: str | None


@dataclass(frozen=True)
class _RocmWheelScan:
    """One `importlib.metadata` scan: per-arch kernel wheels and the split-kernel marker."""

    device_archs: tuple[str, ...]
    split_kernels: bool


#: One lock per process-wide cache below, held THROUGH the first population: a
#: second first-caller waits for the first one's answer instead of probing again
#: (single initializer). The three are never nested, so no lock order exists.
_BUILD_FACTS_LOCK = threading.Lock()
_DEVICE_FACTS_LOCK = threading.Lock()
_ROCM_WHEEL_SCAN_LOCK = threading.Lock()
#: Guards `_PAYLOAD_FAILURE_LOGGED` only.
_PAYLOAD_LOG_LOCK = threading.Lock()
_BUILD_FACTS: TorchBuildFacts | None = None
_DEVICE_FACTS: list[dict[str, object]] | None = None
_ROCM_WHEEL_SCAN: _RocmWheelScan | None = None
#: Whether a failed `torch_health_payload` was already logged at WARNING (later ones go to DEBUG).
_PAYLOAD_FAILURE_LOGGED = False

#: The AMD whl-next runtime distribution (PEP 503 normalized). Its presence means the
#: split-kernel layout: torch ships WITHOUT GPU kernels and each arch's kernels come from an
#: `amd-torch-device-gfxN` wheel. A pytorch.org ROCm build bundles its kernels and lacks it.
_ROCM_SPLIT_KERNELS_DIST = "rocm-sdk-core"

#: An AMD per-arch torch wheel (`amd-torch-device-gfx1201`), after PEP 503 name
#: normalization. Family wheels (`amd-torch-device-gfx12-0`, `-gfx110x`) do not
#: match: their suffix is not a plain hex arch.
_ROCM_DEVICE_WHEEL = re.compile(r"^amd-torch-device-(gfx[0-9a-f]+)$")


def _version_or_none(value: object) -> str | None:
    """`value` stripped when it is a usable version string, else `None`."""
    return value.strip() if _is_version_string(value) else None


def torch_build_facts() -> TorchBuildFacts | None:
    """Flavour and versions of the torch this process imports; `None` when torch is unavailable.

    Honours `is_torch_available()` (including its simulate-missing override)
    before importing torch. The first success is cached for the process; a
    `None` is not cached, so a later call can still see torch. Raises only what
    a broken torch import raises after `is_torch_available()` accepted it.
    """
    global _BUILD_FACTS
    with _BUILD_FACTS_LOCK:
        if _BUILD_FACTS is not None:
            return _BUILD_FACTS
        if not is_torch_available():
            return None
        import torch  # type: ignore  # lazy: importing this module must stay torch-free

        version = getattr(torch, "version", None)
        _BUILD_FACTS = TorchBuildFacts(
            flavour=flavour_of(torch),
            version=str(getattr(torch, "__version__", "")),
            hip_version=_version_or_none(getattr(version, "hip", None)),
            cuda_version=_version_or_none(getattr(version, "cuda", None)),
        )
        return _BUILD_FACTS


def _gcn_arch_base(props: object) -> str | None:
    """Base arch (`gfx1201` of `gfx1201:sramecc-:xnack-`), lower-cased; `None` when not a `gfx` arch (CUDA)."""
    raw = getattr(props, "gcnArchName", None)
    if not isinstance(raw, str):
        return None
    base = raw.split(":", 1)[0].strip().lower()
    return base if base.startswith("gfx") else None


def gpu_device_facts() -> list[dict[str, object]]:
    """Every visible CUDA/ROCm device as `{"index", "name", "gcn_arch", "integrated"}`, in index order.

    `gcn_arch` is the base arch before any `:feature` suffix, lower-cased, and
    `None` on CUDA; `integrated` is `torch_device.is_integrated_gpu`. Computed
    once per process and cached, also when empty (no torch, no device, a failed
    probe: `torch_device._cuda_device_properties` logs and returns `[]`). The
    caller gates on torch availability; this function does not.
    """
    global _DEVICE_FACTS
    with _DEVICE_FACTS_LOCK:
        if _DEVICE_FACTS is not None:
            return list(_DEVICE_FACTS)
        from . import torch_device  # lazy: pulls `ai_device` and, at call time, torch

        devices: list[dict[str, object]] = [
            {
                "index": index,
                "name": str(getattr(props, "name", "")),
                "gcn_arch": _gcn_arch_base(props),
                "integrated": torch_device.is_integrated_gpu(props),
            }
            for index, props in enumerate(torch_device._cuda_device_properties())
        ]
        _DEVICE_FACTS = devices
    log.info("torch GPU devices: %s", devices)
    return list(devices)


def _normalized_distribution_name(name: str) -> str:
    """PEP 503 normalization: lower-case, runs of `-`, `_`, `.` become one `-`."""
    return re.sub(r"[-_.]+", "-", name).lower()


def _rocm_wheel_scan() -> _RocmWheelScan:
    """The cached scan of installed distributions behind the two ROCm wheel facts.

    Cached for the process after the first successful scan, also when nothing
    matched. Raises what the metadata scan raises (`OSError` on an unreadable
    `site-packages`); `torch_health_payload` contains it.
    """
    global _ROCM_WHEEL_SCAN
    with _ROCM_WHEEL_SCAN_LOCK:
        if _ROCM_WHEEL_SCAN is not None:
            return _ROCM_WHEEL_SCAN
        archs: set[str] = set()
        split_kernels = False
        for dist in importlib_metadata.distributions():
            name = dist.metadata["Name"]
            if not isinstance(name, str):
                # A broken dist-info without a Name names no wheel.
                continue
            normalized = _normalized_distribution_name(name)
            if normalized == _ROCM_SPLIT_KERNELS_DIST:
                split_kernels = True
            match = _ROCM_DEVICE_WHEEL.match(normalized)
            if match is not None:
                archs.add(match.group(1))
        _ROCM_WHEEL_SCAN = _RocmWheelScan(device_archs=tuple(sorted(archs)), split_kernels=split_kernels)
        return _ROCM_WHEEL_SCAN


def installed_rocm_device_archs() -> list[str]:
    """Sorted, de-duplicated gfx archs of the installed `amd-torch-device-gfxN` wheels.

    Family wheels are excluded (see `_ROCM_DEVICE_WHEEL`). Cached; raises what
    the metadata scan raises (see `_rocm_wheel_scan`).
    """
    return list(_rocm_wheel_scan().device_archs)


def rocm_split_kernels_installed() -> bool:
    """Whether `rocm-sdk-core` is installed: the AMD whl-next layout, whose GPU kernels ship as
    per-arch `amd-torch-device-gfxN` wheels (so an arch without its wheel has NO kernels).

    False for a pytorch.org ROCm build (kernels bundled) and for CPU/CUDA installs.
    Cached; raises what the metadata scan raises (see `_rocm_wheel_scan`).
    """
    return _rocm_wheel_scan().split_kernels


def torch_health_payload() -> dict[str, object] | None:
    """The health snapshot's `torch` object, or `None` when torch is unavailable or a fact failed.

    Shape: `{"flavour": "cpu"|"cuda"|"rocm", "version": str, "hip_version": str|None,
    "cuda_version": str|None, "devices": [gpu_device_facts() entries],
    "rocm_device_archs": [str], "rocm_split_kernels": bool}`. Never raises: a failure is logged (WARNING the
    first time, DEBUG after) and yields `None`. Cheap after the first call: every
    part is cached.
    """
    global _PAYLOAD_FAILURE_LOGGED
    try:
        facts = torch_build_facts()
        if facts is None:
            # No torch: never touch the device probe (it imports torch itself and
            # would bypass the simulate-missing override).
            return None
        return {
            "flavour": facts.flavour.value,
            "version": facts.version,
            "hip_version": facts.hip_version,
            "cuda_version": facts.cuda_version,
            "devices": gpu_device_facts(),
            "rocm_device_archs": installed_rocm_device_archs(),
            "rocm_split_kernels": rocm_split_kernels_installed(),
        }
    except Exception:  # noqa: BLE001 - the health snapshot must never die on a torch fact
        with _PAYLOAD_LOG_LOCK:
            first = not _PAYLOAD_FAILURE_LOGGED
            _PAYLOAD_FAILURE_LOGGED = True
        log.log(logging.WARNING if first else logging.DEBUG, "torch health facts failed; publishing torch=None", exc_info=True)
        return None
