"""
File: modules/ai_backend/test_handlers_translate.py

Unit tests for the translate IPC handler group
(modules/ai_backend/ipc/handlers/translate.py).

Covered:
    translate.deep — validates service/source/target/params/texts from header,
                     calls machine_translation.translate_batch(),
                     returns {service, translated, errors, results}, empty blob.

All service calls are mocked; no real translation is performed.
"""

from __future__ import annotations

import threading
from types import SimpleNamespace
from unittest.mock import MagicMock

import pytest

from modules.ai_backend.ipc.handlers import translate as translate_mod
from modules.ai_backend.ipc.protocol import METHOD_TRANSLATE_DEEP
from modules.ai_backend.ipc.registry import HandlerContext, METHOD_HANDLERS


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_ctx(mt_service=None):
    state = SimpleNamespace(machine_translation=mt_service or MagicMock())
    return HandlerContext(
        state=state,
        events=MagicMock(),
        get_health_snapshot=lambda: {},
    )


def _call(handler_fn, ctx, header, blob=b""):
    return handler_fn(ctx, header, blob, threading.Event())


def _ok_results(n=2):
    return [{"ok": True, "text": f"translated_{i}"} for i in range(n)]


def _mixed_results():
    return [
        {"ok": True, "text": "translated"},
        {"ok": False, "error": "service error"},
        {"ok": True, "text": "another"},
    ]


# ---------------------------------------------------------------------------
# Registration
# ---------------------------------------------------------------------------

def test_translate_deep_is_registered():
    assert METHOD_TRANSLATE_DEEP in METHOD_HANDLERS


# ---------------------------------------------------------------------------
# translate.deep — happy path
# ---------------------------------------------------------------------------

class TestTranslateDeepHappyPath:
    def test_calls_translate_batch_with_correct_args(self):
        svc = MagicMock()
        svc.translate_batch.return_value = _ok_results(2)
        ctx = _make_ctx(svc)
        header = {
            "service": "google",
            "source": "ko",
            "target": "en",
            "params": {"formality": "less"},
            "texts": ["안녕", "세상"],
        }

        _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, header)

        svc.translate_batch.assert_called_once_with(
            service="google",
            source="ko",
            target="en",
            params={"formality": "less"},
            texts=["안녕", "세상"],
        )

    def test_response_keys(self):
        svc = MagicMock()
        svc.translate_batch.return_value = _ok_results(2)
        ctx = _make_ctx(svc)
        header = {"service": "google", "source": "auto", "target": "ru", "texts": ["hello"]}

        resp, blob = _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, header)

        assert set(resp.keys()) == {"service", "translated", "errors", "results"}
        assert blob == b""

    def test_translated_and_errors_counts_all_ok(self):
        svc = MagicMock()
        results = _ok_results(3)
        svc.translate_batch.return_value = results
        ctx = _make_ctx(svc)
        header = {"texts": ["a", "b", "c"]}

        resp, _ = _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, header)

        assert resp["translated"] == 3
        assert resp["errors"] == 0
        assert resp["results"] == results

    def test_translated_and_errors_counts_mixed(self):
        svc = MagicMock()
        results = _mixed_results()
        svc.translate_batch.return_value = results
        ctx = _make_ctx(svc)
        header = {"texts": ["a", "b", "c"]}

        resp, _ = _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, header)

        assert resp["translated"] == 2
        assert resp["errors"] == 1

    def test_service_normalized_to_lowercase(self):
        svc = MagicMock()
        svc.translate_batch.return_value = _ok_results(1)
        ctx = _make_ctx(svc)
        header = {"service": "  GOOGLE  ", "texts": ["hello"]}

        resp, _ = _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, header)

        assert resp["service"] == "google"

    def test_blob_is_always_empty(self):
        svc = MagicMock()
        svc.translate_batch.return_value = _ok_results(1)
        ctx = _make_ctx(svc)
        header = {"texts": ["hello"]}

        _, blob = _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, header)

        assert blob == b""


# ---------------------------------------------------------------------------
# translate.deep — defaults
# ---------------------------------------------------------------------------

