"""
File: modules/ai_backend/ipc/handlers/reline.py

Methods hosted here:
    reline.models  — list available Reline models (METHOD_RELINE_MODELS)
    reline.process — run Reline on an on-disk image path (METHOD_RELINE_PROCESS)

Reline operates entirely on on-disk paths: ``image_path`` (required) is the
input file; the processed PNG is written to ``output_path`` (optional).  No
image bytes cross the socket.  ``params`` mirrors the Rust ``RelineOptions``
struct.

Request fields for reline.models:
    (none)

Response fields for reline.models (status=ok):
    models: object[]  (each {name, filename, downloaded})

Request fields for reline.process:
    image_path:  string  (required, on-disk path)
    output_path: string|null
    params:      object  (RelineOptions; upscale.enabled==true requires Torch)

Response fields for reline.process (status=ok):
    Full Reline service result object passed through verbatim (includes at
    least ``ok``); the processed PNG is written to ``output_path`` on disk.
    No image bytes cross the socket.
"""

from __future__ import annotations

import threading
import traceback
from typing import Any

from ..protocol import METHOD_RELINE_MODELS, METHOD_RELINE_PROCESS
from ..registry import HandlerContext, register


def _handle_reline_models(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`reline.models`: list available Reline models.

    Calls ``ctx.state.reline.list_models()`` and returns the list under
    ``models``.  No request fields; no blob in either direction.
    """
    try:
        models = ctx.state.reline.list_models()
    except Exception as exc:
        traceback.print_exc()
        raise RuntimeError(str(exc)) from exc

    return {"models": models}, b""


def _handle_reline_process(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`reline.process`: run Reline image processing on an on-disk path.

    Request header fields:
        ``image_path`` (required string): on-disk input image path.
        ``output_path`` (optional string|null): on-disk output path; when
            absent/null the service picks a default.
        ``params`` (optional object): RelineOptions dict.  When
            ``params.upscale.enabled`` is true the service requires Torch
            (the service itself raises if Torch is unavailable).

    Returns the service result dict verbatim (opaque pass-through); only
    ``ok`` is guaranteed by the protocol.  No blob in either direction.
    """
    image_path_raw = header.get("image_path")
    if not isinstance(image_path_raw, str) or not image_path_raw.strip():
        raise ValueError("Field 'image_path' is required.")

    output_path_raw = header.get("output_path")
    if output_path_raw is not None and not isinstance(output_path_raw, str):
        raise ValueError("Field 'output_path' must be a string.")

    params_raw = header.get("params", {})
    if params_raw is None:
        params_raw = {}
    if not isinstance(params_raw, dict):
        raise ValueError("Field 'params' must be an object.")

    image_path = image_path_raw.strip()
    output_path: str | None = None
    if isinstance(output_path_raw, str) and output_path_raw.strip():
        output_path = output_path_raw.strip()

    try:
        result = ctx.state.reline.process_image_file(
            image_path=image_path,
            output_path=output_path,
            params=params_raw,
        )
    except (ValueError, FileNotFoundError):
        # Invalid input or missing file: propagate the original type so the
        # dispatcher and callers can distinguish them (FileNotFoundError stays
        # FileNotFoundError; ValueError stays ValueError). The dispatcher turns
        # both into response{status:error} on the wire regardless.
        raise
    except Exception as exc:
        traceback.print_exc()
        raise RuntimeError(str(exc)) from exc

    # The reline handler returns the result dict verbatim (opaque pass-through).
    return dict(result), b""


register(METHOD_RELINE_MODELS, _handle_reline_models)
register(METHOD_RELINE_PROCESS, _handle_reline_process)
