"""
File: modules/ai_backend/runtime/large_models/test_degrade.py

Purpose:
Contract tests of `degrade.py`: which exceptions are memory failures and how
big their deficit is, the rung order of the ladder (device failures: tighter
device room -> fallbacks -> more economical VRAM steps, the RAM step kept;
host failures: more economical RAM steps only, the VRAM step kept), the retry
cap, and the learned floors per axis (each axis only lowered, merged
independently, reset, a failing store write kept in memory).

Notes:
Pure: the plans come from the real planner over the header-only toy spec of
`_test_fixtures`; the ladder's re-plan callback is a fake that records calls.
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from typing import Any

from ..memory_budget import HostMemoryShort
from . import degrade as degrade_module
from ._test_fixtures import GIB, budgets, simple_tables, toy_files, toy_formats, toy_inventories, toy_spec
from .degrade import DEFAULT_OOM_DEFICIT_BYTES, FailureKind, FloorEntry, FloorKey, Ladder, LearnedFloors, Replan, RungKind, RunFailure, classify_failure, oom_deficit_bytes
from .moves import DeviceShort
from .planner import Planned, PlanRefusal, plan
from .spec import Fallback, LargeModelSpec, RequestShape
from .tiers import Limits, RamStep, StepFloor, Steps, VramStep

SHAPE = RequestShape(width=512, height=512)

_MAX, _PHASE, _HALF, _MIN = VramStep.MAX, VramStep.PHASE, VramStep.HALF, VramStep.MIN
_KEEP, _RUN, _RMIN = RamStep.KEEP, RamStep.RUN, RamStep.MIN


class _Plans:
    """Real plans of the toy model from the real planner (explicit step pairs)."""

    def __init__(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        directory = Path(self._tmp.name)
        files = {"big": toy_files(directory / "big", "big")}
        self.base_spec = toy_spec(files)
        self.inventories = toy_inventories(self.base_spec, files)
        self.formats = {"big": toy_formats()["big"]}
        self.tables = simple_tables()

    def close(self) -> None:
        self._tmp.cleanup()

    def plan(self, steps: Steps, *, vram_free_gib: float = 30.0, spec: LargeModelSpec | None = None) -> Any:
        return plan(spec or self.base_spec, self.inventories, format="big", formats=self.formats, steps=steps, limits=Limits(), budgets=budgets(vram_free_gib=vram_free_gib), shape=SHAPE, tables=self.tables)



class ClassifyTests(unittest.TestCase):
    def test_memory_failures_are_classified(self) -> None:
        short = classify_failure(DeviceShort(10 * GIB, 8 * GIB), step="a->b")
        assert short is not None
        self.assertIs(short.kind, FailureKind.DEVICE_SHORT)
        self.assertEqual(short.deficit_bytes, 2 * GIB)
        host = classify_failure(HostMemoryShort(5 * GIB, 1 * GIB), step="load:dit")
        assert host is not None
        self.assertIs(host.kind, FailureKind.HOST_SHORT)
        self.assertEqual(host.deficit_bytes, 4 * GIB)
        self.assertFalse(host.kind.device_side)
        hip = classify_failure(RuntimeError("HIP out of memory. Tried to allocate 2.00 GiB"), step="denoise", phase="denoise")
        assert hip is not None
        self.assertIs(hip.kind, FailureKind.DEVICE_OOM)
        self.assertEqual(hip.deficit_bytes, 2 * GIB)
        self.assertEqual(hip.phase, "denoise")

    def test_other_errors_are_not_memory_failures(self) -> None:
        self.assertIsNone(classify_failure(ValueError("out of memory"), step="x"))
        self.assertIsNone(classify_failure(RuntimeError("shape mismatch"), step="x"))
        self.assertIsNone(classify_failure(KeyboardInterrupt(), step="x"))

    def test_torch_oom_type_is_recognized(self) -> None:
        try:
            import torch
        except ImportError:
            self.skipTest("torch is not installed")
        failure = classify_failure(torch.OutOfMemoryError("CUDA out of memory. Tried to allocate 512.00 MiB"), step="decode")
        assert failure is not None
        self.assertIs(failure.kind, FailureKind.DEVICE_OOM)
        self.assertEqual(failure.deficit_bytes, 512 * 1024**2)

    def test_deficit_parsing(self) -> None:
        self.assertEqual(oom_deficit_bytes("Tried to allocate 20.00 MiB (GPU 0)"), 20 * 1024**2)
        self.assertEqual(oom_deficit_bytes("tried to allocate 3 KiB"), 3 * 1024)
        self.assertEqual(oom_deficit_bytes("HIP out of memory"), DEFAULT_OOM_DEFICIT_BYTES)


class _Recorder:
    """A fake re-plan callback: answers from a table and records every request."""

    def __init__(self, answers: dict[tuple[Steps, bool, int], Any], default: Any) -> None:
        self.answers = answers
        self.default = default
        self.calls: list[Replan] = []

    def __call__(self, request: Replan) -> Any:
        self.calls.append(request)
        tiled = bool(isinstance(request.options, dict) and request.options.get("tiled"))
        return self.answers.get((request.steps, tiled, request.extra_device_bytes), self.default)


def _failure(kind: FailureKind = FailureKind.DEVICE_OOM, *, phase: str | None = "denoise", deficit: int = GIB) -> RunFailure:
    return RunFailure(kind, phase or "load:dit", phase, deficit, "test")


class LadderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.plans = _Plans()
        self.addCleanup(self.plans.close)

    def _planned(self, steps: Steps, **kwargs: Any) -> Planned:
        result = self.plans.plan(steps, **kwargs)
        self.assertIsInstance(result, Planned)
        assert isinstance(result, Planned)
        return result

    def test_device_failure_walks_only_the_vram_axis(self) -> None:
        start = self._planned(Steps(_MAX, _RUN))
        phase = self._planned(Steps(_PHASE, _RUN))
        refusal = PlanRefusal("device_short", "device", Steps(_MAX, _RUN))
        recorder = _Recorder({(Steps(_PHASE, _RUN), False, GIB): phase}, refusal)
        ladder = Ladder(self.plans.base_spec, start=start, options={}, replan=recorder)
        self.assertTrue(ladder.take_retry(_failure()))
        # The denoise peak of PHASE is lower: the encoder is parked by then.
        self.assertLess(phase.forecast.phases["denoise"].device_peak, start.forecast.phases["denoise"].device_peak)
        rung = ladder.degrade(_failure())
        assert rung is not None
        self.assertIs(rung.kind, RungKind.NEXT_VRAM_STEP)
        self.assertEqual(ladder.plan.steps, Steps(_PHASE, _RUN))
        self.assertTrue(ladder.degraded and ladder.vram_lowered)
        self.assertFalse(ladder.ram_lowered)
        self.assertEqual(ladder.reason, "device_oom")
        # The next step is planned with the observed deficit out of the room.
        self.assertEqual(ladder.extra_device_bytes, GIB)
        # No tighter re-plan for a plan that juggles nothing, no fallback
        # declared, and never another RAM step.
        self.assertEqual([call.steps for call in recorder.calls], [Steps(_PHASE, _RUN)])

    def test_device_failure_skips_steps_that_do_not_lower_the_failing_step(self) -> None:
        start = self._planned(Steps(_MAX, _KEEP))
        half = self._planned(Steps(_HALF, _KEEP), vram_free_gib=8.0)
        # The PHASE re-plan answers a plan that needs as much at denoise as the
        # failed one: it cannot help and is skipped.
        recorder = _Recorder({(Steps(_PHASE, _KEEP), False, GIB): start, (Steps(_HALF, _KEEP), False, GIB): half}, PlanRefusal("device_short", "device", None))
        ladder = Ladder(self.plans.base_spec, start=start, options={}, replan=recorder)
        rung = ladder.degrade(_failure())
        assert rung is not None
        self.assertEqual(ladder.plan.steps, Steps(_HALF, _KEEP))
        self.assertEqual([call.steps for call in recorder.calls], [Steps(_PHASE, _KEEP), Steps(_HALF, _KEEP)])

    def test_device_failure_never_changes_the_ram_step(self) -> None:
        start = self._planned(Steps(_HALF, _KEEP), vram_free_gib=8.0)
        lower = self._planned(Steps(_MIN, _RMIN))
        refusal = PlanRefusal("device_short", "device", None)
        # Every VRAM step below refuses; any other pair would lower the peak.
        recorder = _Recorder({(Steps(_HALF, _KEEP), False, GIB): refusal, (Steps(_MIN, _KEEP), False, GIB): refusal}, lower)
        ladder = Ladder(self.plans.base_spec, start=start, options={}, replan=recorder)
        self.assertIsNone(ladder.degrade(_failure()))
        self.assertEqual([call.steps for call in recorder.calls], [Steps(_HALF, _KEEP), Steps(_MIN, _KEEP)])
        self.assertIs(ladder.current, start)

    def test_juggling_plan_first_tightens_the_device_room(self) -> None:
        start = self._planned(Steps(_HALF, _KEEP), vram_free_gib=6.0)
        tighter = self._planned(Steps(_HALF, _KEEP), vram_free_gib=5.0)
        self.assertLess(tighter.forecast.device_peak, start.forecast.device_peak)
        recorder = _Recorder({(Steps(_HALF, _KEEP), False, GIB): tighter}, PlanRefusal("device_short", "device", None))
        ladder = Ladder(self.plans.base_spec, start=start, options={}, replan=recorder)
        rung = ladder.degrade(_failure(deficit=GIB))
        assert rung is not None
        self.assertIs(rung.kind, RungKind.TIGHTER_DEVICE)
        self.assertEqual(ladder.extra_device_bytes, GIB)
        self.assertEqual(ladder.plan.steps, Steps(_HALF, _KEEP))
        self.assertFalse(ladder.vram_lowered or ladder.ram_lowered)

    def test_fallback_before_next_vram_step_and_only_once(self) -> None:
        spec = LargeModelSpec(
            family=self.plans.base_spec.family,
            components=self.plans.base_spec.components,
            phases=self.plans.base_spec.phases,
            fallbacks=(Fallback("tiled", frozenset({"decode"}), lambda options: {**options, "tiled": True}),),
        )
        start = self._planned(Steps(_MAX, _KEEP))
        phase = self._planned(Steps(_PHASE, _KEEP))
        recorder = _Recorder({(Steps(_MAX, _KEEP), True, 0): start, (Steps(_PHASE, _KEEP), True, GIB): phase, (Steps(_PHASE, _KEEP), False, GIB): phase, (Steps(_PHASE, _KEEP), True, 2 * GIB): phase}, PlanRefusal("device_short", "device", None))
        ladder = Ladder(spec, start=start, options={"tiled": False}, replan=recorder)
        first = ladder.degrade(_failure(phase="decode"))
        assert first is not None
        self.assertIs(first.kind, RungKind.FALLBACK)
        self.assertEqual(ladder.options, {"tiled": True})
        self.assertEqual(ladder.applied_fallbacks, ["tiled"])
        second = ladder.degrade(_failure(phase="decode"))
        assert second is not None
        self.assertIs(second.kind, RungKind.NEXT_VRAM_STEP)
        # PHASE lowers the decode peak (DiT and encoder parked by then).
        self.assertLess(phase.forecast.phases["decode"].device_peak, start.forecast.phases["decode"].device_peak)
        self.assertEqual(ladder.plan.steps, Steps(_PHASE, _KEEP))
        # A failure in another phase never applies the decode fallback.
        other = Ladder(spec, start=start, options={"tiled": False}, replan=recorder)
        rung = other.degrade(_failure(phase="denoise"))
        assert rung is not None
        self.assertIs(rung.kind, RungKind.NEXT_VRAM_STEP)

    def test_host_failure_walks_only_the_ram_axis(self) -> None:
        start = self._planned(Steps(_PHASE, _KEEP))
        run = self._planned(Steps(_PHASE, _RUN))
        tight = self._planned(Steps(_HALF, _KEEP), vram_free_gib=5.0)
        self.assertLess(run.forecast.host_peak, start.forecast.host_peak)
        recorder = _Recorder({(Steps(_PHASE, _RUN), False, 0): run, (Steps(_HALF, _KEEP), False, GIB): tight}, PlanRefusal("host_short", "host", None))
        ladder = Ladder(self.plans.base_spec, start=start, options={}, replan=recorder)
        rung = ladder.degrade(_failure(FailureKind.HOST_SHORT, phase=None))
        assert rung is not None
        self.assertIs(rung.kind, RungKind.NEXT_RAM_STEP)
        self.assertEqual(ladder.plan.steps, Steps(_PHASE, _RUN))
        self.assertTrue(ladder.ram_lowered)
        self.assertFalse(ladder.vram_lowered)
        self.assertEqual(ladder.extra_device_bytes, 0, "a host failure carries no device deficit")
        self.assertEqual([call.steps for call in recorder.calls], [Steps(_PHASE, _RUN)])

    def test_no_gentler_plan_leaves_the_plan_unchanged(self) -> None:
        start = self._planned(Steps(_MIN, _RMIN))
        ladder = Ladder(self.plans.base_spec, start=start, options={}, replan=_Recorder({}, PlanRefusal("device_short", "device", None)))
        self.assertIsNone(ladder.degrade(_failure()))
        self.assertIsNone(ladder.degrade(_failure(FailureKind.HOST_SHORT, phase=None)))
        self.assertIs(ladder.current, start)
        self.assertFalse(ladder.degraded)

    def test_retry_cap(self) -> None:
        start = self._planned(Steps(_MAX, _KEEP))
        ladder = Ladder(self.plans.base_spec, start=start, options={}, replan=_Recorder({}, PlanRefusal("x", None, None)), max_retries=3)
        self.assertEqual([ladder.take_retry(_failure()) for _ in range(4)], [True, True, True, False])
        self.assertEqual(ladder.retries, 3)

    def test_reclaim_rung_is_not_a_degrade(self) -> None:
        start = self._planned(Steps(_MAX, _KEEP))
        ladder = Ladder(self.plans.base_spec, start=start, options={}, replan=_Recorder({}, PlanRefusal("x", None, None)))
        ladder.note_reclaim(_failure(), GIB)
        self.assertFalse(ladder.degraded)
        self.assertIsNone(ladder.reason)
        self.assertIs(ladder.history[0].kind, RungKind.RECLAIM)


class _ListStore:
    def __init__(self, entries: list[FloorEntry] | None = None, *, fail: bool = False) -> None:
        self.entries = list(entries or [])
        self.fail = fail
        self.writes = 0

    def read_floors(self) -> list[FloorEntry]:
        return list(self.entries)

    def write_floors(self, entries: Any) -> None:
        self.writes += 1
        if self.fail:
            raise OSError("read-only")
        self.entries = list(entries)


class LearnedFloorsTests(unittest.TestCase):
    KEY = FloorKey("rocm_gfx1201", "flux3", "bf16")

    def test_each_axis_only_lowers_and_persists(self) -> None:
        store = _ListStore()
        floors = LearnedFloors(store)
        self.assertIsNone(floors.get(self.KEY))
        self.assertTrue(floors.record(FloorEntry(self.KEY, _HALF, None, "device_oom", "denoise")))
        self.assertFalse(floors.record(FloorEntry(self.KEY, _PHASE, None, "device_oom", "denoise")), "never raised")
        # The RAM axis is learned on its own; the VRAM floor is kept.
        self.assertTrue(floors.record(FloorEntry(self.KEY, None, _RUN, "host_short", "load:dit")))
        self.assertEqual(floors.get(self.KEY), StepFloor(_HALF, _RUN))
        self.assertTrue(floors.record(FloorEntry(self.KEY, _MIN, _KEEP, "device_oom", "decode")))
        self.assertEqual(floors.get(self.KEY), StepFloor(_MIN, _RUN), "each axis merged independently")
        self.assertEqual(store.writes, 3)
        self.assertEqual(LearnedFloors(store).get(self.KEY), StepFloor(_MIN, _RUN))

    def test_reset_by_model(self) -> None:
        other = FloorKey("rocm_gfx1201", "flux2", "bf16")
        store = _ListStore([FloorEntry(self.KEY, _PHASE, None, "r", "s"), FloorEntry(other, None, _RMIN, "r", "s")])
        floors = LearnedFloors(store)
        self.assertEqual(floors.reset("flux3"), 1)
        self.assertEqual([entry.key for entry in floors.entries()], [other])
        self.assertEqual(floors.reset(), 1)
        self.assertEqual(store.entries, [])
        self.assertEqual(floors.reset(), 0)

    def test_failing_store_keeps_the_floor_in_memory(self) -> None:
        floors = LearnedFloors(_ListStore(fail=True))
        with self.assertLogs(degrade_module.log, level="WARNING"):
            self.assertTrue(floors.record(FloorEntry(self.KEY, _PHASE, None, "r", "s")))
        self.assertEqual(floors.get(self.KEY), StepFloor(vram=_PHASE))

    def test_auto_or_empty_is_never_a_floor(self) -> None:
        with self.assertRaises(ValueError):
            FloorEntry(self.KEY, VramStep.AUTO, None, "r", "s")
        with self.assertRaises(ValueError):
            FloorEntry(self.KEY, None, RamStep.AUTO, "r", "s")
        with self.assertRaises(ValueError):
            FloorEntry(self.KEY, None, None, "r", "s")


if __name__ == "__main__":
    unittest.main()
