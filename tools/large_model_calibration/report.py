"""
File: tools/large_model_calibration/report.py

Purpose:
Output of a measurement run (either driver): `report.json` (environment,
settings including the `vram_cap` facts, every combo with its steps, the step
grids with the monotonic check), `report.csv` (one row per combo x step), the
`compare/` side-by-side PNGs of the FLUX.3 `text` / `vl_refs` pairs, and the
final summary log.

Key declarations:
- `Report` - accumulates outcomes, rewrites both files after every combo (a
  killed run keeps what it measured)
- `grids()` - the VRAM x RAM grid of every combo group: per cell the full
  forecast (before the run) beside the measured device peak and host growth,
  and the per-axis monotonic check (`MONOTONIC_TOLERANCE`)
- `measured_peaks()` - the measured device peak and host growth of one run
- `environment()` - torch / ROCm / CUDA / device / host facts
- `write_comparisons()`

Notes:
- A combo is a FLUX.3 `measure.Combo` or a FLUX.2 `flux2.Flux2Combo`; the
  report reads only `combo_id`, `grid_key`, `vram_step`, `ram_step` and
  `describe()` of it.
- Host growth = the max IN-STEP sampled RssAnon + RssShmem minus the value
  before the run (`recorder` notes).
- JSON keeps the original (Cyrillic) reference file names
  (`ensure_ascii=False`); output file names are ASCII (combo ids).
"""

from __future__ import annotations

import csv
import json
import logging
import platform
import sys
from pathlib import Path
from typing import Any

from modules.ai_backend.runtime.large_models.tiers import RAM_ORDER, VRAM_ORDER, RamStep, VramStep

from .measure import COMPARE_PAIR, ComboOutcome
from .recorder import GIB, record_dict

log = logging.getLogger(__name__)

#: A lower step may measure at most this much more than a higher one before the
#: monotonic check flags it (allocator rounding, phase-end sampling noise).
MONOTONIC_TOLERANCE = 256 * 1024**2

#: `report.csv` columns, in order.
CSV_COLUMNS = (
    "combo_id",
    "family",
    "variant",
    "size",
    "case",
    "encoder",
    "ae_dtype",
    "vram_step",
    "ram_step",
    "vram_effective",
    "ram_effective",
    "status",
    "model",
    "phase",
    "action",
    "failed",
    "overlapped",
    "peak_alloc_gib",
    "peak_reserved_gib",
    "alloc_begin_gib",
    "alloc_end_gib",
    "reserved_end_gib",
    "free_end_gib",
    "device_used_end_gib",
    "non_torch_end_gib",
    "footprint_peak_gib",
    "rss_peak_gib",
    "rss_end_gib",
    "forecast_device_peak_gib",
    "forecast_host_peak_gib",
    "wall_s",
    "tokens",
    "pixels",
    "passes",
)


def environment(device: str | None, service_device: bool) -> dict[str, Any]:
    """torch / HIP / CUDA versions, the device's name, arch, capability and memory, host RAM, Python, argv."""
    import psutil
    import torch

    from modules.ai_backend.runtime.large_models.cost_tables import detect_device_class

    env: dict[str, Any] = {
        "python": sys.version.split()[0],
        "platform": platform.platform(),
        "argv": sys.argv,
        "torch": torch.__version__,
        "hip": getattr(torch.version, "hip", None),
        "cuda": torch.version.cuda,
        "host_ram_gib": psutil.virtual_memory().total / GIB,
        "device_binding": "service" if service_device else "explicit",
        "device": device,
    }
    if device is not None and torch.device(device).type == "cuda":
        props = torch.cuda.get_device_properties(torch.device(device))
        env["device_name"] = props.name
        env["device_arch"] = str(getattr(props, "gcnArchName", ""))
        env["device_capability"] = [int(props.major), int(props.minor)]
        env["device_total_gib"] = props.total_memory / GIB
        env["device_class"] = detect_device_class(device)
    return env


