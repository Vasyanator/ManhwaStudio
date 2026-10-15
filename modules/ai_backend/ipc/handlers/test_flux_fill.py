"""
File: modules/ai_backend/ipc/handlers/test_flux_fill.py

Purpose:
Unit tests for the cancellation wiring of the `inpaint.flux_fill` IPC handler
(`modules/ai_backend/ipc/handlers/flux_fill.py`), called directly with a fake
`AppState.flux_fill_inpaint` service (no torch, no weights, no network).

Coverage:
- the handler hands `cancel_event.is_set` to the service as `should_cancel`, so
  the weight download observes a cancel while it transfers or waits to retry;
- a `DownloadCancelled` raised by the download while the event is set becomes
  the dispatcher's `Interrupted`;
- a retry announced through the service's progress callback reaches the wire as
  an ordinary `download` frame: same four header fields, only the `label` text
  differs (no protocol change).
"""

from __future__ import annotations

import threading
from types import SimpleNamespace
from typing import Any, Callable

import pytest

from modules.ai_backend.engines.model_download import DownloadCancelled
from modules.ai_backend.ipc.protocol import METHOD_INPAINT_FLUX_FILL
from modules.ai_backend.ipc.registry import HandlerContext, Interrupted, get_handler

IMAGE_PNG = b"\x89PNG-image"
MASK_PNG = b"MASK"


class _FakeFluxService:
    """Stand-in for `AppState.flux_fill_inpaint` that records `should_cancel`.

    `behaviour(progress_callback, should_cancel)` runs inside
    `inpaint_image_bytes` and may raise to model a failed download.
    """

    def __init__(self, behaviour: Callable[[Any, Callable[[], bool] | None], None]) -> None:
        self._behaviour = behaviour
        self.should_cancel: Callable[[], bool] | None = None

    def inpaint_image_bytes(
        self,
        image_bytes: bytes,
        mask_bytes: bytes,
        *,
        params: dict[str, Any] | None = None,
        progress_callback: Any = None,
        should_cancel: Callable[[], bool] | None = None,
    ) -> dict[str, Any]:
        self.should_cancel = should_cancel
        self._behaviour(progress_callback, should_cancel)
        return {"image_png": b"RESULT", "source_size": [1, 1], "device": "cpu", "mode": "inpaint", "quant": "Q8_0"}


class _RecordingEmitter:
    """Collects the progress headers the handler emits."""

    def __init__(self) -> None:
        self.frames: list[dict[str, Any]] = []

    def emit(self, fields: dict[str, Any], blob: bytes) -> None:
        self.frames.append(dict(fields))


def _ctx(svc: _FakeFluxService, emitter: _RecordingEmitter | None = None) -> HandlerContext:
    ctx = HandlerContext(state=SimpleNamespace(flux_fill_inpaint=svc), events=None, get_health_snapshot=lambda: {})
    if emitter is not None:
        ctx.progress_emitter = emitter  # type: ignore[attr-defined]
    return ctx


def _header() -> dict[str, Any]:
    return {"image_len": len(IMAGE_PNG), "mask_len": len(MASK_PNG), "params": {}}


def test_the_cancel_event_reaches_the_service_as_should_cancel() -> None:
    svc = _FakeFluxService(lambda _cb, _cancel: None)
    cancel = threading.Event()

    get_handler(METHOD_INPAINT_FLUX_FILL)(_ctx(svc), _header(), IMAGE_PNG + MASK_PNG, cancel)

    assert svc.should_cancel is not None
    assert svc.should_cancel() is False
    cancel.set()
    assert svc.should_cancel() is True


def test_a_cancelled_download_becomes_interrupted() -> None:
    cancel = threading.Event()

    def download_then_cancel(_cb: Any, should_cancel: Callable[[], bool] | None) -> None:
        cancel.set()
        assert should_cancel is not None and should_cancel()
        raise DownloadCancelled("cancelled during the retry wait")

    svc = _FakeFluxService(download_then_cancel)

    with pytest.raises(Interrupted):
        get_handler(METHOD_INPAINT_FLUX_FILL)(_ctx(svc), _header(), IMAGE_PNG + MASK_PNG, cancel)


def test_a_retry_label_travels_in_the_existing_frame_shape() -> None:
    label = "Сеть недоступна, повтор через 2 с (попытка 1/5): flux1-fill-dev-Q8_0.gguf"

    def report_retry(cb: Any, _cancel: Any) -> None:
        cb("download", 4096, 40960, label)

    emitter = _RecordingEmitter()
    svc = _FakeFluxService(report_retry)

    get_handler(METHOD_INPAINT_FLUX_FILL)(_ctx(svc, emitter), _header(), IMAGE_PNG + MASK_PNG, threading.Event())

    assert emitter.frames == [{"phase": "download", "step": 4096, "total": 40960, "label": label}]
