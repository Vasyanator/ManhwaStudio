"""
File: modules/ai_backend/runtime/large_models/planner.py

Purpose:
Fits the user's two resource axes («Видеопамять» x «Оперативная память») to
the measured budgets: turns the `ProfileTemplate` of a step pair into a
concrete `ResidencyPlan` (choosing among the template's alternatives and
solving how many blocks stay on the device), resolves «Авто» on either axis,
and refuses with suggestions when nothing fits.

Main responsibilities:
- `Budgets` - the budget inputs of one planning call (host, device, baseline
  RSS, the model's own resident bytes, bytes claimed by other pools, bytes other
  pools could free on request: `Reclaimable`);
- `plan(...)` -> `Planned` (with its `PlanFit`: FREE, or NEEDS_RECLAIM and how
  much) | `PlanRefusal` (`Suggestion`s);
- `plan_axis(...)` - every concrete step of one axis planned explicitly with
  the other axis fixed, for the two step pickers;
- `guarded_forecast(...)` - the forecast the device guard enforces (the
  device activations of uncalibrated device-executed phases taken out).

The step grid (one per planning call and weight format): every concrete pair
`(vram, ram)` is one CELL, planned in this order:
1. its own template, strict (forecast device peak with every activation
   <= the step's device room);
2. its own template, relaxed (the uncalibrated rule below);
3. when its own template fits its room but not under the room chain's caps
   (below): the plan of the nearest fitting cell above it on either axis
   ("same as"); a cell whose own template does not fit its room is refused
   (never raised on either axis to fit).
The cell's device room is the whole room, except HALF: at most half of the
usable card (`min(total, caps) - reserve`). MIN forces `k = 0`.

Per-axis monotonic invariant, BY CONSTRUCTION (the room chain): a cell is
capped at the nearest fitting cell above it in its column (VRAM axis: its
full device peak) and in its row (RAM axis: its host growth). A cell whose
own template fits its room but cannot get under its caps takes that
neighbour's plan (`same_as_vram` / `same_as_ram`) when the neighbour's plan
fits the cell's own room and both caps, else it is refused. Hence, for the same request and budgets, a lower VRAM step never
forecasts more device bytes than a higher one (full forecast, every
activation counted), and a lower RAM step never more host growth. The grid
does not depend on the request or the learned floor; those only choose which
cells answer.

Contracts:
- Deterministic and pure: every input is passed in.
- A plan fits when its forecast device peak <= its step's device room
  (+ reclaimable device bytes) and its host growth <= host room + reclaimable
  host bytes (and the user caps hold). `PlanFit` says whether it fits in free
  memory alone or how many bytes per side must be reclaimed first; the planner
  never reclaims itself. Device room never exceeds `total - reserve`
  (`budget.DeviceBudget.headroom`); unknown budgets refuse.
- The planner NEVER switches the weight format: other formats appear only as
  `Suggestion(kind="format")`, computed with this same planner.
- An explicit (non-Auto) step is honoured exactly, apart from a learned floor
  (`StepFloor`: the effective step on each axis is the more economical of the
  request and the floor); it is never raised on the other axis to fit — it is
  refused, with `Suggestion(kind="steps")` naming the pair that fits with the
  other axis lowered. «Авто» walks VRAM `MAX -> MIN` with a fixed RAM step,
  RAM `KEEP -> MIN` with a fixed VRAM step, and both RAM-major
  (`tiers.STEP_WALK`), starting at the floor; it takes the first STRICT fit,
  else the first relaxed one. `ResidencyPlan.steps_requested` keeps the request.
- **Uncalibrated forecasts** are judged PER PHASE (`PhaseForecast.calibrated`;
  a family without measured entries, e.g. FLUX.2 until its calibration run, or
  a borrowed table of an unknown device class, makes every phase uncalibrated).
  The device ACTIVATION of an uncalibrated phase that executes on the device is
  an estimate, so it chooses the plan but never refuses it; a measured phase's
  activation stays binding, and a CPU-executed phase puts no activation on the
  device at all (so a CPU encoder with an unmeasured entry relaxes nothing).
  A relaxed cell judges its device room on the guarded peak; the room chain
  always uses the full peak. The weight bytes (exact, from the headers) and
  the whole HOST side stay strict: never a host OOM.
- `Planned.forecast` is the guarded forecast (`guarded_forecast`; equal to
  `Planned.full_forecast` when `device_calibrated`): the admission's
  reservation, the fit and the ladder's comparisons use the numbers the guard
  enforced. `Planned.full_forecast` (every activation counted) is the
  monotone one and what the step pickers show; `calibrated=False` tells the UI
  it is an estimate.

Host run device (`Budgets.host_device`: `cpu` / `mps`, every component
executes in host memory):
- the device budget is not read and never refuses (no `budget_unknown` for
  the device); the forecast folds every device-side byte into the host side
  (`forecast(host_device=True)`: device peaks are `0`), so the HOST room alone
  judges the plan, strictly (activations included): never a host OOM;
- the VRAM axis does not apply: a request's VRAM step (explicit, «Авто» or a
  learned VRAM floor) is ignored and the RAM step picks the cell
  (`tiers.host_device_steps`: `KEEP` -> `(MAX, KEEP)`, `RUN` -> `(PHASE,
  RUN)`, `MIN` -> `(PHASE, MIN)`); juggling placements are never planned
  (blocks would be copied host -> host). «Авто» on the RAM axis walks those
  three cells. The VRAM picker rows are not applicable (the manager reports
  them so).

Host pin (`LargeModelSpec.host_pin(options)`, e.g. "encoder on the CPU"):
- a pinned component is placed by `tiers.host_pinned_policy(<the cell's RAM
  step>)` instead of its role's row of the cell's template: CPU execution from
  RAM at every VRAM step, so no cell's device side (weights, activation,
  staging) holds any of it, and the VRAM ladder / «Авто» walk never moves it to
  the device; the RAM step alone decides whether it is kept, parked during the
  run, or read from its files per run. The other components plan as before;
  the per-axis invariant holds unchanged (the pinned placement is the same
  along a row and RAM-monotone along a column);
- ignored on a host run device (everything executes in host memory there).

Notes:
- Stdlib only.
"""