class TestTranslateDeepDefaults:
    def test_service_defaults_to_google_when_absent(self):
        svc = MagicMock()
        svc.translate_batch.return_value = _ok_results(1)
        ctx = _make_ctx(svc)

        resp, _ = _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"texts": ["x"]})

        _, kwargs = svc.translate_batch.call_args
        assert kwargs["service"] == "google"
        assert resp["service"] == "google"

    def test_service_defaults_to_google_when_null(self):
        svc = MagicMock()
        svc.translate_batch.return_value = _ok_results(1)
        ctx = _make_ctx(svc)

        _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"service": None, "texts": ["x"]})

        _, kwargs = svc.translate_batch.call_args
        assert kwargs["service"] == "google"

    def test_source_defaults_to_auto(self):
        svc = MagicMock()
        svc.translate_batch.return_value = _ok_results(1)
        ctx = _make_ctx(svc)

        _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"texts": ["x"]})

        _, kwargs = svc.translate_batch.call_args
        assert kwargs["source"] == "auto"

    def test_source_defaults_to_auto_when_null(self):
        svc = MagicMock()
        svc.translate_batch.return_value = _ok_results(1)
        ctx = _make_ctx(svc)

        _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"source": None, "texts": ["x"]})

        _, kwargs = svc.translate_batch.call_args
        assert kwargs["source"] == "auto"

    def test_target_defaults_to_ru(self):
        svc = MagicMock()
        svc.translate_batch.return_value = _ok_results(1)
        ctx = _make_ctx(svc)

        _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"texts": ["x"]})

        _, kwargs = svc.translate_batch.call_args
        assert kwargs["target"] == "ru"

    def test_target_defaults_to_ru_when_null(self):
        svc = MagicMock()
        svc.translate_batch.return_value = _ok_results(1)
        ctx = _make_ctx(svc)

        _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"target": None, "texts": ["x"]})

        _, kwargs = svc.translate_batch.call_args
        assert kwargs["target"] == "ru"

    def test_params_defaults_to_empty_dict(self):
        svc = MagicMock()
        svc.translate_batch.return_value = _ok_results(1)
        ctx = _make_ctx(svc)

        _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"texts": ["x"]})

        _, kwargs = svc.translate_batch.call_args
        assert kwargs["params"] == {}

    def test_params_none_treated_as_empty_dict(self):
        svc = MagicMock()
        svc.translate_batch.return_value = _ok_results(1)
        ctx = _make_ctx(svc)

        _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"params": None, "texts": ["x"]})

        _, kwargs = svc.translate_batch.call_args
        assert kwargs["params"] == {}

    def test_texts_items_coerced_to_str(self):
        svc = MagicMock()
        svc.translate_batch.return_value = _ok_results(3)
        ctx = _make_ctx(svc)

        _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"texts": [None, 42, "hello"]})

        _, kwargs = svc.translate_batch.call_args
        assert kwargs["texts"] == ["", "42", "hello"]


# ---------------------------------------------------------------------------
# translate.deep — validation errors
# ---------------------------------------------------------------------------

class TestTranslateDeepValidation:
    def test_missing_texts_raises_value_error(self):
        ctx = _make_ctx()

        with pytest.raises(ValueError, match="'texts' must be an array"):
            _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {})

    def test_non_list_texts_raises_value_error(self):
        ctx = _make_ctx()

        with pytest.raises(ValueError, match="'texts' must be an array"):
            _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"texts": "not a list"})

    def test_empty_texts_raises_value_error(self):
        ctx = _make_ctx()

        with pytest.raises(ValueError, match="'texts' must not be empty"):
            _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"texts": []})

    def test_non_string_service_raises_value_error(self):
        ctx = _make_ctx()

        with pytest.raises(ValueError, match="'service' must be a string"):
            _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"service": 99, "texts": ["x"]})

    def test_non_string_source_raises_value_error(self):
        ctx = _make_ctx()

        with pytest.raises(ValueError, match="'source' must be a string"):
            _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"source": True, "texts": ["x"]})

    def test_non_string_target_raises_value_error(self):
        ctx = _make_ctx()

        with pytest.raises(ValueError, match="'target' must be a string"):
            _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"target": [], "texts": ["x"]})

    def test_non_dict_params_raises_value_error(self):
        ctx = _make_ctx()

        with pytest.raises(ValueError, match="'params' must be an object"):
            _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"params": "bad", "texts": ["x"]})

    def test_service_value_error_re_raised(self):
        svc = MagicMock()
        svc.translate_batch.side_effect = ValueError("unknown service: bogus")
        ctx = _make_ctx(svc)

        with pytest.raises(ValueError, match="unknown service"):
            _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"texts": ["x"]})

    def test_service_runtime_error_re_raised(self):
        svc = MagicMock()
        svc.translate_batch.side_effect = RuntimeError("connection timeout")
        ctx = _make_ctx(svc)

        with pytest.raises(RuntimeError, match="connection timeout"):
            _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"texts": ["x"]})


# ---------------------------------------------------------------------------
# translate.deep — no ok key in response (dispatcher adds status)
# ---------------------------------------------------------------------------

def test_no_ok_key_in_response():
    svc = MagicMock()
    svc.translate_batch.return_value = _ok_results(1)
    ctx = _make_ctx(svc)

    resp, _ = _call(METHOD_HANDLERS[METHOD_TRANSLATE_DEEP], ctx, {"texts": ["hello"]})

    assert "ok" not in resp
