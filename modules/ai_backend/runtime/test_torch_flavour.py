"""
File: modules/ai_backend/runtime/test_torch_flavour.py

Purpose:
Tests for `torch_flavour` (the one owner of "which torch build is this") and
characterization of every backend site that asks that question, so the four
sites provably agree with the owner.

Main responsibilities:
- `flavour_of` / `is_rocm_build` over a table of fake torch modules;
- the health facts: `torch_build_facts`, `gpu_device_facts`,
  `installed_rocm_device_archs`, `rocm_split_kernels_installed` and `torch_health_payload` over a fake torch,
  fake device properties and a fake `importlib.metadata` (shape, caching,
  never raising);
- the "site agreement" section: `rocm_runtime.configure_rocm_runtime`,
  `rocm_mmap_transfer._is_rocm_build`, `large_models.cost_tables.detect_device_class`
  and `residency.arbiter._is_rocm_build` answer the same fake modules the way
  the owner does.

Notes:
- Every fake torch is a plain `types.ModuleType` injected into `sys.modules`
  through `monkeypatch`, so no real torch is needed and no GPU code runs.
- The site tests were written against the code BEFORE the owner existed and
  recorded its answers; only the rows marked "B1" changed when the sites were
  routed through the owner (B1: an empty or whitespace-only `torch.version.hip`
  is "not ROCm" everywhere, accepted by the manager).
"""

from __future__ import annotations

import sys
import types
from pathlib import Path

import pytest

_MODULE_DIR = Path(__file__).resolve().parent
# `modules/ai_backend/runtime` -> parents[1] = `modules`, parents[2] = repo root.
_PROJECT_ROOT = _MODULE_DIR.parents[2]
if str(_PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(_PROJECT_ROOT))

from modules import ai_device  # noqa: E402
from modules.ai_backend.runtime import rocm_mmap_transfer, rocm_runtime, torch_device, torch_flavour  # noqa: E402
from modules.ai_backend.runtime.large_models import cost_tables  # noqa: E402
from modules.ai_backend.runtime.residency import arbiter  # noqa: E402
from modules.ai_backend.runtime.torch_flavour import TorchFlavour, flavour_of, is_rocm_build  # noqa: E402

# Marker for "the fake module has no `version` attribute at all".
_NO_VERSION = object()

# Every environment key `configure_rocm_runtime` may write; restored per test.
_ROCM_ENV_KEYS = (
    "MIOPEN_FIND_MODE",
    "MIOPEN_USER_DB_PATH",
    "MIOPEN_CUSTOM_CACHE_DIR",
    "PYTORCH_CUDA_ALLOC_CONF",
    "PYTORCH_HIP_ALLOC_CONF",
    "PYTORCH_ALLOC_CONF",
    "MS_ALLOW_EXPANDABLE_SEGMENTS",
)


def _fake_torch(hip: object, cuda: object = None) -> types.ModuleType:
    """A fake `torch` with `version.hip` / `version.cuda` (or no `version` for `_NO_VERSION`).

    It also carries the minimum `backends.cudnn` and `cuda` surface the sites
    touch: one visible device whose properties name a gfx arch and an sm 8.6
    capability, so `detect_device_class` can tell the ROCm and CUDA branches apart.
    """
    torch = types.ModuleType("torch")
    if hip is not _NO_VERSION:
        torch.version = types.SimpleNamespace(hip=hip, cuda=cuda)
    torch.backends = types.SimpleNamespace(cudnn=types.SimpleNamespace(benchmark=True))
    props = types.SimpleNamespace(name="Fake GPU", gcnArchName="gfx1201:sramecc-:xnack-", major=8, minor=6)
    torch.cuda = types.SimpleNamespace(is_available=lambda: True, get_device_properties=lambda _index: props, current_device=lambda: 0)
    return torch


@pytest.mark.parametrize(("hip", "cuda", "expected"), [
    (None, None, TorchFlavour.CPU),
    (None, "12.8", TorchFlavour.CUDA),
    ("7.2.53211", None, TorchFlavour.ROCM),
    # A ROCm wheel may report a CUDA version too; hip wins.
    ("7.2.53211", "12.8", TorchFlavour.ROCM),
    ("", None, TorchFlavour.CPU),
    ("   ", "12.8", TorchFlavour.CUDA),
    (None, "  ", TorchFlavour.CPU),
    # Non-string values are never versions.
    (7.2, None, TorchFlavour.CPU),
    (None, 12, TorchFlavour.CPU),
    (_NO_VERSION, None, TorchFlavour.CPU),
])
def test_flavour_of_table(hip: object, cuda: object, expected: TorchFlavour) -> None:
    torch = _fake_torch(hip, cuda)
    assert flavour_of(torch) is expected
    assert is_rocm_build(torch) is (expected is TorchFlavour.ROCM)


