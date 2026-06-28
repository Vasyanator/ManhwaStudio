"""
File: modules/ai_backend/test_handlers_inpaint.py

Purpose:
Unit tests for the v2 IPC inpaint handlers
(`modules/ai_backend/ipc/handlers/inpaint.py`), exercised by calling the
registered handlers directly with mocked AppState inpaint services (no torch,
no models).

Coverage:
- the concatenated request blob (image_png ++ mask_png) is split correctly by
  the ``image_len`` / ``mask_len`` header fields;
- ``params`` is passed through verbatim to the service;
- the result PNG is returned as the RESPONSE BLOB (raw bytes, byte-identical to
  the service's ``image_png`` result), and the metadata fields ride in the
  response header (the ``image_png`` key is not in the header);
- a length-mismatch (image_len + mask_len != len(blob)) raises ValueError;
- the ``.unload`` handlers return ``{"unloaded": bool}`` from ``state.unload()``.
"""

from __future__ import annotations

import threading
from types import SimpleNamespace
from typing import Any

import pytest

from modules.ai_backend.ipc.protocol import (
    METHOD_INPAINT_AOT,
    METHOD_INPAINT_AOT_UNLOAD,
    METHOD_INPAINT_LAMA_MPE,
    METHOD_INPAINT_LAMA_MPE_UNLOAD,
    METHOD_INPAINT_LAMA_V2,
    METHOD_INPAINT_LAMA_V2_UNLOAD,
)
from modules.ai_backend.ipc.registry import HandlerContext, get_handler

IMAGE_PNG = b"\x89PNG-image-bytes"
MASK_PNG = b"MASK-bytes!!"
RESULT_PNG = b"\x89PNG-result-bytes-\x00\x01\x02"


class _FakeService:
    """Records the args passed to ``inpaint_image_bytes`` and returns a canned
    result whose ``image_png`` is the raw ``RESULT_PNG`` bytes (mirroring the
    real services, which return raw PNG bytes)."""

    def __init__(self, result: dict[str, Any]) -> None:
        self._result = result
        self.calls: list[tuple[bytes, bytes, dict[str, Any]]] = []
        self.unload_return = True
        self.unload_calls = 0

    def inpaint_image_bytes(
        self, image_bytes: bytes, mask_bytes: bytes, *, params: dict[str, Any]
    ) -> dict[str, Any]:
        self.calls.append((image_bytes, mask_bytes, params))
        return self._result

    def unload(self) -> bool:
        self.unload_calls += 1
        return self.unload_return


def _ctx(**services: Any) -> HandlerContext:
    state = SimpleNamespace(**services)
    return HandlerContext(
        state=state,
        events=None,
        get_health_snapshot=lambda: {},
    )


def _concat_header(extra: dict[str, Any] | None = None) -> dict[str, Any]:
    header: dict[str, Any] = {
        "image_len": len(IMAGE_PNG),
        "mask_len": len(MASK_PNG),
    }
    if extra:
        header.update(extra)
    return header


def _no_cancel() -> threading.Event:
    return threading.Event()


# ---------------------------------------------------------------------------
# lama_v2
# ---------------------------------------------------------------------------


def test_lama_v2_splits_blob_passes_params_and_returns_png_blob() -> None:
    result = {
        "image_png": RESULT_PNG,
        "source_size": [640, 480],
        "device": "cuda",
        "refine": True,
        "model_name": "big-lama",
    }
    svc = _FakeService(result)
    ctx = _ctx(lama_inpaint=svc)
    params = {"refine": True, "n_iters": 3, "model_name": "big-lama"}
    header = _concat_header({"params": params})

    handler = get_handler(METHOD_INPAINT_LAMA_V2)
    resp_header, resp_blob = handler(ctx, header, IMAGE_PNG + MASK_PNG, _no_cancel())

    # Blob split: image and mask reach the service intact.
    assert svc.calls == [(IMAGE_PNG, MASK_PNG, params)]
    # Result PNG is the RESPONSE BLOB (raw bytes, byte-identical to the service).
    assert resp_blob == RESULT_PNG
    assert resp_blob == result["image_png"]
    # Metadata rides in the header; the image bytes are not in the header.
    assert resp_header == {
        "engine": "lama_v2",
        "source_size": [640, 480],
        "device": "cuda",
        "refine": True,
        "model_name": "big-lama",
    }
    assert "image_png" not in resp_header
    assert "image_png_base64" not in resp_header


def test_lama_v2_length_mismatch_raises() -> None:
    svc = _FakeService({"image_png": b""})
    ctx = _ctx(lama_inpaint=svc)
    # Header claims more mask bytes than the blob actually carries.
    header = {"image_len": len(IMAGE_PNG), "mask_len": len(MASK_PNG) + 5}
    handler = get_handler(METHOD_INPAINT_LAMA_V2)
    with pytest.raises(ValueError, match="blob length mismatch"):
        handler(ctx, header, IMAGE_PNG + MASK_PNG, _no_cancel())
    assert svc.calls == []  # service never called on bad split


