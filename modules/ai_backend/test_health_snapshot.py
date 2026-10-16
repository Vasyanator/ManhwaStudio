"""
File: modules/ai_backend/test_health_snapshot.py

Purpose:
Unit tests for `server._build_health_snapshot`:
- FIX-6: it must be resilient to a single service's `.health()` raising. One
  failing/missing service should yield a per-service `{"status":"error", ...}`
  placeholder instead of throwing and killing the whole snapshot (and the
  periodic `health` event that depends on it);
- S-014: it must build while every REAL service's instance lock is held (as a
  model load or an inference holds it), because each `health()` reads only that
  service's published `runtime.health_snapshot.HealthSnapshot`;
- v13: `model_manager` keeps the small pool's four counters and gains
  `small_models`, `large_models`, `limits` and `budget`, each isolated (a
  failing part becomes the error placeholder, the rest survives); the budget
  probes only devices that hold or reserve bytes;
- `server.build_app_state` wires ONE arbiter into both pools and the services,
  and lists FLUX.2 as the `flux2_klein` profiles adapter (in-memory settings
  document, temp cache root: no user document is touched).

Notes:
The FIX-6 tests: the snapshot only reads attributes and calls `.health()` on
each service, so a lightweight `SimpleNamespace` of fakes stands in for the real
`AppState` (no torch / model stack needed). The fake MUST carry every attribute
`_build_health_snapshot` touches: an attribute lookup happens outside
`_safe_service_health`, so a missing one raises `AttributeError` instead of
producing an error placeholder. Importing `server` still pulls the whole service
stack, so this test needs the backend's Python environment (e.g. `jaconv`).
The S-014 test builds the real services with their device probes patched to
`cpu` (no accelerator is initialized) and loads no model.
"""

from __future__ import annotations

import json
import threading
from contextlib import ExitStack
from types import SimpleNamespace
from typing import Any

import pytest

from modules.ai_backend.detection import ctd as ctd_module
from modules.ai_backend.detection.ctd import CtdTextDetectorService
from modules.ai_backend.detection.paddle import PaddleTextDetectorService
from modules.ai_backend.detection.surya import SuryaTextDetectorService
from modules.ai_backend.engines.paddle_onnx import RuntimeFactory
from modules.ai_backend.inpaint.aot import AotInpaintService
from modules.ai_backend.inpaint.flux2_klein import Flux2KleinInpaintService
from modules.ai_backend.inpaint.flux2_klein import hardware as flux2_hardware
from modules.ai_backend.inpaint.flux_fill import FluxFillInpaintService
from modules.ai_backend.inpaint.lama import LamaInpaintService
from modules.ai_backend.inpaint.lama_mpe import LamaMpeInpaintService
from modules.ai_backend.ocr import easy as easy_module
from modules.ai_backend.ocr.baberu import BaberuOcrService
from modules.ai_backend.ocr.easy import EasyOcrService
from modules.ai_backend.ocr.manga import MangaOcrService
from modules.ai_backend.ocr.paddle import PaddleOcrService
from modules.ai_backend.ocr.paddle_vl import PaddleVlOcrService
from modules.ai_backend.ocr.surya import SuryaOcrService
from modules.ai_backend.reline.service import RelineService
from modules.ai_backend.runtime.device_service import _MemoryUserConfig
from modules.ai_backend.runtime.large_models import _test_fixtures as large_fx
from modules.ai_backend.runtime.large_models.manager import LargeModelManager, MemoryLimitsStore
from modules.ai_backend.runtime.memory_budget import DeviceBudget, HostBudget
from modules.ai_backend.runtime.model_manager import SmallModelManager
from modules.ai_backend.runtime.residency.arbiter import ResidencyArbiter
from modules.ai_backend.runtime.residency.holdings import FootprintHint, Runtime
from modules.ai_backend.server import _build_health_snapshot, build_app_state
from modules.ai_backend.translate.machine_translation import MachineTranslationService
from modules.ai_backend.watermark.service import WatermarkRemovalService


class _OkService:
    def __init__(self, name: str) -> None:
        self._name = name

    def health(self) -> dict:
        return {"status": "ok", "name": self._name}


class _RaisingService:
    def health(self) -> dict:
        raise RuntimeError("torch import exploded")


GIB = 1 << 30