def test_flavour_of_accepts_any_object() -> None:
    assert flavour_of(object()) is TorchFlavour.CPU
    assert flavour_of(None) is TorchFlavour.CPU
    assert is_rocm_build(types.SimpleNamespace(version=None)) is False


def test_flavour_values_are_stable_ids() -> None:
    assert [flavour.value for flavour in TorchFlavour] == ["cpu", "cuda", "rocm"]


def test_torch_flavour_imports_no_torch() -> None:
    import subprocess

    code = (
        "import sys\n"
        f"sys.path.insert(0, {str(_PROJECT_ROOT)!r})\n"
        "import modules.ai_backend.runtime.torch_flavour\n"
        "assert 'torch' not in sys.modules, 'torch_flavour imported torch'\n"
    )
    result = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, timeout=120, check=False)
    assert result.returncode == 0, result.stderr


@pytest.fixture
def install_torch(monkeypatch: pytest.MonkeyPatch, tmp_path: Path):
    """Install a fake torch and isolate every piece of global state the sites move."""
    for key in _ROCM_ENV_KEYS:
        monkeypatch.delenv(key, raising=False)
    monkeypatch.setattr(rocm_runtime, "configure_error_text", lambda **_kwargs: None)
    monkeypatch.setattr(rocm_runtime, "_resolve_cache_root", lambda: tmp_path)
    monkeypatch.setattr(rocm_mmap_transfer, "_rocm_build", None)
    monkeypatch.setattr(cost_tables, "is_torch_available", lambda: True)
    monkeypatch.setattr(arbiter, "is_torch_available", lambda: True)

    def install(hip: object) -> None:
        monkeypatch.setitem(sys.modules, "torch", _fake_torch(hip))

    return install


# (hip value, is ROCm per the owner). Old answers that differ are noted per site.
_SITE_ROWS = [
    (None, False),
    ("7.2.0", True),
    ("", False),
    ("   ", False),
    (_NO_VERSION, False),
]


@pytest.mark.parametrize(("hip", "rocm"), _SITE_ROWS)
def test_configure_rocm_runtime_agrees(install_torch, hip: object, rocm: bool) -> None:
    install_torch(hip)
    assert rocm_runtime.configure_rocm_runtime() is rocm


@pytest.mark.parametrize(("hip", "rocm"), _SITE_ROWS)
def test_mmap_transfer_is_rocm_build_agrees(install_torch, hip: object, rocm: bool) -> None:
    install_torch(hip)
    assert rocm_mmap_transfer._is_rocm_build() is rocm


@pytest.mark.parametrize(("hip", "expected"), [
    (None, "cuda_sm_86"),
    ("7.2.0", "rocm_gfx1201"),
    # B1: the code before the owner tested `hip is not None`, so an empty or
    # whitespace-only string read as ROCm here ("rocm_gfx1201").
    ("", "cuda_sm_86"),
    ("   ", "cuda_sm_86"),
    # The code before the owner read `torch.version` unguarded and raised
    # `AttributeError` for a torch without `version`, against its "never raises"
    # contract; the owner reads such a module as "not ROCm".
    (_NO_VERSION, "cuda_sm_86"),
])
def test_detect_device_class_agrees(install_torch, hip: object, expected: str) -> None:
    install_torch(hip)
    assert cost_tables.detect_device_class("cuda:0") == expected


@pytest.mark.parametrize(("hip", "rocm"), [
    (None, False),
    ("7.2.0", True),
    ("", False),
    # B1: the code before the owner tested `bool(hip)`, so a whitespace-only
    # string read as ROCm here.
    ("   ", False),
    (_NO_VERSION, False),
])
def test_arbiter_is_rocm_build_agrees(install_torch, hip: object, rocm: bool) -> None:
    install_torch(hip)
    assert arbiter._is_rocm_build() is rocm


@pytest.mark.parametrize(("hip", "rocm"), _SITE_ROWS)
def test_ai_device_build_kind_agrees(hip: object, rocm: bool) -> None:
    # `ai_device` diagnostics: (is CUDA build, is ROCm build). The code before the
    # owner tested `is not None`, so "" / whitespace read as ROCm and a torch
    # without `version` raised (B1 applies).
    is_cuda, is_rocm = ai_device._torch_build_kind(_fake_torch(hip))
    assert is_rocm is rocm
    assert is_cuda is False


def test_ai_device_build_kind_cuda() -> None:
    assert ai_device._torch_build_kind(_fake_torch(None, "12.8")) == (True, False)
    # A ROCm wheel reporting a CUDA version too is a ROCm build only.
    assert ai_device._torch_build_kind(_fake_torch("7.2.0", "12.8")) == (False, True)


# ---------------------------------------------------------------------------
# Health facts
# ---------------------------------------------------------------------------


