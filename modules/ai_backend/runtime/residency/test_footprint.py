"""
File: modules/ai_backend/runtime/residency/test_footprint.py

Purpose:
Pure tests of `footprint.py` and of the slack-credit rule in `holdings.py`:
file hints, device naming, the estimate order, the load-window rule (max with
the hint, DEFAULT replaced, ONNX bound filter), the release-window rule (only
raises, capped) and `usable_device_headroom`.

Notes:
Scratch files live in a `tempfile.TemporaryDirectory` removed by the test.
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from modules.ai_backend.runtime.memory_budget import GIB, MIB, DeviceBudget
from modules.ai_backend.runtime.residency import footprint as fp
from modules.ai_backend.runtime.residency.holdings import Footprint, FootprintHint, Runtime, Source, usable_device_headroom


def _sample(host: int = 0, free: int | None = None, reserved: int | None = None) -> fp.Sample:
    return fp.Sample(host_rss_anon=host, device_free=free, device_reserved=reserved)


class HintTests(unittest.TestCase):
    def test_hint_from_files_on_gpu_and_cpu(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            (root / "a.bin").write_bytes(b"x" * 1000)
            sub = root / "dir"
            sub.mkdir()
            (sub / "b.bin").write_bytes(b"x" * 600)
            gpu = fp.hint_from_files([root / "a.bin", sub], "cuda")
            self.assertEqual((gpu.device, gpu.device_bytes, gpu.host_bytes), ("cuda:0", 1600, fp.GPU_MODEL_HOST_BYTES))
            onnx = fp.hint_from_files([root / "a.bin"], "cuda:1", onnx=True)
            self.assertEqual((onnx.device_bytes, onnx.runtime), (1250, Runtime.ONNX))
            cpu = fp.hint_from_files([root / "a.bin"], "cpu", onnx=True)
            self.assertEqual((cpu.device, cpu.device_bytes, cpu.host_bytes), ("cpu", 0, 1250))
            missing = fp.hint_from_files([root / "nope.bin"], "cuda:0")
            self.assertEqual((missing.device_bytes, missing.host_bytes), (None, None))

    def test_only_cuda_devices_are_device_accounted(self) -> None:
        self.assertTrue(fp.is_device_accounted("CUDA:1"))
        for device in ("cpu", "mps", "dml:0", None, ""):
            self.assertFalse(fp.is_device_accounted(device))

    def test_a_host_device_hint_folds_into_the_host_side(self) -> None:
        estimate = fp.estimate_footprint(FootprintHint(device="mps", device_bytes=3 * GIB, host_bytes=1 * GIB), None)
        self.assertEqual((estimate.device, estimate.device_bytes, estimate.host_bytes, estimate.source), (None, 0, 4 * GIB, Source.DECLARED))


class EstimateTests(unittest.TestCase):
    def test_unknown_sizes_take_the_default(self) -> None:
        estimate = fp.estimate_footprint(FootprintHint(device="cuda:0"), None)
        self.assertEqual((estimate.device_bytes, estimate.host_bytes, estimate.source), (fp.DEFAULT_DEVICE_BYTES, fp.DEFAULT_HOST_BYTES, Source.DEFAULT))
        no_hint = fp.estimate_footprint(None, None)
        self.assertEqual((no_hint.device, no_hint.host_bytes), (None, fp.DEFAULT_HOST_BYTES))

    def test_learned_wins_on_the_same_device_only(self) -> None:
        learned = Footprint(device="cuda:0", device_bytes=3 * GIB, host_bytes=0, source=Source.MEASURED)
        self.assertEqual(fp.estimate_footprint(FootprintHint(device="cuda:0", device_bytes=GIB, host_bytes=0), learned).source, Source.LEARNED)
        self.assertEqual(fp.estimate_footprint(FootprintHint(device="cuda:1", device_bytes=GIB, host_bytes=0), learned).source, Source.DECLARED)


class MeasureTests(unittest.TestCase):
    def test_the_first_measurement_replaces_a_declared_hint_in_both_directions(self) -> None:
        estimate = Footprint(device="cuda:0", device_bytes=2 * GIB, host_bytes=256 * MIB, source=Source.DECLARED)
        lower = fp.measured_footprint(estimate, _sample(0, reserved=0), _sample(10 * MIB, reserved=GIB))
        self.assertEqual((lower.device_bytes, lower.host_bytes, lower.source), (GIB, 10 * MIB, Source.MEASURED))
        higher = fp.measured_footprint(estimate, _sample(0, reserved=0), _sample(GIB, reserved=3 * GIB))
        self.assertEqual((higher.device_bytes, higher.host_bytes), (3 * GIB, GIB))

    def test_a_learned_estimate_is_only_raised(self) -> None:
        estimate = Footprint(device="cuda:0", device_bytes=2 * GIB, host_bytes=256 * MIB, source=Source.LEARNED)
        lower = fp.measured_footprint(estimate, _sample(0, reserved=0), _sample(10 * MIB, reserved=GIB))
        self.assertEqual((lower.device_bytes, lower.host_bytes), (2 * GIB, 256 * MIB))

    def test_a_default_estimate_is_replaced(self) -> None:
        estimate = Footprint(device="cuda:0", device_bytes=fp.DEFAULT_DEVICE_BYTES, host_bytes=fp.DEFAULT_HOST_BYTES, source=Source.DEFAULT)
        measured = fp.measured_footprint(estimate, _sample(0, reserved=0), _sample(100 * MIB, reserved=500 * MIB))
        self.assertEqual((measured.device_bytes, measured.host_bytes), (500 * MIB, 100 * MIB))

    def test_the_onnx_free_delta_is_believed_only_within_bounds(self) -> None:
        estimate = Footprint(device="cuda:0", device_bytes=GIB, host_bytes=0, source=Source.DECLARED, runtime=Runtime.ONNX)
        inside = fp.measured_footprint(estimate, _sample(free=10 * GIB), _sample(free=8 * GIB))
        self.assertEqual(inside.device_bytes, 2 * GIB)
        too_big = fp.measured_footprint(estimate, _sample(free=10 * GIB), _sample(free=4 * GIB))
        self.assertEqual(too_big.device_bytes, GIB)
        too_small = fp.measured_footprint(Footprint(device="cuda:0", device_bytes=GIB, host_bytes=0, source=Source.DEFAULT, runtime=Runtime.ONNX), _sample(free=10 * GIB), _sample(free=10 * GIB - 100 * MIB))
        self.assertEqual(too_small.device_bytes, GIB)

    def test_remeasure_only_raises_and_is_capped(self) -> None:
        first = Footprint(device="cuda:0", device_bytes=GIB, host_bytes=GIB, source=Source.DECLARED, runtime=Runtime.ONNX)
        same = fp.remeasured_footprint(first, first, _sample(GIB, free=8 * GIB), _sample(0, free=9 * GIB))
        self.assertIs(same, first)
        grown = fp.remeasured_footprint(first, first, _sample(0, free=8 * GIB), _sample(0, free=7 * GIB))
        self.assertEqual(grown.device_bytes, 2 * GIB)
        capped = fp.remeasured_footprint(first, first, _sample(0, free=20 * GIB), _sample(0, free=0))
        self.assertEqual(capped.device_bytes, int(fp.REMEASURE_CAP_FACTOR * GIB))


class SlackHeadroomTests(unittest.TestCase):
    def _budget(self, *, total: int, free: int, allocated: int, reserved: int, reserve: int, cap: int | None = None) -> DeviceBudget:
        return DeviceBudget(device="cuda:0", total=total, free=free, torch_allocated=allocated, torch_reserved=reserved, non_torch=total - free - reserved, reserve=reserve, cap=cap)

    def test_slack_is_credited(self) -> None:
        # Another process X = 4, own allocated Y = 6, own slack Z = 2, total 24, reserve 0.5 (GiB).
        budget = self._budget(total=24 * GIB, free=12 * GIB, allocated=6 * GIB, reserved=8 * GIB, reserve=512 * MIB)
        self.assertEqual(usable_device_headroom(budget, credit_slack=True), 24 * GIB - 4 * GIB - 6 * GIB - 512 * MIB)
        self.assertEqual(usable_device_headroom(budget, credit_slack=False), budget.headroom())

    def test_the_cap_still_binds(self) -> None:
        budget = self._budget(total=24 * GIB, free=12 * GIB, allocated=6 * GIB, reserved=8 * GIB, reserve=0, cap=16 * GIB)
        # Used without the slack is 10 GiB, so a 16 GiB cap leaves 6 GiB.
        self.assertEqual(usable_device_headroom(budget, credit_slack=True), 6 * GIB)

    def test_an_unknown_device_has_no_headroom(self) -> None:
        self.assertEqual(usable_device_headroom(self._budget(total=0, free=0, allocated=0, reserved=0, reserve=0), credit_slack=True), 0)


if __name__ == "__main__":
    unittest.main()
