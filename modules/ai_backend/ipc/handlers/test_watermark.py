"""
File: modules/ai_backend/ipc/handlers/test_watermark.py

Purpose:
Unit tests for the v2 IPC visible-watermark handlers
(`modules/ai_backend/ipc/handlers/watermark.py`), exercised by calling the
registered handlers directly with a fake ``AppState.watermark`` service (NO
torch, NO models, NO socket). The fake is deliberately a local stand-in rather
than the real `watermark/service.py`: these tests pin the WIRE shape of the four
methods, which must hold independently of the service implementation.

Streaming mechanism under test:
The handler reads its per-request ``ProgressEmitter`` from
``HandlerContext.progress_emitter`` (the dispatcher's streaming hook) and pushes
FLUX-style two-phase frames: header ``{phase, step, total, label}``, empty blob.
The tests attach a real ``dispatcher.ProgressEmitter`` bound to a fake dispatcher
whose ``_write`` records each frame, so the EXACT on-the-wire progress header is
asserted. With no emitter attached, emission degrades to a no-op.

Coverage:
- `watermark.detect`: mask PNG in the RESPONSE BLOB (raw bytes), metadata
  (``model``/``device``/``source_size``/``mask_coverage``) in the header;
- `watermark.remove`: response blob = clean_png ++ mask_png split by the
  ``image_len``/``mask_len`` response header ints;
- progress frames with the exact ``{phase,step,total,label}`` key set and an
  empty blob, both phases (``download`` bytes / ``generate`` tiles);
- no emitter => no frames, still a correct terminal response;
- request validation: empty blob, non-object ``params``, ``params`` absent/None;
- error mapping: ValueError / FileNotFoundError / generic Exception propagate;
- cancel before start and cancel observed after the service returns;
- `watermark.status` passes the catalog through; `watermark.unload` returns
  ``{"unloaded": bool}``.
"""

from __future__ import annotations

import threading
from types import SimpleNamespace
from typing import Any

import pytest

from modules.ai_backend.ipc.dispatcher import ProgressEmitter
from modules.ai_backend.ipc.protocol import (
    ALL_METHODS,
    HEADER_ID,
    HEADER_KIND,
    HEADER_VERSION,
    KIND_PROGRESS,
    METHOD_WATERMARK_DETECT,
    METHOD_WATERMARK_REMOVE,
    METHOD_WATERMARK_STATUS,
    METHOD_WATERMARK_UNLOAD,
)
from modules.ai_backend.ipc.registry import HandlerContext, Interrupted, get_handler

IMAGE_PNG = b"\x89PNG-input-image-bytes"
MASK_PNG = b"\x89PNG-mask-L8\x00\x01"
CLEAN_PNG = b"\x89PNG-cleaned-\x02\x03\x04"

REQUEST_ID = 77

# (phase, step, total, label) tuples the fake service replays through the
# progress callback: one download phase frame and two generate phase frames.
PROGRESS_SCRIPT: list[tuple[str, int, int, str]] = [
    ("download", 1024, 85_672_345, "model_best.pth.tar"),
    ("generate", 1, 2, "tile row 1/2"),
    ("generate", 2, 2, "tile row 2/2"),
]


# ---------------------------------------------------------------------------
# Fakes
# ---------------------------------------------------------------------------


