"""
File: modules/ai_backend/runtime/test_torch_device.py

Purpose:
Unit tests for `runtime/torch_device.py`: the `General.ai_device` resolution
order, the `not-selected` sentinel, both per-service device policies
(`accept_mps`, `mps_last_resort`), the CPU-only answer of a failing device probe,
and `clear_torch_cache` (CUDA always, MPS only with `include_mps`, a failing
release logged rather than raised, no torch tolerated).

Notes:
No GPU and no real torch needed: `UserConfig` and `AIDevice` are replaced on the
module under test, and a fake `torch` module is installed in `sys.modules` for
the cache tests.
"""

from __future__ import annotations

import sys
import types
import unittest
from typing import Any
from unittest import mock

from . import torch_device


class _Config:
    """Stand-in for root `config.UserConfig`: only the `config` dict is read."""

    def __init__(self, ai_device: Any) -> None:
        self.config: dict[str, Any] = {} if ai_device is None else {"General": {"ai_device": ai_device}}


class _Devices:
    """Stand-in for `AIDevice`; `available=None` makes the probe raise."""

    def __init__(self, available: list[str] | None) -> None:
        self._available = available

    def detect_available_devices(self) -> list[str]:
        if self._available is None:
            raise RuntimeError("probe failed")
        return list(self._available)


def _resolve(ai_device: Any, available: list[str] | None, fallback: str, **policy: bool) -> str:
    with mock.patch.object(torch_device, "UserConfig", _Config(ai_device)), mock.patch.object(
        torch_device, "AIDevice", _Devices(available)
    ):
        return torch_device.resolve_selected_backend_device(fallback, **policy)


class ResolveSelectedBackendDeviceTests(unittest.TestCase):
    def test_available_configured_device_wins(self) -> None:
        self.assertEqual(_resolve("cuda:1", ["cpu", "cuda", "cuda:0", "cuda:1"], "cpu"), "cuda:1")
        self.assertEqual(_resolve(" CPU ", ["cpu", "cuda"], "cuda"), "cpu")

    def test_sentinel_and_unset_use_the_fallback(self) -> None:
        for value in (None, "", "not-selected", 5):
            with self.subTest(value=value):
                self.assertEqual(_resolve(value, ["cpu", "cuda"], "cuda"), "cuda")
                self.assertEqual(_resolve(value, ["cpu", "cuda"], "cpu"), "cpu")

    def test_unavailable_cuda_index_degrades_to_plain_cuda(self) -> None:
        self.assertEqual(_resolve("cuda:3", ["cpu", "cuda", "cuda:0"], "cpu"), "cuda")

    def test_unavailable_choice_falls_back_then_cuda_then_cpu(self) -> None:
        self.assertEqual(_resolve("cuda", ["cpu"], "cpu"), "cpu")
        # An unknown fallback normalizes to `cpu`, which is always available.
        self.assertEqual(_resolve("vulkan", ["cpu", "cuda"], "garbage"), "cpu")
        # An unavailable fallback moves on to `cuda`, then `cpu`.
        self.assertEqual(_resolve("vulkan", ["cpu", "cuda"], "cuda:5"), "cuda")
        self.assertEqual(_resolve("vulkan", ["cpu"], "cuda:5"), "cpu")

    def test_failing_probe_counts_as_cpu_only(self) -> None:
        self.assertEqual(_resolve("cuda", None, "cuda"), "cpu")

    def test_mps_is_honoured_by_default(self) -> None:
        self.assertEqual(_resolve("mps", ["cpu", "mps"], "cpu"), "mps")

    def test_accept_mps_false_treats_mps_as_unknown(self) -> None:
        self.assertEqual(_resolve("mps", ["cpu", "mps"], "cpu", accept_mps=False), "cpu")
        self.assertEqual(_resolve("mps", ["cpu", "mps", "cuda"], "mps", accept_mps=False), "cpu")

    def test_mps_last_resort_only_when_nothing_else_matches(self) -> None:
        self.assertEqual(_resolve("vulkan", ["cpu", "mps"], "cuda"), "cpu")
        self.assertEqual(_resolve("vulkan", ["cpu", "mps"], "cuda", mps_last_resort=True), "mps")
        self.assertEqual(_resolve("vulkan", ["cpu", "mps", "cuda"], "cuda:5", mps_last_resort=True), "cuda")
        self.assertEqual(_resolve("cpu", ["cpu", "mps"], "cpu", mps_last_resort=True), "cpu")