def _gib(value: int | None) -> float | None:
    """`value` bytes in GiB rounded to 3 places, `None` stays `None`."""
    return None if value is None else round(value / GIB, 3)


def _gib_text(value: int | None) -> str:
    """CSV form of `_gib` (empty for `None`)."""
    return "" if value is None else f"{value / GIB:.3f}"


def measured_peaks(outcome: ComboOutcome) -> tuple[int | None, int | None]:
    """(device peak, host growth) bytes of one run: max per-step `footprint_peak` (peak reserved
    when no device baseline), max in-step RssAnon + RssShmem minus the value before the run."""
    ended = [record for record in outcome.phases if record.ended]
    footprints = [record.footprint_peak for record in ended if record.footprint_peak is not None]
    device = max(footprints) if footprints else max((record.peak_reserved for record in ended), default=None)
    rss = max((record.rss_peak for record in ended if record.rss_peak is not None), default=None)
    before = outcome.rss_private_before
    host = rss - before if rss is not None and before is not None else None
    return device, host


def _token_counts(outcome: ComboOutcome) -> dict[str, int | None]:
    """FLUX.3 `image_tokens`, `ref_tokens`, `text_tokens` and their sum `sequence_tokens` from the last
    recorded step; `None` where no step carried them (FLUX.2)."""
    info: dict[str, Any] = outcome.phases[-1].info if outcome.phases else {}
    counts: dict[str, int | None] = {key: info.get(key) for key in ("image_tokens", "ref_tokens", "text_tokens")}
    parts = list(counts.values())
    counts["sequence_tokens"] = sum(parts) if all(isinstance(part, int) for part in parts) else None
    return counts


def _combo_dict(outcome: ComboOutcome) -> dict[str, Any]:
    """JSON form of one `ComboOutcome`."""
    combo = outcome.combo
    peaks = [record for record in outcome.phases if record.ended]
    top = max(peaks, key=lambda record: record.peak_alloc, default=None)
    device, host = measured_peaks(outcome)
    planned = outcome.planned
    return {
        "combo_id": combo.combo_id,
        **combo.describe(),
        "vram_effective": outcome.vram_effective,
        "ram_effective": outcome.ram_effective,
        "degraded": outcome.degraded,
        "planned": planned,
        "compute_path": outcome.compute_path,
        "forecast_device_peak_gib": _gib(planned.get("full_device_peak")),
        "forecast_host_growth_gib": _gib(planned.get("host_growth")),
        "measured_device_peak_gib": _gib(device),
        "measured_host_growth_gib": _gib(host),
        "rss_private_before_gib": _gib(outcome.rss_private_before),
        **_token_counts(outcome),
        "status": outcome.status,
        "first_in_process": outcome.first_in_process,
        "failed_phase": outcome.failed_phase,
        "error": outcome.error,
        "traceback": outcome.traceback,
        "image": str(outcome.image_path) if outcome.image_path is not None else None,
        "image_mean": outcome.image_mean,
        "image_std": outcome.image_std,
        "width": outcome.width,
        "height": outcome.height,
        "seed": outcome.seed,
        "text_real_tokens": outcome.text_real_tokens,
        "fp8_applied": outcome.fp8_applied,
        "wall_s": round(outcome.wall_s, 3),
        "max_peak_alloc_gib": round(top.peak_alloc / GIB, 3) if top is not None else None,
        "max_peak_phase": top.step if top is not None else None,
        "max_peak_reserved_gib": round(max((r.peak_reserved for r in peaks), default=0) / GIB, 3),
        "phases": [record_dict(record) for record in outcome.phases],
    }