class _FakeWatermarkService:
    """Stand-in for ``AppState.watermark`` (``WatermarkRemovalService``).

    ``detect_mask_bytes`` / ``remove_watermark_bytes`` replay ``PROGRESS_SCRIPT``
    through the supplied ``progress_callback`` and return a canned result dict
    whose PNG values are raw bytes, mirroring the real service. ``raise_exc``
    makes the call fail after the progress replay.
    """

    def __init__(
        self,
        *,
        detect_result: dict[str, Any] | None = None,
        remove_result: dict[str, Any] | None = None,
        progress: list[tuple[str, int, int, str]] | None = None,
        raise_exc: BaseException | None = None,
    ) -> None:
        self._detect_result = (
            detect_result if detect_result is not None else _default_detect_result()
        )
        self._remove_result = (
            remove_result if remove_result is not None else _default_remove_result()
        )
        self._progress = progress if progress is not None else list(PROGRESS_SCRIPT)
        self._raise_exc = raise_exc
        self.detect_calls: list[tuple[bytes, dict[str, Any]]] = []
        self.remove_calls: list[tuple[bytes, dict[str, Any]]] = []
        self.status_calls = 0
        self.unload_calls = 0
        self.unload_return = True

    def _replay(self, progress_callback: Any) -> None:
        if progress_callback is None:
            return
        for phase, step, total, label in self._progress:
            progress_callback(phase, step, total, label)

    def detect_mask_bytes(
        self,
        image_png: bytes,
        params: dict[str, Any] | None = None,
        progress_callback: Any = None,
    ) -> dict[str, Any]:
        self.detect_calls.append((image_png, params if params is not None else {}))
        self._replay(progress_callback)
        if self._raise_exc is not None:
            raise self._raise_exc
        return self._detect_result

    def remove_watermark_bytes(
        self,
        image_png: bytes,
        params: dict[str, Any] | None = None,
        progress_callback: Any = None,
    ) -> dict[str, Any]:
        self.remove_calls.append((image_png, params if params is not None else {}))
        self._replay(progress_callback)
        if self._raise_exc is not None:
            raise self._raise_exc
        return self._remove_result

    def status(self) -> dict[str, Any]:
        self.status_calls += 1
        return _default_status()

    def unload(self) -> bool:
        self.unload_calls += 1
        return self.unload_return


class _RecordingDispatcher:
    """Minimal fake dispatcher exposing ``_write`` so a real ``ProgressEmitter``
    can record the frames it would put on the wire."""

    def __init__(self) -> None:
        self.frames: list[tuple[dict[str, Any], bytes]] = []

    def _write(self, header: dict[str, Any], blob: bytes = b"") -> None:
        self.frames.append((dict(header), blob))


def _default_detect_result() -> dict[str, Any]:
    return {
        "mask_png": MASK_PNG,
        "model": "slbr",
        "device": "cuda",
        "source_size": [800, 1200],
        "mask_coverage": 0.0725,
    }


def _default_remove_result() -> dict[str, Any]:
    return {
        "image_png": CLEAN_PNG,
        "mask_png": MASK_PNG,
        "model": "slbr",
        "device": "cpu",
        "source_size": [512, 512],
    }


def _default_status() -> dict[str, Any]:
    return {
        "models": [
            {"id": "slbr", "weights_ready": True, "code_ready": True},
            {"id": "wdnet", "weights_ready": False, "code_ready": False},
        ],
        "default_model": "slbr",
        "downloaded_models": ["slbr"],
        "code_ready_models": ["slbr"],
    }


def _ctx(svc: _FakeWatermarkService, *, emitter: Any = None) -> HandlerContext:
    state = SimpleNamespace(watermark=svc)
    ctx = HandlerContext(state=state, events=None, get_health_snapshot=lambda: {})
    if emitter is not None:
        # The dispatcher's streaming hook: a per-request emitter on the context.
        ctx.progress_emitter = emitter
    return ctx


def _no_cancel() -> threading.Event:
    return threading.Event()


def _emitter() -> tuple[ProgressEmitter, _RecordingDispatcher]:
    disp = _RecordingDispatcher()
    return ProgressEmitter(disp, REQUEST_ID), disp


# ---------------------------------------------------------------------------
# Registration / protocol wiring
# ---------------------------------------------------------------------------


def test_all_four_methods_are_registered_and_declared() -> None:
    for method in (
        METHOD_WATERMARK_DETECT,
        METHOD_WATERMARK_REMOVE,
        METHOD_WATERMARK_STATUS,
        METHOD_WATERMARK_UNLOAD,
    ):
        assert method in ALL_METHODS
        assert get_handler(method) is not None


# ---------------------------------------------------------------------------
# watermark.detect
# ---------------------------------------------------------------------------


def test_detect_returns_mask_in_blob_and_metadata_in_header() -> None:
    svc = _FakeWatermarkService(progress=[])
    ctx = _ctx(svc)
    params = {"model": "slbr", "downscale_to": 512, "threshold": 0.5, "dilate_px": 4}

    handler = get_handler(METHOD_WATERMARK_DETECT)
    resp_header, resp_blob = handler(ctx, {"params": params}, IMAGE_PNG, _no_cancel())

    assert svc.detect_calls == [(IMAGE_PNG, params)]
    # Mask travels as raw bytes in the RESPONSE BLOB, never in the header.
    assert resp_blob == MASK_PNG
    assert resp_header == {
        "model": "slbr",
        "device": "cuda",
        "source_size": [800, 1200],
        "mask_coverage": 0.0725,
    }
    assert "mask_png" not in resp_header
    assert "mask_png_base64" not in resp_header


