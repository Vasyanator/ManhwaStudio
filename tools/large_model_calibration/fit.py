"""
File: tools/large_model_calibration/fit.py

Purpose:
The `fit` and `check` subcommands. `fit` turns measurement reports
(`report.json` of `flux3` / `flux2` runs on ONE device class) into the large-model
manager's cost table `calibration/<device_class>.json`; `check` judges the
forecasts of a run against what it measured (the acceptance numbers).

Key functions:
- `collect_points()` - per cost-table key the measured points of the model
  phases (`PhasePoint`), from the steps the recorder annotated with `work`
- `fit_entry()` - one cost-table entry from its points (`EntryFit`)
- `fit_device()` - the device constants (runtime overhead, park / restore /
  per-pass block copy / warm disk rates)
- `build_table()` - the whole table over a base table
- `check_reports()` - forecast vs measured per cell, monotonicity, which
  cells fit

Fit rules (the forecast's arithmetic, `runtime/large_models/forecast.py`):
- An entry is keyed like the forecast looks it up: (family, phase, variant,
  format, compute_path, exec). A phase computed by a non-DiT component (text
  encoder, autoencoder) does not depend on the DiT weight format: its entry
  keeps `format` / `compute_path` `"*"`.
- Device activation `y` = peak allocated - every handle's ledger device bytes
  at the step end - the phase's `extra_device_bytes` - for a juggled compute
  component the staging the forecast charges separately (`prefetch_depth + 1`
  largest non-resident blocks; the block sizes come with the phase's `work`);
  CPU activation `y` = the in-step RSS peak - RSS at the step begin.
- `fixed_bytes + bytes_per_<x> * x` is the UPPER ENVELOPE of the allocated
  activations (`x` = tokens, or pixels when the phase has no tokens): the
  non-negative line through the points with the smallest total excess.
  `reserved_factor` = the largest (peak reserved - ledger bytes) / envelope
  over the points (>= 1), so every measured point is covered in reserved
  bytes as well. With one distinct `x`, the base entry's shape is scaled up
  to cover it (or a line through the origin without a base entry).
- Seconds: the minimum wall time per distinct `x` (steady state; the first
  run of a process pays kernel set-up), fitted by non-negative least squares
  to `seconds_fixed + passes * (s_k * kt + s_k2 * kt^2) + s_mp * mp`
  (denoise: the two pass terms; other phases: fixed + one slope; with one
  distinct `x` the slope alone) - from wholly resident phases only (a juggled
  phase adds the forecast's copy term).
- An entry fitted from at least one point is `measured: true` (written in the
  base's order, new keys after it); an entry the runs did not reach keeps the
  base table's (`measured` kept only when the
  base table is of the same device class, else `false`). The catch-all
  entries always come from the base.
- Device constants: `runtime_overhead_bytes` = the largest non-torch device
  memory at any step end; park / restore = median bytes moved / wall over
  transitions that parked / restored >= `MIN_MOVE_BYTES`; per-pass block H2D
  = the slowest of the host-juggled denoise passes (remainder bytes / (pass
  time - the resident fit)); warm disk = median of component loads read faster than
  `WARM_FACTOR` x the base cold rate. The cold disk rate, the margin and the
  prefetch depth are kept from the base (a cold read needs a dropped page
  cache; the margin and depth are policy).

Notes:
- No torch: `fit` / `check` run anywhere (stdlib plus the manager's own
  `cost_tables.parse_tables`, which validates the written table).
"""

from __future__ import annotations

import argparse
import itertools
import json
import logging
import statistics
from collections.abc import Iterable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger(__name__)

GIB = 1024**3
MIB = 1024**2

#: Transitions moving less than this are not used for a rate.
MIN_MOVE_BYTES = 512 * MIB

#: A load faster than this many times the base cold rate counts as a warm (page-cache) read.
WARM_FACTOR = 4.0

#: Key fields of an entry, in the table's order.
KEY_FIELDS = ("family", "phase", "variant", "format", "compute_path", "exec")

#: Roles whose phases depend on the DiT weight format (`spec.Role` values).
FORMAT_ROLES = frozenset({"dit"})


@dataclass(frozen=True)
class PhasePoint:
    """One measured model phase: `x` (tokens or pixels), `passes`, activation bytes
    (`alloc`, `reserved`; host bytes for a CPU phase; a juggled phase's staging taken
    out), wall `seconds`, whether the compute component was wholly resident
    (`resident`: the time fits), whether the bytes are usable (`sized`: resident, CPU, or
    juggled with known block sizes), the host-juggled remainder bytes (`host_remainder`,
    else 0) and where it came from (`source`)."""

    x: int
    tokens: int
    pixels: int
    passes: int
    alloc: int
    reserved: int
    seconds: float
    resident: bool
    sized: bool
    host_remainder: int
    source: str


