"""
File: modules/ai_backend/ipc/handlers/test_textdetector.py

Unit tests for the forward-only text-detector handlers:
    textdetector.ctd.forward    (METHOD_TEXTDETECTOR_CTD_FORWARD)
    textdetector.paddle.forward (METHOD_TEXTDETECTOR_PADDLE_FORWARD)
    textdetector.surya.forward  (METHOD_TEXTDETECTOR_SURYA_FORWARD)

Strategy
--------
The services (``text_detector_ctd`` / ``_paddle`` / ``_surya``) are
``unittest.mock.MagicMock`` stand-ins, so no model and no Torch is loaded. The
handlers are driven directly (no socket), verifying:

1. the request blob reaches ``forward_tiles`` as ``uint8 [n, h, w, 3]`` with
   the tile-major / row-major layout preserved;
2. the response header (`engine`, `n`, `map_width`, `map_height`, `channels`)
   and the blob layout (tile-major, then channel-major) per engine;
3. request validation: `n`, size, alignment, exact blob length, request and
   response size limits — each a ``ValueError`` before the service runs;
4. service-output validation: shape, dtype, response size — ``RuntimeError``.
"""

from __future__ import annotations

import threading
from unittest.mock import MagicMock

import numpy as np
import pytest

from modules.ai_backend.ipc.handlers import textdetector as td
from modules.ai_backend.ipc.handlers.textdetector import (
    FORWARD_SPECS,
    _handle_textdetector_ctd_forward,
    _handle_textdetector_paddle_forward,
    _handle_textdetector_surya_forward,
    encode_forward_response,
    parse_forward_request,
)
from modules.ai_backend.ipc.protocol import (
    ALL_METHODS,
    MAX_BLOB_BYTES,
    METHOD_TEXTDETECTOR_CTD_FORWARD,
    METHOD_TEXTDETECTOR_PADDLE_FORWARD,
    METHOD_TEXTDETECTOR_SURYA_FORWARD,
)
from modules.ai_backend.ipc.registry import METHOD_HANDLERS, HandlerContext

_NO_CANCEL = threading.Event()

# (handler, AppState field, engine) for the parametrized round-trip tests.
_ENGINES = [
    (_handle_textdetector_ctd_forward, "text_detector_ctd", "ctd"),
    (_handle_textdetector_paddle_forward, "text_detector_paddle", "paddle"),
    (_handle_textdetector_surya_forward, "text_detector_surya", "surya"),
]


def _ctx(state: MagicMock) -> HandlerContext:
    return HandlerContext(
        state=state,
        events=MagicMock(),
        get_health_snapshot=lambda: {"ok": True},
    )


def _tiles(n: int, width: int, height: int) -> np.ndarray:
    """Deterministic tiles whose every byte encodes its position."""
    return (np.arange(n * height * width * 3, dtype=np.uint64) % 251).astype(np.uint8).reshape(n, height, width, 3)


