"""
File: modules/ai_backend/test_handlers_textdetector.py

Unit tests for the v2 IPC text-detector handlers:
    textdetector.ctd    (METHOD_TEXTDETECTOR_CTD)
    textdetector.paddle (METHOD_TEXTDETECTOR_PADDLE)
    textdetector.surya  (METHOD_TEXTDETECTOR_SURYA)

Strategy
--------
The service objects (``text_detector_ctd``, ``text_detector_paddle``,
``text_detector_surya``) are replaced with ``unittest.mock.MagicMock``
instances so no Torch models are loaded.  Tests drive the handlers directly
(no socket/dispatcher), verifying:

1.  ``page_path`` branch  -> ``detect_page`` called; no ``detect_image_bytes``.
2.  ``path`` alias branch -> same as above.
3.  Blob branch           -> ``detect_image_bytes`` called; no ``detect_page``.
4.  Neither source        -> ``ValueError`` raised.
5.  Response header fields match the HTTP shape per engine.
6.  Mask PNG arrives as raw bytes in the response blob (byte-identical to the
    service's ``mask_png`` result).
7.  ``params`` is forwarded to CTD; paddle/surya take no params.
8.  ``FileNotFoundError`` propagates (so the dispatcher turns it into
    ``status:"error"`` with the message).
"""

from __future__ import annotations

import threading
from unittest.mock import MagicMock

import pytest

from modules.ai_backend.ipc.handlers.textdetector import (
    _handle_textdetector_ctd,
    _handle_textdetector_paddle,
    _handle_textdetector_surya,
)
from modules.ai_backend.ipc.protocol import (
    METHOD_TEXTDETECTOR_CTD,
    METHOD_TEXTDETECTOR_PADDLE,
    METHOD_TEXTDETECTOR_SURYA,
)
from modules.ai_backend.ipc.registry import METHOD_HANDLERS, HandlerContext

# ---------------------------------------------------------------------------
# Helpers / fixtures
# ---------------------------------------------------------------------------

# A tiny 1x1 white PNG for round-trip testing.  Produced once so every test
# uses the same stable bytes.
_MASK_PNG_BYTES = (
    b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01"
    b"\x08\x02\x00\x00\x00\x90wS\xde\x00\x00\x00\x0cIDATx\x9cc\xf8\x0f\x00"
    b"\x00\x01\x01\x00\x05\x18\xd8N\x00\x00\x00\x00IEND\xaeB`\x82"
)
_FAKE_IMAGE_BLOB = b"FAKE_PNG_BYTES"

# A cancel event that is never set (handlers are synchronous here).
_NO_CANCEL = threading.Event()


def _make_service_result(extra: dict | None = None) -> dict:
    """Return a dict that looks like what a *TextDetectorService returns."""
    base = {
        "source_size": [800, 1200],
        "blocks": [{"x": 10, "y": 20, "w": 100, "h": 50}],
        "mask_png": _MASK_PNG_BYTES,
    }
    if extra:
        base.update(extra)
    return base


def _ctx(state: MagicMock) -> HandlerContext:
    return HandlerContext(
        state=state,
        events=MagicMock(),
        get_health_snapshot=lambda: {"ok": True},
    )


# ---------------------------------------------------------------------------
# Registration smoke test
# ---------------------------------------------------------------------------

def test_methods_are_registered() -> None:
    """All three text-detector methods must be present in the handler registry."""
    assert METHOD_TEXTDETECTOR_CTD in METHOD_HANDLERS
    assert METHOD_TEXTDETECTOR_PADDLE in METHOD_HANDLERS
    assert METHOD_TEXTDETECTOR_SURYA in METHOD_HANDLERS


# ===========================================================================
# textdetector.ctd
# ===========================================================================