@dataclass
class EntryFit:
    """One fitted cost entry and the evidence behind it."""

    key: tuple[str, ...]
    by_tokens: bool
    points: list[PhasePoint] = field(default_factory=list)
    fixed_bytes: int = 0
    slope: float = 0.0
    reserved_factor: float = 1.0
    seconds_fixed: float = 0.0
    seconds_per_ktoken_pass: float = 0.0
    seconds_per_ktoken2_pass: float = 0.0
    seconds_per_mpixel: float = 0.0
    bytes_measured: bool = False
    seconds_measured: bool = False


# ---------------------------------------------------------------- reading


def load_reports(paths: Iterable[Path]) -> list[tuple[Path, dict[str, Any]]]:
    """Every `report.json` under `paths` (a file, or a directory searched recursively), decoded.

    # Raises
    `FileNotFoundError` when nothing is found; `ValueError` for a file that is not JSON.
    """
    files: list[Path] = []
    for path in paths:
        if path.is_dir():
            files.extend(sorted(path.rglob("report.json")))
        elif path.is_file():
            files.append(path)
        else:
            raise FileNotFoundError(f"{path}: no such report or directory")
    if not files:
        raise FileNotFoundError("no report.json found")
    reports = []
    for file in files:
        try:
            reports.append((file, json.loads(file.read_text(encoding="utf-8"))))
        except json.JSONDecodeError as exc:
            raise ValueError(f"{file}: not JSON ({exc})") from exc
    return reports


def _component(placements: dict[str, Any], name: str, model: str | None) -> dict[str, Any] | None:
    """The placement of component `name` in a step's placements snapshot: in the handle of `model`
    (`work["model"]`; any handle when unknown) that holds it - an earlier combo's dropped handle of
    the same model family (another format or encoder) stays listed with nothing held."""
    found = [
        component
        for handle in placements.values()
        if model is None or handle.get("model") == model
        for component in [handle.get("components", {}).get(name)]
        if component is not None
    ]
    held = [component for component in found if component.get("tier") is not None]
    return held[0] if held else (found[0] if found else None)


def _device_bytes(placements: dict[str, Any]) -> int:
    """Ledger device bytes of every component of every handle."""
    return sum(int(component.get("device_bytes", 0)) for handle in placements.values() for component in handle.get("components", {}).values())


def _host_bytes(placements: dict[str, Any]) -> int:
    """Ledger host bytes of every component of every handle."""
    return sum(int(component.get("host_bytes", 0)) for handle in placements.values() for component in handle.get("components", {}).values())


def _usable(combo: dict[str, Any]) -> bool:
    """A combo whose steps describe the plan it asked for: ran `ok` and did not degrade."""
    return combo.get("status") == "ok" and not combo.get("degraded")


def entry_key(work: dict[str, Any], exec_kind: str) -> tuple[str, ...]:
    """The cost-table key a phase is looked up with (`format` / `compute_path` `*` off the DiT)."""
    on_format = work.get("role") in FORMAT_ROLES
    return (work["family"], work["phase"], work["variant"], work["format"] if on_format else "*", work["compute_path"] if on_format else "*", exec_kind)


def _staging(work: dict[str, Any], placement: dict[str, Any], prefetch_depth: int) -> int | None:
    """The device staging the forecast charges a juggled phase: `prefetch_depth + 1` times the
    largest block not resident on the device (`None` when the block sizes are unknown)."""
    blocks = work.get("block_bytes")
    if blocks is None:
        return None
    rest = blocks[int(placement.get("blocks_device", 0)):]
    return (prefetch_depth + 1) * max(rest, default=0)