def test_detect_emits_two_phase_progress_frames() -> None:
    svc = _FakeWatermarkService()
    emitter, disp = _emitter()
    ctx = _ctx(svc, emitter=emitter)

    handler = get_handler(METHOD_WATERMARK_DETECT)
    handler(ctx, {"params": {}}, IMAGE_PNG, _no_cancel())

    assert len(disp.frames) == len(PROGRESS_SCRIPT)
    for (frame_header, frame_blob), (phase, step, total, label) in zip(
        disp.frames, PROGRESS_SCRIPT
    ):
        assert frame_header[HEADER_KIND] == KIND_PROGRESS
        assert frame_header[HEADER_ID] == REQUEST_ID
        # Exact FLUX-contract payload keys, nothing else beyond the frame
        # envelope (v/id/kind) the emitter itself fills in.
        envelope = (HEADER_VERSION, HEADER_ID, HEADER_KIND)
        assert {
            k: v for k, v in frame_header.items() if k not in envelope
        } == {"phase": phase, "step": step, "total": total, "label": label}
        assert frame_blob == b""  # never a preview blob


def test_detect_without_emitter_is_a_noop_and_still_answers() -> None:
    svc = _FakeWatermarkService()
    ctx = _ctx(svc)  # no emitter attached
    handler = get_handler(METHOD_WATERMARK_DETECT)
    resp_header, resp_blob = handler(ctx, {"params": {}}, IMAGE_PNG, _no_cancel())
    assert resp_blob == MASK_PNG
    assert resp_header["model"] == "slbr"


def test_detect_params_absent_or_none_defaults_to_empty_object() -> None:
    svc = _FakeWatermarkService(progress=[])
    ctx = _ctx(svc)
    handler = get_handler(METHOD_WATERMARK_DETECT)
    handler(ctx, {}, IMAGE_PNG, _no_cancel())
    handler(ctx, {"params": None}, IMAGE_PNG, _no_cancel())
    assert [params for _, params in svc.detect_calls] == [{}, {}]


def test_detect_non_object_params_raises() -> None:
    svc = _FakeWatermarkService(progress=[])
    ctx = _ctx(svc)
    handler = get_handler(METHOD_WATERMARK_DETECT)
    with pytest.raises(ValueError, match="must be an object"):
        handler(ctx, {"params": ["not", "an", "object"]}, IMAGE_PNG, _no_cancel())
    assert svc.detect_calls == []


def test_detect_empty_blob_raises() -> None:
    svc = _FakeWatermarkService(progress=[])
    ctx = _ctx(svc)
    handler = get_handler(METHOD_WATERMARK_DETECT)
    with pytest.raises(ValueError, match="watermark.detect requires the input image"):
        handler(ctx, {"params": {}}, b"", _no_cancel())
    assert svc.detect_calls == []


def test_detect_missing_result_fields_fall_back_to_defaults() -> None:
    # A service result without the optional metadata still yields a valid header.
    svc = _FakeWatermarkService(detect_result={"mask_png": MASK_PNG}, progress=[])
    ctx = _ctx(svc)
    handler = get_handler(METHOD_WATERMARK_DETECT)
    resp_header, resp_blob = handler(ctx, {"params": {}}, IMAGE_PNG, _no_cancel())
    assert resp_blob == MASK_PNG
    assert resp_header == {
        "model": "",
        "device": "cpu",
        "source_size": [0, 0],
        "mask_coverage": 0.0,
    }


# ---------------------------------------------------------------------------
# watermark.remove
# ---------------------------------------------------------------------------


def test_remove_concatenates_clean_and_mask_with_lengths() -> None:
    svc = _FakeWatermarkService(progress=[])
    ctx = _ctx(svc)
    params = {"model": "slbr", "tile": 512, "overlap": 64}

    handler = get_handler(METHOD_WATERMARK_REMOVE)
    resp_header, resp_blob = handler(ctx, {"params": params}, IMAGE_PNG, _no_cancel())

    assert svc.remove_calls == [(IMAGE_PNG, params)]
    assert resp_blob == CLEAN_PNG + MASK_PNG
    assert resp_header == {
        "model": "slbr",
        "device": "cpu",
        "source_size": [512, 512],
        "image_len": len(CLEAN_PNG),
        "mask_len": len(MASK_PNG),
    }
    # The declared lengths must split the blob back into the two PNGs exactly.
    image_len = resp_header["image_len"]
    mask_len = resp_header["mask_len"]
    assert image_len + mask_len == len(resp_blob)
    assert resp_blob[:image_len] == CLEAN_PNG
    assert resp_blob[image_len : image_len + mask_len] == MASK_PNG