from __future__ import annotations

import itertools
import logging
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field, replace
from enum import Enum
from types import MappingProxyType
from typing import Any, Literal

from ..memory_budget import DeviceBudget, HostBudget
from .cost_tables import CostTables
from .forecast import Forecast, forecast
from .inventory import ComponentInventory
from .spec import FormatInfo, LargeModelSpec, RequestShape
from .tiers import (
    IDLE_PHASE,
    RAM_ORDER,
    STEP_WALK,
    VRAM_ORDER,
    ComponentPlan,
    Limits,
    PhasePlan,
    Placement,
    ProfileTemplate,
    RamStep,
    ResidencyPlan,
    StepFloor,
    Steps,
    Tier,
    VramStep,
    derive_transitions,
    more_economical_ram,
    more_economical_vram,
    ram_index,
    host_device_steps,
    host_pinned_policy,
    template_for,
    vram_index,
)

log = logging.getLogger(__name__)

#: Refusal reasons (stable machine codes; the UI maps them to text).
REASON_BUDGET_UNKNOWN = "budget_unknown"
REASON_DEVICE_SHORT = "device_short"
REASON_HOST_SHORT = "host_short"
REASON_STEPS_UNSUPPORTED = "steps_unsupported"
REASON_FORMAT_UNAVAILABLE = "format_unavailable"

#: The axis names of `plan_axis`.
Axis = Literal["vram", "ram"]


@dataclass(frozen=True)
class Reclaimable:
    """Bytes other memory pools could free for this request (idle holdings), per side."""

    device_bytes: int = 0
    host_bytes: int = 0


class FitKind(Enum):
    """Whether a plan fits in free memory (`FREE`) or only after a reclaim (`NEEDS_RECLAIM`)."""

    FREE = "free"
    NEEDS_RECLAIM = "needs_reclaim"


@dataclass(frozen=True)
class PlanFit:
    """How a plan fits: for `NEEDS_RECLAIM`, the bytes per side to reclaim before admission (0 for `FREE`)."""

    kind: FitKind
    device_bytes: int = 0
    host_bytes: int = 0


@dataclass(frozen=True)
class Budgets:
    """Budget inputs of one planning call (bytes).

    `baseline_rss` is the backend's anonymous RSS without any large-pool
    holding — every handle's host bytes taken out, this model's own and its
    pair partner's included (the manager's `_large_free_rss`); host peaks
    include it, so a reported host peak is what the process would hold with
    only this plan's large model loaded. It never enters the host ROOM
    (`host_room`: probed availability + own bytes - claims, + `reclaimable`),
    and a plan's host growth is its peak minus this same baseline: holdings of
    other large handles count as occupied unless reclaimable. `own_*_bytes` are what THIS model already holds (a warm
    model): the plan may reuse them, so they are credited back. `claimed_*_bytes`
    are bytes other memory pools have claimed (outstanding reservations) but
    that the readings do not show yet; they are taken out of the room.
    `reclaimable` is what other pools could free on request (used only when the
    free room is not enough; reported through `PlanFit`). `observed_deficit_bytes`
    is what a running session found missing on the device (the degrade
    ladder's accumulated deficit): it is taken out of every step's device room
    AFTER the step's share (so it tightens HALF too). `host_device`: the run
    device IS the host (`cpu` / `mps`; module notes): `device` is then not read
    (a zero budget), and the `own_*` / `claimed_*` / `reclaimable` figures must
    carry the device-side bytes on the host side. `device_cap_bytes` is a
    per-handle ceiling on the device room (`None` = none): the largest device
    peak this one plan may forecast, applied after the claims and the
    reclaimable bytes (it only ever lowers the room, never the user's
    `vram_cap`, and does not change what «Не больше половины» halves); a
    caller uses it to keep one handle below what another step of the same
    operation would give it. Ignored on a host run device (no device room).
    """

    host: HostBudget
    device: DeviceBudget
    baseline_rss: int
    own_device_bytes: int = 0
    own_host_bytes: int = 0
    claimed_device_bytes: int = 0
    claimed_host_bytes: int = 0
    reclaimable: Reclaimable = Reclaimable()
    observed_deficit_bytes: int = 0
    host_device: bool = False
    device_cap_bytes: int | None = None

    def device_room(self, vram_cap: int | None) -> int:
        """Device bytes a plan's peak may reach, with the user cap applied (never above `total - reserve`)."""
        caps = [cap for cap in (self.device.cap, vram_cap) if cap is not None]
        credited = replace(self.device, free=self.device.free + max(0, self.own_device_bytes), cap=min(caps) if caps else None)
        return max(0, credited.headroom() - max(0, self.claimed_device_bytes))

    def device_usable(self, vram_cap: int | None) -> int:
        """The usable size of the card: `min(total, caps) - reserve` (what «Не больше половины» halves)."""
        size = min([self.device.total, *(cap for cap in (self.device.cap, vram_cap) if cap is not None)])
        return max(0, size - max(0, self.device.reserve))

    def host_room(self) -> int:
        """Host bytes a plan's host growth (its peak above `baseline_rss`) may reach: probed headroom + own bytes - claims; independent of `baseline_rss`."""
        return max(0, self.host.headroom() + max(0, self.own_host_bytes) - max(0, self.claimed_host_bytes))


