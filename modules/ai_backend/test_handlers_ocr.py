"""
File: modules/ai_backend/test_handlers_ocr.py

Purpose:
Unit tests for the v2 IPC OCR handlers
(`modules/ai_backend/ipc/handlers/ocr.py`).

Main responsibilities:
- each handler decodes the request blob and passes it through to the matching
  service `recognize_image_bytes`, with the same kwargs the HTTP `*_worker`
  uses;
- request params/defaults are parsed from the header identically to the HTTP
  handlers (`easy_langs`, `paddle_lang`, `paddle_vl_script`, the surya knobs,
  `paddle_onnx_model`/`paddle_onnx_device`, `join_newlines`/`reflect_strings`);
- response fields match the HTTP shape (`engine`/`lines`/`text`, plus
  `task_name` for surya and `model`/`device` for paddle_onnx);
- a set `cancel_event` raises `Interrupted`; an empty blob raises `ValueError`;
  invalid params raise `ValueError`.

Notes:
The service methods are mocked (a fake object recording its call), so these
tests need NO torch/paddle/models. Handlers are invoked directly with a
`HandlerContext` wrapping a `_FakeState`.
"""

from __future__ import annotations

import threading
from types import SimpleNamespace
from typing import Any

import pytest

from modules.ai_backend.ipc import registry
from modules.ai_backend.ipc.handlers import ocr as ocr_handlers
from modules.ai_backend.ipc.protocol import (
    METHOD_OCR_EASY,
    METHOD_OCR_PADDLE,
    METHOD_OCR_PADDLE_ONNX,
    METHOD_OCR_PADDLE_VL,
    METHOD_OCR_SURYA,
)
from modules.ai_backend.ipc.registry import HandlerContext, Interrupted

IMG = b"\x89PNG\r\n\x1a\nFAKEIMAGEBYTES"


class _FakeOcr:
    """A stand-in OCR service: records the last call and returns a fixed result."""

    def __init__(self, result: dict[str, Any]) -> None:
        self._result = result
        self.calls: list[tuple[tuple[Any, ...], dict[str, Any]]] = []

    def recognize_image_bytes(self, *args: Any, **kwargs: Any) -> dict[str, Any]:
        self.calls.append((args, kwargs))
        return self._result

    @property
    def last_kwargs(self) -> dict[str, Any]:
        return self.calls[-1][1]

    @property
    def last_image(self) -> Any:
        return self.calls[-1][0][0]


def _make_ctx(**services: _FakeOcr) -> HandlerContext:
    state = SimpleNamespace(**services)
    return HandlerContext(state=state, events=None, get_health_snapshot=lambda: {})


def _no_cancel() -> threading.Event:
    return threading.Event()


def _canceled() -> threading.Event:
    ev = threading.Event()
    ev.set()
    return ev


# ---------------------------------------------------------------------------
# Registration
# ---------------------------------------------------------------------------

@pytest.mark.parametrize(
    "method",
    [
        METHOD_OCR_EASY,
        METHOD_OCR_PADDLE,
        METHOD_OCR_PADDLE_VL,
        METHOD_OCR_SURYA,
        METHOD_OCR_PADDLE_ONNX,
    ],
)
def test_methods_registered(method: str) -> None:
    assert registry.get_handler(method) is not None


# ---------------------------------------------------------------------------
# ocr.easy
# ---------------------------------------------------------------------------

def test_easy_defaults_and_response() -> None:
    fake = _FakeOcr({"lines": ["a", "b"], "text": "a\nb"})
    ctx = _make_ctx(easy_ocr=fake)

    header, blob = ocr_handlers._handle_ocr_easy(ctx, {}, IMG, _no_cancel())

    assert fake.last_image == IMG
    assert fake.last_kwargs == {
        "join_newlines": True,
        "reflect_strings": False,
        "langs": "ko",
    }
    assert header == {"engine": "easyocr", "lines": ["a", "b"], "text": "a\nb"}
    assert blob == b""


