"""
File: modules/ai_backend/test_handlers_sdxl.py

Purpose:
Unit tests for the v2 IPC SDXL inpaint handlers
(`modules/ai_backend/ipc/handlers/sdxl.py`), exercised by calling the registered
handlers directly with a mocked ``AppState.sdxl_inpaint`` service (NO torch, NO
models). SDXL is the only streaming method: it emits ``progress{id}`` frames per
diffusion step, then a terminal ``response`` with the result PNG in the blob.

Streaming mechanism under test:
The handler reads its per-request ``ProgressEmitter`` from the
``HandlerContext.progress_emitter`` attribute (the dispatcher's streaming hook).
``ProgressEmitter.emit(fields, blob)`` writes a real ``progress{id}`` frame. The
tests attach a real ``dispatcher.ProgressEmitter`` bound to a fake dispatcher
whose ``_write`` records each frame, so we assert the EXACT on-the-wire progress
frame shape (header ``{step,total}`` + raw preview PNG blob) plus the terminal
response (result PNG blob + metadata header). When no emitter is attached,
progress emission degrades to a no-op and only the terminal response is produced.

Coverage:
- N progress frames with ``{step,total}`` in the header and the raw preview PNG
  in the progress-frame BLOB (not base64);
- terminal response: result PNG as the RESPONSE BLOB (raw bytes), metadata
  (``engine``/``source_size``/``device``/``mode``) in the header;
- request blob split by ``image_len``/``mask_len``; ``params`` passed through;
- preview is optional (``None`` preview => empty progress blob);
- no-emitter => no progress, still a correct terminal response;
- ``inpaint.sdxl.unload`` returns ``{"unloaded": bool}``;
- error mapping: ValueError / FileNotFoundError / generic Exception propagate
  (the dispatcher maps them to ``response{status:"error"}``);
- cancel before start and cancel observed after the service returns.
"""

from __future__ import annotations

import threading
from types import SimpleNamespace
from typing import Any

import pytest

from modules.ai_backend.ipc.dispatcher import ProgressEmitter
from modules.ai_backend.ipc.protocol import (
    HEADER_ID,
    HEADER_KIND,
    KIND_PROGRESS,
    METHOD_INPAINT_SDXL,
    METHOD_INPAINT_SDXL_UNLOAD,
)
from modules.ai_backend.ipc.registry import HandlerContext, Interrupted, get_handler

IMAGE_PNG = b"\x89PNG-image-bytes"
MASK_PNG = b"MASK-bytes!!"
RESULT_PNG = b"\x89PNG-result-\x00\x01\x02"
PREVIEW_PNG = b"\x89PNG-preview-step"

REQUEST_ID = 42


# ---------------------------------------------------------------------------
# Fakes
# ---------------------------------------------------------------------------


class _FakeSdxlService:
    """Stand-in for ``AppState.sdxl_inpaint``.

    ``inpaint_image_bytes`` invokes the supplied ``progress_callback`` a few times
    (with a fake preview) then returns a canned result dict whose ``image_png`` is
    the raw ``RESULT_PNG`` bytes (mirroring the real service, which returns raw
    PNG bytes). No torch / models involved.
    """

    def __init__(
        self,
        result: dict[str, Any] | None = None,
        *,
        n_steps: int = 3,
        preview: Any = "PREVIEW",
        raise_exc: BaseException | None = None,
    ) -> None:
        self._result = result if result is not None else _default_result()
        self._n_steps = n_steps
        self._preview = preview
        self._raise_exc = raise_exc
        self.calls: list[tuple[bytes, bytes, dict[str, Any]]] = []
        self.unload_return = True
        self.unload_calls = 0

    def inpaint_image_bytes(
        self,
        image_bytes: bytes,
        mask_bytes: bytes,
        *,
        params: dict[str, Any],
        progress_callback: Any = None,
    ) -> dict[str, Any]:
        self.calls.append((image_bytes, mask_bytes, params))
        if progress_callback is not None:
            for step in range(1, self._n_steps + 1):
                progress_callback(step, self._n_steps, self._preview)
        if self._raise_exc is not None:
            raise self._raise_exc
        return self._result

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


def _default_result() -> dict[str, Any]:
    return {
        "image_png": RESULT_PNG,
        "source_size": [640, 480],
        "device": "cuda",
        "mode": "nine_channel",
    }


