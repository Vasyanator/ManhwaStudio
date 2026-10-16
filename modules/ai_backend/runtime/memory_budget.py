"""
File: modules/ai_backend/runtime/memory_budget.py

Purpose:
The single owner of "how much host and device memory is usable right now" for
the backend: the probes and the reserve arithmetic that every memory pool
(small models, large models) and the residency arbiter above them share. It
reads the figures, applies the reserves and answers one number per side
(`headroom()`); it knows nothing about models, pools or leases.

Main responsibilities:
- host budget (`HostBudget`, `read_host_budget`): `MemAvailable`, the cgroup v2
  limit along this process's cgroup path (`memory.max` and `memory.high`, the
  tightest level wins), this process's private memory `RssAnon + RssShmem`
  (pinned host memory is accounted as `RssShmem` on CUDA); Windows/macOS
  through psutil without a cgroup;
- device budget (`DeviceBudget`, `read_device_budget`): total / free / torch
  allocated / torch reserved / non-torch usage of one device, the reserve and an
  optional cap; the torch probe warms the device up once (one small matmul) so
  the BLAS workspace that appears at the first matmul is already in the reading;
- the probes as protocols (`HostProbes`, `DeviceProbe`) with the real
  implementations (`LinuxHostProbes`, `PsutilHostProbes`, `TorchDeviceProbe`),
  so tests inject figures;
- the live host guard (`HostWatch`, `HostMemoryShort`) for loaders and moves;
- the Windows sysmem-fallback heuristic (`detect_sysmem_spill`, `SpillVerdict`,
  `step_time_suggests_spill`).

Contracts:
- **No host OOM.** Host headroom is `min(MemAvailable, cgroup limit - cgroup
  usage) - reserve`. Page cache is never counted as usable beyond what the
  kernel already counts in `MemAvailable`; in a cgroup only `inactive_file` is
  treated as reclaimable. An unreadable figure is `0` = unknown, and unknown
  available memory yields headroom `0`, i.e. the planner refuses rather than
  guesses.
- **Never plan device memory above `total - reserve`.** Device headroom is
  `min(free, cap - used) - reserve`, additionally clamped to `total - reserve`;
  an unknown total gives headroom `0`.
- Readers never raise; the probes log a figure they could not read at DEBUG
  and report it as unknown.

Notes:
- Stdlib only at import; psutil and torch are imported lazily inside the probes.
- cgroup v1 is not read (no limit is reported there); `MemAvailable` still
  bounds the plan.
"""

from __future__ import annotations

import logging
import sys
import threading
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Protocol

from .memory_probe import cuda_device_index
from .torch_support import is_torch_available

log = logging.getLogger(__name__)

GIB = 1024**3
MIB = 1024**2

#: Host bytes no plan may touch: keeps the editor and the desktop alive. The
#: same figure FLUX.2 klein has documented since it once OOM-killed the editor.
DEFAULT_HOST_RESERVE_BYTES = 2 * GIB

#: Device bytes no plan may touch on ROCm / on CUDA (the CUDA context is larger).
DEFAULT_DEVICE_RESERVE_ROCM_BYTES = 512 * MIB
DEFAULT_DEVICE_RESERVE_CUDA_BYTES = 768 * MIB


def default_device_reserve(*, is_rocm: bool) -> int:
    """The default device reserve in bytes for a ROCm (`True`) or CUDA (`False`) build."""
    return DEFAULT_DEVICE_RESERVE_ROCM_BYTES if is_rocm else DEFAULT_DEVICE_RESERVE_CUDA_BYTES


# =====================================================================
#  Host
# =====================================================================
@dataclass(frozen=True)
class CgroupReading:
    """The tightest cgroup v2 level along this process's cgroup path.

    `path` is that level relative to the cgroup root; `limit` the smaller of its
    `memory.max` and `memory.high` in bytes; `used` its `memory.current` minus
    its `inactive_file` (the only page cache counted as reclaimable).
    """

    path: str
    limit: int
    used: int

    @property
    def headroom(self) -> int:
        """Bytes this level still admits, never negative."""
        return max(0, self.limit - self.used)


