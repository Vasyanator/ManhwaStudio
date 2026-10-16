"""
File: modules/ai_backend/runtime/large_models/test_tiers.py

Purpose:
Unit tests for `tiers.py`: the step orders and the RAM-major Auto walk, the
composition table of `template_for` (all 12 concrete pairs), placement and
policy validation, the floor and plan rules (`AUTO` is never a plan value),
and transitions derived from consecutive states.
"""

from __future__ import annotations

import unittest
from types import MappingProxyType

from .spec import ExecDevice, Role
from .tiers import (
    AUTO_STEPS,
    IDLE_PHASE,
    RAM_ORDER,
    STEP_WALK,
    VRAM_ORDER,
    ComponentPlan,
    PhasePlan,
    Placement,
    RamStep,
    ResidencyPlan,
    RolePolicy,
    StepFloor,
    Steps,
    Tier,
    VramStep,
    derive_transitions,
    more_economical_ram,
    more_economical_vram,
    template_for,
)

_RUN = ExecDevice.RUN_DEVICE


def _state(name: str, **tiers: tuple[Tier, int | None]) -> PhasePlan:
    return PhasePlan(name, MappingProxyType({component: ComponentPlan(component, tier, _RUN, resident, Tier.DROPPED) for component, (tier, resident) in tiers.items()}))


_CPU = ExecDevice.CPU
_D, _H, _X, _K = Tier.DEVICE, Tier.HOST, Tier.DROPPED, Tier.DISK


def _active(placements: tuple[Placement, ...]) -> list[tuple[Tier, ExecDevice, bool, int | None]]:
    return [(placement.tier, placement.exec_device, placement.juggle, placement.resident_cap) for placement in placements]


class StepOrderTests(unittest.TestCase):
    def test_orders_and_the_ram_major_walk(self) -> None:
        self.assertNotIn(VramStep.AUTO, VRAM_ORDER)
        self.assertNotIn(RamStep.AUTO, RAM_ORDER)
        self.assertEqual(len(STEP_WALK), 12)
        self.assertEqual(STEP_WALK[:5], (Steps(VramStep.MAX, RamStep.KEEP), Steps(VramStep.PHASE, RamStep.KEEP), Steps(VramStep.HALF, RamStep.KEEP), Steps(VramStep.MIN, RamStep.KEEP), Steps(VramStep.MAX, RamStep.RUN)))
        self.assertEqual(STEP_WALK[-1], Steps(VramStep.MIN, RamStep.MIN))
        self.assertFalse(AUTO_STEPS.concrete)
        self.assertTrue(STEP_WALK[0].concrete)
        self.assertEqual(STEP_WALK[0].label, "max/keep")

    def test_more_economical(self) -> None:
        self.assertIs(more_economical_vram(VramStep.PHASE, VramStep.HALF), VramStep.HALF)
        self.assertIs(more_economical_vram(VramStep.MIN, VramStep.MAX), VramStep.MIN)
        self.assertIs(more_economical_ram(RamStep.KEEP, RamStep.RUN), RamStep.RUN)

    def test_floor_is_never_auto(self) -> None:
        self.assertTrue(StepFloor().empty)
        self.assertFalse(StepFloor(vram=VramStep.HALF).empty)
        with self.assertRaises(ValueError):
            StepFloor(vram=VramStep.AUTO)
        with self.assertRaises(ValueError):
            StepFloor(ram=RamStep.AUTO)


class CompositionTests(unittest.TestCase):
    """The table of the `tiers` module header, cell by cell (12 pairs)."""

    def test_every_pair(self) -> None:
        on_device = (_D, ExecDevice.RUN_DEVICE, False, None)
        on_cpu = (_H, _CPU, False, None)
        for vram in VRAM_ORDER:
            for ram in RAM_ORDER:
                with self.subTest(vram=vram, ram=ram):
                    template = template_for(Steps(vram, ram))
                    self.assertEqual(set(template.roles), set(Role))
                    rest = _K if ram is RamStep.MIN else _H
                    park = _X if ram is RamStep.MIN else _H
                    dit = template.roles[Role.DIT]
                    encoder = template.roles[Role.ENCODER]
                    ae = template.roles[Role.AE]
                    expected_dit = {VramStep.MAX: [on_device], VramStep.PHASE: [on_device], VramStep.HALF: [(rest, ExecDevice.RUN_DEVICE, True, None)], VramStep.MIN: [(rest, ExecDevice.RUN_DEVICE, True, 0)]}[vram]
                    self.assertEqual(_active(dit.active), expected_dit)
                    if vram is VramStep.MIN:
                        expected_encoder = [on_cpu] if ram is not RamStep.MIN else [(_K, ExecDevice.RUN_DEVICE, True, 0)]
                    else:
                        expected_encoder = [on_device, (rest, ExecDevice.RUN_DEVICE, True, None)] + ([on_cpu] if ram is not RamStep.MIN else [])
                    self.assertEqual(_active(encoder.active), expected_encoder)
                    self.assertEqual(_active(ae.active), [on_device])
                    idle = _D if vram is VramStep.MAX else park
                    keep = _D if vram is VramStep.MAX else (_H if ram is RamStep.KEEP else _X)
                    for policy in (dit, encoder, ae):
                        self.assertEqual((policy.idle, policy.keep), ((idle,), (keep,)), "every step is strict: one idle and one keep tier")
                    self.assertEqual(template.device_share, 0.5 if vram is VramStep.HALF else None)

    def test_auto_has_no_template(self) -> None:
        for steps in (AUTO_STEPS, Steps(VramStep.MAX, RamStep.AUTO), Steps(VramStep.AUTO, RamStep.KEEP)):
            with self.assertRaises(ValueError):
                template_for(steps)