def _fake_maps(spec, tiles: np.ndarray) -> np.ndarray:
    """Maps whose value encodes (tile, channel) so the layout is checkable."""
    n, height, width = tiles.shape[:3]
    channels = len(spec.channels)
    maps = np.empty((n, channels, height // spec.map_stride, width // spec.map_stride), dtype=np.uint8)
    for t in range(n):
        for c in range(channels):
            maps[t, c] = 10 * t + c
    return maps


def test_methods_are_registered_and_listed() -> None:
    for method in (
        METHOD_TEXTDETECTOR_CTD_FORWARD,
        METHOD_TEXTDETECTOR_PADDLE_FORWARD,
        METHOD_TEXTDETECTOR_SURYA_FORWARD,
    ):
        assert method in METHOD_HANDLERS
        assert method in ALL_METHODS
    for removed in ("textdetector.ctd", "textdetector.paddle", "textdetector.surya"):
        assert removed not in METHOD_HANDLERS
        assert removed not in ALL_METHODS


def test_forward_specs_match_the_protocol_table() -> None:
    """PROTOCOL.md §5.3 and `ms_backend_ipc::textdetector` carry the same table."""
    assert FORWARD_SPECS["ctd"].align == 64
    assert FORWARD_SPECS["ctd"].map_stride == 1
    assert FORWARD_SPECS["ctd"].channels == ("seg", "shrink")
    assert FORWARD_SPECS["paddle"].align == 32
    assert FORWARD_SPECS["paddle"].map_stride == 1
    assert FORWARD_SPECS["paddle"].channels == ("prob",)
    assert FORWARD_SPECS["surya"].align == 4
    assert FORWARD_SPECS["surya"].map_stride == 4
    assert FORWARD_SPECS["surya"].channels == ("text",)


@pytest.mark.parametrize("handler, field, engine", _ENGINES)
def test_round_trip_layout(handler, field: str, engine: str) -> None:
    spec = FORWARD_SPECS[engine]
    n, width, height = 2, 2 * 64, 64
    tiles = _tiles(n, width, height)
    state = MagicMock()
    service = getattr(state, field)
    service.forward_tiles.side_effect = lambda arr: _fake_maps(spec, arr)

    header, blob = handler(_ctx(state), {"n": n, "width": width, "height": height}, tiles.tobytes(), _NO_CANCEL)

    service.forward_tiles.assert_called_once()
    received = service.forward_tiles.call_args.args[0]
    assert received.dtype == np.uint8
    assert received.shape == (n, height, width, 3)
    assert np.array_equal(received, tiles)

    map_w, map_h = width // spec.map_stride, height // spec.map_stride
    assert header == {
        "engine": engine,
        "n": n,
        "map_width": map_w,
        "map_height": map_h,
        "channels": list(spec.channels),
    }
    channels = len(spec.channels)
    assert len(blob) == n * channels * map_w * map_h
    plane = map_w * map_h
    for t in range(n):
        for c in range(channels):
            start = (t * channels + c) * plane
            assert set(blob[start : start + plane]) == {10 * t + c}


@pytest.mark.parametrize(
    "header, blob_len, match",
    [
        ({"n": 0, "width": 64, "height": 64}, 0, "at least 1"),
        ({"n": 1, "width": 96, "height": 64}, 96 * 64 * 3, "multiple of 64"),
        ({"n": 1, "width": 64, "height": 0}, 0, "multiple of 64"),
        ({"n": 1, "width": 64, "height": 64}, 64 * 64 * 3 - 1, "expected"),
        ({"n": 2, "width": 64, "height": 64}, 64 * 64 * 3, "expected"),
        ({"n": "1", "width": 64, "height": 64}, 64 * 64 * 3, "integer"),
        ({"n": True, "width": 64, "height": 64}, 64 * 64 * 3, "integer"),
        ({"width": 64, "height": 64}, 64 * 64 * 3, "integer"),
        ({"n": 1, "width": 64.0, "height": 64}, 64 * 64 * 3, "integer"),
    ],
)
def test_invalid_requests_never_reach_the_service(header: dict, blob_len: int, match: str) -> None:
    state = MagicMock()
    with pytest.raises(ValueError, match=match):
        _handle_textdetector_ctd_forward(_ctx(state), header, bytes(blob_len), _NO_CANCEL)
    state.text_detector_ctd.forward_tiles.assert_not_called()


def test_paddle_and_surya_alignment() -> None:
    state = MagicMock()
    with pytest.raises(ValueError, match="multiple of 32"):
        _handle_textdetector_paddle_forward(_ctx(state), {"n": 1, "width": 48, "height": 32}, bytes(48 * 32 * 3), _NO_CANCEL)
    with pytest.raises(ValueError, match="multiple of 4"):
        _handle_textdetector_surya_forward(_ctx(state), {"n": 1, "width": 6, "height": 4}, bytes(6 * 4 * 3), _NO_CANCEL)
    state.text_detector_paddle.forward_tiles.assert_not_called()
    state.text_detector_surya.forward_tiles.assert_not_called()


def test_request_over_blob_limit_is_rejected() -> None:
    side = 2048
    n = MAX_BLOB_BYTES // (side * side * 3) + 1
    with pytest.raises(ValueError, match="MAX_BLOB_BYTES"):
        parse_forward_request(FORWARD_SPECS["ctd"], {"n": n, "width": side, "height": side}, b"")


def test_response_over_blob_limit_is_rejected_before_the_service(monkeypatch: pytest.MonkeyPatch) -> None:
    """CTD returns 2 maps of 1 byte per 3 input bytes: shrink the limit to hit only the response."""
    spec = FORWARD_SPECS["ctd"]
    n, side = 1, 64
    request_bytes = n * side * side * 3
    monkeypatch.setattr(td, "MAX_BLOB_BYTES", request_bytes)
    # Request fits exactly; the CTD response (2 channels) is 2/3 of it and fits too.
    parse_forward_request(spec, {"n": n, "width": side, "height": side}, bytes(request_bytes))
    # A 4-channel engine at the same size would need 4/3 of the request: rejected.
    wide = td.ForwardSpec(engine="wide", align=64, map_stride=1, channels=("a", "b", "c", "d"))
    with pytest.raises(ValueError, match="response"):
        parse_forward_request(wide, {"n": n, "width": side, "height": side}, bytes(request_bytes))


def test_service_output_shape_and_dtype_are_checked() -> None:
    spec = FORWARD_SPECS["surya"]
    with pytest.raises(RuntimeError, match="shape"):
        encode_forward_response(spec, 1, 64, 64, np.zeros((1, 1, 64, 64), dtype=np.uint8))
    with pytest.raises(RuntimeError, match="shape"):
        encode_forward_response(spec, 2, 64, 64, np.zeros((1, 1, 16, 16), dtype=np.uint8))
    with pytest.raises(RuntimeError, match="uint8"):
        encode_forward_response(spec, 1, 64, 64, np.zeros((1, 1, 16, 16), dtype=np.float32))
    with pytest.raises(RuntimeError, match="shape"):
        encode_forward_response(spec, 1, 64, 64, b"\x00" * 256)


def test_service_errors_propagate() -> None:
    state = MagicMock()
    state.text_detector_surya.forward_tiles.side_effect = RuntimeError("non-finite heatmap")
    with pytest.raises(RuntimeError, match="non-finite"):
        _handle_textdetector_surya_forward(_ctx(state), {"n": 1, "width": 8, "height": 8}, bytes(8 * 8 * 3), _NO_CANCEL)
