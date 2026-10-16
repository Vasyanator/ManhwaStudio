"""
File: modules/ai_backend/runtime/large_models/forecast.py

Purpose:
The one forecast arithmetic of a residency plan: peak device bytes, peak host
bytes and seconds for every phase, every phase transition and the between-runs
state, plus the time summary and the plan lines the UI shows. The planner
checks fits with exactly these numbers, so what the user sees is what the guard
enforces.

Main responsibilities:
- `forecast(...)` -> `Forecast` (`PhaseForecast` per phase / transition,
  `TimeForecast`, `PlanLine`s, `calibrated`, `device_calibrated`);
- the per-component byte accounting of a `ComponentPlan`
  (`component_device_bytes`, `component_host_bytes`).

Arithmetic (all bytes):
- phase device peak = device-resident weights + activation x reserved factor
  (+ the spec's extra device bytes) + staging for non-resident blocks of the
  compute component (`prefetch_depth + 1` largest remaining blocks) + margin
  (`margin_fraction` of that sum, at most `margin_cap_bytes`) + runtime overhead;
- phase host peak = baseline RSS + host-resident weights + CPU activation +
  host staging when blocks stream from disk;
- transition peak = max(before, after) of the weights + ONE unit in flight
  (largest moving block or tensor) — a component that moves is counted in its
  old place before and in its new place after, never in both; on the host the
  unit in flight is at least the load transient
  (`ComponentInventory.load_host_bytes`: the loader's read buffer of one file
  tensor plus its conversion outputs) of every component the boundary reads
  from its files (loads run one at a time, so the largest one counts);
- the first run passes the boundaries from an empty process (every component
  read cold): each such boundary's HOST peak is folded into the steady
  transition of the same name (`idle-><first phase>` for the first one), so
  a plan that never reloads between runs still carries its cold-load
  transient; the device side of a transition stays the steady one;
- overall peaks = max over phases, transitions and the idle state.

Host run device (`host_device=True`: `cpu` / `mps`, whose "device" memory is
the host's): every device-side byte is host memory, so it is folded into the
host side and the device peaks are `0` (no separate device, no runtime
overhead, no device margin):
- a state's weights are (0, device + host weights);
- a phase's host peak = baseline + all its weights + its activation (the
  DEVICE cost entry's bytes x reserved factor: the same tensors, now in RAM)
  + any staging; its seconds come from the CPU cost entry, and the phase is
  never `calibrated` (no table measures a host-executed device phase);
- a transition's host peak = baseline + max(before, after) of all weights +
  ONE unit in flight (at least the load transient, as above).

Calibration is judged PER PHASE: a phase is calibrated when its cost entry
is measured and the table itself is calibrated (`PhaseForecast.calibrated`).
`Forecast.calibrated` = every phase calibrated (what the UI shows as "exact");
`Forecast.device_calibrated` = every phase that EXECUTES ON THE DEVICE
calibrated (what the planner's device guard needs: a CPU phase puts no
activation on the device, so its estimate cannot make a device peak wrong).

Contract the consumers must keep: the forecast assumes the caching allocator's
cache is released at every phase boundary (`clear_torch_cache` in the
transition choreography). Cache kept from an earlier phase is not modelled.

Notes:
- Stdlib only. Pure: every input is passed in.
"""

from __future__ import annotations

import logging
from collections.abc import Mapping
from dataclasses import dataclass, replace
from types import MappingProxyType
from typing import Any

from .cost_tables import EXEC_CPU, EXEC_DEVICE, CostTables
from .inventory import ComponentInventory
from .spec import ExecDevice, LargeModelSpec, RequestShape
from .tiers import ComponentPlan, PhasePlan, ResidencyPlan, Tier

log = logging.getLogger(__name__)


@dataclass(frozen=True)
class PhaseForecast:
    """Peaks (bytes) and seconds of one phase, transition or the idle state, with the weight/activation split.

    `calibrated` is whether this entry's activation and seconds come from a
    measured cost entry of a calibrated table; transitions and the idle state
    carry no activation and are always `True`.
    """

    device_peak: int
    host_peak: int
    seconds: float
    device_weights: int
    device_activation: int
    host_weights: int
    host_activation: int
    calibrated: bool = True


@dataclass(frozen=True)
class TimeForecast:
    """Seconds: `load_cold_s` = first run from an empty process at the cold disk rate;
    `load_warm_s` = reads of a repeated run (from the between-runs state) at the
    page-cache rate; `per_step_s` = one pass of the most-repeated phase;
    `total_s` = first run end to end (cold load + phases + moves)."""

    load_cold_s: float
    load_warm_s: float
    per_step_s: float
    total_s: float


