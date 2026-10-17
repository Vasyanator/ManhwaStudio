"""
File: modules/ai_backend/runtime/large_models/tiers.py

Purpose:
The vocabulary of residency plans and the user's two resource axes
(«Видеопамять» = `VramStep`, «Оперативная память» = `RamStep`), composed into
tier TEMPLATES by one function (`template_for`): where each component's bytes
live during each phase of a run, and the per-role policies a step pair is
made of. The free variable of a template (how many blocks stay on the device)
is solved by `planner.plan`, not here.

Main responsibilities:
- `Tier`, `Limits` (the user's hard caps);
- the steps: `VramStep` / `VRAM_ORDER`, `RamStep` / `RAM_ORDER`, `Steps`
  (`AUTO_STEPS`, `STEP_WALK` = the RAM-major Auto walk), `StepFloor` (a learned
  floor per axis) and the per-axis order helpers (`vram_index`, `ram_index`,
  `more_economical_vram`, `more_economical_ram`); the host-run-device mapping
  (`HOST_DEVICE_VRAM`, `host_device_steps`: no VRAM axis there);
- the plan types `ComponentPlan`, `PhasePlan`, `Transition`, `ResidencyPlan`
  and `derive_transitions`;
- the templates: `Placement`, `RolePolicy`, `ProfileTemplate`, `template_for`,
  and the policy of a component the request pins to the host
  (`host_pinned_policy`, below).

Composition (`template_for`; rest = HOST for keep/run, DISK for min; park = HOST
for keep/run, DROPPED for min; cpu_ok = RAM step is not min):

| role           | MAX              | PHASE    | HALF          | MIN                           |
|----------------|------------------|----------|---------------|-------------------------------|
| DiT active     | DEVICE           | DEVICE   | JUGGLE(rest)  | JUGGLE(rest), k = 0           |
| encoder active | DEVICE, JUGGLE(rest), CPU if cpu_ok (MAX/PHASE/HALF)  | CPU if cpu_ok, else JUGGLE(DISK) k = 0 |
| AE active      | DEVICE           | DEVICE   | DEVICE        | DEVICE                        |
| idle in run    | DEVICE           | park     | park          | park                          |
| between runs   | DEVICE           | HOST if keep, else DROPPED (PHASE / HALF / MIN)    |

A component the request pins to host execution (`LargeModelSpec.host_pin`,
e.g. "encoder on the CPU") ignores its role's row: the RAM step alone places
it, at every VRAM step (`host_pinned_policy`):

| pinned component | KEEP | RUN     | MIN     |
|------------------|------|---------|---------|
| active           | CPU  | CPU     | CPU     |
| idle in run      | HOST | HOST    | DROPPED |
| between runs     | HOST | DROPPED | DROPPED |

So it is never on the device (a VRAM step's device side excludes it), and
under MIN it is read from its files for its own phase and dropped right after
(nothing parked; the CPU executes from RAM, so the whole component is in RAM
while its phase runs).

HALF judges the device side against at most half of the usable card
(`ProfileTemplate.device_share`). Every step is STRICT: one idle and one keep
tier, never "RAM if it fits, else dropped" — a step that does not fit is not
that step (Auto moves on, an explicit step is refused).

Contracts:
- `ComponentPlan.resident_blocks` is `None` when `tier` applies to the whole
  component; an integer `k` means "non-block weights and the first `k` blocks
  on the device, the remaining blocks in `tier`" (`HOST` = parked in RAM and
  copied in per pass, `DISK` = streamed from the file per pass). A plan never
  carries `k == block count` (that is normalized to `DEVICE`).
- `VramStep.AUTO` / `RamStep.AUTO` are request values only: a
  `ResidencyPlan.steps` is always a concrete pair.
- Tier rank for "more resident" comparisons: `DEVICE` > partial > `HOST` >
  `DISK` = `DROPPED` (neither of the last two holds bytes between passes).

Notes:
- Stdlib only; imports `spec` for `Role` / `ExecDevice`.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from enum import Enum
from types import MappingProxyType

from .spec import ExecDevice, Role


class Tier(Enum):
    """Where a component's (or its non-resident blocks') bytes live."""

    DEVICE = "device"
    HOST = "host"
    DISK = "disk"
    DROPPED = "dropped"


class VramStep(Enum):
    """«Видеопамять»: how much of the card the model may occupy, fastest first; `AUTO` = the first that fits."""

    MAX = "max"
    PHASE = "phase"
    HALF = "half"
    MIN = "min"
    AUTO = "auto"


class RamStep(Enum):
    """«Оперативная память»: where the parts not on the card live, fastest first; `AUTO` = the first that fits."""

    KEEP = "keep"
    RUN = "run"
    MIN = "min"
    AUTO = "auto"


#: The concrete VRAM steps from the fastest to the most economical.
VRAM_ORDER: tuple[VramStep, ...] = (VramStep.MAX, VramStep.PHASE, VramStep.HALF, VramStep.MIN)

#: The concrete RAM steps from the fastest to the most economical.
RAM_ORDER: tuple[RamStep, ...] = (RamStep.KEEP, RamStep.RUN, RamStep.MIN)

#: Fraction of the usable card the HALF step may plan on.
HALF_DEVICE_SHARE = 0.5


def vram_index(step: VramStep) -> int:
    """Position of a concrete VRAM step in `VRAM_ORDER` (`ValueError` for `AUTO`)."""
    return VRAM_ORDER.index(step)


def ram_index(step: RamStep) -> int:
    """Position of a concrete RAM step in `RAM_ORDER` (`ValueError` for `AUTO`)."""
    return RAM_ORDER.index(step)


def more_economical_vram(first: VramStep, second: VramStep) -> VramStep:
    """The more economical (later in `VRAM_ORDER`) of two concrete VRAM steps."""
    return first if vram_index(first) >= vram_index(second) else second


def more_economical_ram(first: RamStep, second: RamStep) -> RamStep:
    """The more economical (later in `RAM_ORDER`) of two concrete RAM steps."""
    return first if ram_index(first) >= ram_index(second) else second


@dataclass(frozen=True)
class Steps:
    """A position on both axes. A request may hold `AUTO` on either axis; a plan's `steps` never does."""

    vram: VramStep
    ram: RamStep

    @property
    def concrete(self) -> bool:
        """Whether neither axis is `AUTO`."""
        return self.vram is not VramStep.AUTO and self.ram is not RamStep.AUTO

    @property
    def label(self) -> str:
        """`"<vram>/<ram>"`, for logs."""
        return f"{self.vram.value}/{self.ram.value}"