class HostProbes(Protocol):
    """Host figures a `HostBudget` is built from; each method is tolerant.

    `meminfo()` returns `MemTotal` / `MemAvailable` in bytes (missing key =
    unknown); `cgroup()` the binding cgroup level or `None` when there is no
    limit (or no cgroup v2); `self_status()` returns `RssAnon` and `RssShmem`
    in bytes.
    """

    def meminfo(self) -> Mapping[str, int]: ...

    def cgroup(self) -> CgroupReading | None: ...

    def self_status(self) -> Mapping[str, int]: ...


def _parse_kib_table(text: str, wanted: frozenset[str]) -> dict[str, int]:
    """`Key:   123 kB` lines (meminfo / status format) -> `{key: bytes}` for `wanted` keys."""
    figures: dict[str, int] = {}
    for line in text.splitlines():
        name, sep, rest = line.partition(":")
        if not sep or name not in wanted:
            continue
        parts = rest.split()
        if not parts or not parts[0].isdigit():
            continue
        scale = 1024 if len(parts) > 1 and parts[1].lower() == "kb" else 1
        figures[name] = int(parts[0]) * scale
    return figures


class LinuxHostProbes:
    """`/proc` + cgroup v2 reader; roots are parameters so tests use a fake tree."""

    def __init__(self, proc_root: Path = Path("/proc"), cgroup_root: Path = Path("/sys/fs/cgroup")) -> None:
        self._proc = proc_root
        self._cgroup_root = cgroup_root

    def meminfo(self) -> Mapping[str, int]:
        """`MemTotal` / `MemAvailable` from `/proc/meminfo`; `{}` when unreadable."""
        try:
            text = (self._proc / "meminfo").read_text(encoding="ascii", errors="replace")
        except OSError as exc:
            log.debug("host budget: /proc/meminfo unreadable (%s)", exc)
            return {}
        return _parse_kib_table(text, frozenset({"MemTotal", "MemAvailable"}))

    def self_status(self) -> Mapping[str, int]:
        """`RssAnon` / `RssShmem` of this process from `/proc/self/status`; `{}` when unreadable."""
        try:
            text = (self._proc / "self" / "status").read_text(encoding="ascii", errors="replace")
        except OSError as exc:
            log.debug("host budget: /proc/self/status unreadable (%s)", exc)
            return {}
        return _parse_kib_table(text, frozenset({"RssAnon", "RssShmem"}))

    def _own_cgroup_path(self) -> str | None:
        """The unified (v2) cgroup path of this process, `None` without one."""
        try:
            text = (self._proc / "self" / "cgroup").read_text(encoding="ascii", errors="replace")
        except OSError as exc:
            log.debug("host budget: /proc/self/cgroup unreadable (%s)", exc)
            return None
        for line in text.splitlines():
            # v2 entry is "0::/path"; v1 controllers have a non-empty middle field.
            hierarchy, _, rest = line.partition(":")
            controllers, _, path = rest.partition(":")
            if hierarchy == "0" and controllers == "":
                return path.strip() or "/"
        return None

    def _read_limit_file(self, level: Path, name: str) -> int | None:
        """A `memory.max`/`memory.high` value: bytes, `None` for `max`/absent/unreadable."""
        try:
            raw = (level / name).read_text(encoding="ascii").strip()
        except OSError:
            return None
        if raw == "max" or not raw.isdigit():
            return None
        return int(raw)

    def _read_used(self, level: Path) -> int | None:
        """`memory.current - inactive_file` of one level; `None` when unreadable."""
        try:
            current_raw = (level / "memory.current").read_text(encoding="ascii").strip()
        except OSError as exc:
            log.debug("host budget: %s/memory.current unreadable (%s)", level, exc)
            return None
        if not current_raw.isdigit():
            return None
        current = int(current_raw)
        inactive_file = 0
        try:
            for line in (level / "memory.stat").read_text(encoding="ascii").splitlines():
                key, _, value = line.partition(" ")
                if key == "inactive_file" and value.strip().isdigit():
                    inactive_file = int(value.strip())
                    break
        except OSError as exc:
            # Without the stat file nothing is counted as reclaimable: the
            # expensive reading, never the optimistic one.
            log.debug("host budget: %s/memory.stat unreadable (%s)", level, exc)
        return max(0, current - inactive_file)

    def cgroup(self) -> CgroupReading | None:
        """The tightest limited level from this process's cgroup up to the root, or `None`."""
        own = self._own_cgroup_path()
        if own is None:
            return None
        parts = [part for part in own.split("/") if part]
        tightest: CgroupReading | None = None
        # Walk leaf -> root: a parent's limit binds every child, whatever the child says.
        for depth in range(len(parts), -1, -1):
            rel = "/" + "/".join(parts[:depth])
            level = self._cgroup_root.joinpath(*parts[:depth])
            limits = [value for value in (self._read_limit_file(level, "memory.max"), self._read_limit_file(level, "memory.high")) if value is not None]
            if not limits:
                continue
            used = self._read_used(level)
            if used is None:
                # A limited level whose usage is unknown: assume it is full.
                used = min(limits)
            reading = CgroupReading(path=rel, limit=min(limits), used=used)
            if tightest is None or reading.headroom < tightest.headroom:
                tightest = reading
        return tightest