@dataclass(frozen=True)
class Suggestion:
    """A user action that would make the request fit; the user must choose it.

    `kind` is `"format"` (payload: `format`, `vram`, `ram`), `"limits"`
    (payload: `vram`, `ram` that fit without the user's caps) or `"steps"`
    (payload: `vram`, `ram`: the pair that fits with the other axis lowered);
    `text_key` the i18n key.
    """

    kind: str
    text_key: str
    payload: Mapping[str, str]


@dataclass(frozen=True)
class Planned:
    """A fitting plan, the forecasts it was checked with, and whether it needs a reclaim first.

    `forecast` is the guarded forecast (what the guard enforces and the
    reservation uses), `full_forecast` the one with every activation (the
    monotone one, shown to the user). `relaxed` = admitted without its
    estimated device activations (the uncalibrated rule). `same_as_vram` /
    `same_as_ram`: the higher step on that axis whose plan this is (identical
    placement), `None` when this step plans differently.
    """

    plan: ResidencyPlan
    forecast: Forecast
    fit: PlanFit
    full_forecast: Forecast
    relaxed: bool = False
    same_as_vram: VramStep | None = None
    same_as_ram: RamStep | None = None


@dataclass(frozen=True)
class PlanRefusal:
    """Nothing fits. `reason` is a `REASON_*` code; `shortest_resource` `"device"`,
    `"host"` or `None`; `best_steps_tried` the step pair of the closest attempt;
    `forecast` that attempt's full forecast (for display), `None` when no
    attempt could be forecast."""

    reason: str
    shortest_resource: str | None
    best_steps_tried: Steps | None
    suggestions: tuple[Suggestion, ...] = ()
    forecast: Forecast | None = None


PlanResult = Planned | PlanRefusal


@dataclass
class _Attempt:
    """The closest failing attempt so far (smallest overshoot in bytes)."""

    overshoot: int = -1
    resource: str | None = None
    steps: Steps | None = None
    forecast: Forecast | None = None
    reason: str = REASON_STEPS_UNSUPPORTED
    # Whether any forecast tried was not device-calibrated (a relaxed pass can differ only then).
    saw_estimate: bool = False

    def offer(self, overshoot: int, resource: str, steps: Steps, result: Forecast, reason: str) -> None:
        if self.overshoot < 0 or overshoot < self.overshoot:
            self.overshoot, self.resource, self.steps, self.forecast, self.reason = overshoot, resource, steps, result, reason

    def merge(self, other: _Attempt) -> None:
        if other.overshoot >= 0 and other.steps is not None and other.forecast is not None and other.resource is not None:
            self.offer(other.overshoot, other.resource, other.steps, other.forecast, other.reason)
        elif self.overshoot < 0 and other.steps is not None:
            self.steps, self.reason = other.steps, other.reason


@dataclass(frozen=True)
class _Request:
    """Everything one planning call needs besides the steps and the format."""

    spec: LargeModelSpec
    limits: Limits
    budgets: Budgets
    shape: RequestShape
    tables: CostTables
    options: Any
    device_free: int = field(init=False)
    host_free: int = field(init=False)
    device_room: int = field(init=False)
    host_room: int = field(init=False)
    device_usable: int = field(init=False)
    host_pin: frozenset[str] = field(init=False)

    def __post_init__(self) -> None:
        # Validated by the spec (unknown / CPU-incapable names raise); a host
        # run device executes everything in host memory, so a pin means nothing.
        object.__setattr__(self, "host_pin", frozenset() if self.budgets.host_device else self.spec.host_pin(self.options))
        # `*_free`: what fits without touching anyone; `*_room`: plus what other
        # pools could free. Fits are judged against the room; `PlanFit` reports
        # the part above the free figure.
        device_free = self.budgets.device_room(self.limits.vram_cap)
        host_free = self.budgets.host_room()
        reclaimable = self.budgets.reclaimable
        device_room = device_free + max(0, reclaimable.device_bytes)
        cap = self.budgets.device_cap_bytes
        if cap is not None:
            # The per-handle ceiling bounds the whole room, reclaim included.
            device_free = min(device_free, max(0, cap))
            device_room = min(device_room, max(0, cap))
        object.__setattr__(self, "device_free", device_free)
        object.__setattr__(self, "host_free", host_free)
        object.__setattr__(self, "device_room", device_room)
        object.__setattr__(self, "host_room", host_free + max(0, reclaimable.host_bytes))
        object.__setattr__(self, "device_usable", self.budgets.device_usable(self.limits.vram_cap))

    def step_device_room(self, template: ProfileTemplate) -> int:
        """The device room of one step: the whole room, or its share of the usable card (HALF), less the observed deficit."""
        room = self.device_room if template.device_share is None else min(self.device_room, int(self.device_usable * template.device_share))
        return max(0, room - max(0, self.budgets.observed_deficit_bytes))


