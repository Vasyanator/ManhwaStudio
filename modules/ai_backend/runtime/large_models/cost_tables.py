"""
File: modules/ai_backend/runtime/large_models/cost_tables.py

Purpose:
The measured cost model behind every forecast: per-phase activation bytes and
seconds, plus the device-level constants (runtime overhead, allocator margin,
copy and disk rates), loaded from `calibration/<device_class>.json`.

Main responsibilities:
- `device_class_of` / `detect_device_class` - the calibration key of a device
  (`rocm_gfx1201`, `cuda_sm_120`, … or `generic`);
- `load_tables` -> `CostTables`: the file of that class, or — for a class with
  no file — the reference table scaled by `GENERIC_SCALE` and marked
  `calibrated=False`;
- `CostTables.phase_cost` -> `PhaseCost` (the most specific matching entry;
  the catch-all entries are the conservative fallback and are never
  "measured");
- `parse_tables` (the JSON schema check).

JSON schema (`calibration/*.json`):
```
{"device_class": str, "calibrated": bool, "source": str,
 "device": {"runtime_overhead_bytes", "margin_fraction", "margin_cap_bytes",
            "park_bytes_per_s", "restore_bytes_per_s", "block_h2d_bytes_per_s",
            "disk_cold_bytes_per_s", "disk_warm_bytes_per_s", "prefetch_depth"},
 "phases": [{"family", "phase", "variant", "format", "compute_path", "exec",
             "measured": bool,
             "fixed_bytes", "bytes_per_token", "bytes_per_pixel", "reserved_factor",
             "seconds_fixed", "seconds_per_ktoken_pass",
             "seconds_per_ktoken2_pass", "seconds_per_mpixel"}]}
```
Key fields accept `"*"`; `exec` is `"device"` or `"cpu"`. An entry's
`measured` flag is taken as written, also for a field wildcard such as
`decode float32 * *` (a measurement that holds for every format); only a
catch-all entry (`family`, `phase`, `variant`, `format`, `compute_path` all
`"*"`) is forced to `measured=False`. Bytes are
ALLOCATED bytes of the activation; `reserved_factor` turns them into what the
caching allocator reserves inside a phase that starts with an empty cache.

Notes:
- Stdlib only at import; `detect_device_class` imports torch lazily.
"""

from __future__ import annotations

import json
import logging
from collections.abc import Mapping
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any

from ..torch_support import is_torch_available

log = logging.getLogger(__name__)

#: Directory of the tracked calibration files.
CALIBRATION_DIR = Path(__file__).resolve().parent / "calibration"

#: The measured table an uncalibrated device class borrows from.
REFERENCE_DEVICE_CLASS = "rocm_gfx1201"

#: Device class reported when nothing better is known.
GENERIC_DEVICE_CLASS = "generic"

#: How much more conservative a borrowed table is (bytes and seconds x, rates /).
GENERIC_SCALE = 1.1

WILDCARD = "*"
EXEC_DEVICE = "device"
EXEC_CPU = "cpu"

_DEVICE_FIELDS = ("runtime_overhead_bytes", "margin_fraction", "margin_cap_bytes", "park_bytes_per_s", "restore_bytes_per_s", "block_h2d_bytes_per_s", "disk_cold_bytes_per_s", "disk_warm_bytes_per_s", "prefetch_depth")
_RATE_FIELDS = ("park_bytes_per_s", "restore_bytes_per_s", "block_h2d_bytes_per_s", "disk_cold_bytes_per_s", "disk_warm_bytes_per_s")
_KEY_FIELDS = ("family", "phase", "variant", "format", "compute_path", "exec")
_COST_FIELDS = ("fixed_bytes", "bytes_per_token", "bytes_per_pixel", "reserved_factor", "seconds_fixed", "seconds_per_ktoken_pass", "seconds_per_ktoken2_pass", "seconds_per_mpixel")


def device_class_of(*, name: str | None, arch: str | None, is_hip: bool, cuda_capability: tuple[int, int] | None) -> str:
    """Calibration key of a device: `rocm_<gfx arch>`, `cuda_sm_<major><minor>`, or `generic`."""
    if is_hip:
        gfx = (arch or "").split(":", 1)[0].strip().lower()
        return f"rocm_{gfx}" if gfx.startswith("gfx") else GENERIC_DEVICE_CLASS
    if cuda_capability is not None:
        return f"cuda_sm_{cuda_capability[0]}{cuda_capability[1]}"
    log.debug("cost tables: no class for device %r (arch=%r)", name, arch)
    return GENERIC_DEVICE_CLASS


def detect_device_class(device: str) -> str:
    """`device_class_of` for a live torch device; `generic` without torch / GPU (never raises)."""
    if not str(device).lower().startswith("cuda") or not is_torch_available():
        return GENERIC_DEVICE_CLASS
    import torch

    from ..memory_probe import cuda_device_index

    index = cuda_device_index(device)
    try:
        if not torch.cuda.is_available():
            return GENERIC_DEVICE_CLASS
        props = torch.cuda.get_device_properties(index if index is not None else torch.cuda.current_device())
    except (RuntimeError, AssertionError, ValueError) as exc:
        log.debug("cost tables: device properties of %s unavailable (%s)", device, exc)
        return GENERIC_DEVICE_CLASS
    is_hip = getattr(torch.version, "hip", None) is not None
    return device_class_of(name=getattr(props, "name", None), arch=getattr(props, "gcnArchName", None), is_hip=is_hip, cuda_capability=None if is_hip else (int(props.major), int(props.minor)))