def _csv_rows(outcome: ComboOutcome) -> list[dict[str, Any]]:
    """`report.csv` rows of one combo (one per recorded step; one row without step data when none ran)."""
    combo = outcome.combo
    described = combo.describe()
    base = {
        "combo_id": combo.combo_id,
        **{key: described.get(key, "") for key in ("family", "variant", "size", "case", "encoder", "ae_dtype", "vram_step", "ram_step")},
        "vram_effective": outcome.vram_effective or "",
        "ram_effective": outcome.ram_effective or "",
        "status": outcome.status,
    }
    if not outcome.phases:
        return [base]
    rows = []
    for record in outcome.phases:
        info = record.info
        work = record.work or {}
        rows.append(
            {
                **base,
                "model": record.model,
                "phase": record.step,
                "action": info.get("action", ""),
                "failed": record.failed or not record.ended,
                "overlapped": record.overlapped,
                "peak_alloc_gib": f"{record.peak_alloc / GIB:.3f}",
                "peak_reserved_gib": f"{record.peak_reserved / GIB:.3f}",
                "alloc_begin_gib": f"{record.alloc_begin / GIB:.3f}",
                "alloc_end_gib": f"{record.alloc_end / GIB:.3f}",
                "reserved_end_gib": f"{record.reserved_end / GIB:.3f}",
                "free_end_gib": f"{record.free_end / GIB:.3f}",
                "device_used_end_gib": f"{record.device_used_end / GIB:.3f}",
                "non_torch_end_gib": _gib_text(record.non_torch_end),
                "footprint_peak_gib": _gib_text(record.footprint_peak),
                "rss_peak_gib": _gib_text(record.rss_peak),
                "rss_end_gib": _gib_text(record.rss_end),
                "forecast_device_peak_gib": _gib_text(info.get("forecast_device_peak")),
                "forecast_host_peak_gib": _gib_text(info.get("forecast_host_peak")),
                "wall_s": f"{record.wall_s:.3f}",
                "tokens": work.get("tokens", ""),
                "pixels": work.get("pixels", ""),
                "passes": work.get("passes", ""),
            }
        )
    return rows


def _cell(outcome: ComboOutcome) -> dict[str, Any]:
    """One grid cell: requested and effective steps, status, the full forecast and the measured peaks (bytes)."""
    planned = outcome.planned
    device, host = measured_peaks(outcome)
    refused = planned.get("fits") is False
    return {
        "vram": outcome.combo.vram_step.value,
        "ram": outcome.combo.ram_step.value,
        "status": "refused" if refused and outcome.status != "ok" else outcome.status,
        "vram_effective": outcome.vram_effective,
        "ram_effective": outcome.ram_effective,
        "degraded": outcome.degraded,
        "same_as_vram": planned.get("same_as_vram"),
        "same_as_ram": planned.get("same_as_ram"),
        "refusal": f"{planned.get('reason')} ({planned.get('shortest_resource')})" if refused else None,
        "forecast_device": planned.get("full_device_peak"),
        "forecast_host_growth": planned.get("host_growth"),
        "measured_device": device if outcome.status == "ok" else None,
        "measured_host_growth": host if outcome.status == "ok" else None,
    }


def _axis_violations(cells: dict[tuple[str, str], dict[str, Any]], axis: str) -> list[dict[str, Any]]:
    """Per-axis monotonic check of one grid: along `axis` (the other fixed), a lower step must not
    exceed a higher one - measured (tolerance `MONOTONIC_TOLERANCE`; cells that ran `ok` and did not
    degrade) and forecast (exact; cells the manager planned, not refused ones)."""
    order = [step.value for step in (VRAM_ORDER if axis == "vram" else RAM_ORDER)]
    others = [step.value for step in (RAM_ORDER if axis == "vram" else VRAM_ORDER)]
    field_name = "device" if axis == "vram" else "host_growth"
    found: list[dict[str, Any]] = []
    for other in others:
        line = [(step, cells.get((step, other) if axis == "vram" else (other, step))) for step in order]
        for high_index, (high_step, high) in enumerate(line):
            for low_step, low in line[high_index + 1 :]:
                if high is None or low is None:
                    continue
                for kind, tolerance in (("measured", MONOTONIC_TOLERANCE), ("forecast", 0)):
                    if kind == "measured" and (high["status"] != "ok" or low["status"] != "ok" or high["degraded"] or low["degraded"]):
                        continue
                    # A refused cell has no plan (its forecast is the closest attempt).
                    if kind == "forecast" and "refused" in (high["status"], low["status"]):
                        continue
                    high_value, low_value = high[f"{kind}_{field_name}"], low[f"{kind}_{field_name}"]
                    if high_value is None or low_value is None or low_value <= high_value + tolerance:
                        continue
                    found.append({"axis": axis, "kind": kind, "fixed": other, "higher": high_step, "lower": low_step, "higher_gib": _gib(high_value), "lower_gib": _gib(low_value), "excess_gib": _gib(low_value - high_value)})
    return found