_LEVEL = {Tier.DEVICE: 2, Tier.HOST: 1, Tier.DROPPED: 0}


def _active_plan(name: str, placement: Placement, keep: Tier, resident: int | None, block_count: int) -> ComponentPlan:
    """The component's plan while its phase runs (`resident` = solved `k` of a juggle)."""
    if not placement.juggle:
        return ComponentPlan(name, placement.tier, placement.exec_device, None, keep)
    k = block_count if resident is None else resident
    if k >= block_count:
        return ComponentPlan(name, Tier.DEVICE, placement.exec_device, None, keep)
    return ComponentPlan(name, placement.tier, placement.exec_device, k, keep)


def _resting_plan(active: ComponentPlan, tier: Tier) -> ComponentPlan:
    """The component outside its phases: `DEVICE` keeps the active layout, else parked/dropped whole."""
    if tier is Tier.DEVICE:
        return active
    return ComponentPlan(active.component, tier, active.exec_device, None, active.keep_after_run)


def _build_plan(request: _Request, steps: Steps, weight_format: str, inventories: Mapping[str, ComponentInventory], choice: Mapping[str, tuple[Placement, Tier, Tier]], resident: Mapping[str, int]) -> ResidencyPlan:
    """A `ResidencyPlan` from one choice of alternatives and the solved resident block counts."""
    spec = request.spec
    actives: dict[str, ComponentPlan] = {}
    for component in spec.components:
        placement, _, keep = choice[component.name]
        actives[component.name] = _active_plan(component.name, placement, keep, resident.get(component.name), inventories[component.name].block_count)
    first_use = {component.name: next((index for index, phase in enumerate(spec.phases) if component.name in phase.uses), len(spec.phases)) for component in spec.components}
    phases: list[PhasePlan] = []
    for index, phase in enumerate(spec.phases):
        components: dict[str, ComponentPlan] = {}
        for component in spec.components:
            name = component.name
            _, idle, keep = choice[name]
            if name in phase.uses:
                components[name] = actives[name]
            elif index < first_use[name]:
                # Before the first use the steady state of repeated runs holds
                # what was kept, but never more than the idle policy allows.
                components[name] = _resting_plan(actives[name], min(idle, keep, key=_LEVEL.__getitem__))
            else:
                components[name] = _resting_plan(actives[name], idle)
        phases.append(PhasePlan(phase.name, MappingProxyType(components)))
    idle_state = PhasePlan(IDLE_PHASE, MappingProxyType({name: _resting_plan(plan, plan.keep_after_run) for name, plan in actives.items()}))
    ordered = tuple(phases)
    return ResidencyPlan(
        steps_requested=steps,
        steps=steps,
        format=weight_format,
        phases=ordered,
        idle=idle_state,
        transitions=derive_transitions((idle_state, *ordered, idle_state)),
    )


def guarded_forecast(result: Forecast) -> Forecast:
    """The forecast the device guard enforces: `result` itself when `device_calibrated`, else the estimated device activations taken out.

    Only phases with `calibrated` false lose their `device_activation` from
    `device_peak` (a CPU-executed phase has none to lose; transitions and the
    idle state carry none); measured phases keep theirs. The overall
    `device_peak` / `peak_phase` are re-derived; host figures and the per-phase
    `device_activation` are kept as they were.
    """
    if result.device_calibrated:
        return result
    phases = {name: entry if entry.calibrated else replace(entry, device_peak=max(0, entry.device_peak - entry.device_activation)) for name, entry in result.phases.items()}
    every = {**phases, **result.transitions}
    peak_phase = max(every, key=lambda key: every[key].device_peak)
    return replace(result, phases=MappingProxyType(phases), device_peak=every[peak_phase].device_peak, peak_phase=peak_phase)


def _host_growth(request: _Request, result: Forecast) -> int:
    """Host bytes the plan adds above the baseline RSS."""
    return max(0, result.host_peak - max(0, request.budgets.baseline_rss))


def _host_ok(request: _Request, result: Forecast, host_cap: int | None) -> tuple[bool, int]:
    """Whether the host side fits (room, user cap, the room chain's `host_cap`), and by how much it overshoots."""
    growth = _host_growth(request, result)
    overshoot = growth - request.host_room
    if host_cap is not None:
        overshoot = max(overshoot, growth - host_cap)
    cap = request.limits.ram_cap
    if cap is not None:
        rss = request.budgets.host.rss_private
        if rss <= 0:
            return False, max(overshoot, growth)
        overshoot = max(overshoot, rss - max(0, request.budgets.own_host_bytes) + growth - cap)
    return overshoot <= 0, overshoot


def _fit_of(request: _Request, result: Forecast) -> PlanFit:
    """`FREE` when the fitting plan needs no reclaim, else how many bytes per side to reclaim."""
    device_need = max(0, result.device_peak - request.device_free)
    host_need = max(0, _host_growth(request, result) - request.host_free)
    if device_need == 0 and host_need == 0:
        return PlanFit(FitKind.FREE)
    return PlanFit(FitKind.NEEDS_RECLAIM, device_need, host_need)


