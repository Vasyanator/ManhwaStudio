"""
File: modules/ai_backend/runtime/residency/arbiter.py

Purpose:
`ResidencyArbiter` — the one owner of the backend's resident-model ledger: the
lease state machine of BOTH pools (small models, large models), outstanding
memory reservations, the reclaim choreography that evicts / parks other
holdings for a requester, and footprint measurement in clean windows. The two
pools are equal peers: each registers a `PoolPolicy` that proposes reclaims of
its own idle holdings; neither pool knows the other.

Main responsibilities:
- leases: `begin_use` / `finish_load` / `abort_load` / `release` /
  `mark_unloaded` (the lease protocol moved verbatim from the small manager,
  keyed by `(pool, key)`, including the unconditional, idempotent `finish_load`
  of STATE S-058);
- reservations: `reserve(requester, need)` -> `Reservation | Shortfall`,
  `update_reservation`, `release_reservation`; outstanding reservations are
  subtracted from the probed headroom;
- reclaims: offers from every policy, filtered by R1 and the owner rule,
  ordered A (park large) < B (drop small, LRU) < C (drop large), executed
  outside every lock; `trim_slots` for a lowered count cap;
- accounting: `view()` (holdings, per-device split foreign / owned-by-pool /
  own torch slack, reclaimable bytes) and `health()`.

Key structures:
- `ResidencyArbiter`, `ModelUsageLease`, `Reservation`
- `PoolPolicy` (protocol a pool implements), `BudgetProbes` (protocol) and its
  real implementation `MemoryBudgetProbes`
- `ModelMemoryShortError`

Notes:
- ONE lock (a `Condition`), a leaf: nothing is acquired under it (no probe,
  no callback, no policy `execute`, no logging); `Condition.wait` releases it.
  `PoolPolicy.offers` and `slot_capacity` are the only foreign code run under
  it and must be pure.
- Headroom is always re-probed through `..memory_budget` (other processes
  change it, risk K3); the ledger only decides who to reclaim.
- Contracts and the deadlock argument: `MODULE_README.md` in this directory.
"""

from __future__ import annotations

import logging
import threading
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, replace
from typing import Protocol

from ..memory_budget import (
    DEFAULT_HOST_RESERVE_BYTES,
    MIB,
    DeviceBudget,
    DeviceProbe,
    HostBudget,
    HostProbes,
    TorchDeviceProbe,
    default_device_reserve,
    read_device_budget,
    read_host_budget,
)
from ..torch_support import is_torch_available
from .footprint import Sample, estimate_footprint, measured_footprint, normalize_device, remeasured_footprint
from .holdings import (
    Action,
    DeviceAccount,
    Footprint,
    FootprintHint,
    Freed,
    HoldingsView,
    HoldingView,
    HostAccount,
    Need,
    Offer,
    Part,
    Pool,
    Requester,
    Runtime,
    Shortfall,
    Source,
    Tier,
    Urgency,
    usable_device_headroom,
)

log = logging.getLogger(__name__)

UnloadCallback = Callable[[], bool]

#: How long an ADMISSION waits in total for evictions other threads have in flight.
DEFAULT_ADMISSION_WAIT_S = 30.0
#: Reclaim rounds (each executes at least one successful offer and re-probes) per request.
MAX_RECLAIM_ROUNDS = 3
#: Re-probes when a reservation materialized or memory was freed between a probe and the lock.
_MAX_STALE_PROBE_RETRIES = 3

#: Today's count-cap refusal text, kept verbatim for the small pool's slot shortfall.
SLOT_SHORTFALL_MESSAGE = "Не удалось загрузить новую модель: достигнут лимит загруженных моделей, а свободных кандидатов для выгрузки нет."

_RESOURCE_NAMES_RU = {"device": "видеопамяти", "host": "оперативной памяти"}


class ModelMemoryShortError(RuntimeError):
    """A small-pool admission was refused: `shortfall` names the short resource, the deficit and the holders."""

    def __init__(self, shortfall: Shortfall) -> None:
        super().__init__(_shortfall_message(shortfall))
        self.shortfall = shortfall


