"""
File: modules/ai_backend/runtime/large_models/degrade.py

Purpose:
The degrade ladder of a large-model run: which memory failures a run may
recover from, in which order a failed step is retried on a gentler plan, the
retry cap, and the learned floors (the most economical step per axis a model
needed on a device class, remembered so the next run starts there).

Main responsibilities:
- `classify_failure(exc, step=)` -> `RunFailure | None`: a device out-of-memory
  (`torch.OutOfMemoryError` or HIP's "out of memory" `RuntimeError`), a
  `moves.DeviceShort`, a `memory_budget.HostMemoryShort`; anything else is not
  a memory failure and is not retried;
- `Ladder`: the rungs after the manager's rung 0 (reclaim from other pools),
  each a RE-PLAN through the one planner. Device failures (OOM, `DeviceShort`,
  a sysmem spill): `TIGHTER_DEVICE` (same steps, the observed deficit taken out
  of the device room, so juggled components keep fewer blocks resident),
  `FALLBACK` (the spec's model-specific options, e.g. tiled decode),
  `NEXT_VRAM_STEP` (the next more economical VRAM step, same RAM step, deficit
  carried). Host failures: `NEXT_RAM_STEP` (the next more economical RAM step,
  same VRAM step). At most `MAX_RETRIES` retries per run;
- `FloorKey`, `FloorEntry`, `FloorsStore` (protocol), `LearnedFloors`.

Contracts:
- Rungs never hand-edit a plan: every rung's plan comes from the planner, so
  the forecast the UI showed and the guard enforced stay one arithmetic.
- The weight format is never changed by a rung (the planner never switches it).
- A rung walks only the axis of the resource that failed: a device failure
  never changes the RAM step, a host failure never the VRAM step.
- The user's requested steps are never rewritten: a degrade is visible as the
  effective steps and the ladder history; a learned floor is recorded per axis
  by the manager only after a run that SUCCEEDED lower on that axis, only ever
  lowers, and is reset explicitly.
- `LearnedFloors` is thread-safe (one leaf lock around the in-memory table; the
  store is written outside it).

Notes:
- Stdlib only at import; torch is consulted only if the caller already imported
  it (an OOM can only come from a loaded torch).
"""

from __future__ import annotations

import logging
import re
import sys
import threading
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field, replace
from enum import Enum
from typing import Any, Protocol

from ..memory_budget import MIB, HostMemoryShort
from .moves import DeviceShort
from .planner import Planned, PlanResult
from .spec import LargeModelSpec
from .tiers import RAM_ORDER, VRAM_ORDER, RamStep, ResidencyPlan, StepFloor, Steps, VramStep, more_economical_ram, more_economical_vram, ram_index, vram_index

log = logging.getLogger(__name__)

#: Retries of failed steps per run (rung 0 and degrade rungs together).
MAX_RETRIES = 3

#: Deficit assumed for an out-of-memory error whose text names no allocation size.
DEFAULT_OOM_DEFICIT_BYTES = 512 * MIB

_ALLOCATE_RE = re.compile(r"tried to allocate\s+([0-9]+(?:\.[0-9]+)?)\s*(b|bytes|kib|kb|mib|mb|gib|gb|tib|tb)\b", re.IGNORECASE)
_UNIT = {"b": 1, "bytes": 1, "kib": 1024, "kb": 1024, "mib": 1024**2, "mb": 1024**2, "gib": 1024**3, "gb": 1024**3, "tib": 1024**4, "tb": 1024**4}


class FailureKind(Enum):
    """A memory failure the ladder can answer."""

    DEVICE_OOM = "device_oom"
    DEVICE_SHORT = "device_short"
    HOST_SHORT = "host_short"
    SYSMEM_SPILL = "sysmem_spill"

    @property
    def device_side(self) -> bool:
        """Whether the failure is about device memory (the device rungs apply)."""
        return self is not FailureKind.HOST_SHORT