@dataclass(frozen=True)
class _Limits:
    """What one template attempt is judged against: the step's device room, the chain caps, the pass."""

    device_room: int
    device_cap: int | None
    host_cap: int | None
    relaxed: bool

    def device_overshoot(self, full: Forecast) -> int:
        """Bytes above the room (strict: full peak; relaxed: guarded peak) or above the chain cap (full peak)."""
        judged = guarded_forecast(full).device_peak if self.relaxed else full.device_peak
        overshoot = judged - self.device_room
        if self.device_cap is not None:
            overshoot = max(overshoot, full.device_peak - self.device_cap)
        return overshoot


#: `(choice, resident) -> (plan, full forecast)` of one template (memoized by the grid).
_Forecaster = Callable[[Mapping[str, tuple[Placement, Tier, Tier]], Mapping[str, int]], tuple[ResidencyPlan, Forecast]]


def _plan_template(request: _Request, template: ProfileTemplate, inventories: Mapping[str, ComponentInventory], attempt: _Attempt, limits: _Limits, forecaster: _Forecaster) -> Planned | None:
    """The first fitting plan of one step template under `limits`, or `None` (the closest miss goes to `attempt`)."""
    spec = request.spec
    steps = template.steps
    slots: list[list[tuple[Placement, Tier, Tier]]] = []
    for component in spec.components:
        policy = host_pinned_policy(steps.ram) if component.name in request.host_pin else template.roles[component.role]
        # A host run device never juggles: its "device" slots are host memory.
        actives = [placement for placement in policy.active if placement.exec_device in component.exec_devices_allowed and not (placement.juggle and request.budgets.host_device)]
        if not actives:
            log.debug("planner %s: steps %s have no placement for %s allowed by %s", spec.family, steps.label, component.name, sorted(device.value for device in component.exec_devices_allowed))
            return None
        slots.append(list(itertools.product(actives, policy.idle, policy.keep)))
    def _forecast(choice: Mapping[str, tuple[Placement, Tier, Tier]], resident: Mapping[str, int]) -> tuple[ResidencyPlan, Forecast]:
        found = forecaster(choice, resident)
        attempt.saw_estimate = attempt.saw_estimate or not found[1].device_calibrated
        return found

    for combination in itertools.product(*slots):
        choice = {component.name: combination[index] for index, component in enumerate(spec.components)}
        resident: dict[str, int] = {}
        for component in spec.components:
            placement = choice[component.name][0]
            if not placement.juggle:
                continue
            count = inventories[component.name].block_count
            top = count if placement.resident_cap is None else min(count, placement.resident_cap)
            # k = count is the whole component on the device: no staging
            # buffers, so it can fit where k = count - 1 does not; try it first.
            if top == count and limits.device_overshoot(_forecast(choice, {**resident, component.name: count})[1]) <= 0:
                resident[component.name] = count
                continue
            # Largest k < count whose plan fits the device: below the whole
            # component the device peak only grows with k (one more resident
            # block, the same staging), so a binary search is exact. A miss
            # even at k = 0 is judged below on the k = 0 plan.
            low, high = 0, min(top, count - 1)
            while low < high:
                middle = (low + high + 1) // 2
                if limits.device_overshoot(_forecast(choice, {**resident, component.name: middle})[1]) <= 0:
                    low = middle
                else:
                    high = middle - 1
            resident[component.name] = low
        candidate, full = _forecast(choice, resident)
        result = guarded_forecast(full)
        device_over = limits.device_overshoot(full)
        if device_over > 0:
            attempt.offer(device_over, "device", steps, full, REASON_DEVICE_SHORT)
            continue
        host_fits, host_over = _host_ok(request, result, limits.host_cap)
        if not host_fits:
            attempt.offer(host_over, "host", steps, full, REASON_HOST_SHORT)
            continue
        if limits.relaxed and full.device_peak > limits.device_room:
            log.warning("planner %s: %s/%s admitted without its estimated activations: the forecast with the unmeasured activation estimate (device %d B) exceeds the room %d B; a device OOM goes to the degrade ladder", spec.family, steps.label, candidate.format, full.device_peak, limits.device_room)
        return Planned(candidate, result, _fit_of(request, result), full, relaxed=limits.relaxed)
    return None


@dataclass(frozen=True)
class _Cell:
    """One cell of the step grid: its plan (`None` = refused), its own closest miss, and the chain state it hands down."""

    steps: Steps
    planned: Planned | None
    attempt: _Attempt
    # The nearest fitting cell at or above this one in its column / row (the
    # reuse candidates and the room-chain caps of the cells below).
    column_fit: Planned | None
    row_fit: Planned | None