def test_easy_parses_params() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(easy_ocr=fake)
    req = {
        "join_newlines": False,
        "reflect_strings": True,
        "easy_langs": "  ja  ",
    }
    ocr_handlers._handle_ocr_easy(ctx, req, IMG, _no_cancel())
    assert fake.last_kwargs == {
        "join_newlines": False,
        "reflect_strings": True,
        "langs": "ja",
    }


def test_easy_blank_langs_falls_back_to_ko() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(easy_ocr=fake)
    ocr_handlers._handle_ocr_easy(ctx, {"easy_langs": "   "}, IMG, _no_cancel())
    assert fake.last_kwargs["langs"] == "ko"


def test_easy_none_langs_falls_back_to_ko() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(easy_ocr=fake)
    ocr_handlers._handle_ocr_easy(ctx, {"easy_langs": None}, IMG, _no_cancel())
    assert fake.last_kwargs["langs"] == "ko"


def test_easy_non_string_langs_errors() -> None:
    ctx = _make_ctx(easy_ocr=_FakeOcr({"lines": [], "text": ""}))
    with pytest.raises(ValueError, match="easy_langs"):
        ocr_handlers._handle_ocr_easy(ctx, {"easy_langs": 5}, IMG, _no_cancel())


def test_easy_empty_blob_errors() -> None:
    ctx = _make_ctx(easy_ocr=_FakeOcr({"lines": [], "text": ""}))
    with pytest.raises(ValueError, match="frame blob"):
        ocr_handlers._handle_ocr_easy(ctx, {}, b"", _no_cancel())


def test_easy_canceled_before_start() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(easy_ocr=fake)
    with pytest.raises(Interrupted):
        ocr_handlers._handle_ocr_easy(ctx, {}, IMG, _canceled())
    assert fake.calls == []  # service never invoked


# ---------------------------------------------------------------------------
# ocr.paddle
# ---------------------------------------------------------------------------

def test_paddle_defaults_and_response() -> None:
    fake = _FakeOcr({"lines": ["x"], "text": "x"})
    ctx = _make_ctx(paddle_ocr=fake)
    header, blob = ocr_handlers._handle_ocr_paddle(ctx, {}, IMG, _no_cancel())
    assert fake.last_image == IMG
    assert fake.last_kwargs == {
        "join_newlines": True,
        "reflect_strings": False,
        "lang": "korean_v5",
    }
    assert header == {"engine": "paddleocr", "lines": ["x"], "text": "x"}
    assert blob == b""


def test_paddle_parses_lang() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(paddle_ocr=fake)
    ocr_handlers._handle_ocr_paddle(ctx, {"paddle_lang": " chinese_v5 "}, IMG, _no_cancel())
    assert fake.last_kwargs["lang"] == "chinese_v5"


def test_paddle_blank_lang_falls_back() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(paddle_ocr=fake)
    ocr_handlers._handle_ocr_paddle(ctx, {"paddle_lang": ""}, IMG, _no_cancel())
    assert fake.last_kwargs["lang"] == "korean_v5"


def test_paddle_non_string_lang_errors() -> None:
    ctx = _make_ctx(paddle_ocr=_FakeOcr({"lines": [], "text": ""}))
    with pytest.raises(ValueError, match="paddle_lang"):
        ocr_handlers._handle_ocr_paddle(ctx, {"paddle_lang": 1}, IMG, _no_cancel())


def test_paddle_empty_blob_errors() -> None:
    ctx = _make_ctx(paddle_ocr=_FakeOcr({"lines": [], "text": ""}))
    with pytest.raises(ValueError, match="frame blob"):
        ocr_handlers._handle_ocr_paddle(ctx, {}, b"", _no_cancel())


def test_paddle_canceled() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(paddle_ocr=fake)
    with pytest.raises(Interrupted):
        ocr_handlers._handle_ocr_paddle(ctx, {}, IMG, _canceled())
    assert fake.calls == []


# ---------------------------------------------------------------------------
# ocr.paddle_vl
# ---------------------------------------------------------------------------