class PsutilHostProbes:
    """Windows / macOS host reader over psutil; no cgroup."""

    def meminfo(self) -> Mapping[str, int]:
        """psutil `total` / `available` as `MemTotal` / `MemAvailable`; `{}` without psutil."""
        try:
            import psutil
        except ImportError as exc:
            log.debug("host budget: psutil unavailable (%s)", exc)
            return {}
        try:
            virtual = psutil.virtual_memory()
        except (OSError, psutil.Error) as exc:
            log.debug("host budget: psutil.virtual_memory failed (%s)", exc)
            return {}
        return {"MemTotal": int(virtual.total), "MemAvailable": int(virtual.available)}

    def cgroup(self) -> CgroupReading | None:
        """No cgroup outside Linux."""
        return None

    def self_status(self) -> Mapping[str, int]:
        """Private bytes (Windows `private`, else `rss`) as `RssAnon` (no `RssShmem`); `{}` without psutil."""
        try:
            import psutil
        except ImportError as exc:
            log.debug("host budget: psutil unavailable (%s)", exc)
            return {}
        try:
            info = psutil.Process().memory_info()
        except (OSError, psutil.Error) as exc:
            log.debug("host budget: psutil memory_info failed (%s)", exc)
            return {}
        private = getattr(info, "private", None)
        return {"RssAnon": int(private if private is not None else info.rss)}


def default_host_probes() -> HostProbes:
    """The platform's host reader: `/proc` + cgroup on Linux, psutil elsewhere."""
    if sys.platform.startswith("linux"):
        return LinuxHostProbes()
    return PsutilHostProbes()


@dataclass(frozen=True)
class HostBudget:
    """Host memory figures in bytes at one moment; `0` = unknown.

    `cgroup_limit` / `cgroup_used` are `None` when no cgroup limit binds this
    process. `rss_anon` / `rss_shmem` are this process's anonymous and shared
    resident memory (never file pages; `rss_private` is their sum, the figure
    that sees pinned host memory on every backend). `reserve` is subtracted by
    `headroom()`.
    """

    total: int
    available: int
    cgroup_limit: int | None
    cgroup_used: int | None
    rss_anon: int
    rss_shmem: int
    reserve: int

    @property
    def rss_private(self) -> int:
        """This process's private resident bytes, `RssAnon + RssShmem` (`0` = unknown)."""
        return self.rss_anon + self.rss_shmem

    def headroom(self) -> int:
        """Bytes a plan may still allocate on the host: `min(available, cgroup room) - reserve`, >= 0.

        Unknown `available` (`0`) gives `0`: the expensive reading.
        """
        room = self.available
        if self.cgroup_limit is not None:
            room = min(room, max(0, self.cgroup_limit - (self.cgroup_used or 0)))
        return max(0, room - self.reserve)

    @property
    def known(self) -> bool:
        """Whether the figures the headroom depends on were read."""
        return self.available > 0