def grids(outcomes: list[ComboOutcome]) -> list[dict[str, Any]]:
    """The VRAM x RAM grid of every combo group (`grid_key`) with concrete requested steps.

    Cells are keyed `"<vram>/<ram>"`; `violations` lists every per-axis
    monotonic breach (`_axis_violations`). A group with only «Авто» combos
    has no grid.
    """
    groups: dict[str, dict[tuple[str, str], dict[str, Any]]] = {}
    for outcome in outcomes:
        combo = outcome.combo
        if combo.vram_step is VramStep.AUTO or combo.ram_step is RamStep.AUTO:
            continue
        groups.setdefault(combo.grid_key, {})[(combo.vram_step.value, combo.ram_step.value)] = _cell(outcome)
    return [
        {
            "grid": key,
            "cells": {f"{vram}/{ram}": cell for (vram, ram), cell in cells.items()},
            "violations": _axis_violations(cells, "vram") + _axis_violations(cells, "ram"),
        }
        for key, cells in groups.items()
    ]


def cell_text(cell: dict[str, Any] | None) -> str:
    """Summary-table form of a cell: `dev fc/meas | host fc/meas` GiB, or the status."""
    if cell is None:
        return "-"

    def pair(forecast: int | None, measured: int | None) -> str:
        return f"{'?' if forecast is None else f'{forecast / GIB:.1f}'}/{'-' if measured is None else f'{measured / GIB:.1f}'}"

    text = f"{pair(cell['forecast_device'], cell['measured_device'])} | {pair(cell['forecast_host_growth'], cell['measured_host_growth'])}"
    if cell["status"] != "ok":
        text = f"{cell['status'].upper()} {text}"
    if cell["degraded"]:
        text += f" DEG->{cell['vram_effective']}/{cell['ram_effective']}"
    if cell["same_as_vram"] or cell["same_as_ram"]:
        text += " ="
    return text


def log_grid(grid: dict[str, Any]) -> None:
    """Log one grid as a VRAM x RAM table plus its monotonic violations."""
    cells = grid["cells"]
    log.info("grid %s (cell: device forecast/measured GiB | host growth forecast/measured GiB; '=' same plan as a neighbour):", grid["grid"])
    log.info("  %-6s %s", "", "".join(f"{step.value:<34}" for step in RAM_ORDER))
    for vram in VRAM_ORDER:
        log.info("  %-6s %s", vram.value, "".join(f"{cell_text(cells.get(f'{vram.value}/{ram.value}')):<34}" for ram in RAM_ORDER))
    if grid["violations"]:
        for violation in grid["violations"]:
            log.warning("  MONOTONIC VIOLATION %s", violation)
    else:
        log.info("  monotonic: no violation")