def _ctx(svc: _FakeSdxlService, *, emitter: Any = None) -> HandlerContext:
    state = SimpleNamespace(sdxl_inpaint=svc)
    ctx = HandlerContext(state=state, events=None, get_health_snapshot=lambda: {})
    if emitter is not None:
        # The dispatcher's streaming hook: a per-request emitter on the context.
        ctx.progress_emitter = emitter  # type: ignore[attr-defined]
    return ctx


def _header(extra: dict[str, Any] | None = None) -> dict[str, Any]:
    header: dict[str, Any] = {
        "image_len": len(IMAGE_PNG),
        "mask_len": len(MASK_PNG),
    }
    if extra:
        header.update(extra)
    return header


def _no_cancel() -> threading.Event:
    return threading.Event()


def _emitter() -> tuple[ProgressEmitter, _RecordingDispatcher]:
    disp = _RecordingDispatcher()
    return ProgressEmitter(disp, REQUEST_ID), disp


# Patch the preview encoder so PREVIEW -> PREVIEW_PNG raw bytes (no numpy/PIL).
@pytest.fixture(autouse=True)
def _fake_preview_encoder(monkeypatch: pytest.MonkeyPatch) -> None:
    import modules.ai_backend.sdxl_inpaint_service as svc_mod

    def fake_encode(preview_rgb: Any) -> bytes:
        assert preview_rgb is not None
        return PREVIEW_PNG

    monkeypatch.setattr(svc_mod, "_encode_png_bytes_rgb", fake_encode)


# ---------------------------------------------------------------------------
# Streaming: progress frames + terminal response
# ---------------------------------------------------------------------------


def test_streaming_emits_progress_then_terminal_response() -> None:
    svc = _FakeSdxlService(n_steps=3, preview="PREVIEW")
    emitter, disp = _emitter()
    ctx = _ctx(svc, emitter=emitter)
    params = {"steps": 30, "cfg_scale": 7.0, "mode": "nine_channel"}
    header = _header({"params": params})

    handler = get_handler(METHOD_INPAINT_SDXL)
    resp_header, resp_blob = handler(ctx, header, IMAGE_PNG + MASK_PNG, _no_cancel())

    # One progress frame per diffusion step.
    assert len(disp.frames) == 3
    for i, (frame_header, frame_blob) in enumerate(disp.frames, start=1):
        # progress{id} frame shape: kind/id filled by the emitter,
        # {step,total} in the header, raw preview PNG in the BLOB.
        assert frame_header[HEADER_KIND] == KIND_PROGRESS
        assert frame_header[HEADER_ID] == REQUEST_ID
        assert frame_header["step"] == i
        assert frame_header["total"] == 3
        assert frame_blob == PREVIEW_PNG  # raw bytes, NOT base64

    # Blob split: image and mask reach the service intact; params forwarded.
    assert svc.calls == [(IMAGE_PNG, MASK_PNG, params)]

    # Terminal response: result PNG as the RESPONSE BLOB (raw, byte-identical).
    assert resp_blob == RESULT_PNG
    assert resp_blob == _default_result()["image_png"]
    assert resp_header == {
        "engine": "sdxl",
        "source_size": [640, 480],
        "device": "cuda",
        "mode": "nine_channel",
    }
    assert "image_png" not in resp_header
    assert "image_png_base64" not in resp_header


def test_progress_frame_count_matches_steps() -> None:
    svc = _FakeSdxlService(n_steps=7)
    emitter, disp = _emitter()
    ctx = _ctx(svc, emitter=emitter)
    handler = get_handler(METHOD_INPAINT_SDXL)
    handler(ctx, _header({"params": {}}), IMAGE_PNG + MASK_PNG, _no_cancel())
    assert [h["step"] for h, _ in disp.frames] == [1, 2, 3, 4, 5, 6, 7]
    assert all(h["total"] == 7 for h, _ in disp.frames)


def test_none_preview_yields_empty_progress_blob() -> None:
    svc = _FakeSdxlService(n_steps=2, preview=None)
    emitter, disp = _emitter()
    ctx = _ctx(svc, emitter=emitter)
    handler = get_handler(METHOD_INPAINT_SDXL)
    handler(ctx, _header({"params": {}}), IMAGE_PNG + MASK_PNG, _no_cancel())
    assert len(disp.frames) == 2
    for _, frame_blob in disp.frames:
        assert frame_blob == b""  # no preview => empty blob


