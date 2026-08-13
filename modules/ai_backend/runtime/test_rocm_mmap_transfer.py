"""
File: modules/ai_backend/runtime/test_rocm_mmap_transfer.py

Purpose:
Unit tests for the ROCm mmap->GPU staging workaround
(`rocm_mmap_transfer`). They pin the gating contract, the tensor fast paths, the
`/proc/self/maps` classifier, the load-session lifetime of its snapshot and the
thread-local `torch.nn.Module.to` patch.

Main responsibilities:
- verify `mmap_staging_required()` is `False` for the kill switch, a non-ROCm
  Torch build and a non-posix host;
- verify `tensor_needs_staging()` folds in the ROCm gate and the size threshold
  and follows the real mapping kind;
- verify `stage_cpu_tensor()` picks the stock path for every case that cannot
  hit the amdkfd pathology (no-op moves, dtype changes, small tensors, non-cuda
  targets) and never consults `/proc/self/maps` there;
- verify `non_blocking` reaches the final copy on both the stock and the staged
  path, and that the staged copy is a real anonymous clone with the source
  layout;
- verify a failed staging *copy* degrades to a direct transfer while a failed
  *transfer* propagates;
- verify the maps parser separates `rw-p` from `r--p`/`rw-s` and anonymous
  memory, on synthetic rows and on real mappings;
- verify the snapshot lives exactly one load session, nests, and cannot answer a
  lookup from a snapshot taken before an earlier session ended;
- verify the `Module.to` patch is armed per thread, reference counted, restored
  on exit and never bypasses stock dtype validation;
- verify `move_module_to` delegates every non-cuda target to stock Torch.

Notes:
- No GPU and no ROCm build are required: every ROCm-only decision is forced via
  monkeypatch and no test performs a host->device copy, except two explicitly
  `cuda`-gated ones that are skipped when no device is present.
- Where a contract can be checked against real Torch on the CPU it is, rather
  than against a stand-in; stand-ins are used only to observe *which* path was
  taken and with which arguments.
- Torch itself is required only as a CPU library; the whole module skips when it
  is not installed.
"""

from __future__ import annotations

import contextlib
import ctypes
import mmap
import os
import sys
import tempfile
import threading
import warnings
from pathlib import Path

import pytest

_MODULE_DIR = Path(__file__).resolve().parent
# `modules/ai_backend/runtime` -> parents[1] = `modules`, parents[2] = repo root.
_PROJECT_ROOT = _MODULE_DIR.parents[2]
if str(_PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(_PROJECT_ROOT))

torch = pytest.importorskip("torch", reason="rocm_mmap_transfer operates on torch tensors")

from modules.ai_backend.runtime import rocm_mmap_transfer as rmt  # noqa: E402


@pytest.fixture(autouse=True)
def _isolated_module_state(monkeypatch):
    """Reset caches, the kill switch and the thread state between tests."""
    monkeypatch.delenv(rmt._STAGING_ENV_FLAG, raising=False)
    monkeypatch.setattr(rmt, "_rocm_build", None)
    monkeypatch.setattr(rmt, "_maps_unavailable", False)
    monkeypatch.setattr(rmt, "_activation_logged", False)
    _reset_thread_state()
    yield
    # A test that failed mid-block must not leak an installed patch or an open
    # session into the next one.
    _reset_thread_state()
    rmt._patch_depth = 0
    rmt._patch_original = None


def _reset_thread_state() -> None:
    """Zero this thread's staging counters and drop its maps snapshot."""
    rmt._thread_state.patch_depth = 0
    rmt._thread_state.session_depth = 0
    rmt._thread_state.maps_snapshot = None


def _force_staging(monkeypatch, enabled: bool = True) -> None:
    """Pretend the host is (or is not) a ROCm machine that needs staging."""
    monkeypatch.setattr(rmt, "mmap_staging_required", lambda: enabled)


def _fail_on_lookup(_ptr: int) -> bool:
    """Stand-in for `_is_file_backed` that fails the test if it is consulted."""
    raise AssertionError("/proc/self/maps must not be consulted on the fast path")


class _RecordingTo:
    """Stand-in for `_stock_tensor_to` recording every delegated copy."""

    def __init__(self, result=None, error: BaseException | None = None) -> None:
        self.calls: list[tuple] = []
        self._result = result
        self._error = error

    def __call__(self, tensor, device, dtype, non_blocking):
        self.calls.append((tensor, device, dtype, non_blocking))
        if self._error is not None:
            raise self._error
        return tensor if self._result is None else self._result