class Report:
    """The run's report files under `out_dir`; `add` rewrites both after every combo."""

    def __init__(self, out_dir: Path, env: dict[str, Any], settings: dict[str, Any]) -> None:
        self.out_dir = out_dir
        self.env = env
        self.settings = settings
        self.outcomes: list[ComboOutcome] = []
        self.skipped: list[dict[str, str]] = []
        self.comparisons: list[dict[str, str]] = []
        self.parity: dict[str, Any] | None = None

    def add(self, outcome: ComboOutcome) -> None:
        """Record `outcome` and rewrite `report.json` / `report.csv`."""
        self.outcomes.append(outcome)
        self.write()

    def write(self) -> None:
        """Write `report.json` and `report.csv` from everything recorded so far."""
        data = {
            "environment": self.env,
            "settings": self.settings,
            "skipped": self.skipped,
            "combos": [_combo_dict(outcome) for outcome in self.outcomes],
            "grids": grids(self.outcomes),
            "comparisons": self.comparisons,
            "parity": self.parity,
        }
        (self.out_dir / "report.json").write_text(json.dumps(data, indent=2, ensure_ascii=False), encoding="utf-8")
        with (self.out_dir / "report.csv").open("w", newline="", encoding="utf-8") as handle:
            writer = csv.DictWriter(handle, fieldnames=CSV_COLUMNS)
            writer.writeheader()
            for outcome in self.outcomes:
                writer.writerows(_csv_rows(outcome))

    def failed(self) -> list[ComboOutcome]:
        """Every combo whose status is not `ok`."""
        return [outcome for outcome in self.outcomes if outcome.status != "ok"]

    def log_summary(self) -> None:
        """One log line per combo (status, steps, measured vs forecast peaks, wall time), then the grids."""
        log.info("summary (%d combos, %d failed):", len(self.outcomes), len(self.failed()))
        for outcome in self.outcomes:
            entry = _combo_dict(outcome)
            log.info(
                "  %-70s %-5s %s/%s%s device %s GiB (forecast %s), host +%s GiB (forecast +%s), max peak alloc %s GiB (%s), %.1f s%s",
                entry["combo_id"],
                entry["status"],
                entry["vram_effective"],
                entry["ram_effective"],
                " DEGRADED" if entry["degraded"] else "",
                entry["measured_device_peak_gib"],
                entry["forecast_device_peak_gib"],
                entry["measured_host_growth_gib"],
                entry["forecast_host_growth_gib"],
                entry["max_peak_alloc_gib"],
                entry["max_peak_phase"],
                outcome.wall_s,
                f", failed in {outcome.failed_phase}: {outcome.error}" if outcome.status != "ok" else "",
            )
        for grid in grids(self.outcomes):
            log_grid(grid)


def write_comparisons(report: Report) -> None:
    """For every FLUX.3 combo pair differing only in `text` vs `vl_refs`, both `ok`,
    write `compare/<pair_key>__text_vs_vl_refs.png`: the two images
    concatenated horizontally (left `text`, right `vl_refs`; the shorter
    one padded black at the bottom)."""
    from PIL import Image

    left_label, right_label = COMPARE_PAIR
    by_key: dict[str, dict[str, ComboOutcome]] = {}
    for outcome in report.outcomes:
        if outcome.status == "ok" and outcome.combo.encoder.label in COMPARE_PAIR:
            by_key.setdefault(outcome.combo.pair_key, {})[outcome.combo.encoder.label] = outcome
    compare_dir = report.out_dir / "compare"
    for key, pair in by_key.items():
        if left_label not in pair or right_label not in pair:
            continue
        left_path, right_path = pair[left_label].image_path, pair[right_label].image_path
        if left_path is None or right_path is None:
            continue
        with Image.open(left_path) as left, Image.open(right_path) as right:
            canvas = Image.new("RGB", (left.width + right.width, max(left.height, right.height)))
            canvas.paste(left.convert("RGB"), (0, 0))
            canvas.paste(right.convert("RGB"), (left.width, 0))
        compare_dir.mkdir(parents=True, exist_ok=True)
        path = compare_dir / f"{key}__{left_label}_vs_{right_label}.png"
        canvas.save(path)
        report.comparisons.append({"pair": key, "left": str(left_path), "right": str(right_path), "image": str(path)})
        log.info("compare: %s", path)
    report.write()