class _FakeProbes:
    """Budget probes with fixed figures; records which devices were probed."""

    def __init__(self, *, host_raises: bool = False) -> None:
        self.host_raises = host_raises
        self.devices: list[str] = []

    def host(self) -> HostBudget:
        if self.host_raises:
            raise OSError("meminfo unreadable")
        return HostBudget(total=64 * GIB, available=40 * GIB, cgroup_limit=None, cgroup_used=None, rss_anon=GIB, rss_shmem=0, reserve=2 * GIB)

    def device(self, device: str) -> DeviceBudget:
        self.devices.append(device)
        return DeviceBudget(device=device, total=16 * GIB, free=10 * GIB, torch_allocated=0, torch_reserved=0, non_torch=6 * GIB, reserve=GIB, cap=None)


def _pools(probes: _FakeProbes) -> tuple[SmallModelManager, LargeModelManager]:
    """A real small pool and an empty large pool over one arbiter (fake probes, no torch)."""
    arbiter = ResidencyArbiter(probes, admission_wait_s=0.0)
    small = SmallModelManager(arbiter=arbiter)
    large = LargeModelManager(arbiter, limits_store=MemoryLimitsStore(), tables=large_fx.simple_tables(), probes=probes)
    return small, large


def _make_state(*, surya_raises: bool = False) -> SimpleNamespace:
    """Build a fake AppState with every service `_build_health_snapshot` touches."""
    surya = _RaisingService() if surya_raises else _OkService("suryaocr")
    return SimpleNamespace(
        app_version="9.9.9-test",
        easy_ocr=_OkService("easyocr"),
        manga_ocr=_OkService("mangaocr"),
        paddle_ocr=_OkService("paddleocr"),
        paddle_vl_ocr=_OkService("paddleocrvl"),
        surya_ocr=surya,
        baberu_ocr=_OkService("baberuocr"),
        text_detector_ctd=_OkService("ctd"),
        text_detector_paddle=_OkService("paddle"),
        text_detector_surya=_OkService("surya_td"),
        lama_inpaint=_OkService("lama_v2"),
        lama_mpe_inpaint=_OkService("lama_mpe"),
        aot_inpaint=_OkService("aot"),
        flux_fill_inpaint=_OkService("flux_fill"),
        flux2_klein_inpaint=_OkService("flux2_klein"),
        watermark=_OkService("watermark"),
        reline=_OkService("reline"),
        machine_translation=_OkService("mt"),
        model_manager=_OkService("mm"),
        large_models=_OkService("large"),
        health_probes=_FakeProbes(),
    )


def test_snapshot_all_ok_has_full_shape() -> None:
    snap = _build_health_snapshot(_make_state())
    assert snap["ok"] is True
    assert snap["service"] == "mf_ai_backend"
    assert set(snap["ocr"]) == {
        "easyocr", "mangaocr", "paddleocr", "paddleocrvl", "suryaocr", "baberuocr",
    }
    assert snap["ocr"]["suryaocr"] == {"status": "ok", "name": "suryaocr"}
    # `baberuocr` is the key the Rust health parser reads for `ocr.baberu`.
    assert snap["ocr"]["baberuocr"] == {"status": "ok", "name": "baberuocr"}


def test_one_raising_service_does_not_kill_snapshot() -> None:
    # surya.health() raises (e.g. torch import). The whole snapshot must still
    # build, the failing entry becomes a per-service error, and every other
    # entry is intact -- and the overall shape/keys are unchanged (FIX-6).
    snap = _build_health_snapshot(_make_state(surya_raises=True))

    assert snap["ok"] is True
    assert snap["service"] == "mf_ai_backend"
    assert snap["backend_version"] == "9.9.9-test"

    # Failing sub-entry isolated.
    surya_entry = snap["ocr"]["suryaocr"]
    assert surya_entry["status"] == "error"
    assert "torch import exploded" in surya_entry["error"]

    # Every other sub-entry survived untouched.
    assert snap["ocr"]["mangaocr"] == {"status": "ok", "name": "mangaocr"}
    assert snap["text_detector"]["ctd"]["status"] == "ok"
    assert snap["inpaint"]["aot"]["status"] == "ok"
    assert snap["inpaint"]["flux_fill"]["status"] == "ok"
    assert snap["inpaint"]["flux2_klein"]["status"] == "ok"
    assert snap["watermark"]["status"] == "ok"
    assert snap["image_processing"]["reline"]["status"] == "ok"
    assert snap["machine_translation"]["status"] == "ok"
    assert snap["model_manager"]["status"] == "ok"
    # The fake pool has no `holdings`/`arbiter`: those additive parts fail in
    # isolation, the counters part survives.
    assert snap["model_manager"]["small_models"]["status"] == "error"
    assert snap["model_manager"]["budget"]["status"] == "error"

    # Shape/keys unchanged.
    assert set(snap.keys()) == {
        "ok", "service", "backend_version", "snapshot_unix_s",
        "is_torch_available", "ocr", "text_detector", "inpaint", "watermark",
        "image_processing", "machine_translation", "model_manager",
    }