def _shortfall_message(shortfall: Shortfall) -> str:
    """User-facing (Russian, the backend's style) text of a refused admission."""
    if shortfall.resource == "slots":
        return SLOT_SHORTFALL_MESSAGE
    resource = _RESOURCE_NAMES_RU.get(shortfall.resource, shortfall.resource)
    deficit_mib = max(1, (shortfall.deficit + MIB - 1) // MIB)
    holders = ", ".join(shortfall.blocked_by) if shortfall.blocked_by else "нет"
    return f"Не удалось загрузить модель: не хватает {resource} ({deficit_mib} МиБ), а выгрузить нечего; заняты моделями, которые сейчас используются: {holders}."


class BudgetProbes(Protocol):
    """Fresh budgets, read OUTSIDE the arbiter lock; must never raise (unknown = 0, see `..memory_budget`)."""

    def host(self) -> HostBudget: ...

    def device(self, device: str) -> DeviceBudget: ...


def _is_rocm_build() -> bool:
    """Whether the installed torch is a ROCm/HIP build; `False` without torch."""
    if not is_torch_available():
        return False
    import torch

    # A partial or stand-in torch module (tests, broken installs) may lack `version`:
    # that reads as "not ROCm", never as an error out of a lease.
    version = getattr(torch, "version", None)
    return bool(getattr(version, "hip", None))


class MemoryBudgetProbes:
    """The real probes: `memory_budget.read_host_budget` / `read_device_budget` with the process reserves.

    `device_reserve=None` picks the ROCm / CUDA default on the first device
    read. One `TorchDeviceProbe` is kept so its one-time warm-up matmul runs once
    per device.
    """

    def __init__(self, *, host_reserve: int = DEFAULT_HOST_RESERVE_BYTES, device_reserve: int | None = None, host_probes: HostProbes | None = None, device_probe: DeviceProbe | None = None) -> None:
        self._host_reserve = host_reserve
        self._device_reserve = device_reserve
        self._host_probes = host_probes
        self._device_probe: DeviceProbe = device_probe if device_probe is not None else TorchDeviceProbe()

    def host(self) -> HostBudget:
        """The host budget now."""
        return read_host_budget(reserve=self._host_reserve, probes=self._host_probes)

    def device(self, device: str) -> DeviceBudget:
        """`device`'s budget now; all zeros (= unreadable) when it cannot be read, never an exception.

        `read_device_budget` promises not to raise, but its torch probe only
        catches torch's own error types; a partial or stand-in torch (no
        `torch.cuda`, no `torch.version`) raises `AttributeError` / `ImportError`
        there. A lease must not fail on a probe, so those read as "unreadable".
        """
        try:
            if self._device_reserve is None:
                self._device_reserve = default_device_reserve(is_rocm=_is_rocm_build())
            return read_device_budget(device, reserve=self._device_reserve, probe=self._device_probe)
        except (AttributeError, ImportError, TypeError) as exc:
            log.warning("residency: device budget of %s unreadable (%s: %s); treated as unknown", device, type(exc).__name__, exc)
            return DeviceBudget(device=str(device), total=0, free=0, torch_allocated=0, torch_reserved=0, non_torch=0, reserve=self._device_reserve or 0, cap=None)


class PoolPolicy(Protocol):
    """What a pool contributes to the arbiter. Registered once per pool with `register_pool`.

    - `pool`: which pool this is.
    - `slot_capacity()`: the pool's count cap (`None` = no count cap). Called
      UNDER the arbiter lock: must be a plain attribute read.
    - `offers(view, need, requester)`: reclaims of this pool's own idle holdings
      for `requester` (`None` for a cap trim). Called UNDER the arbiter lock: pure
      over `view` and the policy's immutable facts — no I/O, no probe, no other
      lock, no callback. The arbiter filters R1 and the owner rule again.
    - `execute(offer)`: performs one offer with NO arbiter or manager lock held;
      may take the victim service's lock; never calls `reserve`. For a part
      offer it reports the move itself (`update_parts` / `mark_unloaded`); for a
      whole-holding `DROP` with `ok=True` the arbiter marks the holding not
      resident.
    """

    @property
    def pool(self) -> Pool: ...

    def slot_capacity(self) -> int | None: ...

    def offers(self, view: HoldingsView, need: Need, requester: Requester | None) -> Sequence[Offer]: ...

    def execute(self, offer: Offer) -> Freed: ...


class Reservation:
    """Bytes promised to a requester and not yet visible to the probes.

    Created by `reserve`; materialized (removed) by `finish_load` for an
    admission, or released by `release_reservation`. `need` changes only through
    `update_reservation`.
    """

    __slots__ = ("id", "requester", "need")

    def __init__(self, reservation_id: int, requester: Requester, need: Need) -> None:
        self.id = reservation_id
        self.requester = requester
        self.need = need


@dataclass
class _Window:
    """A measurement window: the activity epoch it started at and the sample taken then (`None` until taken)."""

    epoch: int
    device: str | None
    runtime: Runtime
    before: Sample | None = None


@dataclass
class _Holding:
    """Arbiter-side state of one `(pool, key)`; mutated only under the arbiter lock."""

    pool: Pool
    key: str
    owner: str
    footprint: Footprint
    first_footprint: Footprint | None = None
    parts: tuple[Part, ...] = ()
    resident: bool = False
    loading: bool = False
    evicting: bool = False
    in_use_count: int = 0
    last_used_at: float = 0.0
    reclaim: UnloadCallback | None = None
    hint: FootprintHint | None = None
    admission: Reservation | None = None
    load_window: _Window | None = None

    def snapshot(self) -> HoldingView:
        """The immutable view of this holding."""
        return HoldingView(
            pool=self.pool,
            key=self.key,
            owner=self.owner,
            footprint=self.footprint,
            parts=self.parts,
            resident=self.resident,
            loading=self.loading,
            evicting=self.evicting,
            in_use_count=self.in_use_count,
            last_used_at=self.last_used_at,
            reclaim=self.reclaim,
        )


def _single_part(footprint: Footprint) -> tuple[Part, ...]:
    """The one part a whole-model holding consists of."""
    tier = Tier.DEVICE if footprint.device is not None and footprint.device_bytes > 0 else Tier.HOST
    return (Part(name="model", device_bytes=footprint.device_bytes, host_bytes=footprint.host_bytes, tier=tier),)


@dataclass(frozen=True)
class _Deficit:
    """Bytes / slots still missing for a need (<= 0 = covered)."""

    device: int
    host: int
    slots: int
    unchecked: tuple[str, ...] = ()

    @property
    def covered(self) -> bool:
        return self.device <= 0 and self.host <= 0 and self.slots <= 0

    def first_short(self) -> tuple[str, int]:
        """The first short resource and its deficit."""
        if self.device > 0:
            return "device", self.device
        if self.host > 0:
            return "host", self.host
        return "slots", self.slots


class ModelUsageLease:
    """One request's hold on a `(pool, key)`, returned by `ResidencyArbiter.begin_use`.

    `needs_load` is True when this lease reserved the load (the holding is
    flagged `loading` until `mark_loaded` / `mark_load_failed`). `mark_loaded`
    always registers the model resident (lease protocol, `../MODULE_README.md`);
    `mark_load_failed` acts only on a lease that reserved the load and has not
    reported it yet. Both report calls and `release` are idempotent.
    """

    def __init__(self, arbiter: "ResidencyArbiter", pool: Pool, model_key: str, needs_load: bool) -> None:
        self._arbiter = arbiter
        self._pool = pool
        self._model_key = model_key
        self.needs_load = needs_load
        self._load_finished = not needs_load
        self._released = False
        self._window: _Window | None = None

    def mark_loaded(self, unload_callback: UnloadCallback | None = None) -> None:
        """Register the key resident (with `unload_callback`); call it whenever the service holds the model.

        Unconditional by contract, also for a lease with `needs_load=False`: a
        model the service had to reload under such a lease (another request
        switched it out in between) is otherwise held outside the ledger and
        never evictable. Must be called under the service lock that guards the
        model, so no other thread can drop it between the load and this call.
        After it, `mark_load_failed` is a no-op.
        """
        window = self._arbiter.finish_load(self._pool, self._model_key, unload_callback)
        if window is not None:
            self._window = window
        self._load_finished = True

    def mark_load_failed(self) -> None:
        """Abort the load this lease reserved; a no-op after `mark_loaded` or for a `needs_load=False` lease."""
        if self._load_finished:
            return
        self._arbiter.abort_load(self._pool, self._model_key)
        self._load_finished = True

    def release(self) -> None:
        """Drop this lease's in-use count once; the holding may then be evicted or cleaned up."""
        if self._released:
            return
        self._arbiter.release(self._pool, self._model_key, window=self._window)
        self._released = True


class ResidencyArbiter:
    """The shared ledger and reclaim arbiter of both model pools (contracts: this directory's `MODULE_README.md`).

    `probes` reads the budgets (default `MemoryBudgetProbes()`); `clock` stamps
    LRU times and bounds the admission wait; `admission_wait_s` is the total
    time an ADMISSION may wait for evictions in flight elsewhere.
    """

    def __init__(self, probes: BudgetProbes | None = None, *, clock: Callable[[], float] = time.monotonic, admission_wait_s: float = DEFAULT_ADMISSION_WAIT_S) -> None:
        self._probes: BudgetProbes = probes if probes is not None else MemoryBudgetProbes()
        self._clock = clock
        self._admission_wait_s = max(0.0, float(admission_wait_s))
        self._cond = threading.Condition()
        self._holdings: dict[tuple[Pool, str], _Holding] = {}
        self._policies: dict[Pool, PoolPolicy] = {}
        self._reservations: dict[int, Reservation] = {}
        self._next_reservation_id = 1
        self._learned: dict[tuple[Pool, str], Footprint] = {}
        self._evictions_in_flight = 0
        # Budgets ("host" or a device name) already warned about as unreadable for the small pool.
        self._unchecked_warned: set[str] = set()
        # Bumped by every ledger mutation: a measurement window is clean only if it did not move.
        self._epoch = 0
        # Bumped when reserved or held bytes leave the ledger: a probe taken before must be retaken.
        self._budget_epoch = 0

    # ------------------------------------------------------------------ pools
    def register_pool(self, policy: PoolPolicy) -> None:
        """Register `policy` for its pool. Raises `ValueError` when that pool already has one."""
        with self._cond:
            if policy.pool in self._policies:
                raise ValueError(f"pool {policy.pool.value} already registered")
            self._policies[policy.pool] = policy

    # ----------------------------------------------------------------- leases
    def begin_use(self, pool: Pool, key: str, *, owner: str | None = None, hint: FootprintHint | None = None, on_reclaim: UnloadCallback | None = None) -> ModelUsageLease:
        """Take a lease on `(pool, key)`, waiting while it is being loaded or evicted elsewhere.

        Returns `needs_load=True` when the caller must load the model (the
        holding is then flagged `loading` and an ADMISSION reservation is made:
        the pool's slot if it has a count cap, plus the estimated bytes when a
        `hint` was given — `hint=None` reserves no bytes). `owner` defaults to
        the key itself (no owner rule beyond the key); the first lease's owner
        sticks while the holding exists.
        Raises `ValueError` for an empty key and `ModelMemoryShortError` when the
        admission cannot be satisfied. Must be called BEFORE the service lock:
        the admission may wait on an eviction callback that takes that lock.
        """
        normalized_key = str(key).strip()
        if not normalized_key:
            raise ValueError("model_key must be non-empty")
        owner_name = str(owner).strip() if owner is not None else ""
        if not owner_name:
            owner_name = normalized_key
        slot = (pool, normalized_key)

        while True:
            with self._cond:
                holding = self._holdings.get(slot)
                if holding is None:
                    holding = _Holding(pool=pool, key=normalized_key, owner=owner_name, footprint=estimate_footprint(hint, self._learned.get(slot)))
                    self._holdings[slot] = holding
                if on_reclaim is not None:
                    holding.reclaim = on_reclaim
                if hint is not None:
                    holding.hint = hint
                if holding.evicting:
                    self._cond.wait()
                    continue
                if holding.resident:
                    holding.in_use_count += 1
                    holding.last_used_at = self._clock()
                    self._bump_locked()
                    lease = ModelUsageLease(self, pool, normalized_key, needs_load=False)
                    lease._window = self._open_window_locked(holding, own_leases=1)
                    break
                if holding.loading:
                    self._cond.wait()
                    continue
                holding.loading = True
                holding.in_use_count += 1
                holding.last_used_at = self._clock()
                holding.footprint = estimate_footprint(holding.hint, self._learned.get(slot))
                self._bump_locked()
                policy = self._policies.get(pool)
                capacity = policy.slot_capacity() if policy is not None else None
                need = self._admission_need(holding, slots=1 if capacity is not None else 0)
                requester = Requester(pool=pool, key=normalized_key, owner=holding.owner, urgency=Urgency.ADMISSION, holds_lease=True)
                lease = None
                break

        if lease is not None:
            self._take_window_sample(lease._window)
            return lease

        try:
            result = self.reserve(requester, need)
        except BaseException:
            self.abort_load(pool, normalized_key)
            self.release(pool, normalized_key)
            raise
        if isinstance(result, Shortfall):
            self.abort_load(pool, normalized_key)
            self.release(pool, normalized_key)
            log.warning("residency: admission of %s/%s refused: %s short by %d (blocked by %s)", pool.value, normalized_key, result.resource, result.deficit, ", ".join(result.blocked_by) or "-")
            raise ModelMemoryShortError(result)

        with self._cond:
            holding = self._holdings.get(slot)
            window = None
            if holding is not None and holding.loading:
                holding.admission = result
                window = self._open_window_locked(holding, own_leases=1)
                holding.load_window = window
            else:
                self._drop_reservation_locked(result)
        self._take_window_sample(window)
        return ModelUsageLease(self, pool, normalized_key, needs_load=True)

    def finish_load(self, pool: Pool, key: str, on_reclaim: UnloadCallback | None = None, *, measured: Footprint | None = None) -> _Window | None:
        """Mark `(pool, key)` resident and not loading; idempotent (re-registering a resident key only refreshes it).

        Records the footprint: `measured` when given, else the clean-window
        measurement of this load, else the estimate. Materializes the admission
        reservation. Returns the release-measurement window it opened for the
        loading lease (`None` when the activity was not clean); never raises.
        """
        slot = (pool, key)
        with self._cond:
            holding = self._holdings.get(slot)
            window = holding.load_window if holding is not None else None
            estimate = holding.footprint if holding is not None else None
        candidate = None
        after = None
        if window is not None and window.before is not None and estimate is not None:
            after = self._sample(window)
            # Computed outside the lock: the rule logs a rejected ONNX delta.
            candidate = measured_footprint(estimate, window.before, after)

        with self._cond:
            holding = self._holdings.get(slot)
            if holding is None:
                holding = _Holding(pool=pool, key=key, owner=key, footprint=estimate_footprint(None, self._learned.get(slot)))
                self._holdings[slot] = holding
            clean = candidate is not None and holding.load_window is window and holding.footprint is estimate and self._epoch == window.epoch
            changed = not holding.resident or holding.loading or holding.evicting or measured is not None or clean
            if measured is not None:
                holding.footprint = replace(measured, source=Source.MEASURED)
                self._learned[slot] = holding.footprint
            elif clean and candidate is not None:
                holding.footprint = candidate
                self._learned[slot] = candidate
            if holding.first_footprint is None or not holding.resident:
                holding.first_footprint = holding.footprint
            if pool is Pool.SMALL or not holding.parts:
                holding.parts = _single_part(holding.footprint)
            holding.loading = False
            holding.resident = True
            holding.evicting = False
            holding.last_used_at = self._clock()
            holding.load_window = None
            if on_reclaim is not None:
                holding.reclaim = on_reclaim
            if holding.admission is not None:
                self._drop_reservation_locked(holding.admission)
                holding.admission = None
            # A refresh of a model that was resident all along (the unconditional
            # `mark_loaded` of a resident hit) is no memory event: it must not
            # spoil that lease's release-measurement window.
            if changed:
                self._bump_locked()
            release_window = None
            if clean and window is not None:
                release_window = _Window(epoch=self._epoch, device=window.device, runtime=window.runtime, before=after)
            self._cond.notify_all()
        return release_window

    def abort_load(self, pool: Pool, key: str) -> None:
        """Abort a reserved load: clear `loading` / `evicting`, release the admission reservation, clean up if unused."""
        with self._cond:
            holding = self._holdings.get((pool, key))
            if holding is None:
                return
            holding.loading = False
            holding.evicting = False
            holding.load_window = None
            if holding.admission is not None:
                self._drop_reservation_locked(holding.admission)
                holding.admission = None
            self._bump_locked()
            self._cleanup_locked(holding)
            self._cond.notify_all()

    def release(self, pool: Pool, key: str, *, window: _Window | None = None) -> None:
        """Drop one in-use count of `(pool, key)`; with a clean lease `window`, re-measure (only raises)."""
        after = self._sample(window) if window is not None and window.before is not None else None
        with self._cond:
            holding = self._holdings.get((pool, key))
            if holding is None:
                return
            if (
                window is not None
                and window.before is not None
                and after is not None
                and self._epoch == window.epoch
                and holding.resident
                and holding.first_footprint is not None
            ):
                raised = remeasured_footprint(holding.footprint, holding.first_footprint, window.before, after)
                if raised is not holding.footprint:
                    holding.footprint = raised
                    if pool is Pool.SMALL:
                        holding.parts = _single_part(raised)
                    self._learned[(pool, key)] = raised
            if holding.in_use_count > 0:
                holding.in_use_count -= 1
            holding.last_used_at = self._clock()
            self._bump_locked()
            self._cleanup_locked(holding)
            self._cond.notify_all()

    def mark_unloaded(self, pool: Pool, key: str) -> None:
        """The service dropped `(pool, key)`: not resident; the holding survives while a lease uses it."""
        with self._cond:
            holding = self._holdings.get((pool, key))
            if holding is None:
                return
            holding.resident = False
            holding.loading = False
            holding.evicting = False
            holding.load_window = None
            if holding.admission is not None:
                self._drop_reservation_locked(holding.admission)
                holding.admission = None
            holding.last_used_at = self._clock()
            self._budget_epoch += 1
            self._bump_locked()
            self._cleanup_locked(holding)
            self._cond.notify_all()

    def update_parts(self, pool: Pool, key: str, parts: Sequence[Part], *, device: str | None = None) -> None:
        """A load / park / restore / partial drop moved a holding's bytes: record `parts` and their sums.

        `device` (when given) is the device the holding's device-tier parts are
        on; a large pool passes it with the first parts of a holding whose lease
        carried no hint.
        """
        normalized = normalize_device(device)
        with self._cond:
            holding = self._holdings.get((pool, key))
            if holding is None:
                return
            holding.parts = tuple(parts)
            holding.footprint = replace(
                holding.footprint,
                device=normalized if normalized is not None else holding.footprint.device,
                device_bytes=sum(part.device_bytes for part in holding.parts),
                host_bytes=sum(part.host_bytes for part in holding.parts),
            )
            self._budget_epoch += 1
            self._bump_locked()
            self._cond.notify_all()

    # ----------------------------------------------------------- reservations
    def reserve(self, requester: Requester, need: Need) -> Reservation | Shortfall:
        """Promise `need` to `requester`, reclaiming other pools' idle holdings when short.

        Choreography: probe outside the lock; under it, headroom = probe minus
        outstanding reservations; if short, collect every policy's offers,
        filter R1 and the owner rule, sort by R4, take the shortest prefix that
        covers the deficit and mark it evicting; execute outside the lock; re-probe
        and repeat (at most `MAX_RECLAIM_ROUNDS` rounds that freed something). An
        ADMISSION that cannot be covered waits (in total at most
        `admission_wait_s`) while other threads have evictions in flight; an
        IN_RUN request never waits and returns the `Shortfall` at once.

        A need with `strict=False` (an unmeasured estimate) reclaims the same way
        but is admitted even when its bytes still do not fit (only a slot
        shortfall refuses it). A SMALL-pool requester is not byte-checked on a
        side whose budget cannot be read (unreadable device, unknown host).
        """
        need = replace(need, device=normalize_device(need.device))
        deadline = self._clock() + self._admission_wait_s
        skipped: set[tuple[Pool, str, str | None]] = set()
        rounds = 0
        stale_retries = 0
        while True:
            with self._cond:
                budget_epoch = self._budget_epoch
            host, devices = self._probe_for(need)
            outcome: Reservation | Shortfall | None = None
            admitted_short: _Deficit | None = None
            with self._cond:
                if self._budget_epoch != budget_epoch and stale_retries < _MAX_STALE_PROBE_RETRIES:
                    stale_retries += 1
                    continue
                view = self._view_locked(host, devices)
                deficit = self._deficit_locked(view, requester, need)
                unchecked = [name for name in deficit.unchecked if name not in self._unchecked_warned]
                self._unchecked_warned.update(unchecked)
                chosen = None
                if deficit.covered:
                    outcome = self._add_reservation_locked(requester, need)
                else:
                    if rounds < MAX_RECLAIM_ROUNDS:
                        offers = self._collect_offers_locked(view, need, requester, skipped)
                        chosen = _choose_prefix(offers, deficit, view, need, requester, allow_partial=not need.strict)
                    if chosen is None:
                        remaining = deadline - self._clock()
                        if requester.urgency is Urgency.ADMISSION and self._evictions_in_flight > 0 and remaining > 0:
                            self._cond.wait(remaining)
                            continue
                        if not need.strict and deficit.slots <= 0:
                            outcome = self._add_reservation_locked(requester, need)
                            admitted_short = deficit
                        else:
                            resource, amount = deficit.first_short()
                            outcome = Shortfall(resource=resource, deficit=amount, blocked_by=self._blocked_by_locked(view, requester))
                    else:
                        victims = self._mark_evicting_locked(chosen)
            for name in unchecked:
                log.warning("residency: the %s budget cannot be read; small-pool loads there are admitted without a byte check (slot only); first seen for %s/%s", name, requester.pool.value, requester.key)
            if admitted_short is not None:
                resource, amount = admitted_short.first_short()
                log.info("residency: %s/%s admitted although its estimated size does not fit (%s short by %d B): the size is an unmeasured estimate", requester.pool.value, requester.key, resource, amount)
            if outcome is not None:
                return outcome
            if chosen is None:
                # Every branch above either set `outcome`, continued, or chose offers.
                raise RuntimeError("residency: reserve reached the reclaim step without offers")
            if self._execute(chosen, victims, skipped):
                rounds += 1
            stale_retries = 0

    def update_reservation(self, reservation: Reservation, need: Need) -> None:
        """Replace an outstanding reservation's `need` (a large run's phase transition); no reclaim, no check."""
        with self._cond:
            if reservation.id in self._reservations:
                reservation.need = replace(need, device=normalize_device(need.device))
                self._budget_epoch += 1
                self._bump_locked()
                self._cond.notify_all()

    def release_reservation(self, reservation: Reservation) -> None:
        """Release an outstanding reservation; a no-op when it already materialized or was released."""
        with self._cond:
            if reservation.id in self._reservations:
                self._drop_reservation_locked(reservation)
                self._cond.notify_all()

    def trim_slots(self, pool: Pool) -> None:
        """Drop idle holdings of `pool` (LRU, owner rule applies) until its resident count fits its count cap.

        Stops when nothing more is offerable; the cap may then stay exceeded
        until the next admission of that pool reclaims.
        """
        skipped: set[tuple[Pool, str, str | None]] = set()
        while True:
            with self._cond:
                policy = self._policies.get(pool)
                capacity = policy.slot_capacity() if policy is not None else None
                if capacity is None:
                    return
                excess = self._resident_count_locked(pool, exclude=None) - capacity
                if excess <= 0:
                    return
                view = self._view_locked(None, {})
                offers = [offer for offer in self._collect_offers_locked(view, Need(slots=excess), None, skipped) if offer.pool is pool and offer.frees_slots > 0]
                if not offers:
                    return
                chosen = [offers[0]]
                victims = self._mark_evicting_locked(chosen)
            self._execute(chosen, victims, skipped)

    # ------------------------------------------------------------- inspection
    def view(self, device: str | None = None) -> HoldingsView:
        """A fresh view: the host plus every device a holding is on (and `device`), probed now."""
        with self._cond:
            wanted = {holding.footprint.device for holding in self._holdings.values() if holding.footprint.device is not None}
        normalized = normalize_device(device)
        if normalized is not None:
            wanted.add(normalized)
        host = self._probes.host()
        devices = {name: self._probes.device(name) for name in sorted(wanted)}
        with self._cond:
            return self._view_locked(host, devices)

    def holdings(self, pool: Pool) -> dict[str, HoldingView]:
        """Snapshot `{key: HoldingView}` of `pool`'s holdings (no probe)."""
        with self._cond:
            return {holding.key: holding.snapshot() for holding in self._holdings.values() if holding.pool is pool}

    def pool_counters(self, pool: Pool) -> dict[str, int]:
        """`resident` / `in_use` / `loading` holding counts of `pool`."""
        with self._cond:
            return self._pool_counters_locked(pool)

    def health(self) -> dict[str, object]:
        """Ledger counters per pool and the outstanding reservations (no probe; additive health payload)."""
        with self._cond:
            pools = {pool.value: self._pool_counters_locked(pool) for pool in Pool}
            device_bytes: dict[str, int] = {}
            host_bytes = 0
            for reservation in self._reservations.values():
                if reservation.need.device is not None:
                    device_bytes[reservation.need.device] = device_bytes.get(reservation.need.device, 0) + reservation.need.device_bytes
                host_bytes += reservation.need.host_bytes
            return {
                "pools": pools,
                "reservations": {"count": len(self._reservations), "device_bytes": device_bytes, "host_bytes": host_bytes},
                "evictions_in_flight": self._evictions_in_flight,
            }

    # ------------------------------------------------------- locked internals
    def _bump_locked(self) -> None:
        self._epoch += 1

    def _pool_counters_locked(self, pool: Pool) -> dict[str, int]:
        resident = in_use = loading = 0
        for holding in self._holdings.values():
            if holding.pool is not pool:
                continue
            if holding.resident:
                resident += 1
            if holding.loading:
                loading += 1
            if holding.in_use_count > 0:
                in_use += 1
        return {"resident": resident, "in_use": in_use, "loading": loading}

    def _admission_need(self, holding: _Holding, *, slots: int) -> Need:
        """The admission need of a reserved load: its slot plus, for a hinted lease, its estimated bytes."""
        if holding.hint is None:
            return Need(slots=slots)
        footprint = holding.footprint
        return Need(
            device=footprint.device,
            device_bytes=footprint.device_bytes if footprint.device is not None else 0,
            host_bytes=footprint.host_bytes,
            slots=slots,
            torch_slack_usable=footprint.runtime is Runtime.TORCH,
            # Only a measured size may refuse a load; a hint or the default only makes room.
            strict=footprint.source in (Source.MEASURED, Source.LEARNED),
        )

    def _cleanup_locked(self, holding: _Holding) -> None:
        if holding.resident or holding.loading or holding.evicting or holding.in_use_count > 0:
            return
        slot = (holding.pool, holding.key)
        if self._holdings.get(slot) is holding:
            del self._holdings[slot]

    def _resident_count_locked(self, pool: Pool, *, exclude: str | None) -> int:
        return sum(1 for holding in self._holdings.values() if holding.pool is pool and holding.resident and holding.key != exclude)

    def _busy_owners_locked(self) -> dict[str, int]:
        busy: dict[str, int] = {}
        for holding in self._holdings.values():
            if holding.in_use_count > 0:
                busy[holding.owner] = busy.get(holding.owner, 0) + holding.in_use_count
        return busy

    def _add_reservation_locked(self, requester: Requester, need: Need) -> Reservation:
        reservation = Reservation(self._next_reservation_id, requester, need)
        self._next_reservation_id += 1
        self._reservations[reservation.id] = reservation
        self._bump_locked()
        return reservation

    def _drop_reservation_locked(self, reservation: Reservation) -> None:
        if self._reservations.pop(reservation.id, None) is not None:
            self._budget_epoch += 1
            self._bump_locked()

    def _view_locked(self, host: HostBudget | None, devices: Mapping[str, DeviceBudget]) -> HoldingsView:
        snapshots = tuple(holding.snapshot() for holding in self._holdings.values())
        reserved_device: dict[str, int] = {}
        reserved_host = 0
        for reservation in self._reservations.values():
            if reservation.need.device is not None:
                reserved_device[reservation.need.device] = reserved_device.get(reservation.need.device, 0) + reservation.need.device_bytes
            reserved_host += reservation.need.host_bytes
        host_account = None
        if host is not None:
            host_account = HostAccount(
                known=host.known,
                available=host.available,
                reserve=host.reserve,
                rss_private=host.rss_private,
                headroom=host.headroom(),
                held_by_pool=_attribution(snapshots, device=None),
            )
        accounts: dict[str, DeviceAccount] = {}
        for name, budget in devices.items():
            onnx_held = sum(view.footprint.device_bytes for view in snapshots if view.resident and view.footprint.device == name and view.footprint.runtime is Runtime.ONNX)
            accounts[name] = DeviceAccount(
                device=name,
                total=budget.total,
                free=budget.free,
                torch_allocated=budget.torch_allocated,
                torch_reserved=budget.torch_reserved,
                own_slack_bytes=max(0, budget.torch_reserved - budget.torch_allocated),
                foreign_bytes=max(0, budget.non_torch - onnx_held),
                reserve=budget.reserve,
                headroom=usable_device_headroom(budget, credit_slack=True),
                headroom_without_slack=usable_device_headroom(budget, credit_slack=False),
                held_by_pool=_attribution(snapshots, device=name),
            )
        return HoldingsView(holdings=snapshots, host=host_account, devices=accounts, reserved_device=reserved_device, reserved_host=reserved_host, busy_owners=self._busy_owners_locked())

    def _deficit_locked(self, view: HoldingsView, requester: Requester, need: Need) -> _Deficit:
        """What `need` still lacks. A SMALL requester is not byte-checked on an unreadable side.

        An unreadable budget (all-zero device reading: an ONNX EP index torch
        cannot see, torch without CUDA, a failed probe; or unknown host
        `MemAvailable`) means "unknown", which the large pool treats as "refuse";
        small models loaded fine there before the arbiter, so for them that side
        is skipped (named in `unchecked`) and only the slot counts.
        """
        unchecked: list[str] = []
        lenient = requester.pool is Pool.SMALL
        device_deficit = 0
        if need.device is not None and need.device_bytes > 0:
            account = view.devices.get(need.device)
            if lenient and (account is None or account.total <= 0):
                unchecked.append(need.device)
            else:
                headroom = 0
                if account is not None:
                    headroom = account.headroom if need.torch_slack_usable else account.headroom_without_slack
                device_deficit = need.device_bytes - (headroom - view.reserved_device.get(need.device, 0))
        host_deficit = 0
        if need.host_bytes > 0:
            if lenient and (view.host is None or not view.host.known):
                unchecked.append("host")
            else:
                headroom = view.host.headroom if view.host is not None else 0
                host_deficit = need.host_bytes - (headroom - view.reserved_host)
        slot_deficit = 0
        policy = self._policies.get(requester.pool)
        capacity = policy.slot_capacity() if policy is not None else None
        if need.slots > 0 and capacity is not None:
            # Only RESIDENT holdings occupy a slot (today's cap semantics): concurrent loads of
            # different keys are not counted, so the cap can be exceeded by a parallel load.
            slot_deficit = self._resident_count_locked(requester.pool, exclude=requester.key) + need.slots - capacity
        return _Deficit(device=device_deficit, host=host_deficit, slots=slot_deficit, unchecked=tuple(unchecked))

    def _offerable_locked(self, offer: Offer, view: HoldingsView, requester: Requester | None, skipped: set[tuple[Pool, str, str | None]]) -> bool:
        """R1 (only idle holdings) and the owner rule, re-checked by the arbiter for every offer."""
        if (offer.pool, offer.key, offer.part) in skipped:
            return False
        if requester is not None and offer.pool is requester.pool and offer.key == requester.key:
            return False
        holding = self._holdings.get((offer.pool, offer.key))
        if holding is None or not holding.resident or holding.loading or holding.evicting or holding.in_use_count > 0:
            return False
        return not view.owner_busy_for(holding.owner, requester)

    def _collect_offers_locked(self, view: HoldingsView, need: Need, requester: Requester | None, skipped: set[tuple[Pool, str, str | None]]) -> list[Offer]:
        offers: list[Offer] = []
        for policy in self._policies.values():
            for offer in policy.offers(view, need, requester):
                if self._offerable_locked(offer, view, requester, skipped):
                    offers.append(offer)
        offers.sort(key=lambda offer: _offer_sort_key(offer, need))
        return offers

    def _mark_evicting_locked(self, chosen: Sequence[Offer]) -> dict[tuple[Pool, str], tuple[_Holding, int]]:
        """Flag every chosen holding `evicting`; returns `{slot: (holding, pending offers)}`."""
        victims: dict[tuple[Pool, str], tuple[_Holding, int]] = {}
        for offer in chosen:
            slot = (offer.pool, offer.key)
            holding = self._holdings[slot]
            holding.evicting = True
            _, pending = victims.get(slot, (holding, 0))
            victims[slot] = (holding, pending + 1)
        self._evictions_in_flight += len(chosen)
        self._bump_locked()
        return victims

    def _blocked_by_locked(self, view: HoldingsView, requester: Requester) -> tuple[str, ...]:
        owners = set()
        for holding in self._holdings.values():
            if holding.pool is requester.pool and holding.key == requester.key:
                continue
            if not holding.resident and not holding.loading:
                continue
            if holding.loading or holding.evicting or holding.in_use_count > 0 or view.owner_busy_for(holding.owner, requester):
                owners.add(holding.owner)
        return tuple(sorted(owners))

    def _open_window_locked(self, holding: _Holding, *, own_leases: int) -> _Window | None:
        """A measurement window if the holding has a hint and nothing else is active.

        "Active" = other loads, evictions, IN_RUN reservations or leases. A
        hint-less holding is never measured: its lease must cost exactly what it
        did before the arbiter (no probe at all).
        """
        if holding.hint is None or self._evictions_in_flight > 0:
            return None
        if any(reservation.requester.urgency is Urgency.IN_RUN for reservation in self._reservations.values()):
            return None
        leases = 0
        for other in self._holdings.values():
            if other is not holding and (other.loading or other.evicting):
                return None
            leases += other.in_use_count
        if leases != own_leases:
            return None
        return _Window(epoch=self._epoch, device=holding.footprint.device, runtime=holding.footprint.runtime)

    # --------------------------------------------------------- outside lock
    def _probe_for(self, need: Need) -> tuple[HostBudget | None, dict[str, DeviceBudget]]:
        """The budgets a need is checked against; a slots-only need probes nothing."""
        wants_device = need.device is not None and need.device_bytes > 0
        if not wants_device and need.host_bytes <= 0:
            return None, {}
        host = self._probes.host()
        devices = {need.device: self._probes.device(need.device)} if wants_device and need.device is not None else {}
        return host, devices

    def _sample(self, window: _Window) -> Sample:
        """One measurement sample for `window`'s device (host `RssAnon` always)."""
        host = self._probes.host()
        if window.device is None:
            return Sample(host_rss_anon=host.rss_anon, device_free=None, device_reserved=None)
        device = self._probes.device(window.device)
        if not device.known:
            return Sample(host_rss_anon=host.rss_anon, device_free=None, device_reserved=None)
        return Sample(host_rss_anon=host.rss_anon, device_free=device.free, device_reserved=device.torch_reserved)

    def _take_window_sample(self, window: _Window | None) -> None:
        """Take `window`'s opening sample; the window is kept only if nothing moved meanwhile."""
        if window is None:
            return
        sample = self._sample(window)
        with self._cond:
            if self._epoch == window.epoch:
                window.before = sample

    def _execute(self, chosen: Sequence[Offer], victims: dict[tuple[Pool, str], tuple[_Holding, int]], skipped: set[tuple[Pool, str, str | None]]) -> bool:
        """Execute `chosen` one by one with no lock held; returns whether at least one succeeded."""
        progressed = False
        for offer in chosen:
            policy = self._policies[offer.pool]
            try:
                freed = policy.execute(offer)
            except Exception:
                # A failing reclaim must not take the requester down with it: it is
                # skipped for this call like any other unsuccessful one, and logged.
                log.exception("residency: reclaim %s of %s/%s (part %s) raised; skipped", offer.action.value, offer.pool.value, offer.key, offer.part)
                freed = Freed(device_bytes=0, host_bytes=0, ok=False)
            slot = (offer.pool, offer.key)
            with self._cond:
                holding, pending = victims[slot]
                pending -= 1
                victims[slot] = (holding, pending)
                current = self._holdings.get(slot) is holding
                if current and pending == 0:
                    holding.evicting = False
                if current and freed.ok and offer.part is None and offer.action is Action.DROP:
                    holding.resident = False
                    holding.last_used_at = self._clock()
                if current and pending == 0:
                    self._cleanup_locked(holding)
                if not freed.ok:
                    skipped.add((offer.pool, offer.key, offer.part))
                self._evictions_in_flight -= 1
                self._budget_epoch += 1
                self._bump_locked()
                self._cond.notify_all()
            if freed.ok:
                progressed = True
                log.info("residency: reclaimed %s/%s (%s, part %s): device %d B, host %d B", offer.pool.value, offer.key, offer.action.value, offer.part, freed.device_bytes, freed.host_bytes)
            else:
                log.info("residency: reclaim %s of %s/%s did not succeed; skipped for this request", offer.action.value, offer.pool.value, offer.key)
        return progressed


def _attribution(snapshots: Sequence[HoldingView], *, device: str | None) -> dict[Pool, dict[str, int]]:
    """Ledger bytes per pool per owner: on `device`, or on the host (`device=None`)."""
    held: dict[Pool, dict[str, int]] = {}
    for view in snapshots:
        if not view.resident:
            continue
        if device is None:
            amount = view.footprint.host_bytes
        elif view.footprint.device == device:
            amount = view.footprint.device_bytes
        else:
            continue
        owners = held.setdefault(view.pool, {})
        owners[view.owner] = owners.get(view.owner, 0) + amount
    return held


def _offer_sort_key(offer: Offer, need: Need) -> tuple[int, float, float]:
    """R4: class (A < B < C), then reload seconds per freed byte of the needed side, then LRU."""
    if need.device is not None and need.device_bytes > 0 and offer.device == need.device:
        freed = offer.frees_device
    elif need.host_bytes > 0:
        freed = offer.frees_host
    else:
        freed = 1
    return (int(offer.cls), offer.reload_cost_s / max(1, freed), offer.last_used_at)


def _choose_prefix(offers: Sequence[Offer], deficit: _Deficit, view: HoldingsView, need: Need, requester: Requester, *, allow_partial: bool = False) -> list[Offer] | None:
    """The shortest prefix of sorted `offers` (skipping ones that do not help) covering `deficit`; `None` if impossible.

    With `allow_partial` (a non-strict need) every helping offer is taken even
    when together they do not cover the deficit; `None` only when none helps.

    An offer helps when it frees bytes on a side that is still short (device
    bytes only on the needed device; slots only in the requester's pool). A park
    (`frees_host < 0`) is taken only while the host still fits the requester's
    host need plus every park chosen so far.
    """
    device_left = deficit.device
    host_left = deficit.host
    slots_left = deficit.slots
    host_balance = None
    if view.host is not None:
        host_balance = view.host.headroom - view.reserved_host - need.host_bytes
    chosen: list[Offer] = []
    for offer in offers:
        if device_left <= 0 and host_left <= 0 and slots_left <= 0:
            break
        helps_device = device_left > 0 and offer.device is not None and offer.device == need.device and offer.frees_device > 0
        helps_host = host_left > 0 and offer.frees_host > 0
        helps_slots = slots_left > 0 and offer.frees_slots > 0 and offer.pool is requester.pool
        if not (helps_device or helps_host or helps_slots):
            continue
        if offer.frees_host < 0:
            if host_balance is None or host_balance + offer.frees_host < 0:
                continue
        chosen.append(offer)
        if offer.device is not None and offer.device == need.device:
            device_left -= offer.frees_device
        # A park's host cost is guarded by `host_balance`, not counted as a new host deficit.
        host_left -= max(0, offer.frees_host)
        if offer.pool is requester.pool:
            slots_left -= offer.frees_slots
        if host_balance is not None:
            host_balance += offer.frees_host
    if device_left > 0 or host_left > 0 or slots_left > 0:
        if allow_partial and chosen:
            return chosen
        return None
    return chosen