#: «Авто» on both axes (the default request).
AUTO_STEPS = Steps(VramStep.AUTO, RamStep.AUTO)

#: The joint Auto walk, RAM-major: (keep, run, min) x (VRAM max -> min). A disk
#: re-read costs tens of seconds cold, a per-pass host-to-device copy a fraction
#: of a second, so giving up VRAM comes before giving up RAM.
STEP_WALK: tuple[Steps, ...] = tuple(Steps(vram, ram) for ram in RAM_ORDER for vram in VRAM_ORDER)

#: The VRAM step a HOST run device (`cpu`, `mps`: the run device's memory IS
#: the host's) plans with, per RAM step. The VRAM axis does not apply there, so
#: the RAM step alone picks the template: `KEEP` -> `MAX` (every component
#: resident, kept between runs), `RUN` / `MIN` -> `PHASE` (only the running
#: phase's components resident; the rest parked in RAM during the run under
#: `RUN`, dropped under `MIN`; nothing kept between runs). `HALF` / `MIN` would
#: juggle blocks host -> host and are never planned there.
HOST_DEVICE_VRAM: Mapping[RamStep, VramStep] = MappingProxyType({RamStep.KEEP: VramStep.MAX, RamStep.RUN: VramStep.PHASE, RamStep.MIN: VramStep.PHASE})


def host_device_steps(ram: RamStep) -> Steps:
    """The concrete step pair a host run device plans for RAM step `ram` (`HOST_DEVICE_VRAM`); `KeyError` for `AUTO`."""
    return Steps(HOST_DEVICE_VRAM[ram], ram)


@dataclass(frozen=True)
class StepFloor:
    """A learned floor per axis: the most economical step a run needed (`None` = no floor on that axis)."""

    vram: VramStep | None = None
    ram: RamStep | None = None

    def __post_init__(self) -> None:
        if self.vram is VramStep.AUTO or self.ram is RamStep.AUTO:
            raise ValueError("StepFloor: шаг пола не может быть AUTO")

    @property
    def empty(self) -> bool:
        """Whether neither axis has a floor."""
        return self.vram is None and self.ram is None


@dataclass(frozen=True)
class Limits:
    """User limits applied on top of the measured budgets (bytes; `None` = no limit), over every step.

    `vram_cap` bounds the whole device usage (as if the card had that much);
    `ram_cap` bounds this process's anonymous memory.
    """

    vram_cap: int | None = None
    ram_cap: int | None = None