class _FakeDist:
    """An `importlib.metadata.Distribution` stand-in carrying only `metadata["Name"]`."""

    def __init__(self, name: object) -> None:
        self.metadata = {"Name": name}


@pytest.fixture
def facts_env(monkeypatch: pytest.MonkeyPatch):
    """Empty fact caches, torch reported available, a fake torch, two fake devices and fake dists.

    Returns a namespace whose fields the test may change before the first call:
    `devices` (device-property objects), `dists` (distribution names) and
    `probe_calls` / `dist_calls` (call counters).
    """
    env = types.SimpleNamespace(
        devices=[
            types.SimpleNamespace(name="AMD Radeon AI PRO R9700", gcnArchName="gfx1201:sramecc-:xnack-", is_integrated=False),
            types.SimpleNamespace(name="AMD Ryzen 5 9600X 6-Core Processor", gcnArchName="GFX1036", is_integrated=False),
        ],
        dists=["torch", "rocm_sdk_core", "amd-torch-device-gfx1201", "amd_torch_device_gfx12_0", "AMD-Torch-Device-gfx1100", "amd-torch-device-gfx110x", "amd-torchvision-device-gfx1201", None],
        probe_calls=0,
        dist_calls=0,
    )
    for name in ("_BUILD_FACTS", "_DEVICE_FACTS", "_ROCM_WHEEL_SCAN"):
        monkeypatch.setattr(torch_flavour, name, None)
    monkeypatch.setattr(torch_flavour, "_PAYLOAD_FAILURE_LOGGED", False)
    monkeypatch.setattr(torch_flavour, "is_torch_available", lambda: True)
    torch = _fake_torch(" 10.1.0 ")
    torch.__version__ = "2.13.0+rocm10.1.0"
    monkeypatch.setitem(sys.modules, "torch", torch)

    def probe() -> list[object]:
        env.probe_calls += 1
        return list(env.devices)

    def distributions() -> list[_FakeDist]:
        env.dist_calls += 1
        return [_FakeDist(name) for name in env.dists]

    monkeypatch.setattr(torch_device, "_cuda_device_properties", probe)
    monkeypatch.setattr(torch_flavour.importlib_metadata, "distributions", distributions)
    return env


def test_torch_build_facts(facts_env) -> None:
    facts = torch_flavour.torch_build_facts()
    assert facts == torch_flavour.TorchBuildFacts(flavour=TorchFlavour.ROCM, version="2.13.0+rocm10.1.0", hip_version="10.1.0", cuda_version=None)
    # Cached: a different torch afterwards does not change the answer.
    sys.modules["torch"] = _fake_torch(None, "12.8")
    assert torch_flavour.torch_build_facts() is facts


