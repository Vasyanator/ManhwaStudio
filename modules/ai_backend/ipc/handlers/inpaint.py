"""
File: modules/ai_backend/ipc/handlers/inpaint.py

Methods hosted here:
    inpaint.lama_v2        — LaMa-v2 inpainting (METHOD_INPAINT_LAMA_V2)
    inpaint.lama_v2.unload — unload LaMa-v2 model (METHOD_INPAINT_LAMA_V2_UNLOAD)
    inpaint.lama_mpe       — LaMa-MPE inpainting (METHOD_INPAINT_LAMA_MPE)
    inpaint.lama_mpe.unload— unload LaMa-MPE model (METHOD_INPAINT_LAMA_MPE_UNLOAD)
    inpaint.aot            — AOT-GAN inpainting (METHOD_INPAINT_AOT)
    inpaint.aot.unload     — unload AOT model (METHOD_INPAINT_AOT_UNLOAD)

Two-image blob convention (shared by all inpaint methods):
    request blob = image_png ++ mask_png
    header carries ``image_len: int`` and ``mask_len: int`` to split the blob.
The result image PNG goes in the response blob (raw bytes, NOT base64).

The underlying ``AppState`` inpaint services return the result PNG as raw bytes
in the ``image_png`` result key (see e.g. ``lama_inpaint_service.inpaint_image_bytes``);
these handlers put those bytes straight into the response blob.
"""

from __future__ import annotations

import threading
from typing import Any

from ..protocol import (
    METHOD_INPAINT_AOT,
    METHOD_INPAINT_AOT_UNLOAD,
    METHOD_INPAINT_LAMA_MPE,
    METHOD_INPAINT_LAMA_MPE_UNLOAD,
    METHOD_INPAINT_LAMA_V2,
    METHOD_INPAINT_LAMA_V2_UNLOAD,
)
from ..registry import HandlerContext, Interrupted, register


def _read_int_field(header: dict[str, Any], name: str) -> int:
    """Read a required non-negative integer header field, or raise ValueError."""
    if name not in header:
        raise ValueError(f"Missing required header field {name!r}.")
    value = header[name]
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"Header field {name!r} must be an integer.")
    if value < 0:
        raise ValueError(f"Header field {name!r} must be non-negative.")
    return value


def _read_params(header: dict[str, Any]) -> dict[str, Any]:
    """Read the optional ``params`` object header field (defaults to ``{}``)."""
    params_raw = header.get("params", {})
    if params_raw is None:
        params_raw = {}
    if not isinstance(params_raw, dict):
        raise ValueError("Field 'params' must be an object.")
    return params_raw


def _split_image_mask(
    header: dict[str, Any], blob: bytes
) -> tuple[bytes, bytes]:
    """Split the concatenated request blob into ``(image_png, mask_png)``.

    The blob is ``image_png ++ mask_png``; ``image_len`` and ``mask_len`` header
    ints name the two segment lengths.  Their sum must equal ``len(blob)``.
    """
    image_len = _read_int_field(header, "image_len")
    mask_len = _read_int_field(header, "mask_len")
    expected = image_len + mask_len
    if expected != len(blob):
        raise ValueError(
            "Inpaint blob length mismatch: image_len "
            f"({image_len}) + mask_len ({mask_len}) = {expected} "
            f"!= blob length ({len(blob)})."
        )
    image_bytes = blob[:image_len]
    mask_bytes = blob[image_len : image_len + mask_len]
    return image_bytes, mask_bytes


def _result_png_bytes(result: dict[str, Any]) -> bytes:
    """Return the service's raw ``image_png`` result for the response blob.

    The service produces the PNG as raw bytes; ``b""`` when absent.
    """
    return result.get("image_png", b"") or b""


# ---------------------------------------------------------------------------
# inpaint.lama_v2
# ---------------------------------------------------------------------------