@dataclass(frozen=True)
class PlanLine:
    """One component's placement in its first active phase, as block counts."""

    component: str
    tier: Tier
    exec_device: ExecDevice
    blocks_device: int
    blocks_host: int
    blocks_disk: int


@dataclass(frozen=True)
class Forecast:
    """Everything `forecast()` computes; `peak_phase` names where `device_peak` occurs.

    `calibrated`: every phase calibrated; `device_calibrated`: every phase that
    executes on the run device calibrated (module notes).
    """

    phases: Mapping[str, PhaseForecast]
    transitions: Mapping[str, PhaseForecast]
    device_peak: int
    host_peak: int
    peak_phase: str
    time: TimeForecast
    plan_summary: tuple[PlanLine, ...]
    calibrated: bool
    device_calibrated: bool


def component_device_bytes(plan: ComponentPlan, inventory: ComponentInventory) -> int:
    """Device bytes `plan` keeps resident between passes."""
    if plan.tier is Tier.DEVICE:
        return inventory.bytes_total
    if plan.resident_blocks is not None:
        return inventory.resident_bytes(plan.resident_blocks)
    return 0


def component_host_bytes(plan: ComponentPlan, inventory: ComponentInventory) -> int:
    """Host (anonymous RAM) bytes `plan` keeps."""
    if plan.tier is not Tier.HOST:
        return 0
    if plan.resident_blocks is not None:
        return inventory.remainder_bytes(plan.resident_blocks)
    return inventory.bytes_total


def _block_counts(plan: ComponentPlan, inventory: ComponentInventory) -> tuple[int, int, int]:
    """(device, host, disk) block counts of `plan`."""
    total = inventory.block_count
    if plan.tier is Tier.DEVICE:
        return total, 0, 0
    if plan.resident_blocks is not None:
        rest = total - plan.resident_blocks
        return (plan.resident_blocks, rest, 0) if plan.tier is Tier.HOST else (plan.resident_blocks, 0, rest)
    if plan.tier is Tier.HOST:
        return 0, total, 0
    return 0, 0, total


def _moving_unit(inventory: ComponentInventory) -> int:
    """The largest piece a block-wise move has in flight for this component."""
    return max(inventory.max_block_bytes(), inventory.max_tensor_bytes)


