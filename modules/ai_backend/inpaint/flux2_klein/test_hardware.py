"""
File: modules/ai_backend/inpaint/flux2_klein/test_hardware.py

Purpose:
Unit tests for `hardware.py`: the device the service reports and the CUDA index
derived from it.

Main responsibilities:
- verify the reported device is the one that will actually be used, never a
  placeholder `cpu` chosen because nothing is loaded yet;
- verify `_cuda_device_index` parses only what it should.
"""

from __future__ import annotations

import unittest
from unittest.mock import patch

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import hardware
from modules.ai_backend.runtime.model_manager import LoadedModelManager

from ._test_fixtures import _TempTreeCase


class DeviceReportingTests(_TempTreeCase):
    """`cpu` is never a placeholder: it is reported only when it is the answer."""

    def setUp(self) -> None:
        super().setUp()
        self.resolved: list[str] = []

        def _resolve(fallback: str) -> str:
            self.resolved.append(fallback)
            return "cuda:0"

        resolve_patch = patch.object(hardware, "_resolve_selected_backend_device", _resolve)
        resolve_patch.start()
        self.addCleanup(resolve_patch.stop)

        self.snapshots: list[object] = []

        def _snapshot(device: object = None) -> dict[str, int]:
            self.snapshots.append(device)
            return {"vram_total": 0, "vram_free": 0, "ram_total": 0, "ram_free": 0}

        memory_patch = patch.object(hardware, "memory_snapshot", _snapshot)
        memory_patch.start()
        self.addCleanup(memory_patch.stop)

        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())

    def test_status_before_the_first_load_reports_the_device_that_will_be_used(self) -> None:
        out = self.service.status(self.params())
        # Not "cpu": the run will happen on the configured accelerator, and a
        # user deciding whether to start a tens-of-minutes job must see that.
        self.assertEqual(out["device"], "cuda:0")
        self.assertFalse(out["loaded"])
        self.assertEqual(self.resolved, ["cuda"])

    def test_status_after_a_load_reports_the_actual_device(self) -> None:
        self.service._pipe = object()
        self.service._device = "cuda:1"
        out = self.service.status(self.params())
        self.assertEqual(out["device"], "cuda:1")
        self.assertTrue(out["loaded"])
        # The loaded device is a fact; nothing is resolved on top of it.
        self.assertEqual(self.resolved, [])

    def test_health_follows_the_same_rule(self) -> None:
        self.assertEqual(self.service.health()["device"], "cuda:0")
        self.service._pipe = object()
        self.service._device = "cuda:1"
        self.assertEqual(self.service.health()["device"], "cuda:1")

    def test_the_memory_forecast_is_read_from_the_selected_device(self) -> None:
        # A VRAM forecast compared against another card's free memory is advice
        # about the wrong hardware.
        self.service.estimate(params=self.params(), region_width=512, region_height=512)
        self.assertEqual(self.snapshots, ["cuda:0"])

    def test_a_cpu_selection_is_still_reported_as_cpu(self) -> None:
        with patch.object(hardware, "_resolve_selected_backend_device", lambda _fallback: "cpu"):
            self.assertEqual(self.service.status(self.params())["device"], "cpu")


class CudaDeviceIndexTests(unittest.TestCase):
    def test_an_explicit_ordinal_is_parsed(self) -> None:
        self.assertEqual(svc._cuda_device_index("cuda:1"), 1)

    def test_everything_else_means_the_current_device(self) -> None:
        for value in ("cuda", "cpu", "mps", "", None, "cuda:x"):
            self.assertIsNone(svc._cuda_device_index(value))

if __name__ == "__main__":
    unittest.main()