def test_torch_build_facts_none_without_torch(facts_env, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(torch_flavour, "is_torch_available", lambda: False)
    assert torch_flavour.torch_build_facts() is None
    # `None` is not cached: torch appearing later is seen.
    monkeypatch.setattr(torch_flavour, "is_torch_available", lambda: True)
    assert torch_flavour.torch_build_facts() is not None


def test_gpu_device_facts_shape_and_cache(facts_env) -> None:
    devices = torch_flavour.gpu_device_facts()
    assert devices == [
        {"index": 0, "name": "AMD Radeon AI PRO R9700", "gcn_arch": "gfx1201", "integrated": False},
        # gfx1036 is an iGPU arch (`torch_device._IGPU_ARCHS`); the arch is lower-cased.
        {"index": 1, "name": "AMD Ryzen 5 9600X 6-Core Processor", "gcn_arch": "gfx1036", "integrated": True},
    ]
    devices.clear()
    assert len(torch_flavour.gpu_device_facts()) == 2, "the caller's copy must not alias the cache"
    assert facts_env.probe_calls == 1


def test_gpu_device_facts_cuda_and_empty_cached(facts_env) -> None:
    facts_env.devices = []
    assert torch_flavour.gpu_device_facts() == []
    assert torch_flavour.gpu_device_facts() == []
    # An empty answer is cached too: no repeated device query per snapshot.
    assert facts_env.probe_calls == 1


@pytest.mark.parametrize("arch", [None, "", "NVIDIA GeForce RTX 4090", 7])
def test_gcn_arch_none_for_non_gfx(facts_env, arch: object) -> None:
    props = types.SimpleNamespace(name="NVIDIA GeForce RTX 4090", major=8, minor=9)
    if arch is not None:
        props.gcnArchName = arch
    facts_env.devices = [props]
    assert torch_flavour.gpu_device_facts()[0]["gcn_arch"] is None


def test_installed_rocm_device_archs(facts_env) -> None:
    # Per-arch wheels only, normalized names, sorted; family wheels
    # (`gfx12-0`, `gfx110x`), torchvision wheels and a nameless dist are ignored.
    assert torch_flavour.installed_rocm_device_archs() == ["gfx1100", "gfx1201"]
    facts_env.dists = []
    assert torch_flavour.installed_rocm_device_archs() == ["gfx1100", "gfx1201"]
    assert facts_env.dist_calls == 1


def test_rocm_split_kernels_installed(facts_env) -> None:
    # `rocm_sdk_core` normalizes to `rocm-sdk-core`: the whl-next layout. The
    # answer shares the arch scan's cache: one metadata scan for both facts.
    assert torch_flavour.rocm_split_kernels_installed() is True
    assert torch_flavour.installed_rocm_device_archs() == ["gfx1100", "gfx1201"]
    assert facts_env.dist_calls == 1


def test_rocm_split_kernels_absent_for_bundled_build(facts_env) -> None:
    # A pytorch.org ROCm build: no `rocm-sdk-core`, no device wheels; cached when false too.
    facts_env.dists = ["torch", "pytorch-triton-rocm", "rocm-sdk-core-extra"]
    assert torch_flavour.rocm_split_kernels_installed() is False
    assert torch_flavour.installed_rocm_device_archs() == []
    facts_env.dists = ["rocm-sdk-core"]
    assert torch_flavour.rocm_split_kernels_installed() is False
    assert facts_env.dist_calls == 1


def test_torch_health_payload_shape(facts_env) -> None:
    import json

    payload = torch_flavour.torch_health_payload()
    assert payload == {
        "flavour": "rocm",
        "version": "2.13.0+rocm10.1.0",
        "hip_version": "10.1.0",
        "cuda_version": None,
        "devices": [
            {"index": 0, "name": "AMD Radeon AI PRO R9700", "gcn_arch": "gfx1201", "integrated": False},
            {"index": 1, "name": "AMD Ryzen 5 9600X 6-Core Processor", "gcn_arch": "gfx1036", "integrated": True},
        ],
        "rocm_device_archs": ["gfx1100", "gfx1201"],
        "rocm_split_kernels": True,
    }
    json.dumps(payload)
    # Repeated snapshots query nothing again.
    for _ in range(5):
        assert torch_flavour.torch_health_payload() == payload
    assert (facts_env.probe_calls, facts_env.dist_calls) == (1, 1)


def test_torch_health_payload_none_without_torch_skips_device_probe(facts_env, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(torch_flavour, "is_torch_available", lambda: False)
    assert torch_flavour.torch_health_payload() is None
    assert facts_env.probe_calls == 0


def test_torch_health_payload_never_raises(facts_env, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture) -> None:
    def broken() -> list[object]:
        raise OSError("site-packages unreadable")

    monkeypatch.setattr(torch_flavour.importlib_metadata, "distributions", broken)
    with caplog.at_level("DEBUG", logger=torch_flavour.__name__):
        assert torch_flavour.torch_health_payload() is None
        assert torch_flavour.torch_health_payload() is None
    levels = [record.levelname for record in caplog.records if "torch health facts failed" in record.getMessage()]
    # Logged loudly once, then quietly: the health worker retries every second.
    assert levels == ["WARNING", "DEBUG"]


def test_concurrent_first_callers_probe_once(facts_env, monkeypatch: pytest.MonkeyPatch) -> None:
    # Two threads released together by a barrier both make the FIRST call of
    # every fact; each probe must still run exactly once (single initializer)
    # and both threads must get the same answer.
    import threading

    barrier = threading.Barrier(2)
    torch_checks = 0
    slow_probe = facts_env.devices

    def counting_is_torch_available() -> bool:
        nonlocal torch_checks
        torch_checks += 1
        # Widen the race window: the other thread is already past the barrier.
        threading.Event().wait(0.05)
        return True

    def slow_devices() -> list[object]:
        facts_env.probe_calls += 1
        threading.Event().wait(0.05)
        return list(slow_probe)

    def slow_distributions() -> list[_FakeDist]:
        facts_env.dist_calls += 1
        threading.Event().wait(0.05)
        return [_FakeDist(name) for name in facts_env.dists]

    monkeypatch.setattr(torch_flavour, "is_torch_available", counting_is_torch_available)
    monkeypatch.setattr(torch_device, "_cuda_device_properties", slow_devices)
    monkeypatch.setattr(torch_flavour.importlib_metadata, "distributions", slow_distributions)
    results: list[object] = [None, None]
    errors: list[BaseException] = []

    def worker(slot: int) -> None:
        try:
            barrier.wait(5.0)
            results[slot] = torch_flavour.torch_health_payload()
        except BaseException as exc:  # noqa: BLE001 - re-raised on the test thread below
            errors.append(exc)

    threads = [threading.Thread(target=worker, args=(slot,)) for slot in (0, 1)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(10.0)
    assert not errors
    assert results[0] is not None and results[0] == results[1]
    assert (torch_checks, facts_env.probe_calls, facts_env.dist_calls) == (1, 1, 1)
