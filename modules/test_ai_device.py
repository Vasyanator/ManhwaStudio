"""
File: modules/test_ai_device.py

Purpose:
Unit tests for the pure decision parts of `ai_device`'s CUDA/ROCm diagnosis:
the Windows GPU-listing parser and the lspci line matcher (S-075: the column
name "AdapterCompatibility" and the class text "VGA compatible controller" must
never read as an AMD GPU) and the AMD verdict (problems and the
"driver stack present" rule on Windows and Linux).

Notes:
No subprocess, no torch and no device node is touched: the parser gets literal
listings, the verdict gets its inputs as arguments, and `/dev/kfd` is checked on
paths under pytest's `tmp_path`.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

_PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(_PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(_PROJECT_ROOT))

from modules import ai_device  # noqa: E402

_WMIC_INTEL_NVIDIA = """

AdapterCompatibility=Intel Corporation
Name=Intel(R) UHD Graphics 630


AdapterCompatibility=NVIDIA
Name=NVIDIA GeForce RTX 3060

"""

_POWERSHELL_INTEL = """
Name                 : Intel(R) Arc(TM) A770 Graphics
AdapterCompatibility : Intel Corporation
"""


@pytest.mark.parametrize("listing", [_WMIC_INTEL_NVIDIA, _POWERSHELL_INTEL, "", "AdapterCompatibility=\nName=\n"])
def test_listing_without_amd_values_is_not_amd(listing: str) -> None:
    # The column name contains "ati"; only values may decide.
    assert ai_device._windows_listing_has_amd(listing) is False


@pytest.mark.parametrize("listing", [
    "AdapterCompatibility=Advanced Micro Devices, Inc.\nName=AMD Radeon RX 9070 XT\n",
    "AdapterCompatibility : ATI Technologies Inc.\nName : ATI Radeon HD 5770\n",
    "Name=AMD Radeon(TM) Graphics\n",
    "Name=Radeon RX 7900 XTX\n",
    _WMIC_INTEL_NVIDIA + "AdapterCompatibility=Advanced Micro Devices, Inc.\nName=AMD Radeon AI PRO R9700\n",
])
def test_listing_with_amd_value_is_amd(listing: str) -> None:
    assert ai_device._windows_listing_has_amd(listing) is True


def test_amd_markers_are_words() -> None:
    # "ati" / "amd" inside another word is not a vendor.
    assert ai_device._windows_listing_has_amd("Name=Corporation Graphics Adapter\nAdapterCompatibility=Camdenton Labs\n") is False


def test_listing_nvidia() -> None:
    assert ai_device._windows_listing_has_nvidia(_WMIC_INTEL_NVIDIA) is True
    assert ai_device._windows_listing_has_nvidia(_POWERSHELL_INTEL) is False


@pytest.mark.parametrize(("line", "expected"), [
    ("01:00.0 VGA compatible controller [0300]: NVIDIA Corporation GA106 [GeForce RTX 3060] [10de:2503] (rev a1)", False),
    ("00:02.0 VGA compatible controller [0300]: Intel Corporation CometLake-S GT2 [UHD Graphics 630] [8086:9bc5] (rev 05)", False),
    ("03:00.0 VGA compatible controller [0300]: Advanced Micro Devices, Inc. [AMD/ATI] Navi 48 [Radeon AI PRO R9700] [1002:7551] (rev c0)", True),
    ("10:00.0 Display controller [0380]: Advanced Micro Devices, Inc. [AMD/ATI] Granite Ridge [Radeon Graphics] [1002:13c0] (rev c6)", True),
])
def test_lspci_gpu_line_is_amd(line: str, expected: bool) -> None:
    assert ai_device._lspci_gpu_line_is_amd(line) is expected


def _problems(**overrides: object) -> list[str]:
    args = {"is_linux": False, "is_windows": True, "torch_is_rocm_build": False, "kfd_state": ai_device.KFD_MISSING, "cuda_available": False}
    args.update(overrides)
    return ai_device._amd_problems(**args)  # type: ignore[arg-type]


def test_windows_non_rocm_torch_advises_launcher_install() -> None:
    problems = _problems()
    assert len(problems) == 1
    assert "PyTorch лаунчера" in problems[0]
    # The old advice said ROCm does not work on Windows; it must be gone.
    assert all("как правило, не работает" not in text for text in problems)


def test_windows_never_mentions_kfd() -> None:
    for text in _problems(torch_is_rocm_build=True):
        assert "/dev/kfd" not in text


def test_windows_rocm_build_unavailable() -> None:
    problems = _problems(torch_is_rocm_build=True)
    assert len(problems) == 1
    assert "Adrenalin" in problems[0]
    assert _problems(torch_is_rocm_build=True, cuda_available=True) == []


def test_linux_kfd_ok_and_rocm_build_available_has_no_problem() -> None:
    assert _problems(is_linux=True, is_windows=False, torch_is_rocm_build=True, kfd_state=ai_device.KFD_OK, cuda_available=True) == []


def test_linux_missing_kfd_is_the_problem_not_system_rocm() -> None:
    problems = _problems(is_linux=True, is_windows=False, torch_is_rocm_build=True, kfd_state=ai_device.KFD_MISSING, cuda_available=True)
    assert len(problems) == 1
    assert "/dev/kfd" in problems[0]
    assert "amdgpu" in problems[0]
    assert "/opt/rocm" not in problems[0]


def test_linux_kfd_without_access_names_groups() -> None:
    problems = _problems(is_linux=True, is_windows=False, torch_is_rocm_build=True, kfd_state=ai_device.KFD_NO_ACCESS, cuda_available=True)
    assert len(problems) == 1
    assert "render" in problems[0] and "video" in problems[0]


def test_linux_non_rocm_torch_and_unavailable_rocm_build() -> None:
    assert len(_problems(is_linux=True, is_windows=False, kfd_state=ai_device.KFD_OK)) == 1
    problems = _problems(is_linux=True, is_windows=False, torch_is_rocm_build=True, kfd_state=ai_device.KFD_OK)
    assert len(problems) == 1
    assert "gfx" in problems[0]


def test_other_platforms_have_no_amd_problem() -> None:
    assert _problems(is_linux=False, is_windows=False) == []


@pytest.mark.parametrize(("is_linux", "is_windows", "kfd_state", "expected"), [
    (False, True, ai_device.KFD_MISSING, True),
    (True, False, ai_device.KFD_OK, True),
    (True, False, ai_device.KFD_NO_ACCESS, True),
    (True, False, ai_device.KFD_MISSING, False),
    (False, False, ai_device.KFD_OK, False),
])
def test_amd_driver_stack_present(is_linux: bool, is_windows: bool, kfd_state: str, expected: bool) -> None:
    assert ai_device._amd_driver_stack_present(is_linux=is_linux, is_windows=is_windows, kfd_state=kfd_state) is expected


def test_linux_kfd_state(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    node = tmp_path / "kfd"
    assert ai_device._linux_kfd_state(str(node)) == ai_device.KFD_MISSING
    node.write_bytes(b"")
    assert ai_device._linux_kfd_state(str(node)) == ai_device.KFD_OK
    monkeypatch.setattr(os, "access", lambda _path, mode: mode != os.W_OK)
    assert ai_device._linux_kfd_state(str(node)) == ai_device.KFD_NO_ACCESS
