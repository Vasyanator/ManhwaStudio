"""
File: modules/ai_backend/runtime/large_models/ledger.py

Purpose:
Byte bookkeeping of ONE large-model handle: for every residency unit of every
loaded component, the one tier its weights live in, plus the transient second
copy a move or a residency hook has in flight. It is the enforcement point of
the single-copy invariant ("no RAM copy of weights that are in VRAM") and the
source of the totals the handle reports up to the residency arbiter.

Main responsibilities:
- `UnitRef` (a block, or one non-block file tensor of a component) and
  `UnitState`;
- `Ledger`: `register`, `begin_copy` / `commit` / `release` (every tier change
  goes through them), staging accounting (`set_staging` / `clear_staging`),
  `holdings()` (device and host bytes now), `assert_single_copy()`;
- the debug cross-check against the real allocator and RSS
  (`MS_LARGE_MODELS_DEBUG=1`): `LedgerProbe`, `CrossCheckVerdict`,
  `Ledger.arm_cross_check` / `Ledger.cross_check`, `debug_enabled()`.

Contracts:
- A unit has exactly one `tier` at any time (`DEVICE`, `HOST`, `DISK`,
  `DROPPED`; `DISK` and `DROPPED` hold no bytes). A second, transient copy
  exists only between `begin_copy` and `commit` (the copy is adopted, the old
  one freed) or `release` (the copy is dropped, the tier unchanged); at most
  `max_in_flight` units carry one (`prefetch_depth + 1`).
- A transient copy that lives inside a pre-allocated staging slot (a
  residency hook's device slot) is `staged`: its bytes are already counted by
  `set_staging` and are not counted twice.
- `holdings()` = tier bytes + unstaged transient copies + staging. It is the
  narrow interface the large-model manager forwards to the residency arbiter;
  nothing here imports `runtime/residency`.
- Thread-safe: one leaf lock, held only for dictionary updates (no I/O, no
  callbacks, no torch under it).

Notes:
- Stdlib only; torch is never imported here (probes are injected).
- Byte figures are the inventory's DEVICE bytes after conversion; a HOST copy
  of a unit holds the same converted tensors, so the same figure applies.
"""

from __future__ import annotations

import logging
import os
import threading
from collections.abc import Callable
from dataclasses import dataclass, replace
from typing import Protocol

from .tiers import Tier

log = logging.getLogger(__name__)

#: Environment switch of the debug cross-check (any of "1", "true", "yes").
DEBUG_ENV = "MS_LARGE_MODELS_DEBUG"

#: Tiers that hold bytes, and in which memory.
_BYTES_ON = {Tier.DEVICE: "device", Tier.HOST: "host"}


def debug_enabled() -> bool:
    """Whether `MS_LARGE_MODELS_DEBUG` asks for the allocator/RSS cross-checks (read on every call)."""
    return os.environ.get(DEBUG_ENV, "").strip().lower() in ("1", "true", "yes")


class LedgerError(RuntimeError):
    """A tier change that would break the single-copy invariant, or an unknown/duplicate unit."""


@dataclass(frozen=True)
class UnitRef:
    """One residency unit: a block (`block` set) or one non-block file tensor (`tensor` set)."""

    component: str
    block: str | None = None
    tensor: str | None = None

    def __post_init__(self) -> None:
        if (self.block is None) == (self.tensor is None):
            raise ValueError(f"UnitRef «{self.component}»: нужен ровно один из block / tensor")

    @property
    def sort_key(self) -> tuple[str, str, str]:
        """Stable ordering key (component, block, tensor)."""
        return (self.component, self.block or "", self.tensor or "")

    @property
    def label(self) -> str:
        """`component:block` or `component:tensor`, for logs and messages."""
        return f"{self.component}:{self.block if self.block is not None else self.tensor}"


@dataclass(frozen=True)
class UnitState:
    """A unit's bookkeeping: its tier, its bytes and the transient copy in flight (if any)."""

    ref: UnitRef
    nbytes: int
    tier: Tier
    transient: Tier | None = None
    staged: bool = False


@dataclass(frozen=True)
class Holdings:
    """Bytes a handle holds right now: device (VRAM) and host (RAM, pinned staging included)."""

    device_bytes: int
    host_bytes: int


class LedgerProbe(Protocol):
    """Real memory figures for the debug cross-check (`None` = not measurable here)."""

    def device_allocated(self) -> int | None:
        """Bytes the device allocator has handed out (`torch.cuda.memory_allocated`)."""
        ...

    def host_private(self) -> int | None:
        """This process's `RssAnon + RssShmem` (pinned memory is in RssShmem on CUDA)."""
        ...


@dataclass(frozen=True)
class CrossCheckVerdict:
    """Ledger deltas vs measured deltas since `arm_cross_check` (bytes; `None` = not measured).

    `ok` is false when the device delta differs from the ledger's by more than
    the tolerance, or the host grew more than the ledger allows (a hidden RAM
    copy of device weights is exactly such growth).
    """

    ok: bool
    ledger_device_delta: int
    measured_device_delta: int | None
    ledger_host_delta: int
    measured_host_delta: int | None
    detail: str


