"""
File: modules/ai_backend/ipc/handlers/test_reline.py

Unit tests for the reline IPC handler group
(modules/ai_backend/ipc/handlers/reline.py).

Covered:
    reline.models  — calls list_models(), returns {"models": [...]}, empty blob.
    reline.process — parses image_path/output_path/params from header,
                     calls process_image_file(), returns result dict verbatim,
                     empty blob.
    cancel         — both hand ``cancel_event.is_set`` to the service as
                     ``should_cancel``; a cancel before start or a service error
                     raised while the event is set becomes ``Interrupted``.

All service calls are mocked; no torch or real models are needed.
"""

from __future__ import annotations

import threading
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

import pytest

from modules.ai_backend.ipc.handlers import reline as reline_mod
from modules.ai_backend.ipc.protocol import METHOD_RELINE_MODELS, METHOD_RELINE_PROCESS
from modules.ai_backend.ipc.registry import HandlerContext, Interrupted, METHOD_HANDLERS


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_ctx(reline_service=None):
    state = SimpleNamespace(reline=reline_service or MagicMock())
    return HandlerContext(
        state=state,
        events=MagicMock(),
        get_health_snapshot=lambda: {},
    )


def _call(handler_fn, ctx, header, blob=b""):
    return handler_fn(ctx, header, blob, threading.Event())


# ---------------------------------------------------------------------------
# Registration
# ---------------------------------------------------------------------------

def test_reline_models_is_registered():
    assert METHOD_RELINE_MODELS in METHOD_HANDLERS


def test_reline_process_is_registered():
    assert METHOD_RELINE_PROCESS in METHOD_HANDLERS


# ---------------------------------------------------------------------------
# reline.models
# ---------------------------------------------------------------------------

class TestRelineModels:
    def test_calls_list_models(self):
        svc = MagicMock()
        svc.list_models.return_value = [{"name": "model-a", "filename": "a.pth", "downloaded": True}]
        ctx = _make_ctx(svc)

        resp, blob = _call(METHOD_HANDLERS[METHOD_RELINE_MODELS], ctx, {})

        svc.list_models.assert_called_once()
        assert set(svc.list_models.call_args.kwargs) == {"should_cancel"}
        assert blob == b""

    def test_response_contains_models_key(self):
        svc = MagicMock()
        model_list = [{"name": "m1", "filename": "m1.pth", "downloaded": False}]
        svc.list_models.return_value = model_list
        ctx = _make_ctx(svc)

        resp, blob = _call(METHOD_HANDLERS[METHOD_RELINE_MODELS], ctx, {})

        assert "models" in resp
        assert resp["models"] == model_list

    def test_empty_model_list(self):
        svc = MagicMock()
        svc.list_models.return_value = []
        ctx = _make_ctx(svc)

        resp, blob = _call(METHOD_HANDLERS[METHOD_RELINE_MODELS], ctx, {})

        assert resp["models"] == []
        assert blob == b""

    def test_service_error_is_raised(self):
        svc = MagicMock()
        svc.list_models.side_effect = RuntimeError("network error")
        ctx = _make_ctx(svc)

        with pytest.raises(RuntimeError, match="network error"):
            _call(METHOD_HANDLERS[METHOD_RELINE_MODELS], ctx, {})

    def test_blob_is_ignored(self):
        """No-op: reline.models ignores any blob."""
        svc = MagicMock()
        svc.list_models.return_value = []
        ctx = _make_ctx(svc)

        resp, blob = _call(METHOD_HANDLERS[METHOD_RELINE_MODELS], ctx, {}, b"ignored")

        assert blob == b""


# ---------------------------------------------------------------------------
# reline.process
# ---------------------------------------------------------------------------

_PROCESS_RESULT = {"ok": True, "output": "/tmp/out.png", "elapsed_ms": 123}