def test_no_emitter_still_returns_terminal_response() -> None:
    # Without an attached emitter, progress emission is a no-op; the handler
    # still runs and returns the terminal response correctly.
    svc = _FakeSdxlService(n_steps=4)
    ctx = _ctx(svc)  # no emitter
    handler = get_handler(METHOD_INPAINT_SDXL)
    resp_header, resp_blob = handler(
        ctx, _header({"params": {}}), IMAGE_PNG + MASK_PNG, _no_cancel()
    )
    assert resp_blob == RESULT_PNG
    assert resp_header["engine"] == "sdxl"


# ---------------------------------------------------------------------------
# Blob split / params
# ---------------------------------------------------------------------------


def test_blob_split_by_lengths() -> None:
    svc = _FakeSdxlService(n_steps=0)
    ctx = _ctx(svc)
    image = b"IMAGE-DATA-1234567890"
    mask = b"MASK-XYZ"
    header = {"image_len": len(image), "mask_len": len(mask), "params": {}}
    handler = get_handler(METHOD_INPAINT_SDXL)
    handler(ctx, header, image + mask, _no_cancel())
    assert svc.calls == [(image, mask, {})]


def test_params_defaults_to_empty_dict_when_absent() -> None:
    svc = _FakeSdxlService(n_steps=0)
    ctx = _ctx(svc)
    handler = get_handler(METHOD_INPAINT_SDXL)
    handler(ctx, _header(), IMAGE_PNG + MASK_PNG, _no_cancel())  # no "params"
    assert svc.calls[0][2] == {}


def test_blob_too_short_raises() -> None:
    svc = _FakeSdxlService(n_steps=0)
    ctx = _ctx(svc)
    # Claims more mask bytes than present.
    header = {"image_len": len(IMAGE_PNG), "mask_len": len(MASK_PNG) + 10}
    handler = get_handler(METHOD_INPAINT_SDXL)
    with pytest.raises(ValueError, match="blob length mismatch"):
        handler(ctx, header, IMAGE_PNG + MASK_PNG, _no_cancel())
    assert svc.calls == []


def test_blob_padded_raises() -> None:
    # Trailing bytes after image_len + mask_len must be rejected (strict == check).
    svc = _FakeSdxlService(n_steps=0)
    ctx = _ctx(svc)
    padding = b"\x00\x00\x00"
    header = {"image_len": len(IMAGE_PNG), "mask_len": len(MASK_PNG)}
    handler = get_handler(METHOD_INPAINT_SDXL)
    with pytest.raises(ValueError, match="blob length mismatch"):
        handler(ctx, header, IMAGE_PNG + MASK_PNG + padding, _no_cancel())
    assert svc.calls == []


def test_missing_image_len_raises() -> None:
    svc = _FakeSdxlService(n_steps=0)
    ctx = _ctx(svc)
    handler = get_handler(METHOD_INPAINT_SDXL)
    with pytest.raises(ValueError, match="image_len"):
        handler(ctx, {"mask_len": 1}, b"x", _no_cancel())


def test_empty_image_or_mask_raises() -> None:
    svc = _FakeSdxlService(n_steps=0)
    ctx = _ctx(svc)
    handler = get_handler(METHOD_INPAINT_SDXL)
    with pytest.raises(ValueError, match="image"):
        handler(ctx, {"image_len": 0, "mask_len": len(MASK_PNG)}, MASK_PNG, _no_cancel())
    with pytest.raises(ValueError, match="mask"):
        handler(ctx, {"image_len": len(IMAGE_PNG), "mask_len": 0}, IMAGE_PNG, _no_cancel())


def test_non_object_params_raises() -> None:
    svc = _FakeSdxlService(n_steps=0)
    ctx = _ctx(svc)
    header = _header({"params": ["not", "an", "object"]})
    handler = get_handler(METHOD_INPAINT_SDXL)
    with pytest.raises(ValueError, match="must be an object"):
        handler(ctx, header, IMAGE_PNG + MASK_PNG, _no_cancel())


# ---------------------------------------------------------------------------
# Error mapping (mirror the HTTP path)
# ---------------------------------------------------------------------------


