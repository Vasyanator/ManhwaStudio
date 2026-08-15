"""
File: modules/ai_backend/ipc/handlers/watermark.py

Methods hosted here:
    watermark.detect  — predict the visible-watermark mask of one image (streaming).
    watermark.remove  — experimental direct network pass: cleaned image + mask (streaming).
    watermark.status  — model catalog + on-disk weights/code state.
    watermark.unload  — drop the resident network.

Service: ``ctx.state.watermark`` (``watermark/service.py``,
``WatermarkRemovalService``). The handler layer only reshapes frames; every
decision about models, tiling, downscaling and downloads belongs to the service.

Blob convention:
    request blob = image_png (single image, raw bytes, never base64)
    watermark.detect  response blob = mask_png (L8)
    watermark.remove  response blob = clean_png ++ mask_png, split by the
                      ``image_len`` / ``mask_len`` response header ints — the same
                      length-prefixed appendix convention the inpaint REQUESTS use
                      (``ipc/PROTOCOL.md`` §5.4), applied to a response here.

Streaming: both long methods push ``progress{id}`` frames through the
dispatcher's ``ProgressEmitter`` (``HandlerContext.progress_emitter``) using the
FLUX two-phase contract — ``phase:"download"`` (``step``/``total`` in BYTES,
``label`` naming the file) and ``phase:"generate"`` (tiles/steps done / total).
No preview blob. The emitter is read defensively: when the dispatcher attached
none (non-streaming call path, tests), progress emission is a no-op.
"""

from __future__ import annotations

import threading
import traceback
from typing import Any, Callable

from ..protocol import (
    METHOD_WATERMARK_DETECT,
    METHOD_WATERMARK_REMOVE,
    METHOD_WATERMARK_STATUS,
    METHOD_WATERMARK_UNLOAD,
)
from ..registry import HandlerContext, Interrupted, register

_PROGRESS_EMITTER_ATTR = "progress_emitter"


def _read_params(header: dict[str, Any]) -> dict[str, Any]:
    """Read the optional ``params`` object header field (defaults to ``{}``).

    ``None`` is accepted as "absent"; any other non-object value is a request
    error (``ValueError`` -> ``response{status:"error"}``).
    """
    params_raw = header.get("params", {})
    if params_raw is None:
        params_raw = {}
    if not isinstance(params_raw, dict):
        raise ValueError("Field 'params' must be an object.")
    return params_raw


def _read_image_blob(blob: bytes, method: str) -> bytes:
    """Return the non-empty request blob (the input image PNG) for ``method``.

    Raises ValueError when the blob is empty: unlike the text detectors there is
    no on-disk `page_path` alternative for the watermark methods, so an empty
    blob can only be a client bug.
    """
    if not blob:
        raise ValueError(f"{method} requires the input image in the frame blob.")
    return blob


def _progress_callback(ctx: HandlerContext) -> Callable[[str, int, int, str], None]:
    """Build the service-facing progress callback for one request.

    The returned callable forwards ``(phase, step, total, label)`` as a
    ``progress{id}`` frame with an empty blob. When no ``progress_emitter`` is
    attached to the context it degrades to a no-op, so the service never needs to
    know whether the caller streams. Emission failures are swallowed: a gone peer
    must not abort work that is already running.
    """
    emitter = getattr(ctx, _PROGRESS_EMITTER_ATTR, None)

    def on_progress(phase: str, step: int, total: int, label: str) -> None:
        if emitter is None:
            return
        try:
            emitter.emit(
                {
                    "phase": str(phase),
                    "step": int(step),
                    "total": int(total),
                    "label": str(label),
                },
                b"",
            )
        except Exception:  # noqa: BLE001 - peer gone; keep working
            pass

    return on_progress


def _result_bytes(result: dict[str, Any], key: str) -> bytes:
    """Return the service's raw PNG bytes under ``key``; ``b""`` when absent."""
    return result.get(key, b"") or b""