@dataclass(frozen=True)
class RunFailure:
    """One memory failure at a safe point.

    `step` names where it happened (`"load:<component>"`, `"<from>-><to>"`, or a
    phase name); `phase` is the model phase when the step is one (fallbacks
    match on it), else `None`; `deficit_bytes` the estimated missing bytes;
    `detail` the error text for logs.
    """

    kind: FailureKind
    step: str
    phase: str | None
    deficit_bytes: int
    detail: str


def _is_torch_oom(exc: BaseException) -> bool:
    """`torch.OutOfMemoryError` (or `torch.cuda.OutOfMemoryError`), or HIP's plain `RuntimeError` "out of memory"."""
    torch = sys.modules.get("torch")
    if torch is not None:
        for holder in (torch, getattr(torch, "cuda", None)):
            oom_type = getattr(holder, "OutOfMemoryError", None)
            if isinstance(oom_type, type) and isinstance(exc, oom_type):
                return True
    return isinstance(exc, RuntimeError) and "out of memory" in str(exc).lower()


def oom_deficit_bytes(message: str) -> int:
    """The allocation size an out-of-memory message names ("Tried to allocate 2.00 GiB"), else the default."""
    match = _ALLOCATE_RE.search(message)
    if match is None:
        return DEFAULT_OOM_DEFICIT_BYTES
    return max(1, int(float(match.group(1)) * _UNIT[match.group(2).lower()]))


def classify_failure(exc: BaseException, *, step: str, phase: str | None = None) -> RunFailure | None:
    """The `RunFailure` `exc` represents at `step`, or `None` when it is not a memory failure."""
    if isinstance(exc, DeviceShort):
        return RunFailure(FailureKind.DEVICE_SHORT, step, phase, max(1, exc.needed - exc.limit), str(exc))
    if isinstance(exc, HostMemoryShort):
        return RunFailure(FailureKind.HOST_SHORT, step, phase, max(1, exc.needed - exc.headroom), str(exc))
    if _is_torch_oom(exc):
        return RunFailure(FailureKind.DEVICE_OOM, step, phase, oom_deficit_bytes(str(exc)), str(exc))
    return None


class RungKind(Enum):
    """How a rung changed the plan."""

    RECLAIM = "reclaim"
    TIGHTER_DEVICE = "tighter_device"
    FALLBACK = "fallback"
    NEXT_VRAM_STEP = "next_vram_step"
    NEXT_RAM_STEP = "next_ram_step"


@dataclass(frozen=True)
class Replan:
    """What the ladder asks the manager to plan: concrete `steps`, with `options`, the device room reduced by `extra_device_bytes`."""

    steps: Steps
    options: Any
    extra_device_bytes: int


@dataclass(frozen=True)
class Rung:
    """One step the ladder took: the failure it answered, how, and (except RECLAIM) the new plan."""

    kind: RungKind
    failure: RunFailure
    planned: Planned | None
    options: Any
    detail: str = ""
    extra_device_bytes: int = 0


