"""
File: modules/ai_backend/runtime/residency/holdings.py

Purpose:
The pure value types of the residency arbiter: what a pool holds, what a
requester needs, what a pool offers to reclaim, what a reclaim freed, and the
snapshot (`HoldingsView`) a pool policy decides its offers from. No locks, no
I/O, no probes.

Key structures:
- `Pool`, `Source`, `Runtime`, `Urgency`, `Action`, `OfferClass`, `Tier`
- `FootprintHint` (caller-supplied), `Footprint` (recorded), `Part`
- `Need`, `Requester`, `Offer`, `Freed`, `Shortfall`
- `HoldingView`, `DeviceAccount`, `HostAccount`, `ReclaimableBytes`, `HoldingsView`

Key functions:
- `usable_device_headroom()` — device headroom with this process's own torch
  caching-allocator slack credited back (the arbiter's accounting contract).

Notes:
Stdlib only (plus the frozen budget types of `..memory_budget`, which is
torch-free at import).
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from enum import Enum, IntEnum

from ..memory_budget import DeviceBudget


class Pool(Enum):
    """The two peer pools under one arbiter."""

    SMALL = "small"
    LARGE = "large"


class Source(Enum):
    """Where a recorded footprint's numbers come from."""

    DECLARED = "declared"  # the caller's hint (file / parameter bytes)
    MEASURED = "measured"  # measured in a clean window of this load
    LEARNED = "learned"  # measured at an earlier clean load of the same key (this process only)
    DEFAULT = "default"  # nothing known: the unknown-size default


class Runtime(Enum):
    """The allocator a model's device bytes live in; decides how they are measured."""

    TORCH = "torch"  # torch caching allocator: `memory_reserved` delta, process-local
    ONNX = "onnx"  # onnxruntime arena: only `mem_get_info` sees it


class Urgency(Enum):
    """`ADMISSION` (before a load; may wait for in-flight evictions) or `IN_RUN` (never waits)."""

    ADMISSION = "admission"
    IN_RUN = "in_run"


class Action(Enum):
    """What executing an offer does to its holding (or one part of it)."""

    DROP = "drop"
    PARK_TO_HOST = "park_to_host"


class OfferClass(IntEnum):
    """Reclaim order R4: A park idle large GPU->RAM, B drop idle small, C drop idle large."""

    A = 0
    B = 1
    C = 2


class Tier(Enum):
    """Where one part's bytes currently are."""

    DEVICE = "device"
    HOST = "host"


@dataclass(frozen=True)
class FootprintHint:
    """A caller's pre-load size estimate for one model key.

    `device` is the device the model will occupy (`cuda:<n>`; anything else is
    host-accounted, see `footprint.is_device_accounted`); `None` byte counts are
    unknown and take the unknown-size default. `runtime` picks the measurement
    method and whether torch's cached slack can serve the load.
    """

    device: str | None
    device_bytes: int | None = None
    host_bytes: int | None = None
    runtime: Runtime = Runtime.TORCH


@dataclass(frozen=True)
class Footprint:
    """The bytes a holding is recorded with; `device` is `None` for a host-only holding."""

    device: str | None
    device_bytes: int
    host_bytes: int
    source: Source
    runtime: Runtime = Runtime.TORCH


@dataclass(frozen=True)
class Part:
    """One reclaimable piece of a holding (a large model's component; a small model has one).

    `reload_cost_s` is the seconds a reload would cost (`0.0` = unknown; small
    models use it so their order inside class B is pure LRU).
    """

    name: str
    device_bytes: int
    host_bytes: int
    tier: Tier
    reload_cost_s: float = 0.0


@dataclass(frozen=True)
class Need:
    """Bytes (and small-pool slots) a requester wants to be able to allocate.

    `device` names the device side (`None` = host only). `slots` is the count
    of pool slots it will occupy (the small pool's `General.ai_max_loaded_models`
    cap). `torch_slack_usable` says whether the allocations will come from this
    process's torch caching allocator, which can reuse its own cached-but-free
    blocks: only then is that slack credited to the device headroom. `strict`
    says whether a byte shortfall refuses the need; `False` (an unmeasured
    estimate) still reclaims to make room but is admitted when it does not fit.
    A slot shortfall always refuses.
    """

    device: str | None = None
    device_bytes: int = 0
    host_bytes: int = 0
    slots: int = 0
    torch_slack_usable: bool = True
    strict: bool = True


@dataclass(frozen=True)
class Requester:
    """Who asks: `holds_lease` = the requester's own lease is counted in its owner's busy count."""

    pool: Pool
    key: str
    owner: str
    urgency: Urgency
    holds_lease: bool = False


@dataclass(frozen=True)
class Offer:
    """One reclaim a pool proposes, built purely from a `HoldingsView`.

    `part` is `None` for the whole holding. `frees_host` is negative for a park
    (the bytes move to RAM). `frees_slots` counts pool slots (only meaningful for
    the requester's own pool). `handle` is opaque to the arbiter and passed back
    to the policy's `execute` (the small pool puts the unload callback there).
    """

    pool: Pool
    key: str
    part: str | None
    action: Action
    device: str | None
    frees_device: int
    frees_host: int
    reload_cost_s: float
    cls: OfferClass
    last_used_at: float
    frees_slots: int = 0
    handle: object = None


@dataclass(frozen=True)
class Freed:
    """What executing an offer achieved; `ok=False` = nothing changed (skipped for this call)."""

    device_bytes: int
    host_bytes: int
    ok: bool