def _handle_watermark_detect(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`watermark.detect`: predict the watermark mask of the request-blob image.

    Request: blob = image PNG, header ``params`` (model, downscale_to, threshold,
    dilate_px) inline. Response: blob = L8 mask PNG at the input resolution,
    header = ``model`` / ``device`` / ``source_size`` / ``mask_coverage``.

    Raises Interrupted when ``cancel_event`` is set before the service call or
    once it returns; ValueError for a malformed request.
    """
    if cancel_event.is_set():
        raise Interrupted("watermark.detect canceled before start.")

    image_png = _read_image_blob(blob, "watermark.detect")
    params = _read_params(header)
    on_progress = _progress_callback(ctx)

    try:
        result = ctx.state.watermark.detect_mask_bytes(
            image_png,
            params=params,
            progress_callback=on_progress,
        )
    except (ValueError, FileNotFoundError):
        raise
    except Exception:  # noqa: BLE001 - re-raised below unless this is a cancel
        if cancel_event.is_set():
            raise Interrupted("watermark.detect canceled.") from None
        traceback.print_exc()
        raise

    if cancel_event.is_set():
        raise Interrupted("watermark.detect canceled.")

    fields = {
        "model": str(result.get("model", "")),
        "device": str(result.get("device", "cpu")),
        "source_size": result.get("source_size", [0, 0]),
        "mask_coverage": float(result.get("mask_coverage", 0.0) or 0.0),
    }
    return fields, _result_bytes(result, "mask_png")


def _handle_watermark_remove(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`watermark.remove`: experimental direct network pass over the blob image.

    Request: blob = image PNG, header ``params`` (model, tile, overlap, …)
    inline. Response: blob = ``clean_png ++ mask_png`` with ``image_len`` and
    ``mask_len`` header ints naming the two segment lengths (their sum is always
    the blob length), plus ``model`` / ``device`` / ``source_size``.

    Raises Interrupted when ``cancel_event`` is set before the service call or
    once it returns; ValueError for a malformed request.
    """
    if cancel_event.is_set():
        raise Interrupted("watermark.remove canceled before start.")

    image_png = _read_image_blob(blob, "watermark.remove")
    params = _read_params(header)
    on_progress = _progress_callback(ctx)

    try:
        result = ctx.state.watermark.remove_watermark_bytes(
            image_png,
            params=params,
            progress_callback=on_progress,
        )
    except (ValueError, FileNotFoundError):
        raise
    except Exception:  # noqa: BLE001 - re-raised below unless this is a cancel
        if cancel_event.is_set():
            raise Interrupted("watermark.remove canceled.") from None
        traceback.print_exc()
        raise

    if cancel_event.is_set():
        raise Interrupted("watermark.remove canceled.")

    clean_png = _result_bytes(result, "image_png")
    mask_png = _result_bytes(result, "mask_png")
    fields = {
        "model": str(result.get("model", "")),
        "device": str(result.get("device", "cpu")),
        "source_size": result.get("source_size", [0, 0]),
        "image_len": len(clean_png),
        "mask_len": len(mask_png),
    }
    return fields, clean_png + mask_png


def _handle_watermark_status(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`watermark.status`: the model catalog and its on-disk state, verbatim.

    Response header = the service's ``status()`` dict (``models``,
    ``default_model``, ``downloaded_models``, ``code_ready_models``); no blob.
    """
    return dict(ctx.state.watermark.status()), b""


def _handle_watermark_unload(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`watermark.unload`: drop the resident network; report the flag."""
    unloaded = bool(ctx.state.watermark.unload())
    return {"unloaded": unloaded}, b""


register(METHOD_WATERMARK_DETECT, _handle_watermark_detect)
register(METHOD_WATERMARK_REMOVE, _handle_watermark_remove)
register(METHOD_WATERMARK_STATUS, _handle_watermark_status)
register(METHOD_WATERMARK_UNLOAD, _handle_watermark_unload)