class _Grid:
    """The step grid of one weight format for one planning request (module notes); cells are planned lazily, memoized."""

    def __init__(self, request: _Request, weight_format: str, info: FormatInfo, inventories: Mapping[str, ComponentInventory]) -> None:
        self._request = request
        self.host_device = request.budgets.host_device
        self._format = weight_format
        self._info = info
        self._inventories = inventories
        self._cells: dict[Steps, _Cell] = {}
        self._forecasts: dict[tuple[Steps, tuple[tuple[Placement, Tier, Tier], ...], tuple[int | None, ...]], tuple[ResidencyPlan, Forecast]] = {}

    def _forecaster(self, steps: Steps) -> _Forecaster:
        """The memoized forecast of one template's candidates (every pass and cap of a cell asks for the same ones)."""
        request = self._request
        names = [component.name for component in request.spec.components]

        def run(choice: Mapping[str, tuple[Placement, Tier, Tier]], resident: Mapping[str, int]) -> tuple[ResidencyPlan, Forecast]:
            key = (steps, tuple(choice[name] for name in names), tuple(resident.get(name) for name in names))
            found = self._forecasts.get(key)
            if found is None:
                candidate = _build_plan(request, steps, self._format, self._inventories, choice, resident)
                found = (candidate, forecast(request.spec, candidate, self._inventories, request.shape, request.tables, compute_path=self._info.compute_path, baseline_rss=request.budgets.baseline_rss, options=request.options, host_device=request.budgets.host_device))
                self._forecasts[key] = found
            return found

        return run

    def cell(self, steps: Steps) -> _Cell:
        """The cell of a concrete step pair."""
        found = self._cells.get(steps)
        if found is None:
            found = self._plan_cell(steps)
            self._cells[steps] = found
        return found

    def _plan_cell(self, steps: Steps) -> _Cell:
        request = self._request
        v, r = vram_index(steps.vram), ram_index(steps.ram)
        above = self.cell(Steps(VRAM_ORDER[v - 1], steps.ram)) if v > 0 else None
        left = self.cell(Steps(steps.vram, RAM_ORDER[r - 1])) if r > 0 else None
        column_prev = above.column_fit if above is not None else None
        row_prev = left.row_fit if left is not None else None
        device_cap = column_prev.full_forecast.device_peak if column_prev is not None else None
        host_cap = _host_growth(request, row_prev.full_forecast) if row_prev is not None else None
        template = template_for(steps)
        room = request.step_device_room(template)
        attempt = _Attempt()
        planned = self._own(template, attempt, room, device_cap, host_cap)
        if planned is None and (device_cap is not None or host_cap is not None) and self._own(template, _Attempt(), room, None, None) is not None:
            # The step fits its own room but would need more than a higher
            # step's plan: it repeats that plan instead (the room chain).
            planned = self._reuse(steps, room, device_cap, host_cap, column_prev, row_prev)
        if planned is not None:
            planned = self._mark_same(planned, steps, column_prev, row_prev)
        return _Cell(steps, planned, attempt, planned if planned is not None else column_prev, planned if planned is not None else row_prev)

    def _own(self, template: ProfileTemplate, attempt: _Attempt, room: int, device_cap: int | None, host_cap: int | None) -> Planned | None:
        """The cell's own template: strict first, then relaxed (the uncalibrated rule; only when an estimate was seen)."""
        forecaster = self._forecaster(template.steps)
        planned = _plan_template(self._request, template, self._inventories, attempt, _Limits(room, device_cap, host_cap, False), forecaster)
        if planned is None and attempt.saw_estimate:
            planned = _plan_template(self._request, template, self._inventories, attempt, _Limits(room, device_cap, host_cap, True), forecaster)
        return planned

    def _reuse(self, steps: Steps, room: int, device_cap: int | None, host_cap: int | None, column_prev: Planned | None, row_prev: Planned | None) -> Planned | None:
        """The neighbour plan this cell repeats when its own template fits its room only above its caps (module notes), else `None`."""
        for neighbour in (column_prev, row_prev):
            if neighbour is None:
                continue
            judged = neighbour.forecast.device_peak if neighbour.relaxed else neighbour.full_forecast.device_peak
            if judged > room or (device_cap is not None and neighbour.full_forecast.device_peak > device_cap):
                continue
            if host_cap is not None and _host_growth(self._request, neighbour.full_forecast) > host_cap:
                continue
            return replace(neighbour, plan=replace(neighbour.plan, steps=steps, steps_requested=steps), same_as_vram=None, same_as_ram=None)
        return None

    @staticmethod
    def _mark_same(planned: Planned, steps: Steps, column_prev: Planned | None, row_prev: Planned | None) -> Planned:
        """`same_as_*` set when the plan places everything exactly like the nearest fitting higher step on that axis."""
        same_vram = same_ram = None
        if column_prev is not None and planned.plan.same_placement(column_prev.plan):
            same_vram = column_prev.same_as_vram or column_prev.plan.steps.vram
        if row_prev is not None and planned.plan.same_placement(row_prev.plan):
            same_ram = row_prev.same_as_ram or row_prev.plan.steps.ram
        return replace(planned, same_as_vram=same_vram, same_as_ram=same_ram)


def _effective(steps: Steps, floor: StepFloor | None) -> Steps:
    """The request with the learned floor applied to its concrete axes (AUTO axes stay AUTO)."""
    vram, ram = steps.vram, steps.ram
    if floor is not None:
        if vram is not VramStep.AUTO and floor.vram is not None:
            vram = more_economical_vram(vram, floor.vram)
        if ram is not RamStep.AUTO and floor.ram is not None:
            ram = more_economical_ram(ram, floor.ram)
    return Steps(vram, ram)


