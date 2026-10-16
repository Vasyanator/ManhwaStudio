"""
File: tools/large_model_calibration/recorder.py

Purpose:
The model-independent measurement of one run: a step observer that records,
for every manager step a model service reports (a phase, a transition, a
component load), the device memory, the host memory and the wall time, plus
where the large-model manager held every component when the step began and
ended. Both drivers (`flux3.py`, `flux2.py`) install it as their service's
observer; `fit.py` reads what it recorded.

Key declarations:
- `StepRecord` - the measurements of one step (bytes, seconds)
- `StepRecorder` - the observer (`(step, "begin" | "end", info)`; a FLUX.3
  `Phase` enum or a FLUX.2 manager step string)
- `rss_private()` - RssAnon + RssShmem of this process
- `record_dict()` - JSON form of a `StepRecord`

Notes:
- `begin`: synchronize, `reset_peak_memory_stats`, allocated / reserved, RSS,
  the manager's placements (`LargeModelManager.status()` components per
  handle). `end`: synchronize, peaks, allocated / reserved, `mem_get_info`,
  RSS, placements, wall seconds. Device peaks are therefore PER STEP.
- Host memory INSIDE a step is sampled by a background thread every
  `RSS_SAMPLE_S` (`rss_peak`): a CPU encode's activation or a transient of a
  park would be invisible at the step's end.
- A step that begins while another is open (nested sessions) resets the
  device peak counters: the outer step's peak so far is folded into its
  record first, so its peaks stay true maxima over its window, but it is
  marked `overlapped` (the window includes the inner step's work) and is not
  used for a fit.
- Whole-process device footprint: with the device baseline of other
  processes known, `non_torch_end` = device used - baseline - torch reserved
  at the step end (context, kernel code objects), and `footprint_peak` = peak
  reserved + `non_torch_end`.
- torch / psutil are imported inside methods (a dry run imports neither).
"""

from __future__ import annotations

import logging
import threading
import time
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any

log = logging.getLogger(__name__)

#: Bytes per GiB, for the human-readable fields.
GIB = 1024**3

#: Host-memory sampling period inside a step (seconds).
RSS_SAMPLE_S = 0.02

#: `status()` of the large-model manager: one dict per handle.
StatusSource = Callable[[], list[dict[str, Any]]]


def rss_private() -> int | None:
    """This process's RssAnon + RssShmem (bytes) from `/proc/self/status`; `None` where unreadable (not Linux)."""
    fields: dict[str, int] = {}
    try:
        text = Path("/proc/self/status").read_text(encoding="ascii")
    except OSError as exc:
        log.debug("rss: /proc/self/status unreadable (%s)", exc)
        return None
    for line in text.splitlines():
        name, _, value = line.partition(":")
        if name in ("RssAnon", "RssShmem"):
            fields[name] = int(value.split()[0]) * 1024
    if "RssAnon" not in fields:
        return None
    return fields["RssAnon"] + fields.get("RssShmem", 0)


@dataclass
class StepRecord:
    """Measurements of one step (bytes; `None` where not measurable).

    `model` is the observer's `info["model"]` (FLUX.2: the handle's model) or
    `""`; `placements_begin` / `placements_end` map each handle key to its
    `{model, components: {name: ComponentPlacement.to_dict()}}`; `work` is set
    by the driver for a model phase (`fit.py` keys).
    """

    step: str
    model: str
    info: dict[str, Any]
    alloc_begin: int
    reserved_begin: int
    rss_begin: int | None
    placements_begin: dict[str, Any]
    peak_alloc: int = 0
    peak_reserved: int = 0
    alloc_end: int = 0
    reserved_end: int = 0
    free_end: int = 0
    total_device: int = 0
    device_used_end: int = 0
    non_torch_end: int | None = None
    footprint_peak: int | None = None
    rss_end: int | None = None
    rss_peak: int | None = None
    placements_end: dict[str, Any] = field(default_factory=dict)
    wall_s: float = 0.0
    failed: bool = False
    ended: bool = False
    overlapped: bool = False
    work: dict[str, Any] | None = None


def _placements(status: StatusSource | None) -> dict[str, Any]:
    """`{key: {model, components}}` of every manager handle (empty without a source)."""
    if status is None:
        return {}
    return {entry["key"]: {"model": entry["model"], "components": entry["components"]} for entry in status()}