def collect_points(reports: list[tuple[Path, dict[str, Any]]], prefetch_depth: int) -> dict[tuple[str, ...], list[PhasePoint]]:
    """Per cost-table key, the points of every usable annotated model phase (module rules);
    `prefetch_depth` is the device constant the forecast stages juggled blocks with."""
    points: dict[tuple[str, ...], list[PhasePoint]] = {}
    for path, report in reports:
        for combo in report.get("combos", []):
            if not _usable(combo):
                continue
            for step in combo.get("phases", []):
                work = step.get("work")
                if not work or not step.get("ended") or step.get("failed") or step.get("overlapped"):
                    continue
                placement = _component(step.get("placements_end", {}), work["compute"], work.get("model"))
                if placement is None:
                    log.warning("%s %s %s: compute component %s not in the placements; skipped", path, combo["combo_id"], step["phase"], work["compute"])
                    continue
                cpu = placement.get("exec_device") == "cpu"
                exec_kind = "cpu" if cpu else "device"
                if cpu:
                    begin, peak = step.get("rss_begin"), step.get("rss_peak")
                    if begin is None or peak is None:
                        continue
                    alloc = reserved = max(0, int(peak) - int(begin))
                    resident = sized = True
                else:
                    resident = placement.get("tier") == "device"
                    staging = 0 if resident else _staging(work, placement, prefetch_depth)
                    sized = staging is not None
                    weights = _device_bytes(step.get("placements_end", {})) + int(work.get("extra_device_bytes", 0)) + (staging or 0)
                    alloc = max(0, int(step["peak_alloc"]) - weights)
                    reserved = max(0, int(step["peak_reserved"]) - weights)
                host_remainder = int(placement.get("host_bytes", 0)) if placement.get("tier") == "host" and not cpu else 0
                tokens, pixels = int(work["tokens"]), int(work["pixels"])
                points.setdefault(entry_key(work, exec_kind), []).append(
                    PhasePoint(x=tokens if tokens > 0 else pixels, tokens=tokens, pixels=pixels, passes=max(1, int(work["passes"])), alloc=alloc, reserved=reserved, seconds=float(step["wall_s"]), resident=resident, sized=sized, host_remainder=host_remainder, source=f"{path.parent.name}/{combo['combo_id']}")
                )
    return points


# ---------------------------------------------------------------- fitting


def upper_envelope(xs: list[float], ys: list[float]) -> tuple[float, float]:
    """`(a, b)`, both >= 0, with `a + b*x >= y` for every point and the smallest total excess.

    The optimum of this two-variable linear program is a line through two points,
    or a horizontal line / a line through the origin touching one point; every
    candidate is tried. `ValueError` for no points.
    """
    if not xs:
        raise ValueError("upper_envelope: no points")
    candidates: list[tuple[float, float]] = [(max(ys), 0.0)]
    candidates.extend((0.0, y / x) for x, y in zip(xs, ys) if x > 0)
    for (x1, y1), (x2, y2) in itertools.combinations(zip(xs, ys), 2):
        if x1 != x2:
            slope = (y2 - y1) / (x2 - x1)
            candidates.append((y1 - slope * x1, slope))
    best: tuple[float, float] | None = None
    best_excess = float("inf")
    # Float rounding of a line through two points, relative to the data's scale.
    tolerance = 1e-9 * max(1.0, max(abs(y) for y in ys))
    for a, b in candidates:
        if a < 0 or b < 0:
            continue
        excess = [a + b * x - y for x, y in zip(xs, ys)]
        if min(excess) < -tolerance:
            continue
        total = sum(excess)
        if total < best_excess:
            best, best_excess = (a, b), total
    assert best is not None  # the horizontal line through max(ys) is always feasible
    return best


def nnls(rows: list[list[float]], targets: list[float]) -> list[float]:
    """Non-negative least squares for a few columns: the best non-negative solution over every column subset."""
    columns = len(rows[0]) if rows else 0
    best = [0.0] * columns
    best_error = sum(target * target for target in targets)
    for size in range(1, columns + 1):
        for subset in itertools.combinations(range(columns), size):
            solution = _least_squares([[row[index] for index in subset] for row in rows], targets)
            if solution is None or any(value < 0 for value in solution):
                continue
            full = [0.0] * columns
            for index, value in zip(subset, solution):
                full[index] = value
            error = sum((sum(row[i] * full[i] for i in range(columns)) - target) ** 2 for row, target in zip(rows, targets))
            if error < best_error:
                best, best_error = full, error
    return best


def _least_squares(rows: list[list[float]], targets: list[float]) -> list[float] | None:
    """Ordinary least squares by the normal equations (Gauss-Jordan); `None` when singular."""
    size = len(rows[0])
    matrix = [[sum(row[i] * row[j] for row in rows) for j in range(size)] + [sum(row[i] * target for row, target in zip(rows, targets))] for i in range(size)]
    for column in range(size):
        pivot = max(range(column, size), key=lambda r: abs(matrix[r][column]))
        if abs(matrix[pivot][column]) < 1e-12:
            return None
        matrix[column], matrix[pivot] = matrix[pivot], matrix[column]
        for r in range(size):
            if r != column:
                factor = matrix[r][column] / matrix[column][column]
                matrix[r] = [value - factor * pivot_value for value, pivot_value in zip(matrix[r], matrix[column])]
    return [matrix[i][size] / matrix[i][i] for i in range(size)]


