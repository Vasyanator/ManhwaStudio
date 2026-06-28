"""
File: modules/ai_backend/ipc/handlers/sdxl.py

Methods hosted here:
    inpaint.sdxl        — SDXL inpainting with streaming progress (METHOD_INPAINT_SDXL)
    inpaint.sdxl.unload — unload SDXL model (METHOD_INPAINT_SDXL_UNLOAD)

SDXL is the only method that emits ``progress`` frames during diffusion. This
replaces the legacy NDJSON-over-HTTP hack (``server._handle_inpaint_sdxl``):
instead of writing ``{"type":"progress", ...}`` JSON lines to the HTTP body, the
handler pushes native v2 ``progress{id}`` frames via the dispatcher's
``ProgressEmitter`` (``dispatcher.ProgressEmitter``).

Streaming mechanism (consumes the dispatcher's existing API, no dispatcher
redesign):
    - The dispatcher's ``ProgressEmitter`` exposes ``emit(fields, blob=b"")``,
      which writes a ``progress{id}`` frame (the request id and ``kind`` are
      filled in by the emitter) under the connection write lock.
    - A streaming handler receives its per-request emitter on the
      ``HandlerContext`` as the optional attribute ``progress_emitter`` (set by
      the dispatcher for the duration of the request). When absent (e.g. a
      non-streaming caller or a unit harness that did not attach one), progress
      emission degrades to a no-op and only the terminal ``response`` is sent —
      so the handler stays correct regardless of whether a stream is wired.
    - The per-step diffusion callback turns each
      ``progress_callback(step, total, preview_rgb)`` into one ``progress`` frame
      carrying ``{step, total}`` in the header and, when a preview is available,
      the raw preview PNG bytes in the frame BLOB (NOT base64).

Blob convention (same as the other inpaint methods):
    request blob = image_png ++ mask_png
    header carries ``image_len: int`` and ``mask_len: int`` to split the blob.
The final result PNG goes in the response blob (raw bytes, not base64).
"""

from __future__ import annotations

import threading
import traceback
from typing import Any

from ..protocol import (
    METHOD_INPAINT_SDXL,
    METHOD_INPAINT_SDXL_UNLOAD,
)
from ..registry import HandlerContext, Interrupted, register

# Attribute name under which the dispatcher attaches the per-request
# ``ProgressEmitter`` to the (otherwise shared) ``HandlerContext``. Looked up
# with ``getattr`` so the handler never hard-depends on the dispatcher having
# wired it; absent => progress emission is a no-op.
_PROGRESS_EMITTER_ATTR = "progress_emitter"


def _split_image_mask(header: dict[str, Any], blob: bytes) -> tuple[bytes, bytes]:
    """Split the concatenated ``image_png ++ mask_png`` request blob.

    ``image_len``/``mask_len`` header ints name how many bytes of the blob are
    the image and the trailing mask, respectively (PROTOCOL.md §5.4). Any
    missing/invalid length or a blob too short to satisfy them is a request
    error (``ValueError`` -> ``response{status:"error"}``).
    """
    image_len = header.get("image_len")
    mask_len = header.get("mask_len")
    if isinstance(image_len, bool) or not isinstance(image_len, int) or image_len < 0:
        raise ValueError("Field 'image_len' must be a non-negative integer.")
    if isinstance(mask_len, bool) or not isinstance(mask_len, int) or mask_len < 0:
        raise ValueError("Field 'mask_len' must be a non-negative integer.")
    expected = image_len + mask_len
    if expected != len(blob):
        raise ValueError(
            "Inpaint blob length mismatch: image_len "
            f"({image_len}) + mask_len ({mask_len}) = {expected} "
            f"!= blob length ({len(blob)})."
        )
    image_png = blob[:image_len]
    mask_png = blob[image_len : image_len + mask_len]
    if not image_png:
        raise ValueError("inpaint.sdxl requires a non-empty image in the blob.")
    if not mask_png:
        raise ValueError("inpaint.sdxl requires a non-empty mask in the blob.")
    return image_png, mask_png