class StepRecorder:
    """Step observer measuring on `device`; see the module notes.

    `baseline_used` (device-wide used bytes of other processes, `None` when not
    measurable) enables `non_torch_end` / `footprint_peak`; `status` is the
    manager's `status` (placements per step). A step `end` without a matching
    `begin` raises: a measurement that silently lost a step would be wrong.
    Call `close()` when the run is over (stops the sampler thread).
    """

    def __init__(self, device: str, *, baseline_used: int | None = None, status: StatusSource | None = None) -> None:
        self.device = device
        self.baseline_used = baseline_used
        self.status = status
        self.records: list[StepRecord] = []
        self._open: dict[tuple[str, str], tuple[StepRecord, float]] = {}
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._sampler = threading.Thread(target=self._sample, name="calibration-rss", daemon=True)
        self._sampler.start()

    def close(self) -> None:
        """Stop the host-memory sampler (idempotent)."""
        self._stop.set()
        self._sampler.join(timeout=5.0)

    def _sample(self) -> None:
        """Raise every open step's `rss_peak` to the current RssAnon + RssShmem until `close`."""
        while not self._stop.wait(RSS_SAMPLE_S):
            value = rss_private()
            if value is None:
                continue
            with self._lock:
                for record, _started in self._open.values():
                    record.rss_peak = value if record.rss_peak is None else max(record.rss_peak, value)

    def __call__(self, step: Enum | str, event: str, info: Mapping[str, Any]) -> None:
        import torch

        name = step.value if isinstance(step, Enum) else str(step)
        model = str(info.get("model", ""))
        key = (model, name)
        device = torch.device(self.device)
        if event == "begin":
            torch.cuda.synchronize(device)
            with self._lock:
                outers = [outer for outer, _started in self._open.values()]
            # The reset below would lose an open step's peak so far: keep it.
            for outer in outers:
                outer.overlapped = True
                outer.peak_alloc = max(outer.peak_alloc, torch.cuda.max_memory_allocated(device))
                outer.peak_reserved = max(outer.peak_reserved, torch.cuda.max_memory_reserved(device))
            torch.cuda.reset_peak_memory_stats(device)
            rss = rss_private()
            record = StepRecord(step=name, model=model, info=dict(info), alloc_begin=torch.cuda.memory_allocated(device), reserved_begin=torch.cuda.memory_reserved(device), rss_begin=rss, placements_begin=_placements(self.status), rss_peak=rss)
            with self._lock:
                self.records.append(record)
                self._open[key] = (record, time.monotonic())
            return
        with self._lock:
            opened = self._open.pop(key, None)
        if opened is None:
            raise RuntimeError(f"step {model}:{name}: 'end' without 'begin'")
        record, started = opened
        torch.cuda.synchronize(device)
        free, total = torch.cuda.mem_get_info(device)
        record.peak_alloc = max(record.peak_alloc, torch.cuda.max_memory_allocated(device))
        record.peak_reserved = max(record.peak_reserved, torch.cuda.max_memory_reserved(device))
        record.alloc_end = torch.cuda.memory_allocated(device)
        record.reserved_end = torch.cuda.memory_reserved(device)
        record.free_end = free
        record.total_device = total
        record.device_used_end = total - free
        if self.baseline_used is not None:
            record.non_torch_end = record.device_used_end - self.baseline_used - record.reserved_end
            record.footprint_peak = record.peak_reserved + record.non_torch_end
        record.rss_end = rss_private()
        if record.rss_end is not None:
            record.rss_peak = record.rss_end if record.rss_peak is None else max(record.rss_peak, record.rss_end)
        record.placements_end = _placements(self.status)
        record.wall_s = time.monotonic() - started
        record.failed = bool(info.get("failed", False))
        record.info = {**record.info, **dict(info)}
        record.ended = True
        log.info(
            "  %-28s %-9s peak alloc %6.2f GiB  peak reserved %6.2f GiB  footprint %s GiB  end alloc %6.2f GiB  rss peak %s GiB  %6.1f s%s%s",
            f"{model + ':' if model else ''}{name}",
            info.get("action", ""),
            record.peak_alloc / GIB,
            record.peak_reserved / GIB,
            "  n/a" if record.footprint_peak is None else f"{record.footprint_peak / GIB:5.2f}",
            record.alloc_end / GIB,
            "n/a" if record.rss_peak is None else f"{record.rss_peak / GIB:5.2f}",
            record.wall_s,
            "  FAILED" if record.failed else "",
            "  (overlapped)" if record.overlapped else "",
        )


def _gib(value: int | None) -> float | None:
    """`value` bytes in GiB rounded to 3 places, `None` stays `None`."""
    return None if value is None else round(value / GIB, 3)


def record_dict(record: StepRecord) -> dict[str, Any]:
    """JSON form of one `StepRecord` (bytes, plus GiB fields for reading)."""
    return {
        "phase": record.step,
        "model": record.model,
        "info": record.info,
        "failed": record.failed,
        "ended": record.ended,
        "overlapped": record.overlapped,
        "work": record.work,
        "peak_alloc": record.peak_alloc,
        "peak_reserved": record.peak_reserved,
        "alloc_begin": record.alloc_begin,
        "reserved_begin": record.reserved_begin,
        "alloc_end": record.alloc_end,
        "reserved_end": record.reserved_end,
        "free_end": record.free_end,
        "total_device": record.total_device,
        "device_used_end": record.device_used_end,
        "non_torch_end": record.non_torch_end,
        "footprint_peak": record.footprint_peak,
        "footprint_peak_gib": _gib(record.footprint_peak),
        "rss_begin": record.rss_begin,
        "rss_end": record.rss_end,
        "rss_peak": record.rss_peak,
        "placements_begin": record.placements_begin,
        "placements_end": record.placements_end,
        "peak_alloc_gib": round(record.peak_alloc / GIB, 3),
        "peak_reserved_gib": round(record.peak_reserved / GIB, 3),
        "wall_s": round(record.wall_s, 3),
    }
