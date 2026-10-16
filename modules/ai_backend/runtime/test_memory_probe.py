"""
File: modules/ai_backend/runtime/test_memory_probe.py

Purpose:
Unit tests for `runtime/memory_probe.py`: the snapshot's keys and its `0` =
unknown answer without psutil or torch, the caller-owned wording and logger, the
`torch_available` gate, the device ordinal handed to `torch.cuda.mem_get_info`,
and `cuda_device_index`.

Notes:
No GPU and no real torch needed: psutil and torch are made unimportable through
a scoped `builtins.__import__` patch, or a fake `torch` is installed in
`sys.modules` for the ordinal test.
"""

from __future__ import annotations

import builtins
import logging
import sys
import types
import unittest
from typing import Any
from unittest import mock

from . import memory_probe

_KEYS = ["vram_total", "vram_free", "ram_total", "ram_free"]


def _import_blocking(*blocked: str) -> Any:
    """A `builtins.__import__` replacement that refuses the named top-level modules."""
    real_import = builtins.__import__

    def _guarded(name: str, *args: Any, **kwargs: Any) -> Any:
        if name.split(".", 1)[0] in blocked:
            raise ImportError(f"No module named {name!r} (blocked by test)")
        return real_import(name, *args, **kwargs)

    return _guarded


class MemorySnapshotTests(unittest.TestCase):
    def setUp(self) -> None:
        self.logger = logging.getLogger("test.runtime.memory_probe.owner")

    def test_nothing_available_is_all_zeros_with_owner_records_on_the_given_logger(self) -> None:
        with mock.patch.object(builtins, "__import__", _import_blocking("psutil", "torch")), self.assertLogs(
            self.logger, level=logging.DEBUG
        ) as captured:
            snapshot = memory_probe.memory_snapshot(
                "cuda:0", owner="Some model", logger=self.logger, torch_available=lambda: True
            )
        self.assertEqual(list(snapshot), _KEYS)
        self.assertEqual(set(snapshot.values()), {0})
        messages = [record.getMessage() for record in captured.records]
        self.assertEqual(len(messages), 2, messages)
        self.assertTrue(messages[0].startswith("Some model: host memory unavailable ("))
        self.assertTrue(messages[1].startswith("Some model: device memory unavailable ("))

    def test_torch_is_not_attempted_when_the_gate_says_unavailable(self) -> None:
        with mock.patch.object(builtins, "__import__", _import_blocking("psutil", "torch")), self.assertLogs(
            self.logger, level=logging.DEBUG
        ) as captured:
            snapshot = memory_probe.memory_snapshot(
                owner="Some model", logger=self.logger, torch_available=lambda: False
            )
        self.assertEqual(snapshot["vram_total"], 0)
        self.assertEqual(len(captured.records), 1)

    def test_the_explicit_ordinal_reaches_mem_get_info(self) -> None:
        asked: list[object] = []
        fake_cuda = types.SimpleNamespace(
            is_available=lambda: True,
            mem_get_info=lambda index: (asked.append(index), (5, 7))[1],
        )
        fake_torch = types.ModuleType("torch")
        fake_torch.cuda = fake_cuda  # type: ignore[attr-defined]
        with mock.patch.dict(sys.modules, {"torch": fake_torch}):
            snapshot = memory_probe.memory_snapshot(
                "cuda:1", owner="Some model", logger=self.logger, torch_available=lambda: True
            )
        self.assertEqual(asked, [1])
        self.assertEqual((snapshot["vram_free"], snapshot["vram_total"]), (5, 7))

    def test_without_a_logger_the_module_logger_is_used(self) -> None:
        with mock.patch.object(builtins, "__import__", _import_blocking("psutil")), self.assertLogs(
            memory_probe.log, level=logging.DEBUG
        ) as captured:
            memory_probe.memory_snapshot(owner="Some model", torch_available=lambda: False)
        self.assertEqual(captured.records[0].name, memory_probe.__name__)


class CudaDeviceIndexTests(unittest.TestCase):
    def test_an_explicit_ordinal_is_parsed(self) -> None:
        self.assertEqual(memory_probe.cuda_device_index("cuda:1"), 1)
        self.assertEqual(memory_probe.cuda_device_index(" CUDA:3 "), 3)

    def test_everything_else_means_the_current_device(self) -> None:
        for value in ("cuda", "cpu", "mps", "", None, "cuda:x", "cuda:"):
            with self.subTest(value=value):
                self.assertIsNone(memory_probe.cuda_device_index(value))


if __name__ == "__main__":
    unittest.main()