@dataclass
class _CrossCheckBase:
    probe: LedgerProbe
    holdings: Holdings
    device: int | None
    host: int | None


class Ledger:
    """Per-handle byte ledger (see the module contract).

    `max_in_flight` bounds how many units may carry a transient copy at once
    (`prefetch_depth + 1` for a residency hook set; a block-wise move has one).
    """

    def __init__(self, *, max_in_flight: int = 2) -> None:
        if max_in_flight < 1:
            raise ValueError(f"Ledger: max_in_flight должно быть >= 1, получено {max_in_flight}")
        self._lock = threading.Lock()
        self._units: dict[UnitRef, UnitState] = {}
        self._staging: dict[str, Holdings] = {}
        self._max_in_flight = max_in_flight
        self._cross: _CrossCheckBase | None = None

    @property
    def max_in_flight(self) -> int:
        """The transient-copy bound this ledger enforces."""
        return self._max_in_flight

    # ------------------------------------------------------------------ units
    def register(self, ref: UnitRef, nbytes: int, tier: Tier) -> None:
        """Add a unit in `tier`; `LedgerError` when it is already registered or `nbytes < 0`."""
        if nbytes < 0:
            raise LedgerError(f"Ledger: отрицательный размер блока {ref.label}")
        with self._lock:
            if ref in self._units:
                raise LedgerError(f"Ledger: блок {ref.label} уже зарегистрирован")
            self._units[ref] = UnitState(ref, int(nbytes), tier)

    def forget_component(self, component: str) -> int:
        """Remove every unit of `component` (the module was discarded); returns how many.

        `LedgerError` when one of them still carries a transient copy.
        """
        with self._lock:
            refs = [ref for ref in self._units if ref.component == component]
            busy = [ref.label for ref in refs if self._units[ref].transient is not None]
            if busy:
                raise LedgerError(f"Ledger: компонент «{component}» нельзя забыть, блоки в пути: {busy[:5]}")
            for ref in refs:
                del self._units[ref]
            return len(refs)

    def state(self, ref: UnitRef) -> UnitState:
        """The unit's current state; `LedgerError` for an unknown unit."""
        with self._lock:
            return self._get(ref)

    def tier(self, ref: UnitRef) -> Tier:
        """The unit's current tier."""
        return self.state(ref).tier

    def units(self, component: str | None = None) -> list[UnitState]:
        """Every unit (of `component`, when given), sorted by reference."""
        with self._lock:
            return sorted((state for ref, state in self._units.items() if component is None or ref.component == component), key=lambda state: state.ref.sort_key)

    def begin_copy(self, ref: UnitRef, to: Tier, *, staged: bool = False) -> None:
        """Start a second copy of `ref` in `to` (`DEVICE` or `HOST`).

        `staged=True` marks a copy inside a staging slot already counted by
        `set_staging`. `LedgerError` when the unit already has a copy in flight,
        when `to` is its own tier or holds no bytes, or when the in-flight bound
        would be exceeded.
        """
        if to not in _BYTES_ON:
            raise LedgerError(f"Ledger: копия блока {ref.label} возможна только в DEVICE/HOST, получено {to}")
        with self._lock:
            state = self._get(ref)
            if state.transient is not None:
                raise LedgerError(f"Ledger: у блока {ref.label} уже есть копия в пути ({state.transient})")
            if state.tier is to:
                raise LedgerError(f"Ledger: блок {ref.label} уже в {to}; вторая копия там же запрещена")
            in_flight = sum(1 for unit in self._units.values() if unit.transient is not None)
            if in_flight >= self._max_in_flight:
                raise LedgerError(f"Ledger: в пути уже {in_flight} блок(ов) (предел {self._max_in_flight}), {ref.label} не принят")
            self._units[ref] = replace(state, transient=to, staged=staged)

    def commit(self, ref: UnitRef) -> None:
        """The transient copy becomes the unit's only copy (its tier); the old copy is gone."""
        with self._lock:
            state = self._get(ref)
            if state.transient is None:
                raise LedgerError(f"Ledger: у блока {ref.label} нет копии в пути")
            if state.staged:
                raise LedgerError(f"Ledger: копия {ref.label} в буфере подкачки не может стать постоянной")
            self._units[ref] = replace(state, tier=state.transient, transient=None, staged=False)

    def move_without_copy(self, ref: UnitRef, to: Tier) -> None:
        """Change the tier of a unit whose old copy is simply released (`to` holds no bytes: `DISK` / `DROPPED`)."""
        if to in _BYTES_ON:
            raise LedgerError(f"Ledger: переход {ref.label} в {to} требует копии (begin_copy/commit)")
        with self._lock:
            state = self._get(ref)
            if state.transient is not None:
                raise LedgerError(f"Ledger: у блока {ref.label} копия в пути ({state.transient})")
            self._units[ref] = replace(state, tier=to)

    def release(self, ref: UnitRef) -> None:
        """Drop the transient copy; the unit stays in its tier."""
        with self._lock:
            state = self._get(ref)
            if state.transient is None:
                raise LedgerError(f"Ledger: у блока {ref.label} нет копии в пути")
            self._units[ref] = replace(state, transient=None, staged=False)

    # ---------------------------------------------------------------- staging
    def set_staging(self, owner: str, *, device_bytes: int, host_bytes: int) -> None:
        """Account `owner`'s pre-allocated staging buffers (device slots, host staging)."""
        with self._lock:
            self._staging[owner] = Holdings(max(0, int(device_bytes)), max(0, int(host_bytes)))

    def clear_staging(self, owner: str) -> None:
        """Forget `owner`'s staging (its buffers were freed); unknown owners are ignored."""
        with self._lock:
            self._staging.pop(owner, None)

    # ---------------------------------------------------------------- figures
    def holdings(self, component: str | None = None) -> Holdings:
        """Device and host bytes held now (staging included only for the whole handle)."""
        with self._lock:
            return self._holdings_locked(component)

    def _holdings_locked(self, component: str | None) -> Holdings:
        totals = {"device": 0, "host": 0}
        for ref, state in self._units.items():
            if component is not None and ref.component != component:
                continue
            place = _BYTES_ON.get(state.tier)
            if place is not None:
                totals[place] += state.nbytes
            if state.transient is not None and not state.staged:
                totals[_BYTES_ON[state.transient]] += state.nbytes
        if component is None:
            for staging in self._staging.values():
                totals["device"] += staging.device_bytes
                totals["host"] += staging.host_bytes
        return Holdings(totals["device"], totals["host"])

    def assert_single_copy(self) -> None:
        """Raise `LedgerError` unless every unit has one tier and at most `max_in_flight` copies are in flight."""
        with self._lock:
            in_flight = [state for state in self._units.values() if state.transient is not None]
            if len(in_flight) > self._max_in_flight:
                raise LedgerError(f"Ledger: в пути {len(in_flight)} блок(ов), предел {self._max_in_flight}")
            for state in in_flight:
                if state.transient is state.tier:
                    raise LedgerError(f"Ledger: блок {state.ref.label} дважды в {state.tier}")

    # ------------------------------------------------------------ cross-check
    def arm_cross_check(self, probe: LedgerProbe) -> None:
        """Remember the probe's figures and the ledger's holdings as the baseline of later checks."""
        device = probe.device_allocated()
        host = probe.host_private()
        with self._lock:
            self._cross = _CrossCheckBase(probe, self._holdings_locked(None), device, host)

    def cross_check(self, *, device_tolerance: int, host_slack: int, context: str) -> CrossCheckVerdict | None:
        """Compare ledger deltas with measured deltas since `arm_cross_check`; `None` when not armed.

        Device: `|measured - ledger| <= device_tolerance`. Host: only GROWTH is
        checked, `measured <= max(ledger, 0) + host_slack`: freed host memory
        does not reliably leave the RSS (glibc keeps freed chunks below its
        mmap threshold in its arenas; measured on gfx1201: 1.5 of 2.9 GiB
        stayed resident after a restore), so a shrink the RSS does not show is
        not evidence of a hidden copy, while growth beyond the ledger is. A
        failing verdict is logged at WARNING with `context`; it never raises
        (the figures are diagnostics, the invariant itself is
        `assert_single_copy`).
        """
        with self._lock:
            base = self._cross
            if base is None:
                return None
            now = self._holdings_locked(None)
        device_now = base.probe.device_allocated()
        host_now = base.probe.host_private()
        ledger_device = now.device_bytes - base.holdings.device_bytes
        ledger_host = now.host_bytes - base.holdings.host_bytes
        measured_device = None if device_now is None or base.device is None else device_now - base.device
        measured_host = None if host_now is None or base.host is None else host_now - base.host
        problems: list[str] = []
        if measured_device is not None and abs(measured_device - ledger_device) > device_tolerance:
            problems.append(f"device delta {measured_device} B vs ledger {ledger_device} B")
        if measured_host is not None and measured_host > max(ledger_host, 0) + host_slack:
            problems.append(f"host delta {measured_host} B vs ledger {ledger_host} B (+{host_slack} B slack)")
        verdict = CrossCheckVerdict(not problems, ledger_device, measured_device, ledger_host, measured_host, "; ".join(problems))
        if problems:
            log.warning("large-model ledger cross-check failed after %s: %s", context, verdict.detail)
        else:
            log.debug("large-model ledger cross-check ok after %s: device %s/%s B, host %s/%s B", context, measured_device, ledger_device, measured_host, ledger_host)
        return verdict

    def _get(self, ref: UnitRef) -> UnitState:
        state = self._units.get(ref)
        if state is None:
            raise LedgerError(f"Ledger: неизвестный блок {ref.label}")
        return state


def host_private_probe(read_status: Callable[[], dict[str, int]] | None = None) -> Callable[[], int | None]:
    """A `host_private` reader over `memory_budget`'s `self_status()` (`RssAnon + RssShmem`, `None` if unknown)."""

    def read() -> int | None:
        if read_status is not None:
            status = read_status()
        else:
            from ..memory_budget import default_host_probes

            status = dict(default_host_probes().self_status())
        total = int(status.get("RssAnon", 0)) + int(status.get("RssShmem", 0))
        return total if total > 0 else None

    return read