class _FakeDeviceService:
    """`AiDeviceService.get_state` stand-in: CPU everywhere, no config access."""

    def get_state(self) -> dict[str, Any]:
        return {
            "selected_device": "cpu",
            "selected_onnx_provider": "CPUExecutionProvider",
            "selected_onnx_device_id": "0",
        }


def _make_real_state(monkeypatch: pytest.MonkeyPatch) -> SimpleNamespace:
    """Build every snapshot service for real, with the device probes pinned to `cpu`."""
    monkeypatch.setattr(easy_module, "_resolve_selected_backend_device", lambda _fallback: "cpu")
    monkeypatch.setattr(ctd_module, "_default_device", lambda: "cpu")
    monkeypatch.setattr(ctd_module, "_resolve_selected_backend_device", lambda _fallback: "cpu")
    monkeypatch.setattr(flux2_hardware, "_resolve_selected_backend_device", lambda _fallback: "cpu")
    probes = _FakeProbes()
    manager = SmallModelManager(arbiter=ResidencyArbiter(probes, admission_wait_s=0.0))
    devices = _FakeDeviceService()
    factory = RuntimeFactory(manager)
    # One large pool per arbiter, as `build_app_state` wires it: a FLUX.2 service
    # built without `large_models=` would register a second, private large pool
    # on the shared arbiter.
    large = LargeModelManager(manager.arbiter, limits_store=MemoryLimitsStore(), tables=large_fx.simple_tables(), probes=probes)
    return SimpleNamespace(
        app_version="9.9.9-test",
        easy_ocr=EasyOcrService(manager),
        manga_ocr=MangaOcrService(manager, devices),
        paddle_ocr=PaddleOcrService(factory),
        paddle_vl_ocr=PaddleVlOcrService(manager),
        surya_ocr=SuryaOcrService(manager),
        baberu_ocr=BaberuOcrService(manager, devices),
        text_detector_ctd=CtdTextDetectorService(manager),
        text_detector_paddle=PaddleTextDetectorService(factory),
        text_detector_surya=SuryaTextDetectorService(manager),
        lama_inpaint=LamaInpaintService(manager),
        lama_mpe_inpaint=LamaMpeInpaintService(manager),
        aot_inpaint=AotInpaintService(manager),
        flux_fill_inpaint=FluxFillInpaintService(manager),
        flux2_klein_inpaint=Flux2KleinInpaintService(manager, large_models=large),
        watermark=WatermarkRemovalService(manager),
        reline=RelineService(),
        machine_translation=MachineTranslationService(),
        model_manager=manager,
        large_models=large,
        health_probes=probes,
    )


def test_snapshot_builds_while_every_service_lock_is_held(monkeypatch: pytest.MonkeyPatch) -> None:
    # S-014: a model load or an inference holds its service's instance lock for
    # seconds to minutes. The health worker must still build the snapshot.
    state = _make_real_state(monkeypatch)
    # Service instance locks only: the large pool's `_lock` is a leaf held for
    # dictionary reads (never across a load), so it is not part of S-014.
    locks = [service._lock for name, service in vars(state).items() if hasattr(service, "_lock") and name != "large_models"]
    # Six OCR engines, three detectors, five inpaint engines, watermark.
    assert len(locks) == 15
    held = threading.Event()
    release = threading.Event()

    def hold_every_lock() -> None:
        with ExitStack() as stack:
            for lock in locks:
                stack.enter_context(lock)
            held.set()
            release.wait(10.0)

    holder = threading.Thread(target=hold_every_lock, daemon=True)
    holder.start()
    try:
        assert held.wait(5.0)
        snapshots: list[dict[str, Any]] = []
        builder = threading.Thread(
            target=lambda: snapshots.append(_build_health_snapshot(state)), daemon=True
        )
        builder.start()
        builder.join(5.0)
        assert not builder.is_alive(), "the health snapshot blocked on a service lock"
    finally:
        release.set()
        holder.join(5.0)

    snap = snapshots[0]
    entries = [
        *snap["ocr"].values(),
        *snap["text_detector"].values(),
        *snap["inpaint"].values(),
        snap["watermark"],
    ]
    # Real `health()` answers, not the error placeholder; nothing is loaded yet.
    for entry in entries:
        assert entry.get("status") != "error", entry
    assert snap["ocr"]["baberuocr"] == {"ready": False, "device": None, "last_error": None}
    assert snap["inpaint"]["aot"]["ready"] is False
    assert snap["watermark"]["ready"] is False