class Ladder:
    """The retry and degrade state of ONE run (not thread-safe; a session is driven by one thread).

    `start` is the admitted plan, `options` the request options, `replan` the
    manager's planning callback (fresh budgets, the planner, the user's
    requested steps kept). `max_retries` caps the retries of failed steps.
    """

    def __init__(self, spec: LargeModelSpec, *, start: Planned, options: Any, replan: Callable[[Replan], PlanResult], max_retries: int = MAX_RETRIES) -> None:
        if max_retries < 0:
            raise ValueError(f"Ladder: max_retries < 0 ({max_retries})")
        self._spec = spec
        self._replan = replan
        self._max_retries = max_retries
        self.current = start
        self.start_steps = start.plan.steps
        self.options = options
        self.retries = 0
        self.extra_device_bytes = 0
        self.applied_fallbacks: list[str] = []
        self.history: list[Rung] = []

    @property
    def plan(self) -> ResidencyPlan:
        """The plan in force."""
        return self.current.plan

    @property
    def degraded(self) -> bool:
        """Whether any rung other than a reclaim changed the run."""
        return any(rung.kind is not RungKind.RECLAIM for rung in self.history)

    @property
    def vram_lowered(self) -> bool:
        """Whether the plan in force is on a more economical VRAM step than the admitted one."""
        return vram_index(self.current.plan.steps.vram) > vram_index(self.start_steps.vram)

    @property
    def ram_lowered(self) -> bool:
        """Whether the plan in force is on a more economical RAM step than the admitted one."""
        return ram_index(self.current.plan.steps.ram) > ram_index(self.start_steps.ram)

    @property
    def reason(self) -> str | None:
        """The failure kind of the last non-reclaim rung (`None` when not degraded)."""
        for rung in reversed(self.history):
            if rung.kind is not RungKind.RECLAIM:
                return rung.failure.kind.value
        return None

    def take_retry(self, failure: RunFailure) -> bool:
        """Consume one retry for `failure`; `False` (nothing consumed) when the cap is reached."""
        if self.retries >= self._max_retries:
            log.warning("large models %s: retry cap %d reached at %s (%s)", self._spec.family, self._max_retries, failure.step, failure.kind.value)
            return False
        self.retries += 1
        return True

    def note_reclaim(self, failure: RunFailure, freed_hint: int) -> Rung:
        """Record rung 0 (other pools reclaimed `freed_hint` bytes for this run); the plan is unchanged."""
        rung = Rung(RungKind.RECLAIM, failure, None, self.options, f"reserved {freed_hint} B in run", self.extra_device_bytes)
        self.history.append(rung)
        return rung

    def degrade(self, failure: RunFailure) -> Rung | None:
        """The next gentler plan for `failure`, or `None` when no rung fits (the plan in force is unchanged then).

        Device failures try, in order: the same steps with the deficit taken out
        of the device room (only when the plan juggles blocks), each unused
        fallback of the failing phase, then every more economical VRAM step with
        the same RAM step, also planned with the deficit taken out (the device
        evidently had that much less room than the forecast assumed; the
        deficits of a run accumulate). Host failures try only the more
        economical RAM steps with the same VRAM step. A tighter or next-step
        plan is taken only if it lowers the forecast peak of the failing step
        (device side; the host peak for a host failure): a gentler step that
        needs as much there cannot help. A fallback is taken whenever it plans
        (its effect, e.g. tiling, need not show in the cost tables).
        """
        if failure.kind.device_side:
            extra = self.extra_device_bytes + max(1, failure.deficit_bytes)
            rung = self._tighter(failure, extra) or self._fallback(failure) or self._next_vram(failure, extra)
        else:
            rung = self._next_ram(failure)
        if rung is not None:
            return self._adopt(rung)
        log.warning("large models %s: no gentler plan for %s at %s after %d retr%s", self._spec.family, failure.kind.value, failure.step, self.retries, "y" if self.retries == 1 else "ies")
        return None

    # ------------------------------------------------------------------ rungs
    def _tighter(self, failure: RunFailure, extra: int) -> Rung | None:
        juggles = any(component.resident_blocks is not None for phase in self.plan.phases for component in phase.components.values())
        if not juggles:
            return None
        result = self._replan(Replan(self.plan.steps, self.options, extra))
        if not isinstance(result, Planned) or result.plan.steps != self.plan.steps:
            return None
        if not _lowers(result, self.current, failure):
            return None
        return Rung(RungKind.TIGHTER_DEVICE, failure, result, self.options, f"device room -{extra} B", extra)

    def _fallback(self, failure: RunFailure) -> Rung | None:
        if failure.phase is None:
            return None
        for fallback in self._spec.fallbacks:
            if fallback.name in self.applied_fallbacks or failure.phase not in fallback.phases:
                continue
            options = fallback.apply(self.options)
            # Marked as tried whatever the outcome: a fallback that cannot be
            # planned now will not become plannable by trying it again.
            self.applied_fallbacks.append(fallback.name)
            result = self._replan(Replan(self.plan.steps, options, self.extra_device_bytes))
            if isinstance(result, Planned):
                return Rung(RungKind.FALLBACK, failure, result, options, fallback.name, self.extra_device_bytes)
            log.info("large models %s: fallback %s does not fit (%s)", self._spec.family, fallback.name, getattr(result, "reason", "?"))
        return None

    def _next_vram(self, failure: RunFailure, extra: int) -> Rung | None:
        current = self.plan.steps
        for vram in VRAM_ORDER[vram_index(current.vram) + 1:]:
            result = self._replan(Replan(Steps(vram, current.ram), self.options, extra))
            if isinstance(result, Planned) and _lowers(result, self.current, failure):
                return Rung(RungKind.NEXT_VRAM_STEP, failure, result, self.options, vram.value, extra)
        return None

    def _next_ram(self, failure: RunFailure) -> Rung | None:
        current = self.plan.steps
        for ram in RAM_ORDER[ram_index(current.ram) + 1:]:
            result = self._replan(Replan(Steps(current.vram, ram), self.options, self.extra_device_bytes))
            if isinstance(result, Planned) and _lowers(result, self.current, failure):
                return Rung(RungKind.NEXT_RAM_STEP, failure, result, self.options, ram.value, self.extra_device_bytes)
        return None

    def _adopt(self, rung: Rung) -> Rung:
        assert rung.planned is not None  # every non-reclaim rung carries its plan
        log.warning(
            "large models %s: degrade after %s at %s (deficit %d B): %s -> %s (%s %s), device peak %d -> %d B",
            self._spec.family, rung.failure.kind.value, rung.failure.step, rung.failure.deficit_bytes,
            self.current.plan.steps.label, rung.planned.plan.steps.label, rung.kind.value, rung.detail,
            self.current.forecast.device_peak, rung.planned.forecast.device_peak,
        )
        self.current = rung.planned
        self.options = rung.options
        self.extra_device_bytes = rung.extra_device_bytes
        self.history.append(rung)
        return rung