def _walk(steps: Steps, floor: StepFloor | None, *, host_device: bool = False) -> list[Steps]:
    """The cells a request may resolve to, in preference order («Авто» starts at the floor on its axis).

    On a host run device the VRAM step and floor are ignored: one cell per RAM
    step (`tiers.host_device_steps`).
    """
    effective = _effective(steps, floor)
    vram_from = vram_index(floor.vram) if floor is not None and floor.vram is not None else 0
    ram_from = ram_index(floor.ram) if floor is not None and floor.ram is not None else 0
    rams = RAM_ORDER[ram_from:] if effective.ram is RamStep.AUTO else (effective.ram,)
    if host_device:
        return [host_device_steps(ram) for ram in rams]
    vrams = VRAM_ORDER[vram_from:] if effective.vram is VramStep.AUTO else (effective.vram,)
    return [cell for cell in STEP_WALK if cell.vram in vrams and cell.ram in rams]


def _resolve(grid: _Grid, steps: Steps, floor: StepFloor | None) -> tuple[Planned | None, _Attempt]:
    """The first strict fit along the request's walk, else the first relaxed fit; the closest miss when none fits."""
    cells = [grid.cell(candidate) for candidate in _walk(steps, floor, host_device=grid.host_device)]
    chosen = next((cell.planned for cell in cells if cell.planned is not None and not cell.planned.relaxed), None)
    if chosen is None:
        chosen = next((cell.planned for cell in cells if cell.planned is not None), None)
    attempt = _Attempt()
    for cell in cells:
        attempt.merge(cell.attempt)
    if chosen is not None:
        chosen = replace(chosen, plan=replace(chosen.plan, steps_requested=steps))
    return chosen, attempt


def plan(
    spec: LargeModelSpec,
    inventories: Mapping[str, Mapping[str, ComponentInventory]],
    *,
    format: str,
    formats: Mapping[str, FormatInfo],
    steps: Steps,
    limits: Limits,
    budgets: Budgets,
    shape: RequestShape,
    tables: CostTables,
    options: Any = None,
    floor: StepFloor | None = None,
) -> PlanResult:
    """Plan one run of `spec` in weight format `format` with the requested `steps`.

    `inventories` maps format -> component -> inventory for every format whose
    files are present (needed for the format suggestions); `formats` the
    formats' facts; `steps` concrete or `AUTO` on either axis; `floor` the
    learned floor per axis (`None` = none).

    Returns `Planned` (the first fit of the request's walk, module contract) or
    `PlanRefusal` with format / limits / steps suggestions. The components
    `spec.host_pin(options)` names are placed by the RAM step alone (module
    notes, "Host pin").

    # Raises
    `KeyError` when an inventory misses a component of `spec`; `ValueError`
    from `spec.host_pin` for a pin the spec cannot honour.
    """
    request = _Request(spec, limits, budgets, shape, tables, options)
    device_unknown = not budgets.device.known and not budgets.host_device
    if device_unknown or not budgets.host.known:
        resource = "device" if device_unknown else "host"
        log.warning("planner %s: %s budget unknown, refusing", spec.family, resource)
        return PlanRefusal(REASON_BUDGET_UNKNOWN, resource, None)
    info = formats.get(format)
    if info is None or not info.available or format not in inventories:
        suggestions = _format_suggestions(request, format, formats, inventories, steps, floor)
        return PlanRefusal(REASON_FORMAT_UNAVAILABLE, None, None, suggestions)
    grid = _Grid(request, format, info, inventories[format])
    planned, attempt = _resolve(grid, steps, floor)
    if planned is not None:
        log.info("planner %s: %s -> %s (%s%s), device %d/%d B, host +%d/%d B", spec.family, steps.label, planned.plan.steps.label, format, ", relaxed" if planned.relaxed else "", planned.forecast.device_peak, request.device_room, _host_growth(request, planned.forecast), request.host_room)
        return planned
    suggestions = list(_steps_suggestions(grid, steps, floor))
    suggestions += _format_suggestions(request, format, formats, inventories, steps, floor)
    if limits.vram_cap is not None or limits.ram_cap is not None:
        lifted_request = replace(request, limits=Limits())
        lifted, _ = _resolve(_Grid(lifted_request, format, info, inventories[format]), steps, floor)
        if lifted is not None:
            suggestions.append(Suggestion("limits", "large_models.suggestion.raise_limits", _steps_payload(lifted.plan.steps)))
    log.info("planner %s: %s/%s refused (%s, short on %s, overshoot %d B)", spec.family, steps.label, format, attempt.reason, attempt.resource, attempt.overshoot)
    return PlanRefusal(attempt.reason, attempt.resource, attempt.steps, tuple(suggestions), attempt.forecast)


def _steps_payload(steps: Steps, **extra: str) -> Mapping[str, str]:
    return MappingProxyType({**extra, "vram": steps.vram.value, "ram": steps.ram.value})


def _steps_suggestions(grid: _Grid, steps: Steps, floor: StepFloor | None) -> list[Suggestion]:
    """For each explicit axis of a refused request: the first lower step on that axis that fits, the other axis as requested."""
    effective = _effective(steps, floor)
    found: list[Suggestion] = []
    # No VRAM suggestion on a host run device: the VRAM axis does not apply there.
    if effective.vram is not VramStep.AUTO and not grid.host_device:
        for lower in VRAM_ORDER[vram_index(effective.vram) + 1:]:
            planned, _ = _resolve(grid, Steps(lower, steps.ram), floor)
            if planned is not None:
                found.append(Suggestion("steps", "large_models.suggestion.steps", _steps_payload(planned.plan.steps)))
                break
    if effective.ram is not RamStep.AUTO:
        for lower_ram in RAM_ORDER[ram_index(effective.ram) + 1:]:
            planned, _ = _resolve(grid, Steps(steps.vram, lower_ram), floor)
            if planned is not None:
                suggestion = Suggestion("steps", "large_models.suggestion.steps", _steps_payload(planned.plan.steps))
                if suggestion not in found:
                    found.append(suggestion)
                break
    return found