def _pool_state(probes: _FakeProbes) -> SimpleNamespace:
    small, large = _pools(probes)
    state = _make_state()
    state.model_manager = small
    state.large_models = large
    state.health_probes = probes
    return state


def test_model_manager_payload_is_additive() -> None:
    probes = _FakeProbes()
    state = _pool_state(probes)
    # One resident small model on cuda:1 (declared size), one host-only one.
    for key, device in (("ocr:manga", "cuda:1"), ("ocr:easy", "cpu")):
        lease = state.model_manager.begin_model_use(key, hint=FootprintHint(device=device, device_bytes=GIB if device != "cpu" else 0, host_bytes=GIB // 2, runtime=Runtime.TORCH))
        lease.mark_loaded(lambda: None)
        lease.release()
    entry = _build_health_snapshot(state)["model_manager"]
    # The four small-pool counters keep their names and meaning.
    assert entry["max_loaded_models"] == 3
    assert entry["resident_model_count"] == 2
    assert entry["active_model_count"] == 0
    assert entry["loading_model_count"] == 0
    by_key = {row["key"]: row for row in entry["small_models"]}
    assert set(by_key) == {"ocr:manga", "ocr:easy"}
    assert set(by_key["ocr:manga"]) == {"key", "owner", "device", "device_bytes", "host_bytes", "size_source", "resident", "in_use", "loading"}
    assert by_key["ocr:manga"]["device"] == "cuda:1"
    assert entry["large_models"] == []
    assert entry["limits"] == {"vram_cap": None, "ram_cap": None}
    budget = entry["budget"]
    assert budget["host"]["headroom"] == 38 * GIB
    # Only the device that holds bytes is probed; the CPU holding names no device row.
    assert [row["device"] for row in budget["devices"]] == ["cuda:1"]
    assert budget["devices"][0]["headroom"] == 9 * GIB
    assert set(budget["reservations"]) == {"count", "device_bytes", "host_bytes"}
    json.dumps(entry)


def test_no_device_is_probed_when_nothing_is_held() -> None:
    probes = _FakeProbes()
    entry = _build_health_snapshot(_pool_state(probes))["model_manager"]
    assert entry["budget"]["devices"] == []
    assert probes.devices == []


def test_budget_failure_is_isolated() -> None:
    entry = _build_health_snapshot(_pool_state(_FakeProbes(host_raises=True)))["model_manager"]
    assert entry["budget"]["status"] == "error"
    assert "meminfo unreadable" in entry["budget"]["error"]
    assert entry["resident_model_count"] == 0
    assert entry["small_models"] == []
    assert entry["large_models"] == []


def test_build_app_state_shares_one_arbiter(monkeypatch: pytest.MonkeyPatch, tmp_path: Any) -> None:
    monkeypatch.setattr(easy_module, "_resolve_selected_backend_device", lambda _fallback: "cpu")
    monkeypatch.setattr(ctd_module, "_default_device", lambda: "cpu")
    monkeypatch.setattr(ctd_module, "_resolve_selected_backend_device", lambda _fallback: "cpu")
    monkeypatch.setattr(flux2_hardware, "_resolve_selected_backend_device", lambda _fallback: "cpu")
    config = _MemoryUserConfig()
    config.config["General"]["large_models_vram_cap_bytes"] = 6 * GIB
    state = build_app_state("9.9.9-test", user_config=config, large_models_cache_root=tmp_path)
    assert state.large_models._arbiter is state.model_manager.arbiter
    # The persisted user limit is the large pool's limit in force.
    assert state.large_models.limits().vram_cap == 6 * GIB
    # FLUX.2 serves `large_models.profiles` under its pipeline model name.
    assert dict(state.large_model_adapters) == {"flux2_klein": state.flux2_klein_inpaint}
    snap = _build_health_snapshot(state)
    assert snap["model_manager"]["large_models"] == []
    assert snap["model_manager"]["budget"]["devices"] == []
    # Constructing reads only: the floors store is created lazily, nothing written.
    assert list(tmp_path.iterdir()) == []