@dataclass(frozen=True)
class Shortfall:
    """A request the arbiter could not satisfy: the first short `resource`, by `deficit`.

    `resource` is `"device"`, `"host"` or `"slots"`; `deficit` is in bytes (or
    slots); `blocked_by` names the owners whose in-use, loading or evicting
    holdings could otherwise have been reclaimed.
    """

    resource: str
    deficit: int
    blocked_by: tuple[str, ...]


@dataclass(frozen=True)
class HoldingView:
    """Immutable snapshot of one holding (taken under the arbiter lock)."""

    pool: Pool
    key: str
    owner: str
    footprint: Footprint
    parts: tuple[Part, ...]
    resident: bool
    loading: bool
    evicting: bool
    in_use_count: int
    last_used_at: float
    reclaim: object = None

    @property
    def idle(self) -> bool:
        """Resident, not loading, not evicting, no lease (R1 says only such holdings may be offered)."""
        return self.resident and not self.loading and not self.evicting and self.in_use_count == 0


@dataclass(frozen=True)
class DeviceAccount:
    """One device split into its three parts, plus the headroom the arbiter hands out (bytes).

    `foreign_bytes` is what neither this process's torch allocator nor any
    ledger ONNX holding explains (other processes, the context, kernels);
    `held_by_pool` is the ledger's attribution `{pool: {owner: bytes}}`;
    `own_slack_bytes` is `torch_reserved - torch_allocated` (cached, free for
    torch). `headroom` credits the slack, `headroom_without_slack` does not.
    """

    device: str
    total: int
    free: int
    torch_allocated: int
    torch_reserved: int
    own_slack_bytes: int
    foreign_bytes: int
    reserve: int
    headroom: int
    headroom_without_slack: int
    held_by_pool: Mapping[Pool, Mapping[str, int]]


@dataclass(frozen=True)
class HostAccount:
    """The host budget as the arbiter saw it, with the ledger's attribution `{pool: {owner: bytes}}`.

    `known` is `False` when `MemAvailable` could not be read (headroom then `0`).
    """

    available: int
    reserve: int
    rss_private: int
    headroom: int
    held_by_pool: Mapping[Pool, Mapping[str, int]]
    known: bool = True


@dataclass(frozen=True)
class ReclaimableBytes:
    """Bytes a pool's idle, offerable holdings could free, per side."""

    device_bytes: int = 0
    host_bytes: int = 0


@dataclass(frozen=True)
class HoldingsView:
    """What a pool policy decides its offers from: holdings, budgets, reservations, busy owners.

    `devices` holds the accounts of the devices probed for this view (the
    requested one, and in `ResidencyArbiter.view()` every device a holding is
    on). `reserved_*` are the outstanding (unmaterialized) reservations.
    `busy_owners` maps each owner to its count of in-use leases.
    """

    holdings: tuple[HoldingView, ...]
    host: HostAccount | None
    devices: Mapping[str, DeviceAccount]
    reserved_device: Mapping[str, int]
    reserved_host: int
    busy_owners: Mapping[str, int] = field(default_factory=dict)

    def of_pool(self, pool: Pool) -> tuple[HoldingView, ...]:
        """The holdings of `pool`, in insertion order."""
        return tuple(holding for holding in self.holdings if holding.pool is pool)

    def owner_busy_for(self, owner: str, requester: Requester | None) -> bool:
        """The owner rule: does `owner` have an in-use lease other than the requester's own?

        Under `IN_RUN` the requester's own owner counts as busy unconditionally
        (it holds its service lock while it runs).
        """
        count = self.busy_owners.get(owner, 0)
        if requester is not None and requester.owner == owner:
            if requester.urgency is Urgency.IN_RUN:
                return True
            if requester.holds_lease:
                count -= 1
        return count > 0

    def reclaimable(self, pool: Pool, *, device: str | None, requester: Requester | None = None) -> ReclaimableBytes:
        """Bytes `pool`'s idle holdings (R1 + owner rule for `requester`) could free on `device` / the host.

        A ledger figure (who could be evicted), never a probe: what is actually
        free is always re-probed (risk K3).
        """
        device_bytes = 0
        host_bytes = 0
        for holding in self.of_pool(pool):
            if not holding.idle:
                continue
            # A small holding without an unload callback cannot be dropped (the
            # small policy never offers it); large holdings are moved by their policy.
            if pool is Pool.SMALL and holding.reclaim is None:
                continue
            if self.owner_busy_for(holding.owner, requester):
                continue
            if requester is not None and holding.pool is requester.pool and holding.key == requester.key:
                continue
            if device is not None and holding.footprint.device == device:
                device_bytes += holding.footprint.device_bytes
            host_bytes += holding.footprint.host_bytes
        return ReclaimableBytes(device_bytes=device_bytes, host_bytes=host_bytes)


def usable_device_headroom(budget: DeviceBudget, *, credit_slack: bool) -> int:
    """`budget`'s headroom, with this process's own torch slack counted as usable when `credit_slack`.

    `mem_get_info().free` counts the caching allocator's cached-but-free blocks
    (`torch_reserved - torch_allocated`) as used, as if another process held
    them. A torch allocation reuses them, so for such a need they are credited:
    with another process holding X, own models Y allocated and Z cached, the
    headroom is `total - X - Y - reserve`. The cap and `total - reserve` clamps
    of `DeviceBudget.headroom` still apply. Without the credit this is exactly
    `budget.headroom()`.
    """
    if not credit_slack:
        return budget.headroom()
    if budget.total <= 0:
        return 0
    slack = max(0, budget.torch_reserved - budget.torch_allocated)
    room = budget.free + slack
    if budget.cap is not None:
        room = min(room, max(0, budget.cap - max(0, budget.used - slack)))
    room = min(room, budget.total)
    return max(0, min(room - budget.reserve, budget.total - budget.reserve))
