"""
File: modules/ai_backend/runtime/residency/footprint.py

Purpose:
How big a small model is: the pre-load hint from its files, the unknown-size
default, the estimate a load is admitted with, and the arithmetic that turns
two memory samples around a clean load (or a clean lease) into a recorded
footprint. Pure functions over values; the arbiter takes the samples and
decides whether a window was clean.

Key structures:
- `Sample` — the three figures a measurement compares (host `RssAnon`, device
  free, torch reserved)

Key functions:
- `hint_from_files()` — file-byte hint (x1.25 for ONNX), device- or host-side
- `normalize_device()`, `is_device_accounted()`
- `estimate_footprint()` — learned > declared > default
- `measured_footprint()` — the load-window rule (the first clean measurement
  replaces the hint, later ones only raise; ONNX device delta accepted only
  within [0.5x, 4x] of the estimate)
- `remeasured_footprint()` — the release-window rule (only ever raises, capped)

Notes:
- Stdlib only. A file whose size cannot be read is logged at DEBUG and counted
  as unknown (the default applies), the same tolerant-reader rule as
  `..memory_budget`.
- Only `cuda*` devices are device-accounted: MPS shares host memory, and a
  DirectML adapter cannot be probed here, so their bytes are host-accounted.
"""

from __future__ import annotations

import logging
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path

from ..memory_budget import GIB, MIB
from .holdings import Footprint, FootprintHint, Runtime, Source

log = logging.getLogger(__name__)

#: Unknown-size default of a device-accounted model (no hint bytes, nothing learned).
DEFAULT_DEVICE_BYTES = 2 * GIB
#: Unknown-size default of the host side of any model.
DEFAULT_HOST_BYTES = 1 * GIB
#: Host-side bytes `hint_from_files` assumes for a GPU model (Python objects, staging).
GPU_MODEL_HOST_BYTES = 256 * MIB
#: ONNX sessions need arena and workspace on top of the file bytes.
ONNX_FILE_FACTOR = 1.25
#: A `mem_get_info` delta also sees other processes; it is believed only inside these
#: bounds of the estimate.
ONNX_MEASURE_LOWER = 0.5
ONNX_MEASURE_UPPER = 4.0
#: A release re-measure never raises a side above this multiple of the holding's first record.
REMEASURE_CAP_FACTOR = 4.0


def normalize_device(device: str | None) -> str | None:
    """Canonical device name: `cuda` -> `cuda:0`, `cuda:N` kept, anything else lower-cased; empty -> `None`.

    `cuda` without an ordinal is read as device 0: the backend never changes the
    current CUDA device, so the two spellings name the same card.
    """
    text = str(device or "").strip().lower()
    if not text:
        return None
    if text == "cuda":
        return "cuda:0"
    return text


def is_device_accounted(device: str | None) -> bool:
    """Whether `device`'s bytes are budgeted against a device probe (only `cuda*`, CUDA and ROCm)."""
    normalized = normalize_device(device)
    return normalized is not None and normalized.startswith("cuda")


def _path_bytes(path: Path) -> int | None:
    """Size of a file, or the summed size of every file under a directory; `None` when unreadable."""
    try:
        if path.is_dir():
            return sum(child.stat().st_size for child in path.rglob("*") if child.is_file())
        return path.stat().st_size
    except OSError as exc:
        log.debug("footprint: size of %s unreadable (%s); counted as unknown", path, exc)
        return None


def hint_from_files(paths: Iterable[str | Path], device: str | None, *, onnx: bool = False) -> FootprintHint:
    """A pre-load hint from the checkpoint files a model key encodes.

    On a device-accounted device: `device_bytes` = file bytes (x1.25 for ONNX),
    `host_bytes` = 256 MiB. Elsewhere: `device_bytes` = 0 and `host_bytes` =
    file bytes (x1.25 for ONNX). An unreadable path makes the bytes unknown
    (`None`), so the unknown-size default applies.
    """
    total = 0
    for raw in paths:
        size = _path_bytes(Path(raw))
        if size is None:
            total = -1
            break
        total += size
    runtime = Runtime.ONNX if onnx else Runtime.TORCH
    normalized = normalize_device(device)
    if total < 0:
        return FootprintHint(device=normalized, device_bytes=None, host_bytes=None, runtime=runtime)
    weight_bytes = int(total * ONNX_FILE_FACTOR) if onnx else total
    if is_device_accounted(normalized):
        return FootprintHint(device=normalized, device_bytes=weight_bytes, host_bytes=GPU_MODEL_HOST_BYTES, runtime=runtime)
    return FootprintHint(device=normalized, device_bytes=0, host_bytes=weight_bytes, runtime=runtime)


def normalize_hint(hint: FootprintHint) -> FootprintHint:
    """`hint` with a canonical device; a non-device-accounted device folds its device bytes into the host side."""
    device = normalize_device(hint.device)
    if is_device_accounted(device):
        return FootprintHint(device=device, device_bytes=hint.device_bytes, host_bytes=hint.host_bytes, runtime=hint.runtime)
    if hint.device_bytes is None and hint.host_bytes is None:
        host: int | None = None
    else:
        host = max(0, hint.device_bytes or 0) + max(0, hint.host_bytes or 0)
    return FootprintHint(device=None, device_bytes=0, host_bytes=host, runtime=hint.runtime)