def _base_line(base: dict[str, Any] | None, by_tokens: bool) -> tuple[float, float] | None:
    """`(fixed, slope)` of a base entry, `None` without one."""
    if base is None:
        return None
    return float(base["fixed_bytes"]), float(base["bytes_per_token" if by_tokens else "bytes_per_pixel"])


def fit_entry(key: tuple[str, ...], points: list[PhasePoint], base: dict[str, Any] | None) -> EntryFit:
    """One entry from its points (module rules); `base` is the base table's entry of the same key."""
    by_tokens = any(point.tokens > 0 for point in points)
    fit = EntryFit(key=key, by_tokens=by_tokens, points=points)
    sized = [point for point in points if point.sized]
    if sized:
        xs = [float(point.x) for point in sized]
        allocs = [float(point.alloc) for point in sized]
        if len(set(xs)) >= 2:
            fixed, slope = upper_envelope(xs, allocs)
            fit.fixed_bytes, fit.slope = int(round(fixed)), slope
        else:
            line = _base_line(base, by_tokens)
            if line is not None and line[0] + line[1] * xs[0] > 0:
                scale = max(1.0, max(allocs) / (line[0] + line[1] * xs[0]))
                fit.fixed_bytes, fit.slope = int(round(line[0] * scale)), line[1] * scale
            else:
                fit.fixed_bytes, fit.slope = 0, max(allocs) / xs[0] if xs[0] > 0 else 0.0
                if xs[0] <= 0:
                    fit.fixed_bytes = int(max(allocs))
        envelope = [fit.fixed_bytes + fit.slope * x for x in xs]
        cpu = key[5] == "cpu"
        factors = [point.reserved / value for point, value in zip(sized, envelope) if value > 0]
        fit.reserved_factor = 1.0 if cpu else max([1.0, *factors])
        fit.bytes_measured = True
    # Seconds: steady state per distinct x (the minimum), resident phases only.
    fastest: dict[int, PhasePoint] = {}
    for point in points:
        if not point.resident:
            continue
        if point.x not in fastest or point.seconds < fastest[point.x].seconds:
            fastest[point.x] = point
    chosen = list(fastest.values())
    if chosen:
        # One distinct x cannot tell a constant from a slope: the slope alone then
        # (it grows with the request, the conservative reading for larger ones).
        several = len(chosen) >= 2
        targets = [point.seconds for point in chosen]
        if key[1] == "denoise":
            rows = [[point.passes * point.tokens / 1000.0, point.passes * (point.tokens / 1000.0) ** 2] if several else [point.passes * point.tokens / 1000.0] for point in chosen]
            solution = nnls(rows, targets) + [0.0]
            fit.seconds_per_ktoken_pass, fit.seconds_per_ktoken2_pass = solution[0], solution[1]
        elif by_tokens:
            rows = [[1.0, point.passes * point.tokens / 1000.0] if several else [point.passes * point.tokens / 1000.0] for point in chosen]
            solution = nnls(rows, targets)
            fit.seconds_fixed, fit.seconds_per_ktoken_pass = (solution[0], solution[1]) if several else (0.0, solution[0])
        else:
            rows = [[1.0, point.pixels / 1e6] if several else [point.pixels / 1e6] for point in chosen]
            solution = nnls(rows, targets)
            fit.seconds_fixed, fit.seconds_per_mpixel = (solution[0], solution[1]) if several else (0.0, solution[0])
        fit.seconds_measured = True
    return fit


def predicted_seconds(fit: EntryFit, point: PhasePoint) -> float:
    """The fitted seconds of `point` without any copy term."""
    kt = point.tokens / 1000.0
    return fit.seconds_fixed + point.passes * (fit.seconds_per_ktoken_pass * kt + fit.seconds_per_ktoken2_pass * kt * kt) + fit.seconds_per_mpixel * point.pixels / 1e6