def read_host_budget(*, reserve: int = DEFAULT_HOST_RESERVE_BYTES, probes: HostProbes | None = None) -> HostBudget:
    """Read the host budget now; never raises (unknown figures are `0`)."""
    source = probes if probes is not None else default_host_probes()
    meminfo = source.meminfo()
    cgroup = source.cgroup()
    status = source.self_status()
    budget = HostBudget(
        total=int(meminfo.get("MemTotal", 0)),
        available=int(meminfo.get("MemAvailable", 0)),
        cgroup_limit=cgroup.limit if cgroup is not None else None,
        cgroup_used=cgroup.used if cgroup is not None else None,
        rss_anon=int(status.get("RssAnon", 0)),
        rss_shmem=int(status.get("RssShmem", 0)),
        reserve=max(0, int(reserve)),
    )
    if not budget.known:
        log.warning("host budget: available memory unknown; every plan that needs host memory will be refused (probe=%s)", type(source).__name__)
    return budget


class HostMemoryShort(Exception):
    """The host budget fell below the guard floor at a block/tensor boundary.

    `needed` is the floor plus the bytes the caller was about to allocate,
    `headroom` what the budget answered. The caller stops at a consistent point
    (a block fully moved or not at all).
    """

    def __init__(self, needed: int, headroom: int) -> None:
        super().__init__(f"host memory short: need {needed} B above the reserve, headroom {headroom} B")
        self.needed = needed
        self.headroom = headroom


class HostWatch:
    """Live host guard: `check()` before every allocation-sized step of a load or a move.

    `budget_fn` reads a fresh `HostBudget` (normally `read_host_budget` bound to
    the process reserve); `floor` the headroom that must remain after the step.
    """

    def __init__(self, budget_fn: Callable[[], HostBudget], *, floor: int = 0) -> None:
        self._budget_fn = budget_fn
        self._floor = max(0, int(floor))

    def check(self, needed: int = 0) -> HostBudget:
        """Return the fresh budget, or raise `HostMemoryShort` when `headroom < floor + needed`."""
        budget = self._budget_fn()
        required = self._floor + max(0, int(needed))
        headroom = budget.headroom()
        if headroom < required:
            log.warning("host watch: headroom %d B below required %d B (available=%d, cgroup_limit=%s, rss_private=%d)", headroom, required, budget.available, budget.cgroup_limit, budget.rss_private)
            raise HostMemoryShort(required, headroom)
        return budget


# =====================================================================
#  Device
# =====================================================================
@dataclass(frozen=True)
class DeviceReading:
    """Raw figures of one device in bytes, as a `DeviceProbe` reports them."""

    total: int
    free: int
    torch_allocated: int
    torch_reserved: int


class DeviceProbe(Protocol):
    """Reads one device; `None` when the device cannot be queried."""

    def query(self, device: str) -> DeviceReading | None: ...


class TorchDeviceProbe:
    """`torch.cuda` reader (CUDA and ROCm/HIP builds); torch imported lazily.

    With `warm_up=True` the first query of each device runs one small bf16
    matmul there first: the BLAS workspace (0.2-0.3 GiB, measured on ROCm
    gfx1201 and CUDA sm_120) is allocated only at the first matmul, and a reading
    taken before it would overstate the free memory by that much.
    """

    def __init__(self, torch_available: Callable[[], bool] = is_torch_available, *, warm_up: bool = True) -> None:
        self._torch_available = torch_available
        self._warm_up = warm_up
        self._warmed: set[str] = set()
        self._warm_lock = threading.Lock()

    def _ensure_warm(self, torch: Any, device: str) -> None:
        """Run the one warm-up matmul on `device` once per probe instance."""
        with self._warm_lock:
            if device in self._warmed:
                return
            self._warmed.add(device)
        probe = torch.ones((64, 64), dtype=torch.bfloat16, device=device)
        torch.matmul(probe, probe)
        torch.cuda.synchronize(device)
        del probe

    def query(self, device: str) -> DeviceReading | None:
        """Figures of `device` (`cuda`, `cuda:<n>`); `None` for CPU/MPS, no torch or no GPU."""
        if not str(device).strip().lower().startswith("cuda"):
            return None
        if not self._torch_available():
            return None
        import torch

        index = cuda_device_index(device)
        try:
            if not torch.cuda.is_available():
                return None
            if self._warm_up:
                self._ensure_warm(torch, str(device))
            free, total = torch.cuda.mem_get_info(index)
            allocated = torch.cuda.memory_allocated(index)
            reserved = torch.cuda.memory_reserved(index)
        except (RuntimeError, AssertionError, ValueError) as exc:
            log.debug("device budget: %s unreadable (%s)", device, exc)
            return None
        return DeviceReading(total=int(total), free=int(free), torch_allocated=int(allocated), torch_reserved=int(reserved))