def test_remove_lengths_stay_consistent_when_mask_is_absent() -> None:
    svc = _FakeWatermarkService(
        remove_result={"image_png": CLEAN_PNG, "model": "wdnet"}, progress=[]
    )
    ctx = _ctx(svc)
    handler = get_handler(METHOD_WATERMARK_REMOVE)
    resp_header, resp_blob = handler(ctx, {"params": {}}, IMAGE_PNG, _no_cancel())
    assert resp_blob == CLEAN_PNG
    assert resp_header["image_len"] == len(CLEAN_PNG)
    assert resp_header["mask_len"] == 0
    assert resp_header["image_len"] + resp_header["mask_len"] == len(resp_blob)


def test_remove_emits_two_phase_progress_frames() -> None:
    svc = _FakeWatermarkService()
    emitter, disp = _emitter()
    ctx = _ctx(svc, emitter=emitter)
    handler = get_handler(METHOD_WATERMARK_REMOVE)
    handler(ctx, {"params": {}}, IMAGE_PNG, _no_cancel())

    assert [h["phase"] for h, _ in disp.frames] == [p[0] for p in PROGRESS_SCRIPT]
    assert [h["step"] for h, _ in disp.frames] == [p[1] for p in PROGRESS_SCRIPT]
    assert [h["total"] for h, _ in disp.frames] == [p[2] for p in PROGRESS_SCRIPT]
    assert [h["label"] for h, _ in disp.frames] == [p[3] for p in PROGRESS_SCRIPT]
    assert all(blob == b"" for _, blob in disp.frames)


def test_remove_without_emitter_is_a_noop_and_still_answers() -> None:
    svc = _FakeWatermarkService()
    ctx = _ctx(svc)  # no emitter attached
    handler = get_handler(METHOD_WATERMARK_REMOVE)
    resp_header, resp_blob = handler(ctx, {"params": {}}, IMAGE_PNG, _no_cancel())
    assert resp_blob == CLEAN_PNG + MASK_PNG
    assert resp_header["model"] == "slbr"


def test_remove_empty_blob_raises() -> None:
    svc = _FakeWatermarkService(progress=[])
    ctx = _ctx(svc)
    handler = get_handler(METHOD_WATERMARK_REMOVE)
    with pytest.raises(ValueError, match="watermark.remove requires the input image"):
        handler(ctx, {"params": {}}, b"", _no_cancel())
    assert svc.remove_calls == []


def test_remove_non_object_params_raises() -> None:
    svc = _FakeWatermarkService(progress=[])
    ctx = _ctx(svc)
    handler = get_handler(METHOD_WATERMARK_REMOVE)
    with pytest.raises(ValueError, match="must be an object"):
        handler(ctx, {"params": 7}, IMAGE_PNG, _no_cancel())
    assert svc.remove_calls == []


# ---------------------------------------------------------------------------
# Error mapping
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "method", [METHOD_WATERMARK_DETECT, METHOD_WATERMARK_REMOVE]
)
def test_value_error_propagates(method: str) -> None:
    svc = _FakeWatermarkService(progress=[], raise_exc=ValueError("bad downscale_to"))
    ctx = _ctx(svc)
    handler = get_handler(method)
    with pytest.raises(ValueError, match="bad downscale_to"):
        handler(ctx, {"params": {}}, IMAGE_PNG, _no_cancel())


@pytest.mark.parametrize(
    "method", [METHOD_WATERMARK_DETECT, METHOD_WATERMARK_REMOVE]
)
def test_file_not_found_propagates(method: str) -> None:
    svc = _FakeWatermarkService(
        progress=[], raise_exc=FileNotFoundError("model_best.pth.tar missing")
    )
    ctx = _ctx(svc)
    handler = get_handler(method)
    with pytest.raises(FileNotFoundError, match="model_best"):
        handler(ctx, {"params": {}}, IMAGE_PNG, _no_cancel())