def _handle_inpaint_lama_v2(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`inpaint.lama_v2`: LaMa-v2 inpaint of (image, mask) from the request blob.

    Request blob = image_png ++ mask_png, split by ``image_len``/``mask_len``.
    ``params`` (refine, n_iters, max_scales, px_budget, model_name) is inline in
    the header.  The result PNG is returned as the response blob; the metadata
    (engine/source_size/device/refine/model_name) is the response header.
    """
    if cancel_event.is_set():
        raise Interrupted("inpaint.lama_v2 canceled before start.")

    image_bytes, mask_bytes = _split_image_mask(header, blob)
    params = _read_params(header)

    result = ctx.state.lama_inpaint.inpaint_image_bytes(
        image_bytes,
        mask_bytes,
        params=params,
    )

    if cancel_event.is_set():
        raise Interrupted("inpaint.lama_v2 canceled.")

    return (
        {
            "engine": "lama_v2",
            "source_size": result.get("source_size", [0, 0]),
            "device": result.get("device", "cpu"),
            "refine": bool(result.get("refine", False)),
            "model_name": result.get("model_name"),
        },
        _result_png_bytes(result),
    )


def _handle_inpaint_lama_v2_unload(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`inpaint.lama_v2.unload`: unload the LaMa-v2 model; report the flag."""
    unloaded = bool(ctx.state.lama_inpaint.unload())
    return {"unloaded": unloaded}, b""


# ---------------------------------------------------------------------------
# inpaint.lama_mpe
# ---------------------------------------------------------------------------


def _handle_inpaint_lama_mpe(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`inpaint.lama_mpe`: LaMa-MPE inpaint of (image, mask) from the request blob.

    Request blob = image_png ++ mask_png, split by ``image_len``/``mask_len``.
    ``params`` (inpaint_size) is inline in the header.  The result PNG is the
    response blob; engine/source_size/device/inpaint_size are the header.
    """
    if cancel_event.is_set():
        raise Interrupted("inpaint.lama_mpe canceled before start.")

    image_bytes, mask_bytes = _split_image_mask(header, blob)
    params = _read_params(header)

    result = ctx.state.lama_mpe_inpaint.inpaint_image_bytes(
        image_bytes,
        mask_bytes,
        params=params,
    )

    if cancel_event.is_set():
        raise Interrupted("inpaint.lama_mpe canceled.")

    return (
        {
            "engine": "lama_mpe",
            "source_size": result.get("source_size", [0, 0]),
            "device": result.get("device", "cpu"),
            "inpaint_size": int(result.get("inpaint_size", 2048)),
        },
        _result_png_bytes(result),
    )


def _handle_inpaint_lama_mpe_unload(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`inpaint.lama_mpe.unload`: unload the LaMa-MPE model; report the flag."""
    unloaded = bool(ctx.state.lama_mpe_inpaint.unload())
    return {"unloaded": unloaded}, b""


# ---------------------------------------------------------------------------
# inpaint.aot
# ---------------------------------------------------------------------------


def _handle_inpaint_aot(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`inpaint.aot`: AOT-GAN inpaint of (image, mask) from the request blob.

    Request blob = image_png ++ mask_png, split by ``image_len``/``mask_len``.
    ``params`` (inpaint_size) is inline in the header.  The result PNG is the
    response blob; engine/source_size/device/inpaint_size are the header.
    """
    if cancel_event.is_set():
        raise Interrupted("inpaint.aot canceled before start.")

    image_bytes, mask_bytes = _split_image_mask(header, blob)
    params = _read_params(header)

    result = ctx.state.aot_inpaint.inpaint_image_bytes(
        image_bytes,
        mask_bytes,
        params=params,
    )

    if cancel_event.is_set():
        raise Interrupted("inpaint.aot canceled.")

    return (
        {
            "engine": "aot",
            "source_size": result.get("source_size", [0, 0]),
            "device": result.get("device", "cpu"),
            "inpaint_size": int(result.get("inpaint_size", 2048)),
        },
        _result_png_bytes(result),
    )


def _handle_inpaint_aot_unload(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`inpaint.aot.unload`: unload the AOT model; report the flag."""
    unloaded = bool(ctx.state.aot_inpaint.unload())
    return {"unloaded": unloaded}, b""


register(METHOD_INPAINT_LAMA_V2, _handle_inpaint_lama_v2)
register(METHOD_INPAINT_LAMA_V2_UNLOAD, _handle_inpaint_lama_v2_unload)
register(METHOD_INPAINT_LAMA_MPE, _handle_inpaint_lama_mpe)
register(METHOD_INPAINT_LAMA_MPE_UNLOAD, _handle_inpaint_lama_mpe_unload)
register(METHOD_INPAINT_AOT, _handle_inpaint_aot)
register(METHOD_INPAINT_AOT_UNLOAD, _handle_inpaint_aot_unload)