def test_paddle_vl_defaults_script_none() -> None:
    fake = _FakeOcr({"lines": ["v"], "text": "v"})
    ctx = _make_ctx(paddle_vl_ocr=fake)
    header, blob = ocr_handlers._handle_ocr_paddle_vl(ctx, {}, IMG, _no_cancel())
    assert fake.last_kwargs == {
        "join_newlines": True,
        "reflect_strings": False,
        "script": None,
    }
    assert header == {"engine": "paddleocrvl", "lines": ["v"], "text": "v"}
    assert blob == b""


def test_paddle_vl_script_lowercased() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(paddle_vl_ocr=fake)
    ocr_handlers._handle_ocr_paddle_vl(ctx, {"paddle_vl_script": " Latin "}, IMG, _no_cancel())
    assert fake.last_kwargs["script"] == "latin"


def test_paddle_vl_blank_script_is_none() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(paddle_vl_ocr=fake)
    ocr_handlers._handle_ocr_paddle_vl(ctx, {"paddle_vl_script": "   "}, IMG, _no_cancel())
    assert fake.last_kwargs["script"] is None


def test_paddle_vl_non_string_script_errors() -> None:
    ctx = _make_ctx(paddle_vl_ocr=_FakeOcr({"lines": [], "text": ""}))
    with pytest.raises(ValueError, match="paddle_vl_script"):
        ocr_handlers._handle_ocr_paddle_vl(ctx, {"paddle_vl_script": 7}, IMG, _no_cancel())


def test_paddle_vl_empty_blob_errors() -> None:
    ctx = _make_ctx(paddle_vl_ocr=_FakeOcr({"lines": [], "text": ""}))
    with pytest.raises(ValueError, match="frame blob"):
        ocr_handlers._handle_ocr_paddle_vl(ctx, {}, b"", _no_cancel())


def test_paddle_vl_canceled() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(paddle_vl_ocr=fake)
    with pytest.raises(Interrupted):
        ocr_handlers._handle_ocr_paddle_vl(ctx, {}, IMG, _canceled())
    assert fake.calls == []


# ---------------------------------------------------------------------------
# ocr.surya
# ---------------------------------------------------------------------------

def test_surya_defaults_and_response() -> None:
    fake = _FakeOcr({"lines": ["s"], "text": "s"})
    ctx = _make_ctx(surya_ocr=fake)
    header, blob = ocr_handlers._handle_ocr_surya(ctx, {}, IMG, _no_cancel())
    assert fake.last_image == IMG
    assert fake.last_kwargs == {
        "join_newlines": True,
        "reflect_strings": False,
        "task_name": "ocr_without_boxes",
        "recognize_math": False,
        "sort_lines": False,
        "drop_repeated_text": False,
        "max_sliding_window": None,
        "max_tokens": None,
    }
    assert header == {
        "engine": "suryaocr",
        "task_name": "ocr_without_boxes",
        "lines": ["s"],
        "text": "s",
    }
    assert blob == b""


def test_surya_parses_all_params() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(surya_ocr=fake)
    req = {
        "join_newlines": False,
        "reflect_strings": True,
        "surya_task_name": "  OCR_WITH_BOXES  ",
        "surya_recognize_math": True,
        "surya_sort_lines": True,
        "surya_drop_repeated_text": True,
        "surya_max_sliding_window": 512,
        "surya_max_tokens": 256,
    }
    header, _ = ocr_handlers._handle_ocr_surya(ctx, req, IMG, _no_cancel())
    assert fake.last_kwargs == {
        "join_newlines": False,
        "reflect_strings": True,
        "task_name": "ocr_with_boxes",
        "recognize_math": True,
        "sort_lines": True,
        "drop_repeated_text": True,
        "max_sliding_window": 512,
        "max_tokens": 256,
    }
    # task_name in the response is the normalized (lowercased) value.
    assert header["task_name"] == "ocr_with_boxes"


def test_surya_non_string_task_name_errors() -> None:
    ctx = _make_ctx(surya_ocr=_FakeOcr({"lines": [], "text": ""}))
    with pytest.raises(ValueError, match="surya_task_name"):
        ocr_handlers._handle_ocr_surya(ctx, {"surya_task_name": 3}, IMG, _no_cancel())