class NormalizeBackendDeviceTests(unittest.TestCase):
    def test_known_names_pass_and_others_take_the_fallback(self) -> None:
        self.assertEqual(torch_device.normalize_backend_device(" Cuda:2 ", "cpu"), "cuda:2")
        self.assertEqual(torch_device.normalize_backend_device("cuda0", "cpu"), "cpu")
        self.assertEqual(torch_device.normalize_backend_device(None, " CUDA "), "cuda")
        self.assertEqual(torch_device.normalize_backend_device("x", ""), "cpu")
        self.assertEqual(torch_device.normalize_backend_device("mps", "cpu", accept_mps=False), "cpu")


def _fake_torch(*, cuda_available: bool, cuda_raises: bool = False) -> tuple[types.ModuleType, list[str]]:
    calls: list[str] = []
    torch = types.ModuleType("torch")

    def empty_cache() -> None:
        if cuda_raises:
            raise RuntimeError("driver gone")
        calls.append("cuda.empty_cache")

    torch.cuda = types.SimpleNamespace(  # type: ignore[attr-defined]
        is_available=lambda: cuda_available,
        empty_cache=empty_cache,
        ipc_collect=lambda: calls.append("cuda.ipc_collect"),
    )
    torch.mps = types.SimpleNamespace(empty_cache=lambda: calls.append("mps.empty_cache"))  # type: ignore[attr-defined]
    return torch, calls


class ClearTorchCacheTests(unittest.TestCase):
    def test_releases_cuda_and_mps_by_default(self) -> None:
        torch, calls = _fake_torch(cuda_available=True)
        with mock.patch.dict(sys.modules, {"torch": torch}):
            torch_device.clear_torch_cache(owner="test")
        self.assertEqual(calls, ["cuda.empty_cache", "cuda.ipc_collect", "mps.empty_cache"])

    def test_include_mps_false_leaves_mps_alone(self) -> None:
        torch, calls = _fake_torch(cuda_available=True)
        with mock.patch.dict(sys.modules, {"torch": torch}):
            torch_device.clear_torch_cache(owner="test", include_mps=False)
        self.assertEqual(calls, ["cuda.empty_cache", "cuda.ipc_collect"])

    def test_unavailable_cuda_is_skipped(self) -> None:
        torch, calls = _fake_torch(cuda_available=False)
        with mock.patch.dict(sys.modules, {"torch": torch}):
            torch_device.clear_torch_cache(owner="test")
        self.assertEqual(calls, ["mps.empty_cache"])

    def test_failing_release_is_logged_not_raised(self) -> None:
        torch, calls = _fake_torch(cuda_available=True, cuda_raises=True)
        with mock.patch.dict(sys.modules, {"torch": torch}), self.assertLogs(torch_device.log, "WARNING") as logs:
            torch_device.clear_torch_cache(owner="Svc")
        self.assertEqual(calls, ["mps.empty_cache"])
        self.assertIn("Svc: could not release the torch CUDA cache", logs.output[0])

    def test_missing_torch_is_tolerated(self) -> None:
        # `None` in sys.modules makes `import torch` raise ImportError.
        with mock.patch.dict(sys.modules, {"torch": None}), mock.patch.object(torch_device.gc, "collect") as collect:
            torch_device.clear_torch_cache(owner="test")
        collect.assert_called_once_with()


if __name__ == "__main__":
    unittest.main()
