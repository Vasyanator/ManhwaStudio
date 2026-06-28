"""
File: modules/ai_backend/ipc/handlers/textdetector.py

Methods hosted here:
    textdetector.ctd    — CTD text detector (METHOD_TEXTDETECTOR_CTD)
    textdetector.paddle — Paddle text detector (METHOD_TEXTDETECTOR_PADDLE)
    textdetector.surya  — Surya text detector (METHOD_TEXTDETECTOR_SURYA)

All text-detection methods accept either an on-disk ``page_path`` header
field (alias ``path``) OR inline image bytes in the request blob (exactly one
must be supplied).  They all return a mask PNG in the response blob; the
service produces it as raw bytes in the ``mask_png`` result key.

Handler signature::

    (ctx, header, blob, cancel_event) -> (resp_header_fields, resp_blob)

Registration pattern::

    from ..registry import register
    from ..protocol import METHOD_TEXTDETECTOR_CTD

    def _handle_textdetector_ctd(ctx, header, blob, cancel_event):
        ...
        return {"engine": "ctd", "source_size": [w, h], "blocks": [...]}, mask_png

    register(METHOD_TEXTDETECTOR_CTD, _handle_textdetector_ctd)
"""

from __future__ import annotations

import threading
import traceback
from typing import Any

from ..protocol import (
    METHOD_TEXTDETECTOR_CTD,
    METHOD_TEXTDETECTOR_PADDLE,
    METHOD_TEXTDETECTOR_SURYA,
)
from ..registry import HandlerContext, register


def _resolve_path(header: dict[str, Any]) -> str | None:
    """Return the page_path from the header (or its alias ``path``), stripped.

    Returns ``None`` when neither field is present or both are blank/non-string.
    """
    page_path = header.get("page_path")
    if not isinstance(page_path, str) or not page_path.strip():
        page_path = header.get("path")
    if isinstance(page_path, str) and page_path.strip():
        return page_path.strip()
    return None


def _mask_png_bytes(result: dict[str, Any]) -> bytes:
    """Return the service's raw ``mask_png`` result for the response blob.

    Returns ``b""`` when the field is absent or empty.
    """
    return result.get("mask_png", b"") or b""


# ---------------------------------------------------------------------------
# textdetector.ctd
# ---------------------------------------------------------------------------

def _handle_textdetector_ctd(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`textdetector.ctd`: detect text blocks with the CTD model.

    Request fields (inline in header):
        page_path / path : string | null — on-disk page path (exclusive with blob)
        params           : object = {}   — CTD-specific detection parameters

    blob(req): input image PNG bytes when no page_path is given.

    Response fields (inline in header):
        engine      : "ctd"
        source_size : [w, h]
        blocks      : object[]

    blob(resp): mask PNG (raw bytes, NOT base64).
    """
    params_raw = header.get("params", {})
    if params_raw is None:
        params_raw = {}
    if not isinstance(params_raw, dict):
        raise ValueError("Field 'params' must be an object.")

    page_path = _resolve_path(header)

    try:
        if page_path is not None:
            result = ctx.state.text_detector_ctd.detect_page(page_path, params=params_raw)
        elif blob:
            result = ctx.state.text_detector_ctd.detect_image_bytes(blob, params=params_raw)
        else:
            raise ValueError(
                "Either 'page_path'/'path' must be set in the header, or the"
                " request blob must contain the image bytes."
            )
    except FileNotFoundError as exc:
        raise FileNotFoundError(str(exc)) from exc
    except ValueError:
        raise
    except Exception as exc:
        traceback.print_exc()
        raise RuntimeError(str(exc)) from exc

    mask_png = _mask_png_bytes(result)

    resp = {
        "engine": "ctd",
        "source_size": result.get("source_size", [0, 0]),
        "blocks": result.get("blocks", []),
    }
    return resp, mask_png


register(METHOD_TEXTDETECTOR_CTD, _handle_textdetector_ctd)


# ---------------------------------------------------------------------------
# textdetector.paddle
# ---------------------------------------------------------------------------

def _handle_textdetector_paddle(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`textdetector.paddle`: detect text blocks with the PaddleOCR-based detector.

    Request fields (inline in header):
        page_path / path : string | null — on-disk page path (exclusive with blob)

    blob(req): input image PNG bytes when no page_path is given.

    Response fields (inline in header):
        engine      : "paddle"
        source_size : [w, h]
        blocks      : object[]
        polys       : array[]

    blob(resp): mask PNG (raw bytes, NOT base64).
    """
    page_path = _resolve_path(header)

    try:
        if page_path is not None:
            result = ctx.state.text_detector_paddle.detect_page(page_path)
        elif blob:
            result = ctx.state.text_detector_paddle.detect_image_bytes(blob)
        else:
            raise ValueError(
                "Either 'page_path'/'path' must be set in the header, or the"
                " request blob must contain the image bytes."
            )
    except FileNotFoundError as exc:
        raise FileNotFoundError(str(exc)) from exc
    except ValueError:
        raise
    except Exception as exc:
        traceback.print_exc()
        raise RuntimeError(str(exc)) from exc

    mask_png = _mask_png_bytes(result)

    resp = {
        "engine": "paddle",
        "source_size": result.get("source_size", [0, 0]),
        "blocks": result.get("blocks", []),
        "polys": result.get("polys", []),
    }
    return resp, mask_png


register(METHOD_TEXTDETECTOR_PADDLE, _handle_textdetector_paddle)


# ---------------------------------------------------------------------------
# textdetector.surya
# ---------------------------------------------------------------------------

def _handle_textdetector_surya(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`textdetector.surya`: detect text blocks with the Surya detector.

    Request fields (inline in header):
        page_path / path : string | null — on-disk page path (exclusive with blob)

    blob(req): input image PNG bytes when no page_path is given.

    Response fields (inline in header):
        engine      : "surya"
        source_size : [w, h]
        blocks      : object[]
        lines       : array[]

    blob(resp): mask PNG (raw bytes, NOT base64).
    """
    page_path = _resolve_path(header)

    try:
        if page_path is not None:
            result = ctx.state.text_detector_surya.detect_page(page_path)
        elif blob:
            result = ctx.state.text_detector_surya.detect_image_bytes(blob)
        else:
            raise ValueError(
                "Either 'page_path'/'path' must be set in the header, or the"
                " request blob must contain the image bytes."
            )
    except FileNotFoundError as exc:
        raise FileNotFoundError(str(exc)) from exc
    except ValueError:
        raise
    except Exception as exc:
        traceback.print_exc()
        raise RuntimeError(str(exc)) from exc

    mask_png = _mask_png_bytes(result)

    resp = {
        "engine": "surya",
        "source_size": result.get("source_size", [0, 0]),
        "blocks": result.get("blocks", []),
        "lines": result.get("lines", []),
    }
    return resp, mask_png


register(METHOD_TEXTDETECTOR_SURYA, _handle_textdetector_surya)
