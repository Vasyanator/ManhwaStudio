"""
File: modules/ai_backend/inpaint/flux2_klein/test_runtime_extraction.py

Purpose:
Characterization tests for the FLUX.2 klein names whose implementation lives in
`../../runtime/` (`safetensors_stream`, `memory_probe`, `encoder_truncation`).
They were written against the code BEFORE it moved and pin what FLUX.2 must keep
seeing afterwards: the same refusals, the same log wording on the same loggers,
the same constants, the same names on the same modules, and the same patch
points.

Main responsibilities:
- `hardware.memory_snapshot` reports zeros with the "FLUX.2 klein: ... memory
  unavailable" DEBUG records on the `hardware` logger, and still honours
  `hardware.is_torch_available` as its patch point;
- the constants (`ENCODER_KEEP_LAYERS`, the header ceiling) keep their values;
- every moved name stays reachable on its old module AND on the package.

The streaming single-file loader these tests also pinned is gone (WP6: the
large-model manager's loader reads every component tensor by tensor); its
reader and dtype table are `runtime.safetensors_stream`'s own tests.

Notes:
- No torch, no GPU: torch and psutil are made unimportable through a scoped
  `builtins.__import__` patch where the test needs their absence.
"""

from __future__ import annotations

import builtins
import logging
import unittest
from typing import Any
from unittest.mock import patch

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import components, hardware


def _import_blocking(*blocked: str) -> Any:
    """A `builtins.__import__` replacement that refuses the named top-level modules."""
    real_import = builtins.__import__

    def _guarded(name: str, *args: Any, **kwargs: Any) -> Any:
        if name.split(".", 1)[0] in blocked:
            raise ImportError(f"No module named {name!r} (blocked by test)")
        return real_import(name, *args, **kwargs)

    return _guarded


class MemorySnapshotCharacterizationTests(unittest.TestCase):
    def test_no_psutil_and_no_torch_reports_zeros_and_logs_on_the_hardware_logger(self) -> None:
        with patch.object(hardware, "is_torch_available", lambda: False), patch.object(
            builtins, "__import__", _import_blocking("psutil")
        ), self.assertLogs(svc.log, level=logging.DEBUG) as captured:
            snapshot = hardware.memory_snapshot()
        self.assertEqual(
            snapshot, {"vram_total": 0, "vram_free": 0, "ram_total": 0, "ram_free": 0}
        )
        self.assertEqual(list(snapshot), ["vram_total", "vram_free", "ram_total", "ram_free"])
        self.assertEqual(len(captured.records), 1)
        record = captured.records[0]
        self.assertEqual(record.levelno, logging.DEBUG)
        self.assertEqual(record.name, hardware.__name__)
        self.assertTrue(
            record.getMessage().startswith("FLUX.2 klein: host memory unavailable ("),
            record.getMessage(),
        )

    def test_hardware_is_torch_available_is_still_the_patch_point(self) -> None:
        # Torch "available" by the hardware binding, but unimportable: the probe
        # must try it, fail, and log the device half on the hardware logger.
        with patch.object(hardware, "is_torch_available", lambda: True), patch.object(
            builtins, "__import__", _import_blocking("psutil", "torch")
        ), self.assertLogs(svc.log, level=logging.DEBUG) as captured:
            snapshot = hardware.memory_snapshot("cuda:1")
        self.assertEqual(snapshot["vram_total"], 0)
        self.assertEqual(snapshot["vram_free"], 0)
        messages = [record.getMessage() for record in captured.records]
        self.assertEqual(len(messages), 2, messages)
        self.assertTrue(messages[1].startswith("FLUX.2 klein: device memory unavailable ("))
        self.assertTrue(all(record.name == hardware.__name__ for record in captured.records))

    def test_cuda_device_index_is_unchanged(self) -> None:
        self.assertEqual(hardware._cuda_device_index(" CUDA:2 "), 2)
        self.assertIsNone(hardware._cuda_device_index("cuda:"))


class ConstantsCharacterizationTests(unittest.TestCase):
    def test_encoder_keep_layers(self) -> None:
        self.assertEqual(components.ENCODER_KEEP_LAYERS, 28)
        self.assertEqual(svc.ENCODER_KEEP_LAYERS, 28)

    def test_header_ceiling(self) -> None:
        self.assertEqual(components._MAX_SAFETENSORS_HEADER_BYTES, 100 * 1024 * 1024)


class SurfaceCharacterizationTests(unittest.TestCase):
    #: module -> names that must stay reachable on it AND, identically, on the package.
    _SURFACE = {
        components: (
            "read_safetensors_header",
            "read_safetensors_header_and_data_start",
            "_MAX_SAFETENSORS_HEADER_BYTES",
            "ENCODER_KEEP_LAYERS",
        ),
        hardware: ("memory_snapshot", "_cuda_device_index"),
    }

    def test_every_moved_name_is_reachable_on_its_module_and_the_package(self) -> None:
        for module, names in self._SURFACE.items():
            for name in names:
                with self.subTest(module=module.__name__, name=name):
                    self.assertTrue(hasattr(module, name))
                    self.assertIs(getattr(svc, name), getattr(module, name))

    def test_memory_snapshot_is_defined_by_hardware(self) -> None:
        # A wrapper def owned by `hardware`, not a re-bound import: that is what
        # keeps `hardware.memory_snapshot` the one patch point.
        self.assertEqual(hardware.memory_snapshot.__module__, hardware.__name__)


if __name__ == "__main__":
    unittest.main()