def _step_peak(planned: Planned, failure: RunFailure) -> int:
    """The forecast peak a failure is about: the failing phase / transition (device), the whole run otherwise or for the host."""
    forecast = planned.forecast
    if not failure.kind.device_side:
        return forecast.host_peak
    entry = forecast.phases.get(failure.step) or forecast.transitions.get(failure.step)
    return entry.device_peak if entry is not None else forecast.device_peak


def _lowers(candidate: Planned, current: Planned, failure: RunFailure) -> bool:
    """Whether `candidate` needs less where `failure` happened than `current` does."""
    return _step_peak(candidate, failure) < _step_peak(current, failure)


# =====================================================================
#  Learned floors
# =====================================================================
@dataclass(frozen=True)
class FloorKey:
    """Where a floor applies: one model in one weight format on one device class, with one host pin.

    `host_pin` are the component names the request pinned to host execution
    (`LargeModelSpec.host_pin`, sorted; `()` = none): a pinned request places
    differently, so what it needed says nothing about an unpinned one.
    """

    device_class: str
    model: str
    format: str
    host_pin: tuple[str, ...] = ()


@dataclass(frozen=True)
class FloorEntry:
    """A learned floor per axis: `vram` / `ram` the most economical step a run needed (`None` = no floor on that axis); `reason` the failure kind, `step` where.

    # Raises
    `ValueError` (construction) for an `AUTO` step or an entry with no axis.
    """

    key: FloorKey
    vram: VramStep | None
    ram: RamStep | None
    reason: str
    step: str
    recorded_at: float = field(default=0.0, compare=False)

    def __post_init__(self) -> None:
        if self.vram is VramStep.AUTO or self.ram is RamStep.AUTO:
            raise ValueError("FloorEntry: шаг не может быть AUTO")
        if self.vram is None and self.ram is None:
            raise ValueError("FloorEntry: нужен пол хотя бы по одной оси")

    @property
    def floor(self) -> StepFloor:
        """The entry as the planner's `StepFloor`."""
        return StepFloor(self.vram, self.ram)