@pytest.mark.parametrize("field", ["surya_max_sliding_window", "surya_max_tokens"])
@pytest.mark.parametrize("bad", [0, -5, "10", 3.5, True])
def test_surya_invalid_positive_int_errors(field: str, bad: Any) -> None:
    ctx = _make_ctx(surya_ocr=_FakeOcr({"lines": [], "text": ""}))
    with pytest.raises(ValueError, match=field):
        ocr_handlers._handle_ocr_surya(ctx, {field: bad}, IMG, _no_cancel())


def test_surya_empty_blob_errors() -> None:
    ctx = _make_ctx(surya_ocr=_FakeOcr({"lines": [], "text": ""}))
    with pytest.raises(ValueError, match="frame blob"):
        ocr_handlers._handle_ocr_surya(ctx, {}, b"", _no_cancel())


def test_surya_canceled() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(surya_ocr=fake)
    with pytest.raises(Interrupted):
        ocr_handlers._handle_ocr_surya(ctx, {}, IMG, _canceled())
    assert fake.calls == []


# ---------------------------------------------------------------------------
# ocr.paddle_onnx (routes through paddle_ocr like ocr.paddle)
# ---------------------------------------------------------------------------

def test_paddle_onnx_defaults_and_response() -> None:
    fake = _FakeOcr({"lines": ["o"], "text": "o"})
    ctx = _make_ctx(paddle_ocr=fake)
    header, blob = ocr_handlers._handle_ocr_paddle_onnx(ctx, {}, IMG, _no_cancel())
    assert fake.last_image == IMG
    assert fake.last_kwargs == {
        "join_newlines": True,
        "reflect_strings": False,
        "lang": "korean_v5",
        "device": "cpu",
    }
    assert header == {
        "engine": "paddleocr_onnx",
        "model": "korean_v5",
        "device": "cpu",
        "lines": ["o"],
        "text": "o",
    }
    assert blob == b""


def test_paddle_onnx_parses_model_and_device() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(paddle_ocr=fake)
    req = {"paddle_onnx_model": " Chinese_V5 ", "paddle_onnx_device": " CUDA "}
    header, _ = ocr_handlers._handle_ocr_paddle_onnx(ctx, req, IMG, _no_cancel())
    assert fake.last_kwargs["lang"] == "chinese_v5"
    assert fake.last_kwargs["device"] == "cuda"
    assert header["model"] == "chinese_v5"
    assert header["device"] == "cuda"


def test_paddle_onnx_blank_model_and_device_fall_back() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(paddle_ocr=fake)
    header, _ = ocr_handlers._handle_ocr_paddle_onnx(
        ctx, {"paddle_onnx_model": "", "paddle_onnx_device": None}, IMG, _no_cancel()
    )
    assert header["model"] == "korean_v5"
    assert header["device"] == "cpu"


def test_paddle_onnx_non_string_model_errors() -> None:
    ctx = _make_ctx(paddle_ocr=_FakeOcr({"lines": [], "text": ""}))
    with pytest.raises(ValueError, match="paddle_onnx_model"):
        ocr_handlers._handle_ocr_paddle_onnx(ctx, {"paddle_onnx_model": 9}, IMG, _no_cancel())


def test_paddle_onnx_non_string_device_errors() -> None:
    ctx = _make_ctx(paddle_ocr=_FakeOcr({"lines": [], "text": ""}))
    with pytest.raises(ValueError, match="paddle_onnx_device"):
        ocr_handlers._handle_ocr_paddle_onnx(ctx, {"paddle_onnx_device": 9}, IMG, _no_cancel())


def test_paddle_onnx_empty_blob_errors() -> None:
    ctx = _make_ctx(paddle_ocr=_FakeOcr({"lines": [], "text": ""}))
    with pytest.raises(ValueError, match="frame blob"):
        ocr_handlers._handle_ocr_paddle_onnx(ctx, {}, b"", _no_cancel())


def test_paddle_onnx_canceled() -> None:
    fake = _FakeOcr({"lines": [], "text": ""})
    ctx = _make_ctx(paddle_ocr=fake)
    with pytest.raises(Interrupted):
        ocr_handlers._handle_ocr_paddle_onnx(ctx, {}, IMG, _canceled())
    assert fake.calls == []
