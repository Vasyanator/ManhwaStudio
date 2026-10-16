"""
File: modules/ai_backend/runtime/torch_device.py

Purpose:
The one owner of three decisions Torch services in the backend make: which
concrete Torch device the user's `General.ai_device` choice resolves to right
now, which CUDA/ROCm devices count as integrated GPUs that a large model must
never run on, and how the accelerator allocator cache is released after a model
drop.

Key functions:
- read_configured_device() - `General.ai_device`, `None` for unset / sentinel
- normalize_backend_device() - coerce a device string to a Torch device name
- safe_available_devices() - `AIDevice.detect_available_devices()`, `{"cpu"}` on failure
- resolve_selected_backend_device() - the full resolution rule
- is_integrated_gpu() - the "this is an iGPU" rule over device properties
- resolve_discrete_backend_device() - the resolution rule that never answers an iGPU
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
The large-model services (FLUX.1-Fill, FLUX.2 klein, FLUX.3 klein) resolve
through `resolve_discrete_backend_device` instead: on a machine with an APU the
integrated GPU is also a "cuda" device, and a multi-GB model on it is either
refused for memory or runs at a fraction of the speed while starving the host.
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


#: Device-name fragments (lower-cased) that mark an integrated GPU.
_IGPU_NAME_MARKERS = ("ryzen", "cpu", "radeon graphics integrated")

#: AMD integrated-GPU architectures, matched EXACTLY on the `gcnArchName` base
#: (before any `:feature` suffix). Never a prefix: `gfx103*` also names the
#: discrete RX 6000 cards (gfx1030/1031/1032/1034).
_IGPU_ARCHS = frozenset(
    {
        "gfx902",  # Raven Ridge / Picasso
        "gfx90c",  # Renoir / Cezanne / Lucienne
        "gfx1033",  # Van Gogh
        "gfx1035",  # Rembrandt
        "gfx1036",  # Raphael / Granite Ridge desktop iGPU
        "gfx1037",  # Mendocino
        "gfx1103",  # Phoenix / Hawk Point
        "gfx1150",  # Strix Point
        "gfx1151",  # Strix Halo
        "gfx1152",  # Krackan Point
    }
)


def is_integrated_gpu(props: object) -> bool:
    """True when the device described by `props` (a `torch.cuda` device-properties object) is an iGPU.

    Integrated when `props.is_integrated` exists and is truthy, OR the name
    carries an iGPU marker, OR the arch is one of `_IGPU_ARCHS` exactly. A
    missing attribute counts as "no evidence", never as integrated.
    """
    if bool(getattr(props, "is_integrated", False)):
        return True
    name = str(getattr(props, "name", "")).lower()
    if any(marker in name for marker in _IGPU_NAME_MARKERS):
        return True
    arch = str(getattr(props, "gcnArchName", "")).lower().split(":", 1)[0].strip()
    return arch in _IGPU_ARCHS


def _cuda_device_properties() -> list[object]:
    """Properties of every visible CUDA/ROCm device in index order; `[]` without torch or CUDA.

    Never raises: a failing probe is logged at WARNING and counts as no device.
    The tests replace this function.
    """
    try:
        import torch  # type: ignore
    except Exception:
        # No torch: no CUDA device can be used anyway.
        return []
    try:
        if not torch.cuda.is_available():
            return []
        return [torch.cuda.get_device_properties(index) for index in range(torch.cuda.device_count())]
    except Exception as exc:
        log.warning("torch CUDA device probing failed, assuming no CUDA device: %s", exc, exc_info=True)
        return []


def _best_discrete_cuda_index(devices: list[object]) -> int | None:
    """Index of the non-integrated device with the most `total_memory`; `None` when there is none."""
    best: tuple[int, int] | None = None
    for index, props in enumerate(devices):
        if is_integrated_gpu(props):
            continue
        total = int(getattr(props, "total_memory", 0) or 0)
        if best is None or total > best[1]:
            best = (index, total)
    return None if best is None else best[0]


def _cuda_index(device: str) -> int | None:
    """`<n>` of `cuda:<n>`; `None` for plain `cuda` or a malformed index."""
    _, sep, raw = device.partition(":")
    if not sep:
        return None
    try:
        return int(raw)
    except ValueError:
        return None


def resolve_discrete_backend_device(
    fallback: str,
    *,
    accept_mps: bool = True,
    mps_last_resort: bool = False,
    honour_configured: bool = True,
) -> str:
    """Resolve the device for a large model: never an integrated GPU.

    With `honour_configured` (FLUX.2 / FLUX.3 klein), `General.ai_device` (or the
    normalized `fallback` when nothing is configured) decides first: `cpu` ->
    `cpu`; an available `mps` -> `mps`; a `cuda:<n>` that exists and is discrete
    -> it. A configured iGPU is overridden with a WARNING. Every other case
    (plain `cuda`, an unknown index, an iGPU, an unavailable `mps`, an unknown
    value) takes the discrete device with the most memory as `cuda:<n>`, else
    `mps` when `mps_last_resort` and available, else `cpu`. Without
    `honour_configured` (FLUX.1-Fill) the configuration is ignored: the largest
    discrete device, else (`mps` per `mps_last_resort`) `cpu`. Never raises; a failing probe counts as CPU only.
    """
    devices = _cuda_device_properties()
    best = _best_discrete_cuda_index(devices)
    if honour_configured:
        fallback_norm = normalize_backend_device(fallback, "cpu", accept_mps=accept_mps)
        configured = read_configured_device()
        normalized = normalize_backend_device(configured or fallback_norm, fallback_norm, accept_mps=accept_mps)
        if normalized == "cpu":
            return "cpu"
        if normalized == "mps" and "mps" in safe_available_devices():
            return "mps"
        index = _cuda_index(normalized)
        if index is not None and 0 <= index < len(devices):
            if not is_integrated_gpu(devices[index]):
                return normalized
            log.warning(
                "Configured device %s is an integrated GPU (%s); a large model uses %s instead",
                normalized,
                getattr(devices[index], "name", "?"),
                "cpu" if best is None else f"cuda:{best}",
            )
    if best is not None:
        return f"cuda:{best}"
    if mps_last_resort and "mps" in safe_available_devices():
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