class FloorsStore(Protocol):
    """Persistence of the learned floors (the manager's `LimitsStore` implements it)."""

    def read_floors(self) -> list[FloorEntry]:
        """Every stored floor; an unreadable store answers `[]` (and logs)."""
        ...

    def write_floors(self, entries: Sequence[FloorEntry]) -> None:
        """Replace the stored floors; raises `OSError` when the store cannot be written."""
        ...


class LearnedFloors:
    """The learned floors over a `FloorsStore`, loaded once; every change is written through."""

    def __init__(self, store: FloorsStore) -> None:
        self._store = store
        self._lock = threading.Lock()
        self._entries: dict[FloorKey, FloorEntry] = {entry.key: entry for entry in store.read_floors()}

    def get(self, key: FloorKey) -> StepFloor | None:
        """The floor of `key`, `None` when none is learned."""
        with self._lock:
            entry = self._entries.get(key)
        return entry.floor if entry is not None else None

    def entries(self, model: str | None = None) -> list[FloorEntry]:
        """Every floor (of `model`, when given), sorted by key."""
        with self._lock:
            found = [entry for entry in self._entries.values() if model is None or entry.key.model == model]
        return sorted(found, key=lambda entry: (entry.key.model, entry.key.format, entry.key.device_class, entry.key.host_pin))

    def record(self, entry: FloorEntry) -> bool:
        """Lower the floor of `entry.key` on each axis `entry` names (never raise one); returns whether anything changed.

        Each axis is merged on its own: the stored step becomes the more
        economical of the stored and the new one; an axis `entry` leaves `None`
        keeps what was learned. A store write failure is logged and the floor
        is kept in memory only.
        """
        with self._lock:
            current = self._entries.get(entry.key)
            vram, ram = entry.vram, entry.ram
            if current is not None:
                vram = current.vram if vram is None else (vram if current.vram is None else more_economical_vram(current.vram, vram))
                ram = current.ram if ram is None else (ram if current.ram is None else more_economical_ram(current.ram, ram))
                if (vram, ram) == (current.vram, current.ram):
                    return False
            merged = FloorEntry(entry.key, vram, ram, entry.reason, entry.step, entry.recorded_at)
            self._entries[entry.key] = merged
            snapshot = list(self._entries.values())
        log.warning("large models: learned floor %s/%s on %s (host pin %s) -> vram %s, ram %s (%s at %s)", entry.key.model, entry.key.format, entry.key.device_class, ",".join(entry.key.host_pin) or "-", vram.value if vram else "-", ram.value if ram else "-", entry.reason, entry.step)
        self._write(snapshot)
        return True

    def reset(self, model: str | None = None) -> int:
        """Forget every floor (of `model`, when given); returns how many were cleared."""
        with self._lock:
            doomed = [key for key in self._entries if model is None or key.model == model]
            for key in doomed:
                del self._entries[key]
            snapshot = list(self._entries.values())
        if doomed:
            log.info("large models: %d learned floor(s) reset (model=%s)", len(doomed), model or "*")
            self._write(snapshot)
        return len(doomed)

    def _write(self, snapshot: list[FloorEntry]) -> None:
        try:
            self._store.write_floors(snapshot)
        except OSError as exc:
            log.warning("large models: learned floors could not be saved (%s); they hold until the backend restarts", exc)


def with_requested_steps(plan: ResidencyPlan, requested: Steps) -> ResidencyPlan:
    """`plan` with `steps_requested` set back to the user's request (a rung plans explicit steps)."""
    return plan if plan.steps_requested == requested else replace(plan, steps_requested=requested)