@dataclass(frozen=True)
class DeviceBudget:
    """Device memory figures in bytes at one moment; `0` = unknown.

    `non_torch` is everything on the device that this process's caching
    allocator does not hold (other processes, the context, kernels); `cap` an
    upper bound on the WHOLE device usage (as if the card had `cap` bytes),
    `None` = none.
    """

    device: str
    total: int
    free: int
    torch_allocated: int
    torch_reserved: int
    non_torch: int
    reserve: int
    cap: int | None

    @property
    def used(self) -> int:
        """Bytes in use on the device by anyone."""
        return max(0, self.total - self.free)

    def headroom(self) -> int:
        """Bytes a plan may still allocate: `min(free, cap - used) - reserve`, never above `total - reserve`, >= 0."""
        if self.total <= 0:
            return 0
        room = self.free
        if self.cap is not None:
            room = min(room, max(0, self.cap - self.used))
        room = min(room, self.total)
        return max(0, min(room - self.reserve, self.total - self.reserve))

    @property
    def known(self) -> bool:
        """Whether the device could be read at all."""
        return self.total > 0


def read_device_budget(device: str, *, reserve: int, cap: int | None = None, probe: DeviceProbe | None = None) -> DeviceBudget:
    """Read `device`'s budget now; never raises (an unreadable device is all `0`)."""
    reading = (probe if probe is not None else TorchDeviceProbe()).query(device)
    if reading is None:
        reading = DeviceReading(total=0, free=0, torch_allocated=0, torch_reserved=0)
        log.info("device budget: %s cannot be queried; device plans will be refused", device)
    non_torch = max(0, reading.total - reading.free - reading.torch_reserved)
    return DeviceBudget(
        device=str(device),
        total=reading.total,
        free=reading.free,
        torch_allocated=reading.torch_allocated,
        torch_reserved=reading.torch_reserved,
        non_torch=non_torch,
        reserve=max(0, int(reserve)),
        cap=cap,
    )


# =====================================================================
#  Windows sysmem fallback
# =====================================================================
@dataclass(frozen=True)
class SpillVerdict:
    """Whether the driver spilled device allocations into system RAM.

    `overshoot` is how far this process's device usage went past
    `total - reserve`; `over_plan` how far it went past the planned peak.
    """

    spilled: bool
    overshoot: int
    over_plan: int


def detect_sysmem_spill(before: DeviceBudget, after: DeviceBudget, planned_peak: int) -> SpillVerdict:
    """Heuristic for NVIDIA's Windows sysmem fallback: allocations succeeded past physical memory.

    A spill is reported when this process's reserved bytes plus the non-torch
    usage seen BEFORE the run exceed `total - reserve` after it: on a card
    without the fallback such allocations raise instead. The planner never
    relies on it (it plans up to `total - reserve`); it feeds the degrade
    ladder and the user hint.
    """
    if after.total <= 0:
        return SpillVerdict(spilled=False, overshoot=0, over_plan=0)
    usage = after.torch_reserved + before.non_torch
    overshoot = usage - (after.total - after.reserve)
    over_plan = max(0, after.torch_reserved - before.torch_reserved - planned_peak)
    return SpillVerdict(spilled=overshoot > 0, overshoot=max(0, overshoot), over_plan=over_plan)


def step_time_suggests_spill(observed_s: float, forecast_s: float, *, factor: float = 3.0) -> bool:
    """A denoise step slower than `factor` x its forecast is the timing symptom of a spill."""
    if forecast_s <= 0.0:
        return False
    return observed_s > factor * forecast_s
