"""
File: modules/ai_backend/ipc/handlers/flux_fill.py

Methods hosted here:
    inpaint.flux_fill         — FLUX.1-Fill-dev inpaint/object-removal, streaming.
    inpaint.flux_fill.unload  — drop the loaded pipeline.
    inpaint.flux_fill.status  — quant catalog + on-disk download state.

Streaming: like ``inpaint.sdxl`` the handler pushes ``progress{id}`` frames via
the dispatcher's ``ProgressEmitter`` (``HandlerContext.progress_emitter``). Flux
Fill reports TWO phases, distinguished by a ``phase`` header field:
    - ``phase:"download"`` — ``step``/``total`` are BYTES downloaded / total, with
      a human ``label`` (which file).
    - ``phase:"generate"`` — ``step``/``total`` are diffusion steps.
No preview blob is sent (the GGUF transformer has no cheap latent preview here).

Blob convention (same as the other inpaint methods):
    request blob = image_png ++ mask_png   (split via image_len / mask_len)
The result PNG goes in the response blob (raw bytes).
"""

from __future__ import annotations

import threading
import traceback
from typing import Any

from ..protocol import (
    METHOD_INPAINT_FLUX_FILL,
    METHOD_INPAINT_FLUX_FILL_STATUS,
    METHOD_INPAINT_FLUX_FILL_UNLOAD,
)
from ..registry import HandlerContext, Interrupted, register

_PROGRESS_EMITTER_ATTR = "progress_emitter"


def _split_image_mask(header: dict[str, Any], blob: bytes) -> tuple[bytes, bytes]:
    image_len = header.get("image_len")
    mask_len = header.get("mask_len")
    if isinstance(image_len, bool) or not isinstance(image_len, int) or image_len < 0:
        raise ValueError("Field 'image_len' must be a non-negative integer.")
    if isinstance(mask_len, bool) or not isinstance(mask_len, int) or mask_len < 0:
        raise ValueError("Field 'mask_len' must be a non-negative integer.")
    if image_len + mask_len != len(blob):
        raise ValueError(
            f"Inpaint blob length mismatch: image_len ({image_len}) + mask_len "
            f"({mask_len}) != blob length ({len(blob)})."
        )
    image_png = blob[:image_len]
    mask_png = blob[image_len : image_len + mask_len]
    if not image_png:
        raise ValueError("inpaint.flux_fill requires a non-empty image in the blob.")
    if not mask_png:
        raise ValueError("inpaint.flux_fill requires a non-empty mask in the blob.")
    return image_png, mask_png


def _handle_inpaint_flux_fill(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    if cancel_event.is_set():
        raise Interrupted("inpaint.flux_fill canceled before start.")

    image_png, mask_png = _split_image_mask(header, blob)
    params_raw = header.get("params", {})
    if params_raw is None:
        params_raw = {}
    if not isinstance(params_raw, dict):
        raise ValueError("Field 'params' must be an object.")

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

    try:
        result = ctx.state.flux_fill_inpaint.inpaint_image_bytes(
            image_png,
            mask_png,
            params=params_raw,
            progress_callback=on_progress,
        )
    except (ValueError, FileNotFoundError):
        raise
    except Exception:  # noqa: BLE001
        if cancel_event.is_set():
            raise Interrupted("inpaint.flux_fill canceled.") from None
        traceback.print_exc()
        raise

    if cancel_event.is_set():
        raise Interrupted("inpaint.flux_fill canceled.")

    fields = {
        "engine": "flux_fill",
        "source_size": result.get("source_size", [0, 0]),
        "device": result.get("device", "cpu"),
        "mode": result.get("mode", "object_removal"),
        "quant": result.get("quant", ""),
    }
    return fields, (result.get("image_png", b"") or b"")


register(METHOD_INPAINT_FLUX_FILL, _handle_inpaint_flux_fill)


def _handle_inpaint_flux_fill_unload(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    unloaded = bool(ctx.state.flux_fill_inpaint.unload())
    return {"unloaded": unloaded}, b""


register(METHOD_INPAINT_FLUX_FILL_UNLOAD, _handle_inpaint_flux_fill_unload)


def _handle_inpaint_flux_fill_status(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    return dict(ctx.state.flux_fill_inpaint.status()), b""


register(METHOD_INPAINT_FLUX_FILL_STATUS, _handle_inpaint_flux_fill_status)