def _moves(reports: list[tuple[Path, dict[str, Any]]]) -> tuple[list[float], list[float], list[float]]:
    """(park, restore, load) byte rates of the transitions / loads of usable combos (bytes/s)."""
    parks: list[float] = []
    restores: list[float] = []
    loads: list[float] = []
    for _path, report in reports:
        for combo in report.get("combos", []):
            if not _usable(combo):
                continue
            for step in combo.get("phases", []):
                if step.get("work") or not step.get("ended") or step.get("failed") or step.get("overlapped") or float(step.get("wall_s", 0)) <= 0:
                    continue
                begin, end = step.get("placements_begin", {}), step.get("placements_end", {})
                d_dev = _device_bytes(end) - _device_bytes(begin)
                d_host = _host_bytes(end) - _host_bytes(begin)
                wall = float(step["wall_s"])
                if d_dev < 0 and d_host > 0 and min(-d_dev, d_host) >= MIN_MOVE_BYTES:
                    parks.append(min(-d_dev, d_host) / wall)
                elif d_dev > 0 and d_host < 0 and min(d_dev, -d_host) >= MIN_MOVE_BYTES:
                    restores.append(min(d_dev, -d_host) / wall)
                elif d_dev >= 0 and d_host >= 0 and d_dev + d_host >= MIN_MOVE_BYTES and str(step.get("info", {}).get("action", "loaded")) == "loaded":
                    loads.append((d_dev + d_host) / wall)
    return parks, restores, loads