# =====================================================================
#  Plans
# =====================================================================
@dataclass(frozen=True)
class ComponentPlan:
    """Where one component lives during one phase (see the module contract for `resident_blocks`).

    `exec_device` matters only in a phase that runs the component;
    `keep_after_run` is the component's between-runs tier (same in every phase).
    """

    component: str
    tier: Tier
    exec_device: ExecDevice
    resident_blocks: int | None
    keep_after_run: Tier

    def __post_init__(self) -> None:
        if self.resident_blocks is not None:
            if self.resident_blocks < 0:
                raise ValueError(f"ComponentPlan «{self.component}»: resident_blocks < 0")
            if self.tier not in (Tier.HOST, Tier.DISK):
                raise ValueError(f"ComponentPlan «{self.component}»: частичная резидентность требует HOST или DISK для остатка, получено {self.tier}")


@dataclass(frozen=True)
class PhasePlan:
    """Every component's placement during one phase (`idle` = between runs)."""

    phase: str
    components: Mapping[str, ComponentPlan]


@dataclass(frozen=True)
class Transition:
    """A phase boundary: components whose device residency shrinks (`outgoing`) or grows (`incoming`)."""

    from_phase: str
    to_phase: str
    outgoing: tuple[str, ...]
    incoming: tuple[str, ...]

    @property
    def name(self) -> str:
        """`"<from>-><to>"`, the key the forecast reports this transition under."""
        return f"{self.from_phase}->{self.to_phase}"


IDLE_PHASE = "idle"


def device_rank(plan: ComponentPlan) -> tuple[int, int]:
    """Ordering key of device residency: whole > partial (by `k`) > none."""
    if plan.tier is Tier.DEVICE:
        return (2, 0)
    if plan.resident_blocks is not None:
        return (1, plan.resident_blocks)
    return (0, 0)


def derive_transitions(phases: tuple[PhasePlan, ...]) -> tuple[Transition, ...]:
    """Boundaries between consecutive `phases` (pass the idle state first and last for a full run)."""
    transitions: list[Transition] = []
    for before, after in zip(phases, phases[1:]):
        outgoing: list[str] = []
        incoming: list[str] = []
        for name, current in before.components.items():
            following = after.components[name]
            if device_rank(following) < device_rank(current):
                outgoing.append(name)
            elif device_rank(following) > device_rank(current):
                incoming.append(name)
        transitions.append(Transition(before.phase, after.phase, tuple(outgoing), tuple(incoming)))
    return tuple(transitions)


@dataclass(frozen=True)
class ResidencyPlan:
    """A complete, budget-checked placement of one model for one run.

    `steps_requested` is what the caller asked for (may hold `AUTO`); `steps`
    the concrete pair this plan was planned for (differs from the request when
    Auto resolved or a learned floor applied). `idle` is the between-runs
    state (also the assumed state before the run: the steady state of repeated
    runs); `transitions` cover idle -> first phase, every boundary, last -> idle.
    """

    steps_requested: Steps
    steps: Steps
    format: str
    phases: tuple[PhasePlan, ...]
    idle: PhasePlan
    transitions: tuple[Transition, ...]

    def __post_init__(self) -> None:
        if not self.steps.concrete:
            raise ValueError(f"ResidencyPlan: steps должны быть конкретными, получено {self.steps.label}")

    def same_placement(self, other: ResidencyPlan) -> bool:
        """Whether `other` places every component exactly like this plan (steps and request ignored)."""
        return self.format == other.format and self.phases == other.phases and self.idle == other.idle

    def phase(self, name: str) -> PhasePlan:
        """The phase plan called `name` (`idle` included); `KeyError` otherwise."""
        if name == IDLE_PHASE:
            return self.idle
        for phase in self.phases:
            if phase.phase == name:
                return phase
        raise KeyError(name)


# =====================================================================
#  Templates
# =====================================================================
@dataclass(frozen=True)
class Placement:
    """One way to place a component while its phase runs.

    `juggle=False`: the whole component in `tier` (`DEVICE` with the run
    device, or `HOST` with `ExecDevice.CPU`). `juggle=True`: as many blocks on
    the device as the budget allows, the rest in `tier` (`HOST` or `DISK`),
    executed on the run device. `resident_cap` (juggle only) bounds `k`:
    `0` = nothing of the component stays on the device between blocks (the MIN
    step), `None` = solved from the budget.
    """

    tier: Tier
    exec_device: ExecDevice
    juggle: bool = False
    resident_cap: int | None = None

    def __post_init__(self) -> None:
        if self.resident_cap is not None and (not self.juggle or self.resident_cap < 0):
            raise ValueError(f"Placement: resident_cap допустим только при жонглировании и >= 0, получено {self}")
        if self.juggle:
            if self.tier not in (Tier.HOST, Tier.DISK) or self.exec_device is not ExecDevice.RUN_DEVICE:
                raise ValueError(f"Placement: жонглирование требует HOST/DISK и устройство запуска, получено {self}")
        elif (self.tier, self.exec_device) not in ((Tier.DEVICE, ExecDevice.RUN_DEVICE), (Tier.HOST, ExecDevice.CPU)):
            raise ValueError(f"Placement: недопустимое сочетание {self}")