class TestCtd:
    def _state(self, result: dict | None = None) -> MagicMock:
        state = MagicMock()
        svc = state.text_detector_ctd
        svc.detect_page.return_value = result or _make_service_result()
        svc.detect_image_bytes.return_value = result or _make_service_result()
        return state

    # --- page_path branch ---

    def test_page_path_calls_detect_page(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        header = {"page_path": "/some/page.png"}
        resp_h, resp_b = _handle_textdetector_ctd(ctx, header, b"", _NO_CANCEL)

        state.text_detector_ctd.detect_page.assert_called_once_with(
            "/some/page.png", params={}
        )
        state.text_detector_ctd.detect_image_bytes.assert_not_called()

    def test_page_path_response_fields(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        resp_h, resp_b = _handle_textdetector_ctd(
            ctx, {"page_path": "/p.png"}, b"", _NO_CANCEL
        )
        assert resp_h["engine"] == "ctd"
        assert resp_h["source_size"] == [800, 1200]
        assert isinstance(resp_h["blocks"], list)
        # the mask bytes must NOT be in the header (only in the blob)
        assert "mask_png" not in resp_h
        assert "mask_png_base64" not in resp_h

    def test_page_path_mask_in_blob(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        _, resp_b = _handle_textdetector_ctd(
            ctx, {"page_path": "/p.png"}, b"", _NO_CANCEL
        )
        # Blob must be raw PNG bytes, round-trip-decodable
        assert resp_b == _MASK_PNG_BYTES

    # --- path alias ---

    def test_path_alias_calls_detect_page(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        _handle_textdetector_ctd(ctx, {"path": "/alias.png"}, b"", _NO_CANCEL)
        state.text_detector_ctd.detect_page.assert_called_once_with(
            "/alias.png", params={}
        )

    # --- blob branch ---

    def test_blob_calls_detect_image_bytes(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        _handle_textdetector_ctd(ctx, {}, _FAKE_IMAGE_BLOB, _NO_CANCEL)

        state.text_detector_ctd.detect_image_bytes.assert_called_once_with(
            _FAKE_IMAGE_BLOB, params={}
        )
        state.text_detector_ctd.detect_page.assert_not_called()

    def test_blob_response_fields(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        resp_h, resp_b = _handle_textdetector_ctd(
            ctx, {}, _FAKE_IMAGE_BLOB, _NO_CANCEL
        )
        assert resp_h["engine"] == "ctd"
        assert resp_h["source_size"] == [800, 1200]
        assert "mask_png" not in resp_h
        assert "mask_png_base64" not in resp_h
        assert resp_b == _MASK_PNG_BYTES

    # --- params forwarded ---

    def test_params_forwarded_for_page_path(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        params = {"threshold": 0.5, "max_width": 100}
        _handle_textdetector_ctd(
            ctx, {"page_path": "/p.png", "params": params}, b"", _NO_CANCEL
        )
        state.text_detector_ctd.detect_page.assert_called_once_with(
            "/p.png", params=params
        )

    def test_params_forwarded_for_blob(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        params = {"min_area": 50}
        _handle_textdetector_ctd(
            ctx, {"params": params}, _FAKE_IMAGE_BLOB, _NO_CANCEL
        )
        state.text_detector_ctd.detect_image_bytes.assert_called_once_with(
            _FAKE_IMAGE_BLOB, params=params
        )

    def test_params_none_treated_as_empty_dict(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        _handle_textdetector_ctd(
            ctx, {"page_path": "/p.png", "params": None}, b"", _NO_CANCEL
        )
        state.text_detector_ctd.detect_page.assert_called_once_with(
            "/p.png", params={}
        )

    def test_params_invalid_type_raises(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        with pytest.raises(ValueError, match="params"):
            _handle_textdetector_ctd(
                ctx, {"page_path": "/p.png", "params": "not-a-dict"}, b"", _NO_CANCEL
            )

    # --- neither source ---

    def test_neither_source_raises_value_error(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        with pytest.raises(ValueError, match="page_path"):
            _handle_textdetector_ctd(ctx, {}, b"", _NO_CANCEL)

    # --- FileNotFoundError propagates ---

    def test_file_not_found_propagates(self) -> None:
        state = MagicMock()
        state.text_detector_ctd.detect_page.side_effect = FileNotFoundError(
            "No such file: /missing.png"
        )
        ctx = _ctx(state)
        with pytest.raises(FileNotFoundError, match="missing.png"):
            _handle_textdetector_ctd(
                ctx, {"page_path": "/missing.png"}, b"", _NO_CANCEL
            )


# ===========================================================================
# textdetector.paddle
# ===========================================================================

class TestPaddle:
    def _state(self, result: dict | None = None) -> MagicMock:
        r = result or _make_service_result({"polys": [[1, 2], [3, 4]]})
        state = MagicMock()
        state.text_detector_paddle.detect_page.return_value = r
        state.text_detector_paddle.detect_image_bytes.return_value = r
        return state

    # --- page_path branch ---

    def test_page_path_calls_detect_page(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        _handle_textdetector_paddle(
            ctx, {"page_path": "/page.png"}, b"", _NO_CANCEL
        )
        state.text_detector_paddle.detect_page.assert_called_once_with("/page.png")
        state.text_detector_paddle.detect_image_bytes.assert_not_called()

    def test_page_path_response_fields(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        resp_h, resp_b = _handle_textdetector_paddle(
            ctx, {"page_path": "/page.png"}, b"", _NO_CANCEL
        )
        assert resp_h["engine"] == "paddle"
        assert resp_h["source_size"] == [800, 1200]
        assert isinstance(resp_h["blocks"], list)
        assert "polys" in resp_h
        assert "mask_png" not in resp_h
        assert "mask_png_base64" not in resp_h

    def test_page_path_mask_in_blob(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        _, resp_b = _handle_textdetector_paddle(
            ctx, {"page_path": "/page.png"}, b"", _NO_CANCEL
        )
        assert resp_b == _MASK_PNG_BYTES

    # --- path alias ---

    def test_path_alias(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        _handle_textdetector_paddle(ctx, {"path": "/alias.png"}, b"", _NO_CANCEL)
        state.text_detector_paddle.detect_page.assert_called_once_with("/alias.png")

    # --- blob branch ---

    def test_blob_calls_detect_image_bytes(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        _handle_textdetector_paddle(ctx, {}, _FAKE_IMAGE_BLOB, _NO_CANCEL)
        state.text_detector_paddle.detect_image_bytes.assert_called_once_with(
            _FAKE_IMAGE_BLOB
        )
        state.text_detector_paddle.detect_page.assert_not_called()

    def test_blob_response_fields(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        resp_h, resp_b = _handle_textdetector_paddle(
            ctx, {}, _FAKE_IMAGE_BLOB, _NO_CANCEL
        )
        assert resp_h["engine"] == "paddle"
        assert "polys" in resp_h
        assert "mask_png" not in resp_h
        assert "mask_png_base64" not in resp_h
        assert resp_b == _MASK_PNG_BYTES

    # --- polys present ---

    def test_polys_in_response(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        resp_h, _ = _handle_textdetector_paddle(
            ctx, {"page_path": "/p.png"}, b"", _NO_CANCEL
        )
        assert resp_h["polys"] == [[1, 2], [3, 4]]

    # --- neither source ---

    def test_neither_source_raises(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        with pytest.raises(ValueError, match="page_path"):
            _handle_textdetector_paddle(ctx, {}, b"", _NO_CANCEL)

    # --- FileNotFoundError propagates ---

    def test_file_not_found_propagates(self) -> None:
        state = MagicMock()
        state.text_detector_paddle.detect_page.side_effect = FileNotFoundError("gone")
        ctx = _ctx(state)
        with pytest.raises(FileNotFoundError, match="gone"):
            _handle_textdetector_paddle(
                ctx, {"page_path": "/gone.png"}, b"", _NO_CANCEL
            )


# ===========================================================================
# textdetector.surya
# ===========================================================================

class TestSurya:
    def _state(self, result: dict | None = None) -> MagicMock:
        r = result or _make_service_result(
            {"lines": [{"text": "hello"}, {"text": "world"}]}
        )
        state = MagicMock()
        state.text_detector_surya.detect_page.return_value = r
        state.text_detector_surya.detect_image_bytes.return_value = r
        return state

    # --- page_path branch ---

    def test_page_path_calls_detect_page(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        _handle_textdetector_surya(
            ctx, {"page_path": "/page.png"}, b"", _NO_CANCEL
        )
        state.text_detector_surya.detect_page.assert_called_once_with("/page.png")
        state.text_detector_surya.detect_image_bytes.assert_not_called()

    def test_page_path_response_fields(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        resp_h, resp_b = _handle_textdetector_surya(
            ctx, {"page_path": "/page.png"}, b"", _NO_CANCEL
        )
        assert resp_h["engine"] == "surya"
        assert resp_h["source_size"] == [800, 1200]
        assert isinstance(resp_h["blocks"], list)
        assert "lines" in resp_h
        assert "mask_png" not in resp_h
        assert "mask_png_base64" not in resp_h

    def test_page_path_mask_in_blob(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        _, resp_b = _handle_textdetector_surya(
            ctx, {"page_path": "/page.png"}, b"", _NO_CANCEL
        )
        assert resp_b == _MASK_PNG_BYTES

    # --- path alias ---

    def test_path_alias(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        _handle_textdetector_surya(ctx, {"path": "/alias.png"}, b"", _NO_CANCEL)
        state.text_detector_surya.detect_page.assert_called_once_with("/alias.png")

    # --- blob branch ---

    def test_blob_calls_detect_image_bytes(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        _handle_textdetector_surya(ctx, {}, _FAKE_IMAGE_BLOB, _NO_CANCEL)
        state.text_detector_surya.detect_image_bytes.assert_called_once_with(
            _FAKE_IMAGE_BLOB
        )
        state.text_detector_surya.detect_page.assert_not_called()

    def test_blob_response_fields(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        resp_h, resp_b = _handle_textdetector_surya(
            ctx, {}, _FAKE_IMAGE_BLOB, _NO_CANCEL
        )
        assert resp_h["engine"] == "surya"
        assert "lines" in resp_h
        assert "mask_png" not in resp_h
        assert "mask_png_base64" not in resp_h
        assert resp_b == _MASK_PNG_BYTES

    # --- lines present ---

    def test_lines_in_response(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        resp_h, _ = _handle_textdetector_surya(
            ctx, {"page_path": "/p.png"}, b"", _NO_CANCEL
        )
        assert resp_h["lines"] == [{"text": "hello"}, {"text": "world"}]

    # --- neither source ---

    def test_neither_source_raises(self) -> None:
        state = self._state()
        ctx = _ctx(state)
        with pytest.raises(ValueError, match="page_path"):
            _handle_textdetector_surya(ctx, {}, b"", _NO_CANCEL)

    # --- FileNotFoundError propagates ---

    def test_file_not_found_propagates(self) -> None:
        state = MagicMock()
        state.text_detector_surya.detect_page.side_effect = FileNotFoundError("gone")
        ctx = _ctx(state)
        with pytest.raises(FileNotFoundError, match="gone"):
            _handle_textdetector_surya(
                ctx, {"page_path": "/gone.png"}, b"", _NO_CANCEL
            )


# ===========================================================================
# Cross-engine: mask PNG round-trip decodable
# ===========================================================================

class TestMaskRoundTrip:
    """Verify that the response blob is the raw PNG from the service's
    ``mask_png`` field — for all three engines."""

    def _state_ctd(self) -> MagicMock:
        state = MagicMock()
        state.text_detector_ctd.detect_page.return_value = _make_service_result()
        return state

    def _state_paddle(self) -> MagicMock:
        state = MagicMock()
        state.text_detector_paddle.detect_page.return_value = _make_service_result(
            {"polys": []}
        )
        return state

    def _state_surya(self) -> MagicMock:
        state = MagicMock()
        state.text_detector_surya.detect_page.return_value = _make_service_result(
            {"lines": []}
        )
        return state

    def test_ctd_mask_round_trip(self) -> None:
        _, blob = _handle_textdetector_ctd(
            _ctx(self._state_ctd()), {"page_path": "/p.png"}, b"", _NO_CANCEL
        )
        assert blob == _MASK_PNG_BYTES

    def test_paddle_mask_round_trip(self) -> None:
        _, blob = _handle_textdetector_paddle(
            _ctx(self._state_paddle()), {"page_path": "/p.png"}, b"", _NO_CANCEL
        )
        assert blob == _MASK_PNG_BYTES

    def test_surya_mask_round_trip(self) -> None:
        _, blob = _handle_textdetector_surya(
            _ctx(self._state_surya()), {"page_path": "/p.png"}, b"", _NO_CANCEL
        )
        assert blob == _MASK_PNG_BYTES