class ValidationTests(unittest.TestCase):
    def test_placements(self) -> None:
        Placement(Tier.DEVICE, _RUN)
        Placement(Tier.HOST, ExecDevice.CPU)
        Placement(Tier.DISK, _RUN, juggle=True)
        Placement(Tier.DISK, _RUN, juggle=True, resident_cap=0)
        for bad in ((Tier.HOST, _RUN, False), (Tier.DEVICE, _RUN, True), (Tier.HOST, ExecDevice.CPU, True), (Tier.DISK, _RUN, False)):
            with self.assertRaises(ValueError):
                Placement(*bad)
        with self.assertRaises(ValueError):
            Placement(Tier.DEVICE, _RUN, resident_cap=0)
        with self.assertRaises(ValueError):
            Placement(Tier.HOST, _RUN, juggle=True, resident_cap=-1)

    def test_role_policy_rejects_disk_idle(self) -> None:
        with self.assertRaises(ValueError):
            RolePolicy((Placement(Tier.DEVICE, _RUN),), (Tier.DISK,), (Tier.DROPPED,))
        with self.assertRaises(ValueError):
            RolePolicy((), (Tier.HOST,), (Tier.DROPPED,))

    def test_partial_residency_needs_host_or_disk_remainder(self) -> None:
        with self.assertRaises(ValueError):
            ComponentPlan("dit", Tier.DEVICE, _RUN, 3, Tier.DROPPED)
        with self.assertRaises(ValueError):
            ComponentPlan("dit", Tier.HOST, _RUN, -1, Tier.DROPPED)

    def test_plan_steps_are_never_auto(self) -> None:
        idle = _state(IDLE_PHASE, dit=(Tier.DROPPED, None))
        for steps in (AUTO_STEPS, Steps(VramStep.MAX, RamStep.AUTO)):
            with self.assertRaises(ValueError):
                ResidencyPlan(AUTO_STEPS, steps, "bf16", (), idle, ())
        plan = ResidencyPlan(AUTO_STEPS, Steps(VramStep.PHASE, RamStep.KEEP), "bf16", (_state("denoise", dit=(Tier.DEVICE, None)),), idle, ())
        other = ResidencyPlan(Steps(VramStep.HALF, RamStep.KEEP), Steps(VramStep.HALF, RamStep.KEEP), "bf16", plan.phases, idle, ())
        self.assertTrue(plan.same_placement(other), "steps and request are not part of the placement")
        self.assertIs(plan.phase("idle"), idle)
        self.assertEqual(plan.phase("denoise").phase, "denoise")
        with self.assertRaises(KeyError):
            plan.phase("decode")


class TransitionTests(unittest.TestCase):
    def test_outgoing_and_incoming_follow_device_residency(self) -> None:
        idle = _state(IDLE_PHASE, dit=(Tier.HOST, None), ae=(Tier.DEVICE, None))
        denoise = _state("denoise", dit=(Tier.HOST, 6), ae=(Tier.DEVICE, None))
        decode = _state("decode", dit=(Tier.DROPPED, None), ae=(Tier.DEVICE, None))
        first, second, third = derive_transitions((idle, denoise, decode, idle))
        self.assertEqual((first.name, first.incoming, first.outgoing), ("idle->denoise", ("dit",), ()))
        self.assertEqual((second.incoming, second.outgoing), ((), ("dit",)))
        self.assertEqual((third.incoming, third.outgoing), ((), ()), "HOST and DROPPED hold nothing on the device")


if __name__ == "__main__":
    unittest.main()