class TestRelineProcess:
    def _svc(self, result=None):
        svc = MagicMock()
        svc.process_image_file.return_value = result or dict(_PROCESS_RESULT)
        return svc

    def test_calls_process_image_file_with_required_args(self):
        svc = self._svc()
        ctx = _make_ctx(svc)
        header = {"image_path": "/in/page.png", "output_path": "/out/page.png", "params": {}}

        _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

        _, kwargs = svc.process_image_file.call_args
        assert svc.process_image_file.call_count == 1
        assert kwargs["image_path"] == "/in/page.png"
        assert kwargs["output_path"] == "/out/page.png"
        assert kwargs["params"] == {}
        assert callable(kwargs["should_cancel"])

    def test_output_path_none_when_absent(self):
        svc = self._svc()
        ctx = _make_ctx(svc)
        header = {"image_path": "/in/page.png"}

        _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

        _, kwargs = svc.process_image_file.call_args
        assert kwargs.get("output_path") is None

    def test_output_path_none_when_null(self):
        svc = self._svc()
        ctx = _make_ctx(svc)
        header = {"image_path": "/in/page.png", "output_path": None}

        _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

        _, kwargs = svc.process_image_file.call_args
        assert kwargs.get("output_path") is None

    def test_output_path_none_when_empty_string(self):
        svc = self._svc()
        ctx = _make_ctx(svc)
        header = {"image_path": "/in/page.png", "output_path": "  "}

        _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

        _, kwargs = svc.process_image_file.call_args
        assert kwargs.get("output_path") is None

    def test_result_dict_passed_through_verbatim(self):
        custom_result = {"ok": True, "some_extra_key": 42, "output": "/out/x.png"}
        svc = self._svc(custom_result)
        ctx = _make_ctx(svc)
        header = {"image_path": "/in/page.png"}

        resp, blob = _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

        assert resp == custom_result
        assert blob == b""

    def test_params_default_to_empty_dict(self):
        svc = self._svc()
        ctx = _make_ctx(svc)
        header = {"image_path": "/in/page.png"}

        _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

        _, kwargs = svc.process_image_file.call_args
        assert kwargs.get("params") == {}

    def test_params_none_treated_as_empty_dict(self):
        svc = self._svc()
        ctx = _make_ctx(svc)
        header = {"image_path": "/in/page.png", "params": None}

        _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

        _, kwargs = svc.process_image_file.call_args
        assert kwargs.get("params") == {}

    def test_params_forwarded(self):
        svc = self._svc()
        ctx = _make_ctx(svc)
        params = {"upscale": {"enabled": False}, "sharp": 0.5}
        header = {"image_path": "/in/page.png", "params": params}

        _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

        _, kwargs = svc.process_image_file.call_args
        assert kwargs.get("params") == params

    def test_image_path_whitespace_stripped(self):
        svc = self._svc()
        ctx = _make_ctx(svc)
        header = {"image_path": "  /in/page.png  "}

        _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

        _, kwargs = svc.process_image_file.call_args
        assert kwargs.get("image_path") == "/in/page.png"

    def test_missing_image_path_raises_value_error(self):
        svc = self._svc()
        ctx = _make_ctx(svc)

        with pytest.raises(ValueError, match="image_path"):
            _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, {})

    def test_empty_image_path_raises_value_error(self):
        svc = self._svc()
        ctx = _make_ctx(svc)

        with pytest.raises(ValueError, match="image_path"):
            _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, {"image_path": "   "})

    def test_non_string_image_path_raises_value_error(self):
        svc = self._svc()
        ctx = _make_ctx(svc)

        with pytest.raises(ValueError, match="image_path"):
            _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, {"image_path": 123})

    def test_non_string_output_path_raises_value_error(self):
        svc = self._svc()
        ctx = _make_ctx(svc)
        header = {"image_path": "/in/page.png", "output_path": 42}

        with pytest.raises(ValueError, match="output_path"):
            _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

    def test_non_dict_params_raises_value_error(self):
        svc = self._svc()
        ctx = _make_ctx(svc)
        header = {"image_path": "/in/page.png", "params": "bad"}

        with pytest.raises(ValueError, match="params"):
            _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

    def test_service_file_not_found_propagates_as_file_not_found_error(self):
        # FileNotFoundError must propagate as-is (not wrapped in ValueError) so
        # callers and the dispatcher can distinguish it from a validation failure.
        svc = MagicMock()
        svc.process_image_file.side_effect = FileNotFoundError("no such file")
        ctx = _make_ctx(svc)
        header = {"image_path": "/in/missing.png"}

        with pytest.raises(FileNotFoundError, match="no such file"):
            _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

    def test_service_value_error_re_raised(self):
        svc = MagicMock()
        svc.process_image_file.side_effect = ValueError("bad params")
        ctx = _make_ctx(svc)
        header = {"image_path": "/in/page.png"}

        with pytest.raises(ValueError, match="bad params"):
            _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

    def test_service_runtime_error_re_raised(self):
        svc = MagicMock()
        svc.process_image_file.side_effect = RuntimeError("gpu oom")
        ctx = _make_ctx(svc)
        header = {"image_path": "/in/page.png"}

        with pytest.raises(RuntimeError, match="gpu oom"):
            _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

    def test_response_blob_is_always_empty(self):
        svc = self._svc({"ok": True})
        ctx = _make_ctx(svc)
        header = {"image_path": "/in/page.png"}

        _, blob = _call(METHOD_HANDLERS[METHOD_RELINE_PROCESS], ctx, header)

        assert blob == b""


# ---------------------------------------------------------------------------
# cancel
# ---------------------------------------------------------------------------

_PROCESS_HEADER = {"image_path": "/in/page.png"}


@pytest.mark.parametrize(
    ("method", "service_attr", "header"),
    [
        (METHOD_RELINE_MODELS, "list_models", {}),
        (METHOD_RELINE_PROCESS, "process_image_file", _PROCESS_HEADER),
    ],
)
def test_cancel_before_start_never_reaches_the_service(method, service_attr, header):
    svc = MagicMock()
    cancel = threading.Event()
    cancel.set()

    with pytest.raises(Interrupted):
        METHOD_HANDLERS[method](_make_ctx(svc), header, b"", cancel)

    getattr(svc, service_attr).assert_not_called()


@pytest.mark.parametrize(
    ("method", "service_attr", "header"),
    [
        (METHOD_RELINE_MODELS, "list_models", {}),
        (METHOD_RELINE_PROCESS, "process_image_file", _PROCESS_HEADER),
    ],
)
def test_a_download_cancelled_through_the_event_is_interrupted(method, service_attr, header):
    # The service polls `should_cancel`; a cancel mid-download raises
    # `DownloadCancelled`, which must not be wrapped into a RuntimeError.
    from modules.ai_backend.engines.model_download import DownloadCancelled

    svc = MagicMock()
    cancel = threading.Event()
    seen = []

    def run(**kwargs):
        seen.append(kwargs["should_cancel"]())
        cancel.set()
        seen.append(kwargs["should_cancel"]())
        raise DownloadCancelled("Загрузка файла отменена: model.pth")

    getattr(svc, service_attr).side_effect = run

    with pytest.raises(Interrupted):
        METHOD_HANDLERS[method](_make_ctx(svc), header, b"", cancel)
    assert seen == [False, True]