@dataclass(frozen=True)
class RolePolicy:
    """A template's policy for one component role; every tuple is ordered by preference.

    `active` while one of its phases runs; `idle` while other phases of the run
    run (`DEVICE` keeps the active layout, `HOST` parks it in RAM, `DROPPED`
    forgets it); `keep` between runs, with the same meaning.
    """

    active: tuple[Placement, ...]
    idle: tuple[Tier, ...]
    keep: tuple[Tier, ...]

    def __post_init__(self) -> None:
        allowed = {Tier.DEVICE, Tier.HOST, Tier.DROPPED}
        if not self.active or not self.idle or not self.keep:
            raise ValueError("RolePolicy: пустой список вариантов")
        if not set(self.idle) <= allowed or not set(self.keep) <= allowed:
            raise ValueError(f"RolePolicy: idle/keep допускают только DEVICE/HOST/DROPPED, получено {self}")


@dataclass(frozen=True)
class ProfileTemplate:
    """One concrete step pair: a `RolePolicy` per component role, and the device share the step may plan on.

    `device_share` `None` = the whole device room; a fraction (HALF) = at most
    that share of the usable card (`min(total, caps) - reserve`).
    """

    steps: Steps
    roles: Mapping[Role, RolePolicy]
    device_share: float | None = None


_ON_DEVICE = Placement(Tier.DEVICE, ExecDevice.RUN_DEVICE)
_ON_CPU = Placement(Tier.HOST, ExecDevice.CPU)
_D, _H, _X = Tier.DEVICE, Tier.HOST, Tier.DROPPED


def _juggle(rest: Tier, *, pinned_off_device: bool = False) -> Placement:
    return Placement(rest, ExecDevice.RUN_DEVICE, juggle=True, resident_cap=0 if pinned_off_device else None)


def template_for(steps: Steps) -> ProfileTemplate:
    """The template of a concrete step pair (module table). Pure.

    # Raises
    `ValueError` when either axis is `AUTO`.
    """
    if not steps.concrete:
        raise ValueError(f"template_for: нужны конкретные шаги, получено {steps.label}")
    vram, ram = steps.vram, steps.ram
    rest = Tier.DISK if ram is RamStep.MIN else Tier.HOST
    park = _X if ram is RamStep.MIN else _H
    cpu_ok = ram is not RamStep.MIN
    keep_off_device = _H if ram is RamStep.KEEP else _X
    if vram is VramStep.MAX:
        idle, keep = _D, _D
    else:
        idle, keep = park, keep_off_device
    match vram:
        case VramStep.MAX | VramStep.PHASE:
            dit_active: tuple[Placement, ...] = (_ON_DEVICE,)
        case VramStep.HALF:
            dit_active = (_juggle(rest),)
        case VramStep.MIN:
            dit_active = (_juggle(rest, pinned_off_device=True),)
        case VramStep.AUTO:
            raise AssertionError("unreachable: AUTO rejected above")
    if vram is VramStep.MIN:
        encoder_active: tuple[Placement, ...] = (_ON_CPU,) if cpu_ok else (_juggle(Tier.DISK, pinned_off_device=True),)
    else:
        # The device first; juggled when the card cannot hold it beside the
        # DiT; the CPU (from RAM) last, when RAM may hold it at all.
        encoder_active = (_ON_DEVICE, _juggle(rest)) + ((_ON_CPU,) if cpu_ok else ())
    roles = MappingProxyType({
        Role.DIT: RolePolicy(dit_active, (idle,), (keep,)),
        Role.ENCODER: RolePolicy(encoder_active, (idle,), (keep,)),
        Role.AE: RolePolicy((_ON_DEVICE,), (idle,), (keep,)),
    })
    return ProfileTemplate(steps, roles, HALF_DEVICE_SHARE if vram is VramStep.HALF else None)


def host_pinned_policy(ram: RamStep) -> RolePolicy:
    """The policy of a component pinned to host execution at RAM step `ram` (module table); the VRAM step plays no part. Pure.

    # Raises
    `ValueError` for `RamStep.AUTO`.
    """
    match ram:
        case RamStep.KEEP:
            return RolePolicy((_ON_CPU,), (_H,), (_H,))
        case RamStep.RUN:
            return RolePolicy((_ON_CPU,), (_H,), (_X,))
        case RamStep.MIN:
            return RolePolicy((_ON_CPU,), (_X,), (_X,))
        case RamStep.AUTO:
            raise ValueError("host_pinned_policy: нужен конкретный шаг ОЗУ, получено auto")