@dataclass(frozen=True)
class DeviceConstants:
    """Device-level constants (bytes, bytes/s, fractions).

    `park_bytes_per_s` is a device -> host park into FRESH host memory (page-fault
    bound, far below copy bandwidth); `restore_bytes_per_s` a whole-component
    host -> device restore; `block_h2d_bytes_per_s` the per-pass copy of one
    parked block into a reused device buffer; `runtime_overhead_bytes` the
    non-torch device memory (context, BLAS, kernels) at a phase peak.
    """

    runtime_overhead_bytes: int
    margin_fraction: float
    margin_cap_bytes: int
    park_bytes_per_s: float
    restore_bytes_per_s: float
    block_h2d_bytes_per_s: float
    disk_cold_bytes_per_s: float
    disk_warm_bytes_per_s: float
    prefetch_depth: int


@dataclass(frozen=True)
class PhaseCost:
    """One calibration entry: activation bytes and time of a phase (see the module schema)."""

    family: str
    phase: str
    variant: str
    format: str
    compute_path: str
    exec: str
    measured: bool
    fixed_bytes: int
    bytes_per_token: float
    bytes_per_pixel: float
    reserved_factor: float
    seconds_fixed: float
    seconds_per_ktoken_pass: float
    seconds_per_ktoken2_pass: float
    seconds_per_mpixel: float

    def specificity(self, key: Mapping[str, str]) -> int | None:
        """Number of exactly matching key fields, `None` when some field contradicts `key`."""
        score = 0
        for name in _KEY_FIELDS:
            own = getattr(self, name)
            if own == WILDCARD:
                continue
            if own != key[name]:
                return None
            score += 1
        return score


@dataclass(frozen=True)
class CostTables:
    """A loaded calibration table for one device class.

    `calibrated` is `False` for a borrowed (generic) table, whose entries are
    all unmeasured. A forecast phase is calibrated only when the table is and
    its entry is `measured` (`forecast.PhaseForecast.calibrated`); a catch-all
    entry is never measured, a field wildcard entry keeps its own flag.
    """

    device_class: str
    calibrated: bool
    source: str
    device: DeviceConstants
    phases: tuple[PhaseCost, ...]

    def phase_cost(self, *, family: str, phase: str, variant: str, format: str, compute_path: str, exec: str) -> PhaseCost:
        """The most specific entry matching the key (first one on a tie).

        # Raises
        `LookupError` when not even a wildcard entry matches — a malformed table
        (`parse_tables` requires catch-all entries for both exec kinds).
        """
        key = {"family": family, "phase": phase, "variant": variant, "format": format, "compute_path": compute_path, "exec": exec}
        best: PhaseCost | None = None
        best_score = -1
        for entry in self.phases:
            score = entry.specificity(key)
            if score is not None and score > best_score:
                best, best_score = entry, score
        if best is None:
            raise LookupError(f"cost table {self.device_class}: no entry for {key}")
        return best


def _number(raw: Mapping[str, Any], name: str, where: str) -> float:
    value = raw.get(name)
    if isinstance(value, bool) or not isinstance(value, (int, float)) or value < 0:
        raise ValueError(f"Таблица калибровки {where}: поле «{name}» должно быть неотрицательным числом, получено {value!r}")
    return float(value)