def estimate_footprint(hint: FootprintHint | None, learned: Footprint | None) -> Footprint:
    """The footprint a load is admitted with: learned (same device) > declared > unknown-size default.

    `hint=None` gives a host-only `DEFAULT` record (device unknown); the small
    pool does not reserve bytes for such a lease (today's behaviour).
    """
    normalized = normalize_hint(hint) if hint is not None else None
    if learned is not None and (normalized is None or normalized.device == learned.device):
        return Footprint(device=learned.device, device_bytes=learned.device_bytes, host_bytes=learned.host_bytes, source=Source.LEARNED, runtime=learned.runtime)
    if normalized is None:
        return Footprint(device=None, device_bytes=0, host_bytes=DEFAULT_HOST_BYTES, source=Source.DEFAULT)
    defaulted = False
    if normalized.device is None:
        device_bytes = 0
    elif normalized.device_bytes is None:
        device_bytes = DEFAULT_DEVICE_BYTES
        defaulted = True
    else:
        device_bytes = max(0, normalized.device_bytes)
    if normalized.host_bytes is None:
        host_bytes = DEFAULT_HOST_BYTES
        defaulted = True
    else:
        host_bytes = max(0, normalized.host_bytes)
    source = Source.DEFAULT if defaulted else Source.DECLARED
    return Footprint(device=normalized.device, device_bytes=device_bytes, host_bytes=host_bytes, source=source, runtime=normalized.runtime)


@dataclass(frozen=True)
class Sample:
    """Memory figures at one instant (bytes): host `RssAnon`; device free and torch reserved (`None` = not read)."""

    host_rss_anon: int
    device_free: int | None
    device_reserved: int | None


def _device_delta(runtime: Runtime, before: Sample, after: Sample) -> int | None:
    """Device bytes the window added: torch reserved delta, or the free delta for ONNX; `None` if unread."""
    if runtime is Runtime.TORCH:
        if before.device_reserved is None or after.device_reserved is None:
            return None
        return max(0, after.device_reserved - before.device_reserved)
    if before.device_free is None or after.device_free is None:
        return None
    return max(0, before.device_free - after.device_free)


def measured_footprint(estimate: Footprint, before: Sample, after: Sample) -> Footprint:
    """The footprint recorded after a CLEAN load window (the caller checked cleanliness).

    The first clean measurement REPLACES an unmeasured estimate (`DECLARED` hint or
    `DEFAULT`), in both directions; a `LEARNED` estimate (an earlier measurement)
    is only raised: each side is `max(estimate, measured)`. An ONNX device delta outside [0.5x, 4x] of the
    estimate is not believed (another process changed `free`) and the estimate's
    device side is kept. A side that could not be read keeps the estimate.
    """
    replace = estimate.source in (Source.DECLARED, Source.DEFAULT)
    host_delta = max(0, after.host_rss_anon - before.host_rss_anon)
    host = host_delta if replace else max(estimate.host_bytes, host_delta)
    device_bytes = estimate.device_bytes
    if estimate.device is not None:
        delta = _device_delta(estimate.runtime, before, after)
        if delta is not None and estimate.runtime is Runtime.ONNX:
            lower = ONNX_MEASURE_LOWER * estimate.device_bytes
            upper = ONNX_MEASURE_UPPER * estimate.device_bytes
            if not lower <= delta <= upper:
                log.debug("footprint: ONNX device delta %d B outside [%d, %d] of the estimate; ignored", delta, int(lower), int(upper))
                delta = None
        if delta is not None:
            device_bytes = delta if replace else max(estimate.device_bytes, delta)
    return Footprint(device=estimate.device, device_bytes=device_bytes, host_bytes=host, source=Source.MEASURED, runtime=estimate.runtime)


def remeasured_footprint(current: Footprint, first: Footprint, before: Sample, after: Sample) -> Footprint:
    """`current` raised by what a CLEAN lease window grew (ONNX arena growth at the first inference).

    Only ever raises; each side stays at most `REMEASURE_CAP_FACTOR` x the
    holding's `first` record (an inference's own temporary bytes must not
    inflate the record without bound). Returns `current` itself when nothing grew.
    """
    host_growth = max(0, after.host_rss_anon - before.host_rss_anon)
    host_cap = int(REMEASURE_CAP_FACTOR * max(first.host_bytes, 1))
    host = max(current.host_bytes, min(current.host_bytes + host_growth, host_cap))
    device_bytes = current.device_bytes
    if current.device is not None:
        growth = _device_delta(current.runtime, before, after)
        if growth:
            device_cap = int(REMEASURE_CAP_FACTOR * max(first.device_bytes, 1))
            device_bytes = max(current.device_bytes, min(current.device_bytes + growth, device_cap))
    if host == current.host_bytes and device_bytes == current.device_bytes:
        return current
    return Footprint(device=current.device, device_bytes=device_bytes, host_bytes=host, source=Source.MEASURED, runtime=current.runtime)