class _Calc:
    """Shared state of one `forecast()` call."""

    def __init__(self, inventories: Mapping[str, ComponentInventory], tables: CostTables, baseline_rss: int, *, host_device: bool = False) -> None:
        self.inventories = inventories
        self.tables = tables
        self.baseline = max(0, int(baseline_rss))
        # A host run device: every device-side byte is host memory (module notes).
        self.host_device = host_device
        # Per-phase calibration (module notes): all phases, and the
        # device-executed ones only.
        self.measured = True
        self.device_measured = True

    def _margin(self, device_bytes: int) -> int:
        constants = self.tables.device
        return min(constants.margin_cap_bytes, int(device_bytes * constants.margin_fraction))

    def _device_peak(self, device_sum: int) -> int:
        """`device_sum` + margin + runtime overhead; `0` on a host run device (nothing lives on a separate device)."""
        if self.host_device:
            return 0
        return device_sum + self._margin(device_sum) + self.tables.device.runtime_overhead_bytes

    def weights(self, state: PhasePlan) -> tuple[int, int]:
        """(device, host) weight bytes of a state; on a host run device `(0, device + host)`."""
        device = sum(component_device_bytes(plan, self.inventories[name]) for name, plan in state.components.items())
        host = sum(component_host_bytes(plan, self.inventories[name]) for name, plan in state.components.items())
        if self.host_device:
            return 0, device + host
        return device, host

    def idle(self, state: PhasePlan) -> PhaseForecast:
        device, host = self.weights(state)
        return PhaseForecast(self._device_peak(device), self.baseline + host, 0.0, device, 0, host, 0)

    def phase(self, spec: LargeModelSpec, state: PhasePlan, shape: RequestShape, options: Any, format: str, compute_path: str) -> tuple[PhaseForecast, int]:
        """Forecast one model phase; also returns its pass count."""
        phase_spec = next((phase for phase in spec.phases if phase.name == state.phase), None)
        if phase_spec is None:
            raise KeyError(f"plan phase {state.phase!r} is not a phase of {spec.family}")
        work = phase_spec.work(shape, options)
        compute_plan = state.components[phase_spec.compute]
        inventory = self.inventories[phase_spec.compute]
        exec_kind = EXEC_CPU if compute_plan.exec_device is ExecDevice.CPU else EXEC_DEVICE
        cost = self.tables.phase_cost(family=spec.family, phase=state.phase, variant=work.variant, format=format, compute_path=compute_path, exec=exec_kind)
        # On a host run device the bytes stay those of the entry above (the
        # same tensors, now in RAM) but the speed is the CPU's.
        timing = cost if not self.host_device or exec_kind == EXEC_CPU else self.tables.phase_cost(family=spec.family, phase=state.phase, variant=work.variant, format=format, compute_path=compute_path, exec=EXEC_CPU)
        calibrated = self.tables.calibrated and cost.measured and not (self.host_device and exec_kind == EXEC_DEVICE)
        if not calibrated:
            self.measured = False
            if exec_kind == EXEC_DEVICE and not self.host_device:
                self.device_measured = False
        activation = cost.fixed_bytes + cost.bytes_per_token * work.tokens + cost.bytes_per_pixel * work.pixels
        reserved = int(activation * cost.reserved_factor)
        on_device = exec_kind == EXEC_DEVICE and not self.host_device
        device_act, host_act = (reserved + work.extra_device_bytes, 0) if on_device else (0, reserved + work.extra_device_bytes)

        device_w, host_w = self.weights(state)
        constants = self.tables.device
        device_stage = host_stage = 0
        per_pass_copy = 0.0
        if compute_plan.resident_blocks is not None:
            block = inventory.max_block_bytes(compute_plan.resident_blocks)
            device_stage = (constants.prefetch_depth + 1) * block
            remainder = inventory.remainder_bytes(compute_plan.resident_blocks)
            if compute_plan.tier is Tier.DISK:
                host_stage = device_stage
                per_pass_copy = inventory.file_bytes_for(remainder) / constants.disk_cold_bytes_per_s
            else:
                per_pass_copy = remainder / constants.block_h2d_bytes_per_s
            if self.host_device:
                # The "device" slots are host memory too.
                host_stage += device_stage
                device_stage = 0
        device_sum = device_w + device_act + device_stage
        device_peak = self._device_peak(device_sum)
        host_peak = self.baseline + host_w + host_act + host_stage

        ktokens = work.tokens / 1000.0
        seconds = timing.seconds_fixed + work.passes * (timing.seconds_per_ktoken_pass * ktokens + timing.seconds_per_ktoken2_pass * ktokens * ktokens + per_pass_copy) + timing.seconds_per_mpixel * work.pixels / 1e6
        return PhaseForecast(device_peak, host_peak, seconds, device_w, device_act, host_w, host_act, calibrated), work.passes

    def transition(self, before: PhasePlan, after: PhasePlan, *, disk_rate: float) -> tuple[PhaseForecast, float, float]:
        """Peaks of one boundary; returns (forecast, move seconds, disk-read seconds at `disk_rate`)."""
        constants = self.tables.device
        device_a, host_a = self.weights(before)
        device_b, host_b = self.weights(after)
        in_flight = 0
        load_transient = 0
        move_s = 0.0
        disk_bytes = 0
        for name, plan_a in before.components.items():
            plan_b = after.components[name]
            inventory = self.inventories[name]
            dev_a, dev_b = component_device_bytes(plan_a, inventory), component_device_bytes(plan_b, inventory)
            hst_a, hst_b = component_host_bytes(plan_a, inventory), component_host_bytes(plan_b, inventory)
            if (dev_a, hst_a) == (dev_b, hst_b):
                continue
            in_flight = max(in_flight, _moving_unit(inventory))
            d_dev, d_host = dev_b - dev_a, hst_b - hst_a
            from_disk = 0
            if d_dev > 0:
                from_host = min(d_dev, max(0, -d_host))
                move_s += from_host / constants.restore_bytes_per_s
                from_disk += d_dev - from_host
            to_host = 0
            if d_dev < 0:
                to_host = min(-d_dev, max(0, d_host))
                move_s += to_host / constants.park_bytes_per_s
            if d_host > to_host:
                from_disk += d_host - to_host
            if from_disk > 0:
                # Read from its files: the loader's one-tensor host transient.
                load_transient = max(load_transient, inventory.load_host_bytes)
            disk_bytes += inventory.file_bytes_for(from_disk)
        device_w = max(device_a, device_b)
        device_peak = self._device_peak(device_w + in_flight)
        host_w = max(host_a, host_b)
        host_peak = self.baseline + host_w + max(in_flight, load_transient)
        disk_s = disk_bytes / disk_rate
        return PhaseForecast(device_peak, host_peak, move_s + disk_s, device_w, 0, host_w, 0), move_s, disk_s