def parse_tables(data: Any, *, source: str) -> CostTables:
    """Validate a decoded calibration JSON and build `CostTables`.

    # Raises
    `ValueError` naming `source` and the bad field: wrong types, negative
    numbers, a `reserved_factor` below 1, an unknown `exec`, or no catch-all
    entry (`family`, `phase`, `variant`, `format`, `compute_path` all `"*"`) for
    both `"device"` and `"cpu"`.
    """
    if not isinstance(data, dict):
        raise ValueError(f"Таблица калибровки {source}: корень не является объектом")
    device_raw = data.get("device")
    phases_raw = data.get("phases")
    if not isinstance(device_raw, dict) or not isinstance(phases_raw, list):
        raise ValueError(f"Таблица калибровки {source}: нужны объект «device» и список «phases»")
    numbers = {name: _number(device_raw, name, source) for name in _DEVICE_FIELDS}
    device = DeviceConstants(
        runtime_overhead_bytes=int(numbers["runtime_overhead_bytes"]),
        margin_fraction=numbers["margin_fraction"],
        margin_cap_bytes=int(numbers["margin_cap_bytes"]),
        park_bytes_per_s=numbers["park_bytes_per_s"],
        restore_bytes_per_s=numbers["restore_bytes_per_s"],
        block_h2d_bytes_per_s=numbers["block_h2d_bytes_per_s"],
        disk_cold_bytes_per_s=numbers["disk_cold_bytes_per_s"],
        disk_warm_bytes_per_s=numbers["disk_warm_bytes_per_s"],
        prefetch_depth=int(numbers["prefetch_depth"]),
    )
    for name in _RATE_FIELDS:
        if numbers[name] <= 0:
            raise ValueError(f"Таблица калибровки {source}: «{name}» должно быть > 0")
    entries: list[PhaseCost] = []
    for index, raw in enumerate(phases_raw):
        where = f"{source} phases[{index}]"
        if not isinstance(raw, dict):
            raise ValueError(f"Таблица калибровки {where}: запись не является объектом")
        keys = {}
        for name in _KEY_FIELDS:
            value = raw.get(name)
            if not isinstance(value, str):
                raise ValueError(f"Таблица калибровки {where}: поле «{name}» должно быть строкой")
            keys[name] = value
        if keys["exec"] not in (EXEC_DEVICE, EXEC_CPU, WILDCARD):
            raise ValueError(f"Таблица калибровки {where}: exec «{keys['exec']}» не device/cpu/*")
        measured = raw.get("measured")
        if not isinstance(measured, bool):
            raise ValueError(f"Таблица калибровки {where}: поле «measured» должно быть bool")
        costs = {name: _number(raw, name, where) for name in _COST_FIELDS}
        if costs["reserved_factor"] < 1.0:
            raise ValueError(f"Таблица калибровки {where}: reserved_factor < 1")
        catch_all = all(keys[name] == WILDCARD for name in _KEY_FIELDS[:5])
        entries.append(PhaseCost(**keys, measured=measured and not catch_all, fixed_bytes=int(costs["fixed_bytes"]), **{name: costs[name] for name in _COST_FIELDS if name != "fixed_bytes"}))
    for exec_kind in (EXEC_DEVICE, EXEC_CPU):
        if not any(all(getattr(entry, name) == WILDCARD for name in _KEY_FIELDS[:5]) and entry.exec in (exec_kind, WILDCARD) for entry in entries):
            raise ValueError(f"Таблица калибровки {source}: нет общей записи «*» для exec={exec_kind}")
    calibrated = data.get("calibrated")
    if not isinstance(calibrated, bool):
        raise ValueError(f"Таблица калибровки {source}: поле «calibrated» должно быть bool")
    return CostTables(device_class=str(data.get("device_class", "")), calibrated=calibrated, source=str(data.get("source", source)), device=device, phases=tuple(entries))


def _read_file(path: Path) -> CostTables:
    """Load and validate one calibration file (`OSError` / `ValueError` propagate)."""
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise ValueError(f"Таблица калибровки {path}: не JSON ({exc})") from exc
    return parse_tables(data, source=str(path))


def _scaled(tables: CostTables, device_class: str, scale: float) -> CostTables:
    """`tables` made `scale` times more conservative and marked uncalibrated."""
    device = tables.device
    scaled_device = replace(
        device,
        runtime_overhead_bytes=int(device.runtime_overhead_bytes * scale),
        park_bytes_per_s=device.park_bytes_per_s / scale,
        restore_bytes_per_s=device.restore_bytes_per_s / scale,
        block_h2d_bytes_per_s=device.block_h2d_bytes_per_s / scale,
        disk_cold_bytes_per_s=device.disk_cold_bytes_per_s / scale,
        disk_warm_bytes_per_s=device.disk_warm_bytes_per_s / scale,
    )
    phases = tuple(
        replace(
            entry,
            measured=False,
            fixed_bytes=int(entry.fixed_bytes * scale),
            bytes_per_token=entry.bytes_per_token * scale,
            bytes_per_pixel=entry.bytes_per_pixel * scale,
            seconds_fixed=entry.seconds_fixed * scale,
            seconds_per_ktoken_pass=entry.seconds_per_ktoken_pass * scale,
            seconds_per_ktoken2_pass=entry.seconds_per_ktoken2_pass * scale,
            seconds_per_mpixel=entry.seconds_per_mpixel * scale,
        )
        for entry in tables.phases
    )
    return CostTables(device_class=device_class, calibrated=False, source=f"{tables.source} x{scale}", device=scaled_device, phases=phases)


def load_tables(device_class: str, *, directory: Path = CALIBRATION_DIR) -> CostTables:
    """The calibration table of `device_class`, or the reference table borrowed conservatively.

    A class without its own file gets `REFERENCE_DEVICE_CLASS`'s table scaled by
    `GENERIC_SCALE` and `calibrated=False` (logged at INFO).

    # Raises
    `OSError` when the reference file itself is missing and `ValueError` when a
    file is malformed: the tables are tracked package data, so either is an
    installation defect, never a reason to forecast from nothing.
    """
    own = directory / f"{device_class}.json"
    if own.is_file():
        return _read_file(own)
    log.info("cost tables: no calibration for %s, borrowing %s x%.2f (uncalibrated)", device_class, REFERENCE_DEVICE_CLASS, GENERIC_SCALE)
    return _scaled(_read_file(directory / f"{REFERENCE_DEVICE_CLASS}.json"), device_class, GENERIC_SCALE)