def fit_device(reports: list[tuple[Path, dict[str, Any]]], fits: dict[tuple[str, ...], EntryFit], base_device: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any]]:
    """`(device constants, evidence)`: the measured constants over `base_device` (module rules)."""
    device = dict(base_device)
    evidence: dict[str, Any] = {}
    overheads = [int(step["non_torch_end"]) for _path, report in reports for combo in report.get("combos", []) if _usable(combo) for step in combo.get("phases", []) if step.get("ended") and step.get("non_torch_end") is not None]
    if overheads:
        device["runtime_overhead_bytes"] = int(-(-max(overheads) // MIB) * MIB)
        evidence["runtime_overhead"] = {"samples": len(overheads), "max_gib": round(max(overheads) / GIB, 3), "median_gib": round(statistics.median(overheads) / GIB, 3)}
    parks, restores, loads = _moves(reports)
    if parks:
        device["park_bytes_per_s"] = int(round(statistics.median(parks)))
        evidence["park"] = {"samples": len(parks), "median_gib_s": round(statistics.median(parks) / GIB, 2), "min_gib_s": round(min(parks) / GIB, 2)}
    if restores:
        device["restore_bytes_per_s"] = int(round(statistics.median(restores)))
        evidence["restore"] = {"samples": len(restores), "median_gib_s": round(statistics.median(restores) / GIB, 2), "min_gib_s": round(min(restores) / GIB, 2)}
    warm = [rate for rate in loads if rate > WARM_FACTOR * float(base_device["disk_cold_bytes_per_s"])]
    if warm:
        device["disk_warm_bytes_per_s"] = int(round(statistics.median(warm)))
        evidence["disk_warm"] = {"samples": len(warm), "of_loads": len(loads), "median_gib_s": round(statistics.median(warm) / GIB, 2)}
    copies: list[float] = []
    for fit in fits.values():
        if fit.key[1] != "denoise" or not fit.seconds_measured:
            continue
        for point in fit.points:
            if point.resident or point.host_remainder < MIN_MOVE_BYTES:
                continue
            extra = point.seconds - predicted_seconds(fit, point)
            if extra > 0:
                copies.append(point.host_remainder / (extra / point.passes))
    if copies:
        # The slowest effective rate: the extra time per pass is small and noisy
        # where the prefetch hides the copy behind the compute, so the median would
        # promise an overlap that a faster (less compute-bound) format does not get.
        device["block_h2d_bytes_per_s"] = int(round(min(copies)))
        evidence["block_h2d"] = {"samples": len(copies), "median_gib_s": round(statistics.median(copies) / GIB, 2), "min_gib_s": round(min(copies) / GIB, 2)}
    return device, evidence


# ---------------------------------------------------------------- table


def _entry_dict(fit: EntryFit, base: dict[str, Any] | None) -> dict[str, Any]:
    """The JSON entry of a fit (byte or time fields the runs did not reach come from `base`)."""
    family, phase, variant, weight_format, compute_path, exec_kind = fit.key
    entry: dict[str, Any] = dict(base) if base is not None else {"fixed_bytes": 0, "bytes_per_token": 0.0, "bytes_per_pixel": 0.0, "reserved_factor": 1.0, "seconds_fixed": 0.0, "seconds_per_ktoken_pass": 0.0, "seconds_per_ktoken2_pass": 0.0, "seconds_per_mpixel": 0.0}
    entry.update({"family": family, "phase": phase, "variant": variant, "format": weight_format, "compute_path": compute_path, "exec": exec_kind, "measured": fit.bytes_measured})
    if fit.bytes_measured:
        entry["fixed_bytes"] = int(fit.fixed_bytes)
        entry["bytes_per_token"] = round(fit.slope, 1) if fit.by_tokens else 0.0
        entry["bytes_per_pixel"] = 0.0 if fit.by_tokens else round(fit.slope, 1)
        entry["reserved_factor"] = round(max(1.0, fit.reserved_factor) + 0.005, 2)
    if fit.seconds_measured:
        entry.update({"seconds_fixed": round(fit.seconds_fixed, 4), "seconds_per_ktoken_pass": round(fit.seconds_per_ktoken_pass, 5), "seconds_per_ktoken2_pass": round(fit.seconds_per_ktoken2_pass, 6), "seconds_per_mpixel": round(fit.seconds_per_mpixel, 4)})
    return {name: entry[name] for name in (*KEY_FIELDS[:-1], "exec", "measured", "fixed_bytes", "bytes_per_token", "bytes_per_pixel", "reserved_factor", "seconds_fixed", "seconds_per_ktoken_pass", "seconds_per_ktoken2_pass", "seconds_per_mpixel")}


def _key_of(entry: dict[str, Any]) -> tuple[str, ...]:
    return tuple(str(entry[name]) for name in KEY_FIELDS)


def build_table(reports: list[tuple[Path, dict[str, Any]]], base: dict[str, Any], device_class: str, source: str) -> tuple[dict[str, Any], dict[str, Any]]:
    """`(table, evidence)`: every key the runs reached, fitted; every other base entry kept (module rules)."""
    same_class = base.get("device_class") == device_class
    base_entries = {_key_of(entry): entry for entry in base["phases"]}
    points = collect_points(reports, int(base["device"]["prefetch_depth"]))
    fits = {key: fit_entry(key, group, base_entries.get(key)) for key, group in sorted(points.items())}
    device, device_evidence = fit_device(reports, fits, base["device"])
    fitted = {key: _entry_dict(fit, base_entries.get(key)) for key, fit in fits.items() if fit.bytes_measured or fit.seconds_measured}
    # The base's order first (a refit keeps a small diff), new keys after it, catch-alls last.
    phases: list[dict[str, Any]] = []
    catch_alls = []
    for key, entry in base_entries.items():
        if key in fitted:
            phases.append(fitted[key])
            continue
        kept = dict(entry)
        if not same_class:
            kept["measured"] = False
        (catch_alls if all(entry[name] == "*" for name in KEY_FIELDS[:5]) else phases).append(kept)
    phases.extend(entry for key, entry in fitted.items() if key not in base_entries)
    table = {"device_class": device_class, "calibrated": True, "source": source, "device": device, "phases": phases + catch_alls}
    evidence = {
        "device": device_evidence,
        "entries": [
            {
                "key": dict(zip(KEY_FIELDS, fit.key)),
                "by": "tokens" if fit.by_tokens else "pixels",
                "points": [{"x": point.x, "passes": point.passes, "alloc_gib": round(point.alloc / GIB, 4), "reserved_gib": round(point.reserved / GIB, 4), "envelope_gib": round((fit.fixed_bytes + fit.slope * point.x) / GIB, 4), "seconds": round(point.seconds, 3), "fitted_s": round(predicted_seconds(fit, point), 3), "resident": point.resident, "source": point.source} for point in fit.points],
            }
            for fit in fits.values()
        ],
    }
    return table, evidence


# ---------------------------------------------------------------- check


def _refused_pair(grid: dict[str, Any], violation: dict[str, Any]) -> bool:
    """Whether a forecast violation involves a refused cell (no plan; reports written before
    `report._axis_violations` skipped those)."""
    if violation.get("kind") != "forecast":
        return False
    cells = grid.get("cells", {})
    fixed, axis = violation["fixed"], violation["axis"]
    keys = [f"{step}/{fixed}" if axis == "vram" else f"{fixed}/{step}" for step in (violation["higher"], violation["lower"])]
    return any(cells.get(key, {}).get("status") == "refused" for key in keys)


def check_reports(reports: list[tuple[Path, dict[str, Any]]]) -> dict[str, Any]:
    """Acceptance numbers over every combo of `reports`: per usable combo the forecast vs measured
    device peak and host growth, the misses (host misses of a process's first combo apart), the
    mean over-forecast, the monotonic violations of every grid, and the combos that did not run
    `ok` or degraded."""
    rows = []
    for path, report in reports:
        # One report = one process: its first combo that ran a step pays the process's
        # one-time initialisation (derived here, so older reports are judged alike).
        first_ran = next((combo["combo_id"] for combo in report.get("combos", []) if combo.get("phases")), None)
        for combo in report.get("combos", []):
            planned = combo.get("planned") or {}
            row = {
                "report": path.parent.name,
                "combo_id": combo["combo_id"],
                "status": combo.get("status"),
                "degraded": combo.get("degraded"),
                "fits": planned.get("fits"),
                "steps": f"{combo.get('vram_step')}/{combo.get('ram_step')}",
                "effective": f"{combo.get('vram_effective')}/{combo.get('ram_effective')}",
                "forecast_device_gib": combo.get("forecast_device_peak_gib"),
                "measured_device_gib": combo.get("measured_device_peak_gib"),
                "forecast_host_gib": combo.get("forecast_host_growth_gib"),
                "measured_host_gib": combo.get("measured_host_growth_gib"),
                "first_in_process": combo["combo_id"] == first_ran,
            }
            rows.append(row)
    judged = [row for row in rows if row["status"] == "ok" and not row["degraded"] and row["forecast_device_gib"] is not None and row["measured_device_gib"] is not None]
    device_miss = [row for row in judged if row["forecast_device_gib"] < row["measured_device_gib"]]
    # The first combo of a process also grows by the process's one-time
    # initialisation, which no plan holds: judged on the host side separately.
    host_all = [row for row in judged if row["forecast_host_gib"] is not None and row["measured_host_gib"] is not None]
    host_judged = [row for row in host_all if not row["first_in_process"]]
    host_miss = [row for row in host_judged if row["forecast_host_gib"] < row["measured_host_gib"]]
    first_host_miss = [row for row in host_all if row["first_in_process"] and row["forecast_host_gib"] < row["measured_host_gib"]]
    over = [row["forecast_device_gib"] - row["measured_device_gib"] for row in judged]
    violations = [
        {"report": path.parent.name, "grid": grid["grid"], **violation}
        for path, report in reports
        for grid in report.get("grids", [])
        for violation in grid.get("violations", [])
        if not _refused_pair(grid, violation)
    ]
    return {
        "combos": rows,
        "judged": len(judged),
        "device_misses": device_miss,
        "host_misses": host_miss,
        "first_in_process_host_misses": first_host_miss,
        "mean_device_over_forecast_gib": round(statistics.mean(over), 3) if over else None,
        "max_device_over_forecast_gib": round(max(over), 3) if over else None,
        "mean_host_over_forecast_gib": round(statistics.mean(row["forecast_host_gib"] - row["measured_host_gib"] for row in host_judged), 3) if host_judged else None,
        "monotonic_violations": violations,
        "not_ok": [row for row in rows if row["status"] != "ok" or row["degraded"]],
    }


# ---------------------------------------------------------------- subcommands


def add_fit_parser(subparsers: argparse._SubParsersAction) -> argparse.ArgumentParser:
    """Register the `fit` subcommand."""
    parser = subparsers.add_parser("fit", help="fit calibration/<device_class>.json from report.json files", description="Fit the large-model manager's cost table of one device class from measurement reports (fit rules: fit.py header).")
    parser.add_argument("reports", type=Path, nargs="+", help="report.json files or directories searched for them")
    parser.add_argument("--base", type=Path, required=True, help="base calibration JSON (device constants and entries the runs did not reach)")
    parser.add_argument("--device-class", default=None, help="device class of the table (default: the reports' environment.device_class; all must agree)")
    parser.add_argument("--source", default=None, help="the table's `source` text (default: generated from the reports)")
    parser.add_argument("--out", type=Path, required=True, help="where to write the table (e.g. modules/ai_backend/runtime/large_models/calibration/<class>.json)")
    parser.add_argument("--evidence", type=Path, default=None, help="also write the fit evidence (per entry its points vs the fit) as JSON")
    return parser


def add_check_parser(subparsers: argparse._SubParsersAction) -> argparse.ArgumentParser:
    """Register the `check` subcommand."""
    parser = subparsers.add_parser("check", help="forecast vs measured acceptance over report.json files", description="Per combo the full forecast vs the measured device peak and host growth, the mean over-forecast, misses, monotonic violations.")
    parser.add_argument("reports", type=Path, nargs="+", help="report.json files or directories searched for them")
    parser.add_argument("--json", type=Path, default=None, help="also write the result as JSON")
    return parser


def _setup_logging() -> None:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")


def run_fit(parser: argparse.ArgumentParser, args: argparse.Namespace, *, repo_root: Path) -> int:
    """`fit`: returns 0 on success; usage errors exit 2 through `parser.error`."""
    from modules.ai_backend.runtime.large_models.cost_tables import parse_tables

    _setup_logging()
    try:
        reports = load_reports(args.reports)
        base = json.loads(args.base.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        parser.error(str(exc))
    classes = {report.get("environment", {}).get("device_class") for _path, report in reports}
    device_class = args.device_class or (classes.pop() if len(classes) == 1 else None)
    if device_class is None or (args.device_class is None and None in classes):
        parser.error(f"reports disagree on the device class or do not name it ({sorted(map(str, classes))}); pass --device-class")
    if args.device_class is not None and classes - {args.device_class}:
        parser.error(f"--device-class {args.device_class} but the reports were measured on {sorted(map(str, classes))}")
    names = sorted({report.get("environment", {}).get("device_name", "?") for _path, report in reports})
    torch_versions = sorted({str(report.get("environment", {}).get("torch")) for _path, report in reports})
    source = args.source or f"fitted by tools/large_model_calibration fit from {len(reports)} report(s) ({', '.join(path.parent.name for path, _ in reports)}), {', '.join(names)}, torch {', '.join(torch_versions)}"
    table, evidence = build_table(reports, base, device_class, source)
    parse_tables(table, source=str(args.out))  # the manager's own schema check before anything is written
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(table, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    measured = sum(1 for entry in table["phases"] if entry["measured"])
    log.info("wrote %s: %d entries, %d measured; device %s", args.out, len(table["phases"]), measured, json.dumps(evidence["device"]))
    for entry in table["phases"]:
        log.info("  %-12s %-10s %-9s %-6s %-9s %-6s measured=%-5s fixed %8.1f MiB  /tok %9.1f  /px %7.1f  rf %.2f  s %.3f + %.4f/kt + %.6f/kt2 + %.3f/MP", *(entry[name] for name in KEY_FIELDS), entry["measured"], entry["fixed_bytes"] / MIB, entry["bytes_per_token"], entry["bytes_per_pixel"], entry["reserved_factor"], entry["seconds_fixed"], entry["seconds_per_ktoken_pass"], entry["seconds_per_ktoken2_pass"], entry["seconds_per_mpixel"])
    if args.evidence is not None:
        args.evidence.write_text(json.dumps(evidence, indent=1, ensure_ascii=False), encoding="utf-8")
        log.info("evidence: %s", args.evidence)
    return 0


def run_check(parser: argparse.ArgumentParser, args: argparse.Namespace) -> int:
    """`check`: returns 0 when no judged cell under-forecasts and no grid breaks monotonicity, else 1."""
    _setup_logging()
    try:
        reports = load_reports(args.reports)
    except (OSError, ValueError) as exc:
        parser.error(str(exc))
    result = check_reports(reports)
    for row in result["combos"]:
        log.info("  %-90s %-6s %-11s fits=%-5s device %s / %s GiB  host %s / %s GiB (forecast / measured)", row["combo_id"][:90], row["status"], row["effective"], row["fits"], row["forecast_device_gib"], row["measured_device_gib"], row["forecast_host_gib"], row["measured_host_gib"])
    log.info("judged %d cells: %d device misses, %d host misses (+%d in first-of-process combos); device over-forecast mean %s GiB, max %s GiB; host over-forecast mean %s GiB; %d monotonic violations; %d not ok / degraded", result["judged"], len(result["device_misses"]), len(result["host_misses"]), len(result["first_in_process_host_misses"]), result["mean_device_over_forecast_gib"], result["max_device_over_forecast_gib"], result["mean_host_over_forecast_gib"], len(result["monotonic_violations"]), len(result["not_ok"]))
    for row in result["device_misses"]:
        log.warning("DEVICE MISS %s: forecast %s < measured %s GiB", row["combo_id"], row["forecast_device_gib"], row["measured_device_gib"])
    for row in result["host_misses"]:
        log.warning("HOST MISS %s: forecast %s < measured %s GiB", row["combo_id"], row["forecast_host_gib"], row["measured_host_gib"])
    for violation in result["monotonic_violations"]:
        log.warning("MONOTONIC %s", violation)
    if args.json is not None:
        args.json.write_text(json.dumps(result, indent=1, ensure_ascii=False), encoding="utf-8")
    return 1 if result["device_misses"] or result["host_misses"] or result["monotonic_violations"] else 0