def test_value_error_propagates() -> None:
    svc = _FakeSdxlService(n_steps=1, raise_exc=ValueError("bad params"))
    ctx = _ctx(svc)
    handler = get_handler(METHOD_INPAINT_SDXL)
    with pytest.raises(ValueError, match="bad params"):
        handler(ctx, _header({"params": {}}), IMAGE_PNG + MASK_PNG, _no_cancel())


def test_file_not_found_propagates() -> None:
    svc = _FakeSdxlService(n_steps=0, raise_exc=FileNotFoundError("no model.safetensors"))
    ctx = _ctx(svc)
    handler = get_handler(METHOD_INPAINT_SDXL)
    with pytest.raises(FileNotFoundError, match="no model"):
        handler(ctx, _header({"params": {}}), IMAGE_PNG + MASK_PNG, _no_cancel())


def test_generic_exception_propagates() -> None:
    svc = _FakeSdxlService(n_steps=0, raise_exc=RuntimeError("cuda oom"))
    ctx = _ctx(svc)
    handler = get_handler(METHOD_INPAINT_SDXL)
    with pytest.raises(RuntimeError, match="cuda oom"):
        handler(ctx, _header({"params": {}}), IMAGE_PNG + MASK_PNG, _no_cancel())


# ---------------------------------------------------------------------------
# Cancel
# ---------------------------------------------------------------------------


def test_cancel_before_start_raises_interrupted() -> None:
    svc = _FakeSdxlService(n_steps=0)
    ctx = _ctx(svc)
    cancel = threading.Event()
    cancel.set()
    handler = get_handler(METHOD_INPAINT_SDXL)
    with pytest.raises(Interrupted):
        handler(ctx, _header({"params": {}}), IMAGE_PNG + MASK_PNG, cancel)
    assert svc.calls == []  # never reached the service


def test_cancel_observed_after_service_returns_raises_interrupted() -> None:
    # The service completes, but the cancel event was set meanwhile: the handler
    # must produce the interrupted outcome rather than a normal result.
    svc = _FakeSdxlService(n_steps=0)
    ctx = _ctx(svc)
    cancel = threading.Event()

    orig = svc.inpaint_image_bytes

    def cancel_then_run(*args: Any, **kwargs: Any) -> Any:
        result = orig(*args, **kwargs)
        cancel.set()
        return result

    svc.inpaint_image_bytes = cancel_then_run  # type: ignore[method-assign]
    handler = get_handler(METHOD_INPAINT_SDXL)
    with pytest.raises(Interrupted):
        handler(ctx, _header({"params": {}}), IMAGE_PNG + MASK_PNG, cancel)


def test_exception_during_cancel_maps_to_interrupted() -> None:
    # The service raises while the cancel event is set: mirror the HTTP path's
    # error handling by treating it as an interruption, not an error.
    svc = _FakeSdxlService(n_steps=0, raise_exc=RuntimeError("stopped"))
    ctx = _ctx(svc)
    cancel = threading.Event()

    orig = svc.inpaint_image_bytes

    def cancel_then_raise(*args: Any, **kwargs: Any) -> Any:
        cancel.set()
        return orig(*args, **kwargs)

    svc.inpaint_image_bytes = cancel_then_raise  # type: ignore[method-assign]
    handler = get_handler(METHOD_INPAINT_SDXL)
    with pytest.raises(Interrupted):
        handler(ctx, _header({"params": {}}), IMAGE_PNG + MASK_PNG, cancel)


# ---------------------------------------------------------------------------
# Unload
# ---------------------------------------------------------------------------


def test_unload_returns_flag_true() -> None:
    svc = _FakeSdxlService()
    ctx = _ctx(svc)
    handler = get_handler(METHOD_INPAINT_SDXL_UNLOAD)
    resp_header, resp_blob = handler(ctx, {}, b"", _no_cancel())
    assert resp_header == {"unloaded": True}
    assert resp_blob == b""
    assert svc.unload_calls == 1


def test_unload_returns_flag_false() -> None:
    svc = _FakeSdxlService()
    svc.unload_return = False
    ctx = _ctx(svc)
    handler = get_handler(METHOD_INPAINT_SDXL_UNLOAD)
    resp_header, _ = handler(ctx, {}, b"", _no_cancel())
    assert resp_header == {"unloaded": False}
    assert svc.unload_calls == 1
