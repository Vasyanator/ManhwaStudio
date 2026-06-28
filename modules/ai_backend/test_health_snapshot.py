"""
File: modules/ai_backend/test_health_snapshot.py

Purpose:
Unit tests for FIX-6: `server._build_health_snapshot` must be resilient to a
single service's `.health()` raising. One failing/missing service should yield a
per-service `{"status":"error", ...}` placeholder instead of throwing and killing
the whole snapshot (and the periodic `health` event that depends on it).

Notes:
The snapshot only reads attributes and calls `.health()` on each service, so a
lightweight `SimpleNamespace` of fakes stands in for the real `AppState` (no
torch / model stack needed).
"""

from __future__ import annotations

from types import SimpleNamespace

from modules.ai_backend.server import _build_health_snapshot


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
        text_detector_ctd=_OkService("ctd"),
        text_detector_paddle=_OkService("paddle"),
        text_detector_surya=_OkService("surya_td"),
        lama_inpaint=_OkService("lama_v2"),
        lama_mpe_inpaint=_OkService("lama_mpe"),
        aot_inpaint=_OkService("aot"),
        reline=_OkService("reline"),
        machine_translation=_OkService("mt"),
        model_manager=_OkService("mm"),
    )


def test_snapshot_all_ok_has_full_shape() -> None:
    snap = _build_health_snapshot(_make_state())
    assert snap["ok"] is True
    assert snap["service"] == "mf_ai_backend"
    assert set(snap["ocr"]) == {
        "easyocr", "mangaocr", "paddleocr", "paddleocrvl", "suryaocr",
    }
    assert snap["ocr"]["suryaocr"] == {"status": "ok", "name": "suryaocr"}


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
    assert snap["image_processing"]["reline"]["status"] == "ok"
    assert snap["machine_translation"]["status"] == "ok"
    assert snap["model_manager"]["status"] == "ok"

    # Shape/keys unchanged.
    assert set(snap.keys()) == {
        "ok", "service", "backend_version", "snapshot_unix_s",
        "is_torch_available", "ocr", "text_detector", "inpaint",
        "image_processing", "machine_translation", "model_manager",
    }