def _handle_inpaint_sdxl(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`inpaint.sdxl`: streaming SDXL inpaint (mirrors HTTP ``_handle_inpaint_sdxl``).

    Request: ``image_len``/``mask_len`` split the ``image_png ++ mask_png`` blob;
    ``params`` is an inline object forwarded to the service. During diffusion the
    handler emits one ``progress{id}`` frame per step with ``{step, total}`` in
    the header and the raw preview PNG in the frame blob (when a preview is
    available). On success it returns the terminal ``response`` fields
    (``engine``/``source_size``/``device``/``mode``) with the result PNG (raw
    bytes) as the response blob.

    Errors mirror the HTTP path: ``ValueError``/``FileNotFoundError`` and any
    other exception surface as ``response{status:"error"}`` (the dispatcher maps
    the raised exception). ``cancel_event`` (set by ``cancel{id}``) yields
    ``response{status:"interrupted"}`` — both by raising ``Interrupted`` before
    starting and by the dispatcher's post-run cancel check.
    """
    if cancel_event.is_set():
        raise Interrupted("inpaint.sdxl canceled before start.")

    image_png, mask_png = _split_image_mask(header, blob)

    params_raw = header.get("params", {})
    if params_raw is None:
        params_raw = {}
    if not isinstance(params_raw, dict):
        raise ValueError("Field 'params' must be an object.")

    emitter = getattr(ctx, _PROGRESS_EMITTER_ATTR, None)

    def on_progress(step: int, total: int, preview_rgb: Any) -> None:
        """Per-step diffusion callback -> one ``progress{id}`` frame.

        Header carries ``{step, total}``; the optional latent preview PNG goes in
        the progress frame BLOB as raw bytes. A missing emitter or a
        preview-encode failure never aborts generation.
        """
        if emitter is None:
            return
        preview_blob = b""
        if preview_rgb is not None:
            try:
                # Encode the preview as raw PNG bytes for the frame blob.
                from ...sdxl_inpaint_service import _encode_png_bytes_rgb

                preview_blob = _encode_png_bytes_rgb(preview_rgb)
            except Exception:  # noqa: BLE001 - preview is best-effort
                preview_blob = b""
        try:
            emitter.emit({"step": int(step), "total": int(total)}, preview_blob)
        except Exception:  # noqa: BLE001 - peer gone; generation continues, ignored
            pass

    try:
        result = ctx.state.sdxl_inpaint.inpaint_image_bytes(
            image_png,
            mask_png,
            params=params_raw,
            progress_callback=on_progress,
        )
    except (ValueError, FileNotFoundError):
        # Invalid input / missing model file: surfaced as response{status:error}.
        raise
    except Exception:  # noqa: BLE001 - mirror HTTP: log + report as error
        if cancel_event.is_set():
            raise Interrupted("inpaint.sdxl canceled.") from None
        traceback.print_exc()
        raise

    if cancel_event.is_set():
        raise Interrupted("inpaint.sdxl canceled.")

    result_png = result.get("image_png", b"") or b""
    fields = {
        "engine": "sdxl",
        "source_size": result.get("source_size", [0, 0]),
        "device": result.get("device", "cpu"),
        "mode": result.get("mode", "nine_channel"),
    }
    return fields, result_png


register(METHOD_INPAINT_SDXL, _handle_inpaint_sdxl)


def _handle_inpaint_sdxl_unload(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`inpaint.sdxl.unload`: drop the loaded SDXL model.

    Mirrors HTTP ``_handle_inpaint_sdxl_unload``: returns ``{"unloaded": bool}``
    inline (no blob). A service failure raises and surfaces as
    ``response{status:"error"}``.
    """
    unloaded = bool(ctx.state.sdxl_inpaint.unload())
    return {"unloaded": unloaded}, b""


register(METHOD_INPAINT_SDXL_UNLOAD, _handle_inpaint_sdxl_unload)