def _format_suggestions(request: _Request, current: str, formats: Mapping[str, FormatInfo], inventories: Mapping[str, Mapping[str, ComponentInventory]], steps: Steps, floor: StepFloor | None) -> tuple[Suggestion, ...]:
    """Other available formats whose plan fits the same request, best quality first."""
    found: list[tuple[int, Suggestion]] = []
    for name, info in formats.items():
        if name == current or not info.available or name not in inventories:
            continue
        planned, _ = _resolve(_Grid(request, name, info, inventories[name]), steps, floor)
        if planned is not None:
            found.append((info.quality_rank, Suggestion("format", "large_models.suggestion.format", _steps_payload(planned.plan.steps, format=name))))
    found.sort(key=lambda item: -item[0])
    return tuple(suggestion for _, suggestion in found)


def plan_grid(
    spec: LargeModelSpec,
    inventories: Mapping[str, Mapping[str, ComponentInventory]],
    *,
    format: str,
    formats: Mapping[str, FormatInfo],
    limits: Limits,
    budgets: Budgets,
    shape: RequestShape,
    tables: CostTables,
    options: Any = None,
) -> dict[Steps, PlanResult]:
    """Every concrete step pair planned explicitly (no floor, no walk): the whole step grid of one format, `STEP_WALK` order.

    The cells obey the module's monotonic invariant; a cell's `same_as_vram` /
    `same_as_ram` names the higher step whose plan it repeats. A refused cell
    carries its steps suggestions (format / limits suggestions come only from
    `plan`). An unknown budget or an unavailable format refuses every cell
    with the same refusal `plan` gives.
    """
    request = _Request(spec, limits, budgets, shape, tables, options)
    info = formats.get(format)
    if (not budgets.device.known and not budgets.host_device) or not budgets.host.known or info is None or not info.available or format not in inventories:
        refusal = plan(spec, inventories, format=format, formats=formats, steps=STEP_WALK[0], limits=limits, budgets=budgets, shape=shape, tables=tables, options=options)
        return {cell: refusal for cell in STEP_WALK}
    grid = _Grid(request, format, info, inventories[format])
    cells: dict[Steps, PlanResult] = {}
    for cell in STEP_WALK:
        planned, attempt = _resolve(grid, cell, None)
        cells[cell] = planned if planned is not None else PlanRefusal(attempt.reason, attempt.resource, attempt.steps, tuple(_steps_suggestions(grid, cell, None)), attempt.forecast)
    return cells


def _axis_cells(axis: Axis, fixed: VramStep | RamStep) -> list[tuple[VramStep | RamStep, Steps]]:
    """The (step, cell) pairs of one axis with the other axis at `fixed`; `ValueError` when `fixed` is `AUTO` or of the wrong axis."""
    if axis == "vram":
        if not isinstance(fixed, RamStep) or fixed is RamStep.AUTO:
            raise ValueError(f"planner: для оси vram нужен конкретный шаг ОЗУ, получено {fixed!r}")
        return [(step, Steps(step, fixed)) for step in VRAM_ORDER]
    if not isinstance(fixed, VramStep) or fixed is VramStep.AUTO:
        raise ValueError(f"planner: для оси ram нужен конкретный шаг видеопамяти, получено {fixed!r}")
    return [(step, Steps(fixed, step)) for step in RAM_ORDER]


def axis_rows(grid: Mapping[Steps, PlanResult], *, axis: Axis, fixed: VramStep | RamStep) -> dict[VramStep | RamStep, PlanResult]:
    """One axis of a `plan_grid` result: every concrete step of `axis` with the other axis at `fixed`, fastest first.

    # Raises
    `ValueError` when `fixed` is `AUTO` or does not belong to the other axis.
    """
    return {step: grid[cell] for step, cell in _axis_cells(axis, fixed)}


def plan_axis(
    spec: LargeModelSpec,
    inventories: Mapping[str, Mapping[str, ComponentInventory]],
    *,
    format: str,
    formats: Mapping[str, FormatInfo],
    axis: Axis,
    fixed: VramStep | RamStep,
    limits: Limits,
    budgets: Budgets,
    shape: RequestShape,
    tables: CostTables,
    options: Any = None,
) -> dict[VramStep | RamStep, PlanResult]:
    """Every concrete step of `axis` planned explicitly (no floor, no walk) with the other axis at `fixed`, for the step pickers.

    `axis_rows` over `plan_grid`: the rows are cells of one grid, so they obey
    the module's monotonic invariant.

    # Raises
    `ValueError` when `fixed` is `AUTO` or does not belong to the other axis.
    """
    _axis_cells(axis, fixed)
    return axis_rows(plan_grid(spec, inventories, format=format, formats=formats, limits=limits, budgets=budgets, shape=shape, tables=tables, options=options), axis=axis, fixed=fixed)