def _empty_state(plan: ResidencyPlan) -> PhasePlan:
    """The state of a fresh process: nothing of the model resident."""
    return PhasePlan("empty", MappingProxyType({name: ComponentPlan(name, Tier.DROPPED, cp.exec_device, None, cp.keep_after_run) for name, cp in plan.idle.components.items()}))


def forecast(spec: LargeModelSpec, plan: ResidencyPlan, inventories: Mapping[str, ComponentInventory], shape: RequestShape, tables: CostTables, *, compute_path: str, baseline_rss: int, options: Any = None, host_device: bool = False) -> Forecast:
    """Forecast `plan` for one request.

    `inventories` are the plan format's component inventories; `compute_path`
    the format's `FormatInfo.compute_path`; `baseline_rss` the backend's
    anonymous RSS without any large model (host peaks include it); `options`
    is passed to each phase's `work`; `host_device` folds every device-side
    byte into the host side (a `cpu` / `mps` run device, module notes).

    # Raises
    `KeyError` when the plan or the inventories miss a component of `spec`,
    `LookupError` from a malformed cost table.
    """
    calc = _Calc(inventories, tables, baseline_rss, host_device=host_device)
    phases: dict[str, PhaseForecast] = {"idle": calc.idle(plan.idle)}
    passes_by_phase: dict[str, int] = {}
    for state in plan.phases:
        phases[state.phase], passes_by_phase[state.phase] = calc.phase(spec, state, shape, options, plan.format, compute_path)

    steady = (plan.idle, *plan.phases, plan.idle)
    transitions: dict[str, PhaseForecast] = {}
    move_s = warm_disk_s = 0.0
    constants = tables.device
    for before, after in zip(steady, steady[1:]):
        name = f"{before.phase}->{after.phase}"
        transitions[name], moved, disk_s = calc.transition(before, after, disk_rate=constants.disk_warm_bytes_per_s)
        move_s += moved
        warm_disk_s += disk_s
    cold_s = 0.0
    empty = _empty_state(plan)
    cold = (empty, *plan.phases)
    for before, after in zip(cold, cold[1:]):
        cold_entry, _, disk_s = calc.transition(before, after, disk_rate=constants.disk_cold_bytes_per_s)
        cold_s += disk_s
        # The first run crosses the same boundary from a colder state: its host
        # peak (cold loads and their transients) belongs to that boundary too.
        name = f"{plan.idle.phase if before is empty else before.phase}->{after.phase}"
        steady_entry = transitions[name]
        if cold_entry.host_peak > steady_entry.host_peak:
            transitions[name] = replace(steady_entry, host_peak=cold_entry.host_peak)

    every = {**phases, **transitions}
    peak_phase = max(every, key=lambda key: every[key].device_peak)
    repeated = max(passes_by_phase, key=lambda key: passes_by_phase[key])
    per_step = phases[repeated].seconds / passes_by_phase[repeated]
    compute_s = sum(phases[state.phase].seconds for state in plan.phases)

    uses_by_phase = {phase.name: phase.uses for phase in spec.phases}
    lines: list[PlanLine] = []
    for component in spec.components:
        first = next((state for state in plan.phases if component.name in uses_by_phase[state.phase]), None)
        if first is None:
            continue
        cp = first.components[component.name]
        on_device, on_host, on_disk = _block_counts(cp, inventories[component.name])
        lines.append(PlanLine(component.name, cp.tier, cp.exec_device, on_device, on_host, on_disk))

    result = Forecast(
        phases=MappingProxyType(phases),
        transitions=MappingProxyType(transitions),
        device_peak=every[peak_phase].device_peak,
        host_peak=max(entry.host_peak for entry in every.values()),
        peak_phase=peak_phase,
        time=TimeForecast(load_cold_s=cold_s, load_warm_s=warm_disk_s, per_step_s=per_step, total_s=cold_s + compute_s + move_s),
        plan_summary=tuple(lines),
        calibrated=tables.calibrated and calc.measured,
        device_calibrated=tables.calibrated and calc.device_measured,
    )
    log.debug("forecast %s/%s/%s: device %d B at %s, host %d B, total %.1f s, calibrated=%s (device %s)", spec.family, plan.steps.label, plan.format, result.device_peak, peak_phase, result.host_peak, result.time.total_s, result.calibrated, result.device_calibrated)
    return result