@pytest.mark.parametrize(
    "method", [METHOD_WATERMARK_DETECT, METHOD_WATERMARK_REMOVE]
)
def test_generic_exception_propagates(method: str) -> None:
    svc = _FakeWatermarkService(progress=[], raise_exc=RuntimeError("hip oom"))
    ctx = _ctx(svc)
    handler = get_handler(method)
    with pytest.raises(RuntimeError, match="hip oom"):
        handler(ctx, {"params": {}}, IMAGE_PNG, _no_cancel())


# ---------------------------------------------------------------------------
# Cancel
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "method", [METHOD_WATERMARK_DETECT, METHOD_WATERMARK_REMOVE]
)
def test_cancel_before_start_raises_interrupted(method: str) -> None:
    svc = _FakeWatermarkService(progress=[])
    ctx = _ctx(svc)
    cancel = threading.Event()
    cancel.set()
    handler = get_handler(method)
    with pytest.raises(Interrupted):
        handler(ctx, {"params": {}}, IMAGE_PNG, cancel)
    # The service is never reached.
    assert svc.detect_calls == []
    assert svc.remove_calls == []


@pytest.mark.parametrize(
    ("method", "service_attr"),
    [
        (METHOD_WATERMARK_DETECT, "detect_mask_bytes"),
        (METHOD_WATERMARK_REMOVE, "remove_watermark_bytes"),
    ],
)
def test_cancel_after_service_returns_raises_interrupted(
    method: str, service_attr: str
) -> None:
    # The service completes, but the cancel event was set meanwhile: the handler
    # must produce the interrupted outcome rather than a normal result.
    svc = _FakeWatermarkService(progress=[])
    ctx = _ctx(svc)
    cancel = threading.Event()
    orig = getattr(svc, service_attr)

    def cancel_then_run(*args: Any, **kwargs: Any) -> Any:
        result = orig(*args, **kwargs)
        cancel.set()
        return result

    setattr(svc, service_attr, cancel_then_run)
    handler = get_handler(method)
    with pytest.raises(Interrupted):
        handler(ctx, {"params": {}}, IMAGE_PNG, cancel)


@pytest.mark.parametrize(
    ("method", "service_attr"),
    [
        (METHOD_WATERMARK_DETECT, "detect_mask_bytes"),
        (METHOD_WATERMARK_REMOVE, "remove_watermark_bytes"),
    ],
)
def test_exception_during_cancel_maps_to_interrupted(
    method: str, service_attr: str
) -> None:
    # The service raises while the cancel event is set: that is an interruption,
    # not a request error.
    svc = _FakeWatermarkService(progress=[], raise_exc=RuntimeError("stopped"))
    ctx = _ctx(svc)
    cancel = threading.Event()
    orig = getattr(svc, service_attr)

    def cancel_then_raise(*args: Any, **kwargs: Any) -> Any:
        cancel.set()
        return orig(*args, **kwargs)

    setattr(svc, service_attr, cancel_then_raise)
    handler = get_handler(method)
    with pytest.raises(Interrupted):
        handler(ctx, {"params": {}}, IMAGE_PNG, cancel)


# ---------------------------------------------------------------------------
# Status / unload
# ---------------------------------------------------------------------------


def test_status_passes_the_catalog_through() -> None:
    svc = _FakeWatermarkService()
    ctx = _ctx(svc)
    handler = get_handler(METHOD_WATERMARK_STATUS)
    resp_header, resp_blob = handler(ctx, {}, b"", _no_cancel())
    assert resp_header == _default_status()
    assert resp_blob == b""
    assert svc.status_calls == 1


def test_unload_returns_flag_true() -> None:
    svc = _FakeWatermarkService()
    ctx = _ctx(svc)
    handler = get_handler(METHOD_WATERMARK_UNLOAD)
    resp_header, resp_blob = handler(ctx, {}, b"", _no_cancel())
    assert resp_header == {"unloaded": True}
    assert resp_blob == b""
    assert svc.unload_calls == 1


def test_unload_returns_flag_false() -> None:
    svc = _FakeWatermarkService()
    svc.unload_return = False
    ctx = _ctx(svc)
    handler = get_handler(METHOD_WATERMARK_UNLOAD)
    resp_header, _ = handler(ctx, {}, b"", _no_cancel())
    assert resp_header == {"unloaded": False}
    assert svc.unload_calls == 1