def test_lama_v2_missing_length_field_raises() -> None:
    svc = _FakeService({"image_png": b""})
    ctx = _ctx(lama_inpaint=svc)
    handler = get_handler(METHOD_INPAINT_LAMA_V2)
    with pytest.raises(ValueError, match="image_len"):
        handler(ctx, {"mask_len": 1}, b"x", _no_cancel())


def test_lama_v2_unload_returns_flag() -> None:
    svc = _FakeService({})
    svc.unload_return = False
    ctx = _ctx(lama_inpaint=svc)
    handler = get_handler(METHOD_INPAINT_LAMA_V2_UNLOAD)
    resp_header, resp_blob = handler(ctx, {}, b"", _no_cancel())
    assert resp_header == {"unloaded": False}
    assert resp_blob == b""
    assert svc.unload_calls == 1


# ---------------------------------------------------------------------------
# lama_mpe
# ---------------------------------------------------------------------------


def test_lama_mpe_splits_blob_and_returns_png_blob() -> None:
    result = {
        "image_png": RESULT_PNG,
        "source_size": [100, 200],
        "device": "cpu",
        "inpaint_size": 1536,
    }
    svc = _FakeService(result)
    ctx = _ctx(lama_mpe_inpaint=svc)
    params = {"inpaint_size": 1536}
    header = _concat_header({"params": params})

    handler = get_handler(METHOD_INPAINT_LAMA_MPE)
    resp_header, resp_blob = handler(ctx, header, IMAGE_PNG + MASK_PNG, _no_cancel())

    assert svc.calls == [(IMAGE_PNG, MASK_PNG, params)]
    assert resp_blob == RESULT_PNG
    assert resp_header == {
        "engine": "lama_mpe",
        "source_size": [100, 200],
        "device": "cpu",
        "inpaint_size": 1536,
    }


def test_lama_mpe_unload_returns_flag() -> None:
    svc = _FakeService({})
    ctx = _ctx(lama_mpe_inpaint=svc)
    handler = get_handler(METHOD_INPAINT_LAMA_MPE_UNLOAD)
    resp_header, _ = handler(ctx, {}, b"", _no_cancel())
    assert resp_header == {"unloaded": True}
    assert svc.unload_calls == 1


# ---------------------------------------------------------------------------
# aot
# ---------------------------------------------------------------------------


def test_aot_splits_blob_and_returns_png_blob() -> None:
    result = {
        "image_png": RESULT_PNG,
        "source_size": [12, 34],
        "device": "cuda:1",
        "inpaint_size": 2048,
    }
    svc = _FakeService(result)
    ctx = _ctx(aot_inpaint=svc)
    params = {"inpaint_size": 2048}
    header = _concat_header({"params": params})

    handler = get_handler(METHOD_INPAINT_AOT)
    resp_header, resp_blob = handler(ctx, header, IMAGE_PNG + MASK_PNG, _no_cancel())

    assert svc.calls == [(IMAGE_PNG, MASK_PNG, params)]
    assert resp_blob == RESULT_PNG
    assert resp_header == {
        "engine": "aot",
        "source_size": [12, 34],
        "device": "cuda:1",
        "inpaint_size": 2048,
    }


def test_aot_unload_returns_flag() -> None:
    svc = _FakeService({})
    ctx = _ctx(aot_inpaint=svc)
    handler = get_handler(METHOD_INPAINT_AOT_UNLOAD)
    resp_header, _ = handler(ctx, {}, b"", _no_cancel())
    assert resp_header == {"unloaded": True}
    assert svc.unload_calls == 1


# ---------------------------------------------------------------------------
# shared behavior: params defaults to {} when absent
# ---------------------------------------------------------------------------


def test_params_defaults_to_empty_dict_when_absent() -> None:
    result = {"image_png": RESULT_PNG}
    svc = _FakeService(result)
    ctx = _ctx(lama_inpaint=svc)
    header = _concat_header()  # no "params"
    handler = get_handler(METHOD_INPAINT_LAMA_V2)
    handler(ctx, header, IMAGE_PNG + MASK_PNG, _no_cancel())
    assert svc.calls[0][2] == {}


def test_non_object_params_raises() -> None:
    svc = _FakeService({})
    ctx = _ctx(lama_inpaint=svc)
    header = _concat_header({"params": ["not", "an", "object"]})
    handler = get_handler(METHOD_INPAINT_LAMA_V2)
    with pytest.raises(ValueError, match="must be an object"):
        handler(ctx, header, IMAGE_PNG + MASK_PNG, _no_cancel())


def test_zero_mask_len_splits_all_to_image() -> None:
    result = {"image_png": RESULT_PNG}
    svc = _FakeService(result)
    ctx = _ctx(aot_inpaint=svc)
    header = {"image_len": len(IMAGE_PNG), "mask_len": 0, "params": {}}
    handler = get_handler(METHOD_INPAINT_AOT)
    handler(ctx, header, IMAGE_PNG, _no_cancel())
    image_bytes, mask_bytes, _ = svc.calls[0]
    assert image_bytes == IMAGE_PNG
    assert mask_bytes == b""
