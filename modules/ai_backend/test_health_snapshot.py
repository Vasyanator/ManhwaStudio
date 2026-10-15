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
  service's published `runtime.health_snapshot.HealthSnapshot`.

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
from modules.ai_backend.runtime.model_manager import LoadedModelManager
from modules.ai_backend.server import _build_health_snapshot
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
    manager = LoadedModelManager()
    devices = _FakeDeviceService()
    factory = RuntimeFactory(manager)
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
        flux2_klein_inpaint=Flux2KleinInpaintService(manager),
        watermark=WatermarkRemovalService(manager),
        reline=RelineService(),
        machine_translation=MachineTranslationService(),
        model_manager=manager,
    )


def test_snapshot_builds_while_every_service_lock_is_held(monkeypatch: pytest.MonkeyPatch) -> None:
    # S-014: a model load or an inference holds its service's instance lock for
    # seconds to minutes. The health worker must still build the snapshot.
    state = _make_real_state(monkeypatch)
    locks = [service._lock for service in vars(state).values() if hasattr(service, "_lock")]
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