def _cpu_tensor(nbytes: int = 4 << 20, dtype=torch.float32):
    """A CPU tensor of at least `nbytes`, large enough to pass the threshold."""
    return torch.zeros(nbytes // torch.empty(0, dtype=dtype).element_size(), dtype=dtype)


# --------------------------------------------------------------------------
# mmap_staging_required gating
# --------------------------------------------------------------------------


def test_kill_switch_disables_staging(monkeypatch):
    monkeypatch.setattr(rmt.os, "name", "posix")
    monkeypatch.setattr(rmt, "_is_rocm_build", lambda: True)
    monkeypatch.setenv(rmt._STAGING_ENV_FLAG, "0")
    assert rmt.mmap_staging_required() is False


def test_non_rocm_build_disables_staging(monkeypatch):
    monkeypatch.setattr(rmt.os, "name", "posix")
    # A CUDA/CPU build reports `torch.version.hip is None`.
    monkeypatch.setattr(torch.version, "hip", None, raising=False)
    assert rmt.mmap_staging_required() is False

    # An empty string must be treated the same way as a missing value.
    monkeypatch.setattr(rmt, "_rocm_build", None)
    monkeypatch.setattr(torch.version, "hip", "  ", raising=False)
    assert rmt.mmap_staging_required() is False


def test_missing_hip_attribute_disables_staging(monkeypatch):
    monkeypatch.setattr(rmt.os, "name", "posix")
    monkeypatch.delattr(torch.version, "hip", raising=False)
    assert rmt.mmap_staging_required() is False


def test_rocm_build_on_posix_enables_staging(monkeypatch):
    monkeypatch.setattr(rmt.os, "name", "posix")
    monkeypatch.setattr(torch.version, "hip", "7.2.53211", raising=False)
    assert rmt.mmap_staging_required() is True


def test_non_posix_disables_staging(monkeypatch):
    monkeypatch.setattr(rmt.os, "name", "nt")
    monkeypatch.setattr(torch.version, "hip", "7.2.53211", raising=False)
    assert rmt.mmap_staging_required() is False


# --------------------------------------------------------------------------
# stage_cpu_tensor fast paths
# --------------------------------------------------------------------------


def test_stage_cpu_tensor_rejects_non_tensor():
    with pytest.raises(TypeError):
        rmt.stage_cpu_tensor(object(), "cpu")


def test_stage_cpu_tensor_returns_same_object_for_noop_moves(monkeypatch):
    _force_staging(monkeypatch)
    # A maps lookup on any of these paths would already be a contract break.
    monkeypatch.setattr(rmt, "_is_file_backed", _fail_on_lookup)

    tensor = torch.zeros(4, 4)
    assert rmt.stage_cpu_tensor(tensor, None) is tensor
    assert rmt.stage_cpu_tensor(tensor, "cpu") is tensor
    assert rmt.stage_cpu_tensor(tensor, torch.device("cpu")) is tensor
    assert rmt.stage_cpu_tensor(tensor, "cpu", torch.float32) is tensor


def test_stage_cpu_tensor_dtype_only_conversion(monkeypatch):
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", _fail_on_lookup)

    tensor = torch.zeros(4, 4)
    converted = rmt.stage_cpu_tensor(tensor, None, torch.float64)
    assert converted is not tensor
    assert converted.dtype == torch.float64


def test_small_tensor_skips_maps_lookup(monkeypatch):
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", _fail_on_lookup)

    small = torch.zeros(16, 16, dtype=torch.float32)  # 1 KiB, below the threshold
    assert rmt._should_stage(small, torch.device("cuda")) is False


def test_staging_threshold_is_exactly_one_mib(monkeypatch):
    """The threshold is inclusive: exactly 1 MiB stages, one element less does not."""
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", lambda _ptr: True)

    exact = torch.zeros(rmt._MIN_STAGE_BYTES // 4, dtype=torch.float32)
    assert exact.numel() * exact.element_size() == rmt._MIN_STAGE_BYTES
    assert rmt._should_stage(exact, torch.device("cuda")) is True

    monkeypatch.setattr(rmt, "_is_file_backed", _fail_on_lookup)
    just_below = torch.zeros(rmt._MIN_STAGE_BYTES // 4 - 1, dtype=torch.float32)
    assert rmt._should_stage(just_below, torch.device("cuda")) is False


def test_non_cuda_target_skips_maps_lookup(monkeypatch):
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", _fail_on_lookup)

    big = _cpu_tensor()
    assert rmt._should_stage(big, torch.device("cpu")) is False


def test_disabled_staging_skips_maps_lookup(monkeypatch):
    _force_staging(monkeypatch, enabled=False)
    monkeypatch.setattr(rmt, "_is_file_backed", _fail_on_lookup)

    big = _cpu_tensor()
    assert rmt._should_stage(big, torch.device("cuda")) is False


def test_large_file_backed_cpu_tensor_is_staged(monkeypatch):
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", lambda _ptr: True)

    big = _cpu_tensor()
    assert rmt._should_stage(big, torch.device("cuda")) is True

    monkeypatch.setattr(rmt, "_is_file_backed", lambda _ptr: False)
    assert rmt._should_stage(big, torch.device("cuda")) is False


def test_dtype_change_takes_the_stock_path_without_a_lookup(monkeypatch):
    """A cast allocates anonymous memory on the host, so staging is pointless."""
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", _fail_on_lookup)
    recorder = _RecordingTo(result="moved")
    monkeypatch.setattr(rmt, "_stock_tensor_to", recorder)

    big = _cpu_tensor()
    assert rmt.stage_cpu_tensor(big, "cuda:0", torch.float16) == "moved"
    (tensor, device, dtype, non_blocking), = recorder.calls
    assert tensor is big
    assert device == torch.device("cuda:0")
    assert dtype is torch.float16
    assert non_blocking is False


def test_stock_tensor_to_forwards_every_argument():
    """`_stock_tensor_to` is exactly the per-tensor overload stock Torch uses."""
    recorded: list[tuple] = []

    class _Recording(torch.Tensor):
        """A real tensor recording how `.to()` was invoked."""

        def to(self, *args, **kwargs):
            recorded.append((args, kwargs))
            return self

    tensor = torch.zeros(4).as_subclass(_Recording)
    rmt._stock_tensor_to(tensor, torch.device("cpu"), torch.float64, True)
    assert recorded == [((torch.device("cpu"), torch.float64, True), {})]


def test_non_blocking_reaches_the_stock_path(monkeypatch):
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", lambda _ptr: False)
    recorder = _RecordingTo(result="moved")
    monkeypatch.setattr(rmt, "_stock_tensor_to", recorder)

    big = _cpu_tensor()
    rmt.stage_cpu_tensor(big, "cuda:0", non_blocking=True)
    assert recorder.calls[0][0] is big
    assert recorder.calls[0][3] is True


def test_staging_clones_into_anonymous_memory_and_keeps_non_blocking(monkeypatch):
    """The staged copy is a real clone with the source values and layout."""
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", lambda _ptr: True)
    recorder = _RecordingTo()
    monkeypatch.setattr(rmt, "_stock_tensor_to", recorder)

    # Non-contiguous on purpose: `clone()` defaults to `preserve_format`, which
    # is the layout stock `Tensor.to()` would have produced.
    source = torch.rand(512, 2048).t()
    assert not source.is_contiguous()

    rmt.stage_cpu_tensor(source, "cuda:0", non_blocking=True)
    staged, device, dtype, non_blocking = recorder.calls[0]
    assert staged is not source
    assert staged.data_ptr() != source.data_ptr()
    assert staged.stride() == source.stride()
    assert torch.equal(staged, source)
    assert device == torch.device("cuda:0")
    assert dtype is None
    assert non_blocking is True


def test_failed_staging_copy_falls_back_to_a_direct_transfer(monkeypatch, caplog):
    """Running out of host memory for the clone must not fail the load."""
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", lambda _ptr: True)
    recorder = _RecordingTo(result="moved")
    monkeypatch.setattr(rmt, "_stock_tensor_to", recorder)

    class _CloneFails(torch.Tensor):
        """A real tensor whose staging copy cannot be allocated."""

        def clone(self, *args, **kwargs):
            raise MemoryError("cannot allocate the staging copy")

    source = _cpu_tensor().as_subclass(_CloneFails)
    with caplog.at_level("WARNING"):
        assert rmt.stage_cpu_tensor(source, "cuda:0") == "moved"
    # The fallback transfers the original tensor, not a clone.
    assert recorder.calls[0][0] is source
    assert "Falling back to a direct transfer" in caplog.text


def test_failed_transfer_propagates(monkeypatch):
    """A failure of the copy itself is a real error and must not be swallowed."""
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", lambda _ptr: True)
    recorder = _RecordingTo(error=RuntimeError("HIP out of memory"))
    monkeypatch.setattr(rmt, "_stock_tensor_to", recorder)

    with pytest.raises(RuntimeError, match="HIP out of memory"):
        rmt.stage_cpu_tensor(_cpu_tensor(), "cuda:0")
    # Exactly one attempt: a failed transfer must not be retried unstaged.
    assert len(recorder.calls) == 1


# --------------------------------------------------------------------------
# tensor_needs_staging (public probe)
# --------------------------------------------------------------------------


def test_tensor_needs_staging_rejects_non_tensor():
    with pytest.raises(TypeError):
        rmt.tensor_needs_staging(object())


def test_tensor_needs_staging_follows_the_file_mapping(monkeypatch):
    _force_staging(monkeypatch)
    big = _cpu_tensor()

    monkeypatch.setattr(rmt, "_is_file_backed", lambda _ptr: True)
    assert rmt.tensor_needs_staging(big) is True

    monkeypatch.setattr(rmt, "_is_file_backed", lambda _ptr: False)
    assert rmt.tensor_needs_staging(big) is False


def test_tensor_needs_staging_folds_in_the_gate_and_the_threshold(monkeypatch):
    """A `False` must always mean "nothing to gain", never "I do not know"."""
    monkeypatch.setattr(rmt, "_is_file_backed", _fail_on_lookup)

    # Off a ROCm build the answer is `False` without any lookup at all, so a
    # caller cannot round trip gigabytes on a CUDA or CPU host by forgetting it.
    _force_staging(monkeypatch, enabled=False)
    assert rmt.tensor_needs_staging(_cpu_tensor()) is False

    _force_staging(monkeypatch)
    # Below the threshold there is nothing to gain either.
    assert rmt.tensor_needs_staging(torch.zeros(16, 16)) is False
    # A tensor that is not on the CPU has nothing left to re-home.
    assert rmt.tensor_needs_staging(torch.zeros(1 << 20, device="meta")) is False


@pytest.mark.skipif(not os.path.exists("/proc/self/maps"), reason="Linux-only /proc/self/maps")
@pytest.mark.parametrize(
    ("writable", "shared", "expected"),
    [(True, False, True), (False, False, False), (True, True, False)],
    ids=["rw-p", "r--p", "rw-s"],
)
def test_tensor_needs_staging_on_real_mappings(monkeypatch, writable, shared, expected):
    """End-to-end over real mappings, the way `flux_fill` probes a component."""
    _force_staging(monkeypatch)
    numpy = pytest.importorskip("numpy", reason="needed to wrap a mapping in a tensor")
    with tempfile.NamedTemporaryFile(prefix="ms_probe_", suffix=".bin") as handle:
        handle.write(b"\0" * (2 << 20))
        handle.flush()
        with _private_file_mapping(handle.name, writable=writable, shared=shared) as (_a, view):
            array = numpy.frombuffer(view, dtype=numpy.float32)
            with warnings.catch_warnings():
                # `from_numpy` warns about a read-only array; the tensor is only
                # ever read here, and a read-only source is one of the cases
                # under test.
                warnings.simplefilter("ignore", UserWarning)
                mapped = torch.from_numpy(array)
            assert mapped.numel() * mapped.element_size() >= rmt._MIN_STAGE_BYTES
            assert rmt.tensor_needs_staging(mapped) is expected
            # The mapping cannot be unmapped while a tensor still exports it.
            del array, mapped

    # Anonymous host memory is never worth staging.
    assert rmt.tensor_needs_staging(_cpu_tensor()) is False


# --------------------------------------------------------------------------
# device normalization
# --------------------------------------------------------------------------


def test_normalize_device_keeps_cuda_index_less_for_a_cpu_tensor():
    """Resolving the current CUDA device must not be triggered by a CPU tensor."""
    cpu = torch.zeros(4)
    assert rmt._normalize_device("cuda", cpu) == torch.device("cuda")
    assert rmt._normalize_device("cuda", cpu).index is None
    assert rmt._normalize_device("cuda:1", cpu) == torch.device("cuda", 1)
    assert rmt._normalize_device(torch.device("cuda", 1), cpu).index == 1


@pytest.mark.skipif(not torch.cuda.is_available(), reason="needs a CUDA/ROCm device")
def test_normalize_device_resolves_cuda_index_for_a_device_tensor():
    """`cuda` must resolve to the current index so `already_there` can compare."""
    on_device = torch.zeros(4, device="cuda")
    resolved = rmt._normalize_device("cuda", on_device)
    assert resolved.index == torch.cuda.current_device()
    # A tensor already on the target is returned unchanged, without a lookup.
    assert rmt.stage_cpu_tensor(on_device, "cuda") is on_device


@pytest.mark.skipif(not torch.cuda.is_available(), reason="needs a CUDA/ROCm device")
def test_staged_transfer_matches_stock_on_a_real_file_mapping(monkeypatch):
    """End-to-end on real hardware: staged values equal the mapped source."""
    _force_staging(monkeypatch)
    with tempfile.NamedTemporaryFile(prefix="ms_stage_probe_", suffix=".bin") as handle:
        payload = torch.rand(1 << 19, dtype=torch.float32)  # 2 MiB
        handle.write(payload.numpy().tobytes())
        handle.flush()
        with _private_file_mapping(handle.name, writable=True) as (address, view):
            source = torch.frombuffer(view, dtype=torch.float32)
            assert rmt._is_file_backed(address) is True
            moved = rmt.stage_cpu_tensor(source, "cuda")
            assert moved.device.type == "cuda"
            assert torch.equal(moved.cpu(), payload)
            # The mapping cannot be unmapped while a tensor still exports it.
            del source


# --------------------------------------------------------------------------
# /proc/self/maps parsing and lookup
# --------------------------------------------------------------------------


def test_lookup_on_synthetic_snapshot():
    snapshot = rmt._MapsSnapshot(
        starts=[0x1000, 0x3000, 0x9000],
        ends=[0x2000, 0x4000, 0xA000],
        writable_private=[False, True, False],
    )
    assert rmt._lookup(snapshot, 0x0FFF) is None  # before the first mapping
    assert rmt._lookup(snapshot, 0x1000) is False  # inclusive lower bound
    assert rmt._lookup(snapshot, 0x1FFF) is False
    assert rmt._lookup(snapshot, 0x2000) is None  # exclusive upper bound (a hole)
    assert rmt._lookup(snapshot, 0x3500) is True
    assert rmt._lookup(snapshot, 0xFFFF) is None  # past the last mapping


def test_parse_maps_line_only_accepts_writable_private_file_mappings():
    """Only `rw-p` over a real path triggers the amdkfd stall (measured)."""
    path = "/srv/models/model.safetensors"
    lo, hi = 0x7F2B0C000000, 0x7F2B0C400000

    writable_private = f"7f2b0c000000-7f2b0c400000 rw-p 00000000 08:02 1234567  {path}"
    assert rmt._parse_maps_line(writable_private) == (lo, hi, True)

    read_only_private = f"7f2b0c000000-7f2b0c400000 r--p 00000000 08:02 1234567  {path}"
    assert rmt._parse_maps_line(read_only_private) == (lo, hi, False)

    writable_shared = f"7f2b0c000000-7f2b0c400000 rw-s 00000000 08:02 1234567  {path}"
    assert rmt._parse_maps_line(writable_shared) == (lo, hi, False)

    executable_private = f"7f2b0c000000-7f2b0c400000 r-xp 00000000 08:02 1234567  {path}"
    assert rmt._parse_maps_line(executable_private) == (lo, hi, False)


def test_parse_maps_line_variants():
    anon_row = "7f2b0d000000-7f2b0d001000 rw-p 00000000 00:00 0 "
    assert rmt._parse_maps_line(anon_row) == (0x7F2B0D000000, 0x7F2B0D001000, False)

    # Bracketed pseudo-mappings are anonymous, not file-backed.
    heap_row = "55d0c0000000-55d0c0021000 rw-p 00000000 00:00 0                  [heap]"
    assert rmt._parse_maps_line(heap_row) == (0x55D0C0000000, 0x55D0C0021000, False)

    assert rmt._parse_maps_line("garbage without a range") is None
    assert rmt._parse_maps_line("zzzz-yyyy rw-p 0 0:0 0") is None
    # Empty and inverted ranges cover nothing and must not enter the snapshot.
    assert rmt._parse_maps_line("1000-1000 rw-p 0 00:00 0 /tmp/x") is None
    assert rmt._parse_maps_line("2000-1000 rw-p 0 00:00 0 /tmp/x") is None
    # Truncated rows have no usable perms field.
    assert rmt._parse_maps_line("1000-2000 rw-p") is None
    assert rmt._parse_maps_line("1000-2000 rw 0 00:00 0") is None


@contextlib.contextmanager
def _private_file_mapping(path: str, writable: bool, shared: bool = False):
    """Map `path` and yield `(address, buffer)`, unmapping on exit.

    `writable`/`shared` select the `rw-p`, `r--p` and `rw-s` mapping kinds the
    classifier must tell apart. The address is taken through `numpy`, which -
    unlike `ctypes.from_buffer` - also works for a read-only mapping.
    """
    numpy = pytest.importorskip("numpy", reason="needed to read a mapping's address")
    prot = mmap.PROT_READ | (mmap.PROT_WRITE if writable else 0)
    flags = mmap.MAP_SHARED if shared else mmap.MAP_PRIVATE
    fd = os.open(path, os.O_RDWR if writable else os.O_RDONLY)
    try:
        mapping = mmap.mmap(fd, 0, flags=flags, prot=prot)
    finally:
        os.close(fd)
    array = numpy.frombuffer(mapping, dtype=numpy.uint8)
    try:
        yield int(array.ctypes.data), mapping
    finally:
        del array
        mapping.close()


@pytest.mark.skipif(not os.path.exists("/proc/self/maps"), reason="Linux-only /proc/self/maps")
@pytest.mark.parametrize(
    ("writable", "shared", "expected"),
    [(True, False, True), (False, False, False), (True, True, False)],
    ids=["rw-p", "r--p", "rw-s"],
)
def test_is_file_backed_on_real_mappings(writable, shared, expected):
    """Only a real `rw-p` file mapping is classified as needing staging."""
    with tempfile.NamedTemporaryFile(prefix="ms_maps_probe_", suffix=".bin") as handle:
        handle.write(b"\0" * (2 << 20))
        handle.flush()
        with _private_file_mapping(handle.name, writable=writable, shared=shared) as (addr, _m):
            # The mapping was created after any earlier snapshot, so this also
            # exercises the single stale-snapshot refresh.
            assert rmt._is_file_backed(addr) is expected


@pytest.mark.skipif(not os.path.exists("/proc/self/maps"), reason="Linux-only /proc/self/maps")
def test_anonymous_memory_is_not_file_backed():
    anon = torch.empty(4 << 20, dtype=torch.uint8)
    assert rmt._is_file_backed(anon.data_ptr()) is False
    # A `ctypes` buffer is anonymous heap memory too.
    buffer = ctypes.create_string_buffer(4 << 20)
    assert rmt._is_file_backed(ctypes.addressof(buffer)) is False


def test_unreadable_maps_degrades_to_stock(monkeypatch):
    """An unreadable /proc/self/maps disables staging instead of raising."""
    monkeypatch.setattr(rmt, "_read_maps", lambda: None)
    assert rmt._is_file_backed(0x1234) is False
    # The failure is sticky, so the next tensor does not retry the read.
    monkeypatch.setattr(rmt, "_read_maps", _fail_on_second_read)
    assert rmt._is_file_backed(0x1234) is False


def _fail_on_second_read():
    raise AssertionError("/proc/self/maps must not be re-read after a failure")


# --------------------------------------------------------------------------
# snapshot lifetime: one load session
# --------------------------------------------------------------------------


def _snapshot(lo: int, hi: int, writable_private: bool) -> rmt._MapsSnapshot:
    """A one-row snapshot covering `[lo, hi)`."""
    return rmt._MapsSnapshot(starts=[lo], ends=[hi], writable_private=[writable_private])


class _ScriptedMaps:
    """Stand-in for `_read_maps` handing out a scripted sequence of snapshots."""

    def __init__(self, *snapshots: rmt._MapsSnapshot) -> None:
        self._snapshots = list(snapshots)
        self.reads = 0

    def __call__(self) -> rmt._MapsSnapshot:
        index = min(self.reads, len(self._snapshots) - 1)
        self.reads += 1
        return self._snapshots[index]


def test_snapshot_is_built_once_per_session(monkeypatch):
    reader = _ScriptedMaps(_snapshot(0x1000, 0x2000, True))
    monkeypatch.setattr(rmt, "_read_maps", reader)

    with rmt._maps_session():
        assert rmt._is_file_backed(0x1100) is True
        assert rmt._is_file_backed(0x1200) is True
    assert reader.reads == 1
    # The snapshot does not outlive the session.
    assert rmt._thread_state.maps_snapshot is None


def test_snapshot_is_not_retained_outside_a_session(monkeypatch):
    """A bare lookup cannot trust a snapshot of unknown age, so it re-reads."""
    reader = _ScriptedMaps(_snapshot(0x1000, 0x2000, True))
    monkeypatch.setattr(rmt, "_read_maps", reader)

    assert rmt._is_file_backed(0x1100) is True
    assert rmt._is_file_backed(0x1100) is True
    assert reader.reads == 2
    assert rmt._thread_state.maps_snapshot is None


def test_nested_sessions_release_the_snapshot_only_at_the_outermost_exit(monkeypatch):
    reader = _ScriptedMaps(_snapshot(0x1000, 0x2000, True))
    monkeypatch.setattr(rmt, "_read_maps", reader)

    with rmt._maps_session():
        assert rmt._is_file_backed(0x1100) is True
        with rmt._maps_session():
            assert rmt._is_file_backed(0x1100) is True
        assert rmt._thread_state.maps_snapshot is not None
        assert rmt._is_file_backed(0x1100) is True
    assert rmt._thread_state.maps_snapshot is None
    assert reader.reads == 1


def test_recycled_address_is_reclassified_in_the_next_session(monkeypatch):
    """The regression that motivated session-scoped snapshots.

    A region that was anonymous during one load is freed and handed out again as
    a file mapping for the next one. A process-wide cache answered `False` from
    the stale row and silently lost the workaround; a session-scoped snapshot
    cannot.
    """
    reader = _ScriptedMaps(
        _snapshot(0x1000, 0x2000, False),  # first load: anonymous memory
        _snapshot(0x1000, 0x2000, True),  # second load: a checkpoint mapping
    )
    monkeypatch.setattr(rmt, "_read_maps", reader)

    with rmt._maps_session():
        assert rmt._is_file_backed(0x1100) is False
    with rmt._maps_session():
        assert rmt._is_file_backed(0x1100) is True
    assert reader.reads == 2


def test_uncovered_pointer_refreshes_the_snapshot_once(monkeypatch):
    """A checkpoint mapped after the snapshot was taken forces exactly one re-read."""
    reader = _ScriptedMaps(
        _snapshot(0x1000, 0x2000, True),  # does not cover the probed address
        _snapshot(0x8000, 0x9000, True),
    )
    monkeypatch.setattr(rmt, "_read_maps", reader)

    with rmt._maps_session():
        assert rmt._is_file_backed(0x8100) is True
        assert reader.reads == 2
        # A second miss is not retried again: the refreshed snapshot is current.
        assert rmt._is_file_backed(0x0100) is False
        assert reader.reads == 3


def test_invalidate_maps_cache_forces_a_reread(monkeypatch):
    reader = _ScriptedMaps(_snapshot(0x1000, 0x2000, True))
    monkeypatch.setattr(rmt, "_read_maps", reader)

    with rmt._maps_session():
        assert rmt._is_file_backed(0x1100) is True
        assert rmt._thread_state.maps_snapshot is not None
        rmt.invalidate_maps_cache()
        assert rmt._thread_state.maps_snapshot is None
        assert rmt._is_file_backed(0x1100) is True
    assert reader.reads == 2


# --------------------------------------------------------------------------
# patched_module_to
# --------------------------------------------------------------------------


def test_patch_restores_module_to():
    original = torch.nn.Module.to
    with rmt.patched_module_to(enabled=True):
        assert torch.nn.Module.to is not original
        assert rmt._thread_state.patch_depth == 1
    assert torch.nn.Module.to is original
    assert rmt._thread_state.patch_depth == 0


def test_patch_restores_module_to_on_exception():
    original = torch.nn.Module.to
    with pytest.raises(ValueError):
        with rmt.patched_module_to(enabled=True):
            raise ValueError("boom")
    assert torch.nn.Module.to is original
    assert rmt._thread_state.patch_depth == 0
    assert rmt._thread_state.session_depth == 0


def test_nested_patch_restores_only_at_the_outermost_exit():
    original = torch.nn.Module.to
    with rmt.patched_module_to(enabled=True):
        patched = torch.nn.Module.to
        with rmt.patched_module_to(enabled=True):
            assert torch.nn.Module.to is patched
            assert rmt._thread_state.patch_depth == 2
        # The inner block must not restore while the outer one is still active.
        assert torch.nn.Module.to is patched
        assert rmt._thread_state.patch_depth == 1
    assert torch.nn.Module.to is original
    assert rmt._patch_depth == 0


def test_disabled_patch_is_a_noop():
    original = torch.nn.Module.to
    with rmt.patched_module_to(enabled=False):
        assert torch.nn.Module.to is original
        assert rmt._thread_state.patch_depth == 0
    assert torch.nn.Module.to is original


def test_patched_to_delegates_non_cuda_calls():
    module = torch.nn.Linear(4, 4)
    weight = module.weight
    with rmt.patched_module_to(enabled=True):
        # A CPU move and a dtype-only cast must both take the stock path.
        assert module.to("cpu") is module
        assert module.weight is weight
        module.to(torch.float64)
    assert module.weight.dtype == torch.float64


def test_patched_to_routes_cuda_calls_through_staging(monkeypatch):
    """A cuda target is re-routed; no GPU is touched because staging is faked."""
    calls: list[tuple] = []

    def fake_apply(module, device, dtype, non_blocking=False):
        calls.append((str(device), dtype, non_blocking))
        return module

    monkeypatch.setattr(rmt, "_apply_staged", fake_apply)
    module = torch.nn.Linear(4, 4)
    with rmt.patched_module_to(enabled=True):
        assert module.to("cuda:0", torch.float16) is module
        assert module.to("cuda:0", torch.float16, True) is module
    assert calls == [("cuda:0", torch.float16, False), ("cuda:0", torch.float16, True)]


def test_patched_to_delegates_when_a_memory_format_is_requested(monkeypatch):
    """`memory_format` is not supported by staging, so the call goes to stock."""
    monkeypatch.setattr(rmt, "_apply_staged", _fail_on_apply)
    # An empty module has no tensors, so the stock path touches no device.
    module = torch.nn.Module()
    with rmt.patched_module_to(enabled=True):
        assert module.to("cuda:0", memory_format=torch.channels_last) is module


def _fail_on_apply(*_args, **_kwargs):
    raise AssertionError("this call must take the stock Module.to path")


def test_patch_is_armed_per_thread(monkeypatch):
    """A thread that never entered the block must not see the patch at all."""
    seen: list[str] = []

    def fake_apply(module, device, dtype, non_blocking=False):
        seen.append(threading.current_thread().name)
        return module

    monkeypatch.setattr(rmt, "_apply_staged", fake_apply)
    # An empty module has no tensors, so the stock path touches no device.
    module = torch.nn.Module()
    errors: list[BaseException] = []

    def unrelated_worker():
        try:
            # Deliberately the call the review flagged: another thread doing an
            # unrelated `non_blocking` move while the patch is installed.
            module.to("cuda:0", non_blocking=True)
        except Exception as exc:  # re-raised in the main thread, never swallowed
            errors.append(exc)

    with rmt.patched_module_to(enabled=True):
        worker = threading.Thread(target=unrelated_worker, name="unrelated")
        worker.start()
        worker.join()
        module.to("cuda:0")

    if errors:
        raise errors[0]
    assert seen == [threading.current_thread().name]


def test_worker_thread_has_no_session_of_its_own():
    """The load session is thread-local, like the arming flag."""
    observed: list[tuple[int, int]] = []

    def worker():
        observed.append(
            (rmt._thread_state.patch_depth, rmt._thread_state.session_depth)
        )

    with rmt.patched_module_to(enabled=True):
        thread = threading.Thread(target=worker)
        thread.start()
        thread.join()
    assert observed == [(0, 0)]


def test_patched_to_rejects_a_non_float_dtype_like_stock():
    """`Module.to(cuda, int64)` must fail exactly as stock Torch fails."""
    module = torch.nn.Linear(4, 4)
    with pytest.raises(TypeError) as stock:
        module.to("cpu", torch.int64)

    with rmt.patched_module_to(enabled=True):
        with pytest.raises(TypeError) as staged:
            module.to("cuda:0", torch.int64)
    assert str(staged.value) == str(stock.value)
    # Nothing was converted before the rejection.
    assert module.weight.dtype == torch.float32


# --------------------------------------------------------------------------
# move_module_to
# --------------------------------------------------------------------------


@pytest.mark.parametrize("staging", [False, True])
def test_move_module_to_cpu_matches_stock(monkeypatch, staging):
    _force_staging(monkeypatch, enabled=staging)
    monkeypatch.setattr(rmt, "_is_file_backed", _fail_on_lookup)
    monkeypatch.setattr(rmt, "_apply_staged", _fail_on_apply)

    module = torch.nn.Linear(4, 4)
    weight = module.weight
    reference = torch.nn.Linear(4, 4)
    reference.load_state_dict(module.state_dict())

    assert rmt.move_module_to(module, "cpu") is module
    # A CPU->CPU move rebinds nothing and copies nothing.
    assert module.weight is weight
    assert torch.equal(module.weight, reference.weight)
    assert module.weight.device.type == "cpu"


def test_move_module_to_cpu_with_dtype(monkeypatch):
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", _fail_on_lookup)
    monkeypatch.setattr(rmt, "_apply_staged", _fail_on_apply)

    module = torch.nn.Linear(4, 4)
    module.register_buffer("counter", torch.zeros(3, dtype=torch.int64))

    rmt.move_module_to(module, "cpu", torch.float64)
    assert module.weight.dtype == torch.float64
    assert module.bias.dtype == torch.float64
    # Stock `Module.to` casts floating-point/complex tensors only.
    assert module.counter.dtype == torch.int64


def test_move_module_to_non_cuda_keeps_stock_dtype_validation(monkeypatch):
    """A `cpu` target must not lose stock Torch's dtype rejection."""
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_apply_staged", _fail_on_apply)

    module = torch.nn.Linear(4, 4)
    with pytest.raises(TypeError, match="only accepts floating point or complex"):
        rmt.move_module_to(module, "cpu", torch.int64)


def test_move_module_to_cuda_rejects_a_non_float_dtype(monkeypatch):
    """The staged path validates `dtype` before it touches a single tensor."""
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", _fail_on_lookup)

    module = torch.nn.Linear(4, 4)
    with pytest.raises(TypeError, match="only accepts floating point or complex"):
        rmt.move_module_to(module, "cuda:0", torch.int64)
    assert module.weight.dtype == torch.float32


def test_move_module_to_delegates_unparsable_arguments(monkeypatch):
    """A `device` Torch alone can interpret goes to stock, diagnostics included."""
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_apply_staged", _fail_on_apply)

    module = torch.nn.Linear(4, 4)
    # The `to(tensor)` overload: not a device spec, so stock Torch resolves it.
    assert rmt.move_module_to(module, torch.zeros(1, dtype=torch.float64)) is module
    assert module.weight.dtype == torch.float64


def test_move_module_to_opens_a_session(monkeypatch):
    """One `/proc/self/maps` snapshot serves the whole module, then is dropped."""
    _force_staging(monkeypatch)
    reader = _ScriptedMaps(_snapshot(0x1000, 0x2000, False))
    monkeypatch.setattr(rmt, "_read_maps", reader)
    depths: list[int] = []

    def spy_stage(tensor, device, dtype, *, non_blocking=False):
        depths.append(rmt._thread_state.session_depth)
        rmt._is_file_backed(0x1100)
        return tensor

    monkeypatch.setattr(rmt, "stage_cpu_tensor", spy_stage)
    rmt.move_module_to(torch.nn.Linear(4, 4), "cuda:0")

    assert depths == [1, 1]  # weight and bias, both inside one session
    assert reader.reads == 1
    assert rmt._thread_state.maps_snapshot is None


# --------------------------------------------------------------------------
# _apply_staged traversal contract
# --------------------------------------------------------------------------


def test_apply_staged_preserves_object_level_weight_tying(monkeypatch):
    """The same `Parameter` object registered twice stays one object."""
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", _fail_on_lookup)

    shared = torch.nn.Parameter(torch.zeros(4))
    root = torch.nn.Module()
    root.register_parameter("head", shared)
    root.tail = torch.nn.Module()
    root.tail.register_parameter("head", shared)

    rmt._apply_staged(root, torch.device("cpu"), torch.float64)
    assert root.head is root.tail.head
    assert root.head.dtype == torch.float64


def test_storage_level_sharing_is_split_exactly_like_stock():
    """Two distinct parameters over one storage are split by stock Torch too."""
    base = torch.zeros(8)
    module = torch.nn.Module()
    module.register_parameter("first", torch.nn.Parameter(base[:4]))
    module.register_parameter("second", torch.nn.Parameter(base[4:]))
    assert module.first.untyped_storage().data_ptr() == module.second.untyped_storage().data_ptr()

    module.to(torch.float64)
    assert module.first.untyped_storage().data_ptr() != module.second.untyped_storage().data_ptr()


def test_apply_staged_keeps_the_meta_tensor_hint(monkeypatch):
    """Bypassing stock `Module.to` must not lose its meta-tensor diagnostic."""
    _force_staging(monkeypatch)
    monkeypatch.setattr(rmt, "_is_file_backed", _fail_on_lookup)

    module = torch.nn.Linear(4, 4, device="meta")
    with pytest.raises(NotImplementedError, match="to_empty"):
        rmt._apply_staged(module, torch.device("cpu"), None)
