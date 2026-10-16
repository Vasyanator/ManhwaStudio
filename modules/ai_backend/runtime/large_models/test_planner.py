"""
File: modules/ai_backend/runtime/large_models/test_planner.py

Purpose:
Unit tests for `planner.py` on the toy model with round-number tables: the
step grid (resident blocks solved monotonically in the device budget, HALF
within half of the usable card, MIN with `k = 0`), «Авто» on either axis and
the RAM-major joint walk, the learned floor per axis, refusals (device, host,
unknown budget, unsupported steps, unavailable format) with format, limits and
steps suggestions and never a format switch, reclaimable bytes ->
`PlanFit.NEEDS_RECLAIM`, bytes claimed by other pools, the between-runs state
per step, `plan_axis` rows and `same_as`, the per-handle device ceiling
(`DeviceCapTests`), and the uncalibrated rule judged per
phase (estimated device activations choose the plan but never refuse it;
measured ones, an unmeasured CPU phase, weights and the host side stay
strict), with the tracked gfx1201 table as the CPU-encoder regression; a host
run device (`cpu` / `mps`) planned on the host budget alone (`HostRunDeviceTests`).

`GridInvariantTests` is the property test of the per-axis monotonic
invariant: four toy shapes (DiT-dominant, encoder larger than the DiT, a big
monolithic AE, a front-heavy DiT that makes the room chain bind) x device totals x host sizes x two request shapes x calibrated
/ uncalibrated tables.

Toy sizes (GiB): dit 8 x 1 blocks + 0.0625; encoder 4 x 1; ae 0.25. Tables:
activations encode 0.25, denoise 1, decode 0.5; no overhead, no margin. The
device reserve is 0.5 GiB, the host reserve 2 GiB.
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from typing import Any

from ._test_fixtures import GIB, budgets, simple_tables, toy_files, toy_formats, toy_inventories, toy_spec
from .cost_tables import load_tables
from .planner import REASON_BUDGET_UNKNOWN, REASON_DEVICE_SHORT, REASON_FORMAT_UNAVAILABLE, REASON_HOST_SHORT, REASON_STEPS_UNSUPPORTED, FitKind, PlanRefusal, Planned, axis_rows, guarded_forecast, plan, plan_axis, plan_grid
from .spec import ExecDevice, FormatInfo, RequestShape
from .tiers import AUTO_STEPS, RAM_ORDER, VRAM_ORDER, Limits, RamStep, StepFloor, Steps, Tier, VramStep

_SHAPE = RequestShape(512, 512, prompt_tokens=0, steps=4)
_MAX, _PHASE, _HALF, _MIN = VramStep.MAX, VramStep.PHASE, VramStep.HALF, VramStep.MIN
_KEEP, _RUN, _RMIN = RamStep.KEEP, RamStep.RUN, RamStep.MIN


def _s(vram: VramStep, ram: RamStep) -> Steps:
    return Steps(vram, ram)


class PlannerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._dir = tempfile.TemporaryDirectory()
        root = Path(cls._dir.name)
        cls.files = {"big": toy_files(root, "big"), "small": toy_files(root, "small", dit_block_gib=0.5)}
        cls.spec = toy_spec(cls.files)
        # The encoder may not run on the CPU: the device-short refusals below
        # need a model without the CPU-encoder alternative.
        cls.device_spec = toy_spec(cls.files, encoder_exec=frozenset({ExecDevice.RUN_DEVICE}))
        cls.inventories = toy_inventories(cls.spec, cls.files)

    @classmethod
    def tearDownClass(cls) -> None:
        cls._dir.cleanup()

    def _plan(self, steps: Steps, *, vram_free: float = 32.0, fmt: str = "big", limits: Limits = Limits(), floor: StepFloor | None = None, formats=None, spec=None, **budget_args: float):  # type: ignore[no-untyped-def]
        return plan(spec or self.spec, self.inventories, format=fmt, formats=formats or toy_formats(), steps=steps, limits=limits, budgets=budgets(vram_free_gib=vram_free, **budget_args), shape=_SHAPE, tables=simple_tables(), floor=floor)

    def _planned(self, result: object) -> Planned:
        self.assertIsInstance(result, Planned, getattr(result, "reason", None))
        assert isinstance(result, Planned)
        return result

    def _refused(self, result: object) -> PlanRefusal:
        self.assertIsInstance(result, PlanRefusal, getattr(result, "plan", None) and result.plan.steps)  # type: ignore[union-attr]
        assert isinstance(result, PlanRefusal)
        return result

    @staticmethod
    def _dit_resident(planned: Planned) -> int:
        dit = planned.plan.phase("denoise").components["dit"]
        return 8 if dit.tier is Tier.DEVICE else int(dit.resident_blocks or 0)

    def test_resident_blocks_grow_monotonically_with_the_device_budget(self) -> None:
        seen: list[int] = []
        for half_gib in range(10, 26):
            planned = self._planned(self._plan(_s(_HALF, _KEEP), vram_free=half_gib / 2))
            self.assertLessEqual(planned.forecast.device_peak, int((half_gib / 2 - 0.5) * GIB))
            seen.append(self._dit_resident(planned))
        self.assertEqual(seen, sorted(seen))
        self.assertIn(8, seen)
        self.assertTrue(any(0 < value < 8 for value in seen), seen)
        # room 7.5 GiB: 0.0625 + k + 1 activation + 2 staging blocks <= 7.5 -> k = 4
        self.assertEqual(self._dit_resident(self._planned(self._plan(_s(_HALF, _KEEP), vram_free=8.0))), 4)

    def test_half_plans_on_at_most_half_of_the_usable_card(self) -> None:
        # A 16 GiB card: usable 15.5, half 7.75. PHASE holds the whole DiT
        # (9.06), HALF juggles it: 0.0625 + 4 + 1 + 2 staging = 7.06.
        phase = self._planned(self._plan(_s(_PHASE, _KEEP), vram_free=16.0, vram_total_gib=16.0))
        half = self._planned(self._plan(_s(_HALF, _KEEP), vram_free=16.0, vram_total_gib=16.0))
        self.assertGreater(phase.full_forecast.device_peak, int(7.75 * GIB))
        self.assertLessEqual(half.full_forecast.device_peak, int(7.75 * GIB))
        self.assertEqual(self._dit_resident(half), 4)
        self.assertIsNone(half.same_as_vram)
        # When the DiT fits in half the card, HALF is PHASE ("same as").
        roomy = self._planned(self._plan(_s(_HALF, _KEEP)))
        self.assertIs(roomy.same_as_vram, _PHASE)
        self.assertEqual(self._dit_resident(roomy), 8)

    def test_min_keeps_no_block_on_the_device(self) -> None:
        for ram in RAM_ORDER:
            with self.subTest(ram=ram):
                planned = self._planned(self._plan(_s(_MIN, ram)))
                dit = planned.plan.phase("denoise").components["dit"]
                self.assertEqual((dit.tier, dit.resident_blocks), (Tier.DISK if ram is _RMIN else Tier.HOST, 0))
        encoder = self._planned(self._plan(_s(_MIN, _RMIN))).plan.phase("encode").components["encoder"]
        self.assertEqual((encoder.tier, encoder.resident_blocks, encoder.exec_device), (Tier.DISK, 0, ExecDevice.RUN_DEVICE), "no RAM for a CPU encoder")
        self.assertIs(self._planned(self._plan(_s(_MIN, _KEEP))).plan.phase("encode").components["encoder"].exec_device, ExecDevice.CPU)

    def test_auto_on_both_axes_walks_ram_major(self) -> None:
        self.assertEqual(self._planned(self._plan(AUTO_STEPS)).plan.steps, _s(_MAX, _KEEP))
        # 13.3 GiB with the encoder on the device; at 10.5 GiB of room MAX still
        # fits with the CPU encoder (8.3 weights + 1 denoise).
        planned = self._planned(self._plan(AUTO_STEPS, vram_free=11.0))
        self.assertEqual(planned.plan.steps, _s(_MAX, _KEEP))
        self.assertEqual(planned.plan.steps_requested, AUTO_STEPS)
        self.assertIs(planned.fit.kind, FitKind.FREE)
        self.assertEqual(planned.plan.phase("encode").components["encoder"].exec_device, ExecDevice.CPU)
        # 3 GiB of host room: no KEEP / RUN cell fits; the walk gives up VRAM
        # first inside RAM MIN, where MAX (encoder on the device) is too big.
        short = self._planned(self._plan(AUTO_STEPS, vram_free=11.0, ram_available_gib=5.0))
        self.assertEqual(short.plan.steps, _s(_PHASE, _RMIN))

    def test_auto_on_one_axis(self) -> None:
        self.assertEqual(self._planned(self._plan(_s(VramStep.AUTO, _RMIN), vram_free=11.0)).plan.steps, _s(_PHASE, _RMIN))
        self.assertEqual(self._planned(self._plan(_s(_MIN, RamStep.AUTO), ram_available_gib=5.0)).plan.steps, _s(_MIN, _RMIN))
        self._refused(self._plan(_s(VramStep.AUTO, _KEEP), vram_free=11.0, ram_available_gib=5.0))
        refusal = self._refused(self._plan(_s(_MIN, _KEEP), vram_free=11.0, ram_available_gib=5.0))
        self.assertEqual((refusal.reason, refusal.shortest_resource), (REASON_HOST_SHORT, "host"))

    def test_the_encoder_prefers_the_device_then_juggles_then_the_cpu(self) -> None:
        roomy = self._planned(self._plan(_s(_MAX, _KEEP)))
        self.assertEqual(roomy.plan.phase("encode").components["encoder"].exec_device, ExecDevice.RUN_DEVICE)
        tight = self._planned(self._plan(_s(_MAX, _KEEP), vram_free=11.0))
        encoder = tight.plan.phase("encode").components["encoder"]
        self.assertEqual((encoder.tier, encoder.exec_device), (Tier.HOST, ExecDevice.CPU), "beside the whole DiT no encoder block fits")
        # HALF on 4 GiB: the DiT keeps nothing, the encoder juggles one layer.
        juggled = self._planned(self._plan(_s(_HALF, _KEEP), vram_free=4.0)).plan.phase("encode").components["encoder"]
        self.assertEqual((juggled.tier, juggled.resident_blocks, juggled.exec_device), (Tier.HOST, 1, ExecDevice.RUN_DEVICE))
        self._refused(self._plan(_s(_MAX, _KEEP), vram_free=11.0, spec=self.device_spec))

    def test_learned_floor_per_axis(self) -> None:
        self.assertEqual(self._planned(self._plan(AUTO_STEPS, floor=StepFloor(vram=_HALF))).plan.steps, _s(_HALF, _KEEP))
        self.assertEqual(self._planned(self._plan(AUTO_STEPS, floor=StepFloor(ram=_RUN))).plan.steps, _s(_MAX, _RUN))
        floored = self._planned(self._plan(_s(_PHASE, _KEEP), floor=StepFloor(vram=_HALF)))
        self.assertEqual(floored.plan.steps, _s(_HALF, _KEEP))
        self.assertEqual(floored.plan.steps_requested, _s(_PHASE, _KEEP))
        self.assertEqual(self._planned(self._plan(_s(_MIN, _KEEP), floor=StepFloor(vram=_PHASE, ram=_RUN))).plan.steps, _s(_MIN, _RUN))

    def test_refusal_suggests_formats_and_steps_but_never_switches(self) -> None:
        refusal = self._refused(self._plan(_s(_MAX, _KEEP), vram_free=11.0, spec=self.device_spec))
        self.assertEqual(refusal.reason, REASON_DEVICE_SHORT)
        self.assertEqual(refusal.shortest_resource, "device")
        self.assertEqual(refusal.best_steps_tried, _s(_MAX, _KEEP))
        self.assertIsNotNone(refusal.forecast)
        by_kind: dict[str, list[dict[str, str]]] = {}
        for suggestion in refusal.suggestions:
            by_kind.setdefault(suggestion.kind, []).append(dict(suggestion.payload))
        self.assertEqual(by_kind["format"], [{"format": "small", "vram": "max", "ram": "keep"}])
        # Lowering VRAM with RAM as requested fits; lowering RAM alone cannot.
        self.assertEqual(by_kind["steps"], [{"vram": "phase", "ram": "keep"}])

    def test_unavailable_format_is_refused_with_suggestions(self) -> None:
        refusal = self._refused(self._plan(AUTO_STEPS, fmt="small", formats=toy_formats(small=False)))
        self.assertEqual(refusal.reason, REASON_FORMAT_UNAVAILABLE)
        self.assertEqual([suggestion.payload["format"] for suggestion in refusal.suggestions], ["big"])

    def test_unknown_budget_refuses(self) -> None:
        refusal = self._refused(self._plan(AUTO_STEPS, vram_free=0.0, vram_total_gib=0.0))
        self.assertEqual((refusal.reason, refusal.shortest_resource), (REASON_BUDGET_UNKNOWN, "device"))
        refusal = self._refused(self._plan(AUTO_STEPS, ram_available_gib=0.0))
        self.assertEqual((refusal.reason, refusal.shortest_resource), (REASON_BUDGET_UNKNOWN, "host"))

    def test_short_host_streams_from_disk_never_parks(self) -> None:
        # 3 GiB of host room: MIN/KEEP would park the DiT's 8 GiB in RAM.
        self._refused(self._plan(_s(_MIN, _KEEP), vram_free=7.0, ram_available_gib=5.0))
        planned = self._planned(self._plan(_s(_MIN, RamStep.AUTO), vram_free=7.0, ram_available_gib=5.0))
        dit = planned.plan.phase("denoise").components["dit"]
        self.assertEqual((planned.plan.steps.ram, dit.tier), (_RMIN, Tier.DISK))
        refusal = self._refused(self._plan(_s(_MIN, _RMIN), vram_free=7.0, ram_available_gib=2.5))
        self.assertEqual((refusal.reason, refusal.shortest_resource), (REASON_HOST_SHORT, "host"))

    def test_reclaimable_bytes_make_a_plan_that_needs_a_reclaim(self) -> None:
        planned = self._planned(self._plan(_s(_MAX, _KEEP), vram_free=11.0, reclaim_device_gib=4.0, spec=self.device_spec))
        self.assertIs(planned.fit.kind, FitKind.NEEDS_RECLAIM)
        self.assertEqual(planned.fit.device_bytes, planned.forecast.device_peak - int(10.5 * GIB))
        self.assertEqual(planned.fit.host_bytes, 0)
        # With 1 GiB to reclaim the encoder no longer fits whole: it juggles.
        juggled = self._planned(self._plan(_s(_MAX, _KEEP), vram_free=11.0, reclaim_device_gib=1.0, spec=self.device_spec))
        self.assertIs(juggled.fit.kind, FitKind.NEEDS_RECLAIM)
        self.assertIsNotNone(juggled.plan.phase("encode").components["encoder"].resident_blocks)

    def test_bytes_claimed_by_other_pools_reduce_the_room(self) -> None:
        planned = self._planned(self._plan(AUTO_STEPS, claimed_device_gib=23.0))
        self.assertIn(planned.plan.steps.vram, (_HALF, _MIN))
        self.assertLessEqual(planned.forecast.device_peak, int(8.5 * GIB), "32 - 0.5 reserve - 23 claimed")

    def test_between_runs_state_follows_the_axes(self) -> None:
        kept = self._planned(self._plan(_s(_MAX, _RUN)))
        self.assertTrue(all(plan.tier is Tier.DEVICE for plan in kept.plan.idle.components.values()), "only MAX keeps the card")
        parked = self._planned(self._plan(_s(_PHASE, _KEEP)))
        self.assertTrue(all(plan.tier is Tier.HOST for plan in parked.plan.idle.components.values()))
        dropped = self._planned(self._plan(_s(_PHASE, _RUN)))
        self.assertTrue(all(plan.tier is Tier.DROPPED for plan in dropped.plan.idle.components.values()))
        self.assertEqual(dropped.plan.phase("encode").components["dit"].tier, Tier.DROPPED, "nothing kept means nothing there before first use")

    def test_steps_without_an_allowed_placement_are_unsupported(self) -> None:
        # MIN with RAM gives the encoder only the CPU, which this model forbids.
        refusal = self._refused(self._plan(_s(_MIN, _KEEP), spec=self.device_spec))
        self.assertEqual(refusal.reason, REASON_STEPS_UNSUPPORTED)

    def test_user_cap_refusal_suggests_raising_the_limits(self) -> None:
        refusal = self._refused(self._plan(_s(_MAX, _KEEP), limits=Limits(vram_cap=10 * GIB), spec=self.device_spec))
        limits = [dict(suggestion.payload) for suggestion in refusal.suggestions if suggestion.kind == "limits"]
        self.assertEqual(limits, [{"vram": "max", "ram": "keep"}])

    def test_plan_axis_rows_and_determinism(self) -> None:
        rows = plan_axis(self.spec, self.inventories, format="big", formats=toy_formats(), axis="vram", fixed=_KEEP, limits=Limits(), budgets=budgets(vram_free_gib=11.0), shape=_SHAPE, tables=simple_tables())
        self.assertEqual(list(rows), list(VRAM_ORDER))
        self.assertTrue(all(isinstance(result, Planned) for result in rows.values()))
        half = rows[_HALF]
        assert isinstance(half, Planned)
        self.assertIs(half.same_as_vram, _PHASE)
        ram_rows = plan_axis(self.spec, self.inventories, format="big", formats=toy_formats(), axis="ram", fixed=_MAX, limits=Limits(), budgets=budgets(vram_free_gib=11.0), shape=_SHAPE, tables=simple_tables())
        self.assertEqual(list(ram_rows), list(RAM_ORDER))
        run = ram_rows[_RUN]
        assert isinstance(run, Planned)
        self.assertIs(run.same_as_ram, _KEEP, "MAX keeps everything on the card whatever the RAM step")
        self.assertIsInstance(ram_rows[_RMIN], PlanRefusal, "no CPU encoder without RAM, and 13.3 GiB do not fit")
        with self.assertRaises(ValueError):
            plan_axis(self.spec, self.inventories, format="big", formats=toy_formats(), axis="vram", fixed=_MAX, limits=Limits(), budgets=budgets(vram_free_gib=11.0), shape=_SHAPE, tables=simple_tables())
        self.assertEqual(self._plan(_s(_HALF, _KEEP), vram_free=8.0), self._plan(_s(_HALF, _KEEP), vram_free=8.0))
        # A row is the same cell an explicit request resolves to.
        self.assertEqual(rows[_HALF], self._plan(_s(_HALF, _KEEP), vram_free=11.0))


class HostRunDeviceTests(unittest.TestCase):
    """A `cpu` / `mps` run device: planned on the host budget alone, the VRAM axis does not apply."""

    @classmethod
    def setUpClass(cls) -> None:
        cls._dir = tempfile.TemporaryDirectory()
        root = Path(cls._dir.name)
        cls.files = {"big": toy_files(root, "big")}
        cls.spec = toy_spec(cls.files)
        cls.inventories = toy_inventories(cls.spec, cls.files)

    @classmethod
    def tearDownClass(cls) -> None:
        cls._dir.cleanup()

    @staticmethod
    def _host_budgets(ram_available_gib: float) -> Any:
        from dataclasses import replace

        from ..memory_budget import DeviceBudget

        unread = DeviceBudget(device="cpu", total=0, free=0, torch_allocated=0, torch_reserved=0, non_torch=0, reserve=0, cap=None)
        return replace(budgets(vram_free_gib=0, ram_available_gib=ram_available_gib), device=unread, host_device=True)

    def _plan(self, steps: Steps, ram_available_gib: float, *, floor: StepFloor | None = None) -> Any:
        return plan(self.spec, self.inventories, format="big", formats=toy_formats(small=False), steps=steps, limits=Limits(), budgets=self._host_budgets(ram_available_gib), shape=_SHAPE, tables=simple_tables(overhead_gib=0.5, margin_fraction=0.05), floor=floor)

    def _assert_host_plan(self, result: Any, expected: Steps) -> Planned:
        self.assertIsInstance(result, Planned, getattr(result, "reason", None))
        assert isinstance(result, Planned)
        self.assertEqual(result.plan.steps, expected)
        self.assertEqual(result.full_forecast.device_peak, 0)
        for state in (*result.plan.phases, result.plan.idle):
            for component in state.components.values():
                self.assertIsNone(component.resident_blocks, f"{state.phase}/{component.component} juggles on the host")
        return result

    def test_unread_device_budget_is_not_budget_unknown(self) -> None:
        self._assert_host_plan(self._plan(AUTO_STEPS, 64), _s(_MAX, _KEEP))
        # The same zero device budget on a CUDA device still refuses.
        from dataclasses import replace

        cuda = replace(self._host_budgets(64), host_device=False)
        refusal = plan(self.spec, self.inventories, format="big", formats=toy_formats(small=False), steps=AUTO_STEPS, limits=Limits(), budgets=cuda, shape=_SHAPE, tables=simple_tables())
        self.assertIsInstance(refusal, PlanRefusal)
        assert isinstance(refusal, PlanRefusal)
        self.assertEqual((refusal.reason, refusal.shortest_resource), (REASON_BUDGET_UNKNOWN, "device"))

    def test_the_ram_step_picks_the_cell_and_the_vram_step_is_ignored(self) -> None:
        for vram in (_MAX, _PHASE, _HALF, _MIN, VramStep.AUTO):
            with self.subTest(vram=vram.value):
                self._assert_host_plan(self._plan(Steps(vram, _KEEP), 64), _s(_MAX, _KEEP))
                self._assert_host_plan(self._plan(Steps(vram, _RUN), 64), _s(_PHASE, _RUN))
                self._assert_host_plan(self._plan(Steps(vram, _RMIN), 64), _s(_PHASE, _RMIN))
        # A learned VRAM floor does not apply either; a RAM floor does.
        self._assert_host_plan(self._plan(AUTO_STEPS, 64, floor=StepFloor(_MIN, None)), _s(_MAX, _KEEP))
        self._assert_host_plan(self._plan(AUTO_STEPS, 64, floor=StepFloor(None, _RMIN)), _s(_PHASE, _RMIN))

    def test_the_host_carries_every_byte_and_stays_strict(self) -> None:
        weights = sum(self.inventories["big"][name].bytes_total for name in ("dit", "encoder", "ae"))
        keep = self._assert_host_plan(self._plan(AUTO_STEPS, 64), _s(_MAX, _KEEP))
        # Baseline 2 GiB + every weight + the denoise activation (1 GiB); no device overhead or margin.
        self.assertEqual(keep.full_forecast.host_peak, 2 * GIB + weights + GIB)
        # 10 GiB of room (12 available - 2 reserve): only the per-phase cell fits, and «Авто» walks to it.
        tight = self._assert_host_plan(self._plan(AUTO_STEPS, 12), _s(_PHASE, _RMIN))
        self.assertLessEqual(tight.full_forecast.host_peak - 2 * GIB, 10 * GIB)
        refusal = self._plan(Steps(_MAX, _KEEP), 12)
        self.assertIsInstance(refusal, PlanRefusal)
        assert isinstance(refusal, PlanRefusal)
        self.assertEqual((refusal.reason, refusal.shortest_resource), (REASON_HOST_SHORT, "host"))
        steps = [dict(suggestion.payload) for suggestion in refusal.suggestions if suggestion.kind == "steps"]
        self.assertEqual(steps, [{"vram": "phase", "ram": "min"}], "a RAM suggestion only: the VRAM axis does not apply")
        # Nothing fits a 4 GiB host: refused on the host, never planned past it.
        nothing = self._plan(AUTO_STEPS, 4)
        self.assertIsInstance(nothing, PlanRefusal)
        assert isinstance(nothing, PlanRefusal)
        self.assertEqual((nothing.reason, nothing.shortest_resource), (REASON_HOST_SHORT, "host"))

    def test_grid_rows_on_a_host_device(self) -> None:
        grid = plan_grid(self.spec, self.inventories, format="big", formats=toy_formats(small=False), limits=Limits(), budgets=self._host_budgets(64), shape=_SHAPE, tables=simple_tables())
        rows = axis_rows(grid, axis="ram", fixed=_HALF)
        self.assertEqual({step: result.plan.steps for step, result in rows.items() if isinstance(result, Planned)}, {_KEEP: _s(_MAX, _KEEP), _RUN: _s(_PHASE, _RUN), _RMIN: _s(_PHASE, _RMIN)})


class UncalibratedPlannerTests(unittest.TestCase):
    """Uncalibrated forecasts: estimated activations choose the plan, never refuse it; weights and the host stay strict.

    Toy MAX/KEEP with the encoder on the CPU: 8.3125 GiB of device weights,
    +1 GiB denoise activation. At 9.5 GiB free (9.0 of room) it fits on the
    weights alone, not with the activation.
    """

    @classmethod
    def setUpClass(cls) -> None:
        cls._dir = tempfile.TemporaryDirectory()
        cls.files = {"big": toy_files(Path(cls._dir.name), "big")}
        cls.spec = toy_spec(cls.files)
        cls.inventories = toy_inventories(cls.spec, cls.files)

    @classmethod
    def tearDownClass(cls) -> None:
        cls._dir.cleanup()

    def _plan(self, steps: Steps, *, calibrated: bool, vram_free: float = 9.5, tables: Any = None, **budget_args: float):  # type: ignore[no-untyped-def]
        return plan(self.spec, self.inventories, format="big", formats=toy_formats(small=False), steps=steps, limits=Limits(), budgets=budgets(vram_free_gib=vram_free, **budget_args), shape=_SHAPE, tables=tables or simple_tables(denoise_measured=calibrated))

    def test_calibrated_activation_refuses(self) -> None:
        result = self._plan(_s(_MAX, _KEEP), calibrated=True)
        self.assertIsInstance(result, PlanRefusal)
        assert isinstance(result, PlanRefusal)
        self.assertEqual(result.reason, REASON_DEVICE_SHORT)

    def test_uncalibrated_activation_admits_on_the_weights(self) -> None:
        result = self._plan(_s(_MAX, _KEEP), calibrated=False)
        self.assertIsInstance(result, Planned)
        assert isinstance(result, Planned)
        self.assertTrue(result.relaxed)
        forecast = result.forecast
        self.assertFalse(forecast.calibrated)
        weights = 8 * GIB + 64 * 1024**2 + GIB // 4
        # Only the unmeasured denoise loses its activation: the measured decode
        # keeps its 0.5 GiB and is now the guarded peak.
        self.assertFalse(forecast.device_calibrated)
        self.assertEqual(forecast.phases["denoise"].device_peak, weights, "the unmeasured denoise counts no activation")
        self.assertEqual(forecast.device_peak, weights + GIB // 2, "the measured decode activation stays binding")
        self.assertEqual(forecast.phases["denoise"].device_activation, GIB, "the estimate is still reported")
        self.assertEqual(result.full_forecast.device_peak, weights + GIB, "the full forecast counts it")
        self.assertIs(result.fit.kind, FitKind.FREE)

    def test_activations_still_choose_the_plan(self) -> None:
        # Auto takes the first STRICT fit of the walk: the juggling HALF fits
        # with its activation, so it wins over admitting MAX on its weights.
        result = self._plan(AUTO_STEPS, calibrated=False)
        self.assertIsInstance(result, Planned)
        assert isinstance(result, Planned)
        self.assertEqual(result.plan.steps, _s(_HALF, _KEEP))
        self.assertFalse(result.relaxed)

    def test_weights_that_do_not_fit_still_refuse(self) -> None:
        result = self._plan(_s(_MAX, _KEEP), calibrated=False, vram_free=8.0)
        self.assertIsInstance(result, PlanRefusal)

    def test_an_unmeasured_cpu_phase_relaxes_nothing_on_the_device(self) -> None:
        # The CPU encoder's entry is unmeasured, the measured denoise
        # activation still refuses MAX at 9 GiB of room.
        result = self._plan(_s(_MAX, _KEEP), calibrated=True, tables=simple_tables(encode_cpu_measured=False))
        self.assertIsInstance(result, PlanRefusal)
        assert isinstance(result, PlanRefusal)
        self.assertEqual(result.reason, REASON_DEVICE_SHORT)

    def test_the_host_side_stays_strict(self) -> None:
        # PHASE/KEEP parks the 4 GiB encoder and the DiT in RAM; 3 GiB of host
        # room refuses it even though the forecast is uncalibrated.
        short = self._plan(_s(_PHASE, _KEEP), calibrated=False, vram_free=32.0, ram_available_gib=5.0)
        self.assertIsInstance(short, PlanRefusal)
        assert isinstance(short, PlanRefusal)
        self.assertEqual(short.shortest_resource, "host")


class DeviceCapTests(unittest.TestCase):
    """`Budgets.device_cap_bytes`: the per-handle ceiling bounds the whole device room, reclaim included."""

    def test_cap_bounds_free_room_and_reclaim(self) -> None:
        from dataclasses import replace

        from .planner import _Request

        with tempfile.TemporaryDirectory() as root:
            files = {"bf16": toy_files(Path(root), "bf16")}
            spec = toy_spec(files)
            base = budgets(vram_free_gib=20.0, reclaim_device_gib=6.0)
            open_request = _Request(spec, Limits(), base, RequestShape(1024, 1024), simple_tables(), None)
            self.assertEqual(open_request.device_room, open_request.device_free + 6 * GIB)
            capped = _Request(spec, Limits(vram_cap=int(30 * GIB)), replace(base, device_cap_bytes=10 * GIB), RequestShape(1024, 1024), simple_tables(), None)
            self.assertEqual((capped.device_free, capped.device_room), (10 * GIB, 10 * GIB))
            self.assertEqual(capped.device_usable, open_request.device_usable - 2 * GIB, "HALF's share follows the user's cap, not the handle's")
            loose = _Request(spec, Limits(), replace(base, device_cap_bytes=64 * GIB), RequestShape(1024, 1024), simple_tables(), None)
            self.assertEqual((loose.device_free, loose.device_room), (open_request.device_free, open_request.device_room), "a cap above the room changes nothing")


class ReferenceTableCalibrationTests(unittest.TestCase):
    """Item-1 regression on the tracked gfx1201 table: a CPU encoder keeps the measured denoise / decode
    activations in the guard, whatever the CPU entry's own calibration (it puts nothing on the device)."""

    def test_cpu_encoder_keeps_the_measured_device_activations(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            files = {"bf16": toy_files(Path(root), "bf16")}
            spec = toy_spec(files, encoder_exec=frozenset({ExecDevice.CPU}), family="flux3_klein")
            inventories = toy_inventories(spec, files)
            formats = {"bf16": FormatInfo(True, None, 8 * GIB, "native", 2)}
            result = plan(spec, inventories, format="bf16", formats=formats, steps=_s(_MAX, _KEEP), limits=Limits(), budgets=budgets(vram_free_gib=32.0), shape=RequestShape(1024, 1024, prompt_tokens=512, steps=4), tables=load_tables("rocm_gfx1201"))
        self.assertIsInstance(result, Planned, result)
        assert isinstance(result, Planned)
        forecast = result.forecast
        self.assertIs(result.plan.phase("encode").components["encoder"].exec_device, ExecDevice.CPU)
        self.assertEqual(forecast.calibrated, forecast.phases["encode"].calibrated, "the UI flag follows the only CPU phase")
        self.assertTrue(forecast.device_calibrated)
        self.assertEqual(forecast.phases["encode"].device_activation, 0)
        self.assertIs(guarded_forecast(forecast), forecast, "nothing is relaxed")
        self.assertIs(result.full_forecast, forecast)
        for name in ("denoise", "decode"):
            phase = forecast.phases[name]
            self.assertTrue(phase.calibrated, name)
            self.assertGreater(phase.device_activation, 0, name)
            self.assertGreaterEqual(phase.device_peak, phase.device_weights + phase.device_activation, name)


class GridInvariantTests(unittest.TestCase):
    """The per-axis monotonic invariant over toy models and budgets (module notes of `planner`).

    For every RAM step the full device peaks of the fitting VRAM steps never
    increase; for every VRAM step the host growth of the fitting RAM steps
    never increases; `same_as_*` appears exactly when a row repeats the
    nearest fitting higher row's placement; HALF stays within half of the
    usable card and a MIN row keeps no DiT block on the device unless it
    repeats a higher row.
    """

    DEVICE_TOTALS = (4, 8, 12, 16, 24, 32, 48)
    HOST_SIZES = (8, 16, 32, 64, 128)
    SHAPES = (RequestShape(512, 512, prompt_tokens=0, steps=4), RequestShape(1024, 1024, prompt_tokens=256, steps=8))

    @classmethod
    def setUpClass(cls) -> None:
        cls._dir = tempfile.TemporaryDirectory()
        root = Path(cls._dir.name)
        cls.models: dict[str, Any] = {}
        # `front_heavy_dit`: the first DiT block is the largest, so keeping it
        # resident lowers the staging; MIN (k = 0) would then need more than
        # HALF and must repeat HALF's plan (the room chain binds).
        models = {
            "dit_dominant": {},
            "encoder_larger": {"dit_block_gib": 0.25, "encoder_gib": 8.0},
            "big_monolithic_ae": {"dit_block_gib": 0.5, "ae_gib": 3.0},
            "front_heavy_dit": {"dit_block_gibs": (4.0, 0.5, 0.5, 0.5, 0.5, 0.5)},
        }
        for name, sizes in models.items():
            files = {"big": toy_files(root / name, "big", **sizes)}
            spec = toy_spec(files)
            cls.models[name] = (spec, toy_inventories(spec, files))

    @classmethod
    def tearDownClass(cls) -> None:
        cls._dir.cleanup()

    def _grid(self, spec: Any, inventories: Any, shape: RequestShape, tables: Any, device_gib: int, host_gib: int) -> dict[Steps, Any]:
        return plan_grid(spec, inventories, format="big", formats=toy_formats(small=False), limits=Limits(), budgets=budgets(vram_free_gib=device_gib, vram_total_gib=device_gib, ram_available_gib=host_gib), shape=shape, tables=tables)

    def _check_axis(self, rows: dict[Any, Any], *, measure: Any, same_of: Any) -> int:
        fitting = [(step, result) for step, result in rows.items() if isinstance(result, Planned)]
        for (_, higher), (step, lower) in zip(fitting, fitting[1:]):
            self.assertLessEqual(measure(lower), measure(higher), step)
        for index, (step, result) in enumerate(fitting):
            previous = fitting[index - 1][1] if index > 0 else None
            equal = previous is not None and result.plan.same_placement(previous.plan)
            self.assertEqual(same_of(result) is not None, equal, step)
        return len(fitting)

    def test_invariant_grid(self) -> None:
        tables_variants = {"calibrated": simple_tables(), "uncalibrated": simple_tables(denoise_measured=False)}
        cells = 0
        repeats = 0
        for model, (spec, inventories) in self.models.items():
            for table_name, tables in tables_variants.items():
                for shape in self.SHAPES:
                    for device_gib in self.DEVICE_TOTALS:
                        for host_gib in self.HOST_SIZES:
                            with self.subTest(model=model, tables=table_name, shape=(shape.width, shape.steps), device=device_gib, host=host_gib):
                                usable = (device_gib - 0.5) * GIB
                                grid = self._grid(spec, inventories, shape, tables, device_gib, host_gib)
                                for ram in RAM_ORDER:
                                    rows = axis_rows(grid, axis="vram", fixed=ram)
                                    cells += self._check_axis(rows, measure=lambda result: result.full_forecast.device_peak, same_of=lambda result: result.same_as_vram)
                                    half = rows[_HALF]
                                    if isinstance(half, Planned):
                                        self.assertLessEqual(half.forecast.device_peak, int(usable * 0.5))
                                    minimum = rows[_MIN]
                                    if isinstance(minimum, Planned):
                                        if minimum.same_as_vram is None:
                                            self.assertEqual(minimum.plan.phase("denoise").components["dit"].resident_blocks, 0)
                                        else:
                                            repeats += 1
                                for vram in VRAM_ORDER:
                                    rows = axis_rows(grid, axis="ram", fixed=vram)
                                    self._check_axis(rows, measure=lambda result: max(0, result.full_forecast.host_peak - 2 * GIB), same_of=lambda result: result.same_as_ram)
        self.assertGreater(cells, 500, "the grid must exercise fitting cells")
        self.assertGreater(repeats, 0, "the room chain must bind somewhere (a MIN row repeating a higher one)")

    def test_the_room_chain_makes_a_lower_step_repeat_a_higher_one(self) -> None:
        # DiT blocks 4 + 8 x 0.25 GiB on a 16.9 GiB card (half = 8.2): HALF
        # keeps the big block (transition 4.06 + 4 in flight = 8.06), while MIN's
        # own k = 0 plan stages two 4 GiB copies (0.06 + 8 + 1 = 9.06), above
        # HALF. MIN therefore repeats HALF's plan.
        with tempfile.TemporaryDirectory() as root:
            files = {"big": toy_files(Path(root), "big", dit_block_gibs=(4.0,) + (0.25,) * 8)}
            spec = toy_spec(files)
            rows = plan_axis(spec, toy_inventories(spec, files), format="big", formats=toy_formats(small=False), axis="vram", fixed=_KEEP, limits=Limits(), budgets=budgets(vram_free_gib=16.9, vram_total_gib=16.9), shape=self.SHAPES[0], tables=simple_tables())
        half, minimum = rows[_HALF], rows[_MIN]
        assert isinstance(half, Planned) and isinstance(minimum, Planned)
        self.assertEqual(half.plan.phase("denoise").components["dit"].resident_blocks, 1)
        self.assertIs(minimum.same_as_vram, _HALF)
        self.assertTrue(minimum.plan.same_placement(half.plan))
        self.assertEqual(minimum.plan.steps, _s(_MIN, _KEEP))
        self.assertEqual(minimum.full_forecast.device_peak, half.full_forecast.device_peak)

    def test_an_explicit_request_resolves_to_its_grid_cell(self) -> None:
        spec, inventories = self.models["dit_dominant"]
        for device_gib in (8, 12):
            grid = self._grid(spec, inventories, self.SHAPES[0], simple_tables(), device_gib, 16)
            for cell, result in grid.items():
                with self.subTest(device=device_gib, cell=cell.label):
                    alone = plan(spec, inventories, format="big", formats=toy_formats(small=False), steps=cell, limits=Limits(), budgets=budgets(vram_free_gib=device_gib, vram_total_gib=device_gib, ram_available_gib=16), shape=self.SHAPES[0], tables=simple_tables())
                    self.assertEqual(type(alone), type(result))
                    if isinstance(result, Planned):
                        self.assertEqual(alone, result)


if __name__ == "__main__":
    unittest.main()
