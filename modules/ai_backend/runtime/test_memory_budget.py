"""
File: modules/ai_backend/runtime/test_memory_budget.py

Purpose:
Unit tests for `runtime/memory_budget.py`: the Linux `/proc` + cgroup v2
reader on a fake tree (MemAvailable, RssAnon + RssShmem, unlimited / limited /
`memory.high` / parent-limited cgroups, `inactive_file` as the only reclaimable
cache, cgroup v1), the psutil reader (Windows `private`), the host and device
headroom rules (reserve, cgroup, cap, `total - reserve` ceiling, unknown = 0),
the torch probe gates, `HostWatch` and the sysmem-spill heuristic.

Notes:
No GPU, no real psutil and no real `/proc` reads: every probe gets a fake tree
in a temporary directory or a fake module in `sys.modules`.
"""

from __future__ import annotations

import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

from . import memory_budget as mb

GIB = 1024**3
MIB = 1024**2


class _FakeTree:
    """A temporary `/proc` + `/sys/fs/cgroup` tree."""

    def __init__(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        root = Path(self._dir.name)
        self.proc = root / "proc"
        self.cgroup = root / "cgroup"
        (self.proc / "self").mkdir(parents=True)
        self.cgroup.mkdir()

    def close(self) -> None:
        self._dir.cleanup()

    def meminfo(self, total_kib: int, available_kib: int) -> None:
        (self.proc / "meminfo").write_text(f"MemTotal:       {total_kib} kB\nMemFree:        1 kB\nMemAvailable:   {available_kib} kB\n")

    def status(self, anon_kib: int, shmem_kib: int) -> None:
        (self.proc / "self" / "status").write_text(f"Name:\tpython\nRssAnon:\t{anon_kib} kB\nRssFile:\t999999 kB\nRssShmem:\t{shmem_kib} kB\n")

    def own_cgroup(self, text: str) -> None:
        (self.proc / "self" / "cgroup").write_text(text)

    def level(self, rel: str, *, max_: str | None = None, high: str | None = None, current: int | None = None, inactive_file: int | None = None) -> None:
        level = self.cgroup.joinpath(*[part for part in rel.split("/") if part])
        level.mkdir(parents=True, exist_ok=True)
        if max_ is not None:
            (level / "memory.max").write_text(max_ + "\n")
        if high is not None:
            (level / "memory.high").write_text(high + "\n")
        if current is not None:
            (level / "memory.current").write_text(f"{current}\n")
        if inactive_file is not None:
            (level / "memory.stat").write_text(f"anon 1\nfile 2\ninactive_file {inactive_file}\nactive_file 3\n")

    def probes(self) -> mb.LinuxHostProbes:
        return mb.LinuxHostProbes(proc_root=self.proc, cgroup_root=self.cgroup)


class LinuxProbeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tree = _FakeTree()
        self.addCleanup(self.tree.close)
        self.tree.meminfo(64 * 1024 * 1024, 40 * 1024 * 1024)
        self.tree.status(3 * 1024 * 1024, 1024 * 1024)

    def test_meminfo_and_status_in_bytes(self) -> None:
        budget = mb.read_host_budget(reserve=2 * GIB, probes=self.tree.probes())
        self.assertEqual(budget.total, 64 * GIB)
        self.assertEqual(budget.available, 40 * GIB)
        self.assertEqual(budget.rss_anon, 3 * GIB)
        self.assertEqual(budget.rss_shmem, 1 * GIB)
        self.assertEqual(budget.rss_private, 4 * GIB, "pinned memory is RssShmem on CUDA: private = anon + shmem")

    def test_unlimited_cgroup_is_no_limit(self) -> None:
        self.tree.own_cgroup("0::/user.slice/app.scope\n")
        self.tree.level("/user.slice", max_="max", current=10 * GIB, inactive_file=0)
        self.tree.level("/user.slice/app.scope", max_="max", high="max", current=5 * GIB, inactive_file=0)
        budget = mb.read_host_budget(reserve=2 * GIB, probes=self.tree.probes())
        self.assertIsNone(budget.cgroup_limit)
        self.assertEqual(budget.headroom(), 38 * GIB)

    def test_limited_cgroup_counts_only_inactive_file_as_reclaimable(self) -> None:
        self.tree.own_cgroup("0::/app\n")
        self.tree.level("/app", max_=str(16 * GIB), current=12 * GIB, inactive_file=2 * GIB)
        budget = mb.read_host_budget(reserve=2 * GIB, probes=self.tree.probes())
        self.assertEqual(budget.cgroup_limit, 16 * GIB)
        self.assertEqual(budget.cgroup_used, 10 * GIB)
        # min(40 GiB available, 16 - 10) - 2 reserve
        self.assertEqual(budget.headroom(), 4 * GIB)

    def test_memory_high_is_a_limit_too(self) -> None:
        self.tree.own_cgroup("0::/app\n")
        self.tree.level("/app", max_="max", high=str(8 * GIB), current=4 * GIB, inactive_file=0)
        budget = mb.read_host_budget(reserve=1 * GIB, probes=self.tree.probes())
        self.assertEqual(budget.cgroup_limit, 8 * GIB)
        self.assertEqual(budget.headroom(), 3 * GIB)

    def test_parent_limit_binds_the_child(self) -> None:
        self.tree.own_cgroup("0::/slice/app\n")
        self.tree.level("/slice", max_=str(10 * GIB), current=9 * GIB, inactive_file=0)
        self.tree.level("/slice/app", max_=str(30 * GIB), current=2 * GIB, inactive_file=0)
        budget = mb.read_host_budget(reserve=0, probes=self.tree.probes())
        self.assertEqual(budget.cgroup_limit, 10 * GIB)
        self.assertEqual(budget.headroom(), 1 * GIB)

    def test_limited_level_with_unreadable_usage_counts_as_full(self) -> None:
        self.tree.own_cgroup("0::/app\n")
        self.tree.level("/app", max_=str(10 * GIB))
        budget = mb.read_host_budget(reserve=0, probes=self.tree.probes())
        self.assertEqual(budget.headroom(), 0)

    def test_cgroup_v1_only_reports_no_limit(self) -> None:
        self.tree.own_cgroup("4:memory:/docker/abc\n1:cpu:/docker/abc\n")
        self.assertIsNone(self.tree.probes().cgroup())

    def test_unreadable_proc_is_unknown_and_refuses(self) -> None:
        empty = _FakeTree()
        self.addCleanup(empty.close)
        with self.assertLogs(mb.log, level="WARNING"):
            budget = mb.read_host_budget(reserve=2 * GIB, probes=empty.probes())
        self.assertEqual((budget.total, budget.available, budget.rss_anon), (0, 0, 0))
        self.assertFalse(budget.known)
        self.assertEqual(budget.headroom(), 0)


class PsutilProbeTests(unittest.TestCase):
    def _fake_psutil(self, *, private: int | None) -> types.ModuleType:
        module = types.ModuleType("psutil")

        class Error(Exception):
            pass

        module.Error = Error  # type: ignore[attr-defined]
        module.virtual_memory = lambda: types.SimpleNamespace(total=32 * GIB, available=20 * GIB)  # type: ignore[attr-defined]
        info = types.SimpleNamespace(rss=5 * GIB) if private is None else types.SimpleNamespace(rss=5 * GIB, private=private)
        module.Process = lambda: types.SimpleNamespace(memory_info=lambda: info)  # type: ignore[attr-defined]
        return module

    def test_windows_private_bytes_and_no_cgroup(self) -> None:
        with mock.patch.dict(sys.modules, {"psutil": self._fake_psutil(private=3 * GIB)}):
            budget = mb.read_host_budget(reserve=2 * GIB, probes=mb.PsutilHostProbes())
        self.assertEqual((budget.total, budget.available, budget.rss_anon, budget.rss_shmem), (32 * GIB, 20 * GIB, 3 * GIB, 0))
        self.assertIsNone(budget.cgroup_limit)
        self.assertEqual(budget.headroom(), 18 * GIB)

    def test_macos_falls_back_to_rss(self) -> None:
        with mock.patch.dict(sys.modules, {"psutil": self._fake_psutil(private=None)}):
            self.assertEqual(mb.PsutilHostProbes().self_status(), {"RssAnon": 5 * GIB})

    def test_missing_psutil_is_unknown(self) -> None:
        with mock.patch.dict(sys.modules, {"psutil": None}):
            self.assertEqual(mb.PsutilHostProbes().meminfo(), {})
            self.assertEqual(mb.PsutilHostProbes().self_status(), {})


class _FakeDeviceProbe:
    def __init__(self, reading: mb.DeviceReading | None) -> None:
        self.reading = reading

    def query(self, device: str) -> mb.DeviceReading | None:
        return self.reading


class DeviceBudgetTests(unittest.TestCase):
    def test_headroom_subtracts_reserve_and_derives_non_torch(self) -> None:
        budget = mb.read_device_budget("cuda:0", reserve=512 * MIB, probe=_FakeDeviceProbe(mb.DeviceReading(total=32 * GIB, free=20 * GIB, torch_allocated=8 * GIB, torch_reserved=10 * GIB)))
        self.assertEqual(budget.non_torch, 2 * GIB)
        self.assertEqual(budget.used, 12 * GIB)
        self.assertEqual(budget.headroom(), 20 * GIB - 512 * MIB)

    def test_cap_bounds_whole_device_usage(self) -> None:
        budget = mb.read_device_budget("cuda:0", reserve=512 * MIB, cap=16 * GIB, probe=_FakeDeviceProbe(mb.DeviceReading(total=32 * GIB, free=20 * GIB, torch_allocated=0, torch_reserved=0)))
        # used 12 GiB of a 16 GiB cap -> 4 GiB - reserve
        self.assertEqual(budget.headroom(), 4 * GIB - 512 * MIB)

    def test_never_above_total_minus_reserve(self) -> None:
        # A driver reporting free > total must not let a plan exceed the card.
        budget = mb.DeviceBudget("cuda:0", total=8 * GIB, free=12 * GIB, torch_allocated=0, torch_reserved=0, non_torch=0, reserve=768 * MIB, cap=None)
        self.assertEqual(budget.headroom(), 8 * GIB - 768 * MIB)

    def test_unknown_device_has_zero_headroom(self) -> None:
        with self.assertLogs(mb.log, level="INFO"):
            budget = mb.read_device_budget("cuda:0", reserve=512 * MIB, probe=_FakeDeviceProbe(None))
        self.assertFalse(budget.known)
        self.assertEqual(budget.headroom(), 0)

    def test_torch_probe_skips_non_cuda_and_missing_torch(self) -> None:
        self.assertIsNone(mb.TorchDeviceProbe(lambda: True).query("cpu"))
        self.assertIsNone(mb.TorchDeviceProbe(lambda: True).query("mps"))
        self.assertIsNone(mb.TorchDeviceProbe(lambda: False).query("cuda:0"))

    def test_torch_probe_warms_up_once_and_reads_the_card(self) -> None:
        calls: list[str] = []
        fake_cuda = types.SimpleNamespace(
            is_available=lambda: True,
            mem_get_info=lambda index: (calls.append(f"info:{index}") or (6 * GIB, 8 * GIB)),
            memory_allocated=lambda index: 1 * GIB,
            memory_reserved=lambda index: 2 * GIB,
            synchronize=lambda device: calls.append(f"sync:{device}"),
        )
        fake_torch = types.SimpleNamespace(cuda=fake_cuda, bfloat16="bf16", ones=lambda shape, dtype, device: calls.append(f"ones:{device}") or object(), matmul=lambda a, b: calls.append("matmul"))
        with mock.patch.dict(sys.modules, {"torch": fake_torch}):
            probe = mb.TorchDeviceProbe(lambda: True)
            first = probe.query("cuda:1")
            probe.query("cuda:1")
        self.assertEqual(first, mb.DeviceReading(total=8 * GIB, free=6 * GIB, torch_allocated=1 * GIB, torch_reserved=2 * GIB))
        self.assertEqual(calls.count("matmul"), 1, "the warm-up matmul runs once per device")
        self.assertLess(calls.index("matmul"), calls.index("info:1"), "the reading is taken after the warm-up")

    def test_default_reserves(self) -> None:
        self.assertEqual(mb.default_device_reserve(is_rocm=True), 512 * MIB)
        self.assertEqual(mb.default_device_reserve(is_rocm=False), 768 * MIB)


class HostWatchTests(unittest.TestCase):
    def _budget(self, available: int) -> mb.HostBudget:
        return mb.HostBudget(total=64 * GIB, available=available, cgroup_limit=None, cgroup_used=None, rss_anon=GIB, rss_shmem=0, reserve=2 * GIB)

    def test_check_passes_and_returns_the_reading(self) -> None:
        watch = mb.HostWatch(lambda: self._budget(10 * GIB), floor=1 * GIB)
        self.assertEqual(watch.check(needed=5 * GIB).available, 10 * GIB)

    def test_check_raises_below_floor_plus_needed(self) -> None:
        readings = iter([self._budget(10 * GIB), self._budget(4 * GIB)])
        watch = mb.HostWatch(lambda: next(readings), floor=1 * GIB)
        watch.check(needed=1 * GIB)
        with self.assertLogs(mb.log, level="WARNING"), self.assertRaises(mb.HostMemoryShort) as caught:
            watch.check(needed=2 * GIB)
        self.assertEqual(caught.exception.needed, 3 * GIB)
        self.assertEqual(caught.exception.headroom, 2 * GIB)


class SpillTests(unittest.TestCase):
    def _device(self, reserved: int, non_torch: int = 1 * GIB) -> mb.DeviceBudget:
        return mb.DeviceBudget("cuda:0", total=12 * GIB, free=1 * GIB, torch_allocated=reserved, torch_reserved=reserved, non_torch=non_torch, reserve=768 * MIB, cap=None)

    def test_usage_past_physical_memory_is_a_spill(self) -> None:
        verdict = mb.detect_sysmem_spill(self._device(0), self._device(13 * GIB), planned_peak=10 * GIB)
        self.assertTrue(verdict.spilled)
        self.assertEqual(verdict.overshoot, 13 * GIB + 1 * GIB - (12 * GIB - 768 * MIB))
        self.assertEqual(verdict.over_plan, 3 * GIB)

    def test_usage_within_memory_is_not(self) -> None:
        self.assertFalse(mb.detect_sysmem_spill(self._device(0), self._device(9 * GIB), planned_peak=10 * GIB).spilled)

    def test_step_time(self) -> None:
        self.assertTrue(mb.step_time_suggests_spill(4.0, 1.0))
        self.assertFalse(mb.step_time_suggests_spill(2.9, 1.0))
        self.assertFalse(mb.step_time_suggests_spill(9.0, 0.0))


if __name__ == "__main__":
    unittest.main()
