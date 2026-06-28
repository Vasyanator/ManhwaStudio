"""
File: modules/ai_backend/ipc/handlers/translate.py

Methods hosted here:
    translate.deep — machine translation via DeepL/Google/etc (METHOD_TRANSLATE_DEEP)

Request fields (inline in header):
    service: str = "google"   (defaults to "google" when absent/null)
    source:  str = "auto"     (defaults to "auto" when absent/null)
    target:  str = "ru"       (defaults to "ru" when absent/null)
    params:  object = {}      (defaults to {} when absent/null)
    texts:   str[]            (required, non-empty; each item coerced to str)

Response fields (status=ok):
    service:    str    (normalized: stripped + lowercased, defaulting to "google")
    translated: int    (count of results where ok==True)
    errors:     int    (len(results) - translated)
    results:    object[]  (each {ok: bool, ...}, passed through from service)

No blob in either direction; ``cancel`` is not honored (short network call).
"""

from __future__ import annotations

import threading
import traceback
from typing import Any

from ..protocol import METHOD_TRANSLATE_DEEP
from ..registry import HandlerContext, register


def _handle_translate_deep(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`translate.deep`: translate a batch of text segments.

    Validates and coerces all fields exactly as the HTTP ``_handle_translate_deep``
    handler does, then calls ``ctx.state.machine_translation.translate_batch``.
    ``ValueError`` from the service is re-raised (-> response{status:"error"});
    other exceptions are also re-raised (dispatcher converts to error response).
    """
    # --- service ---
    service_raw = header.get("service", "google")
    if service_raw is None:
        service_raw = "google"
    if not isinstance(service_raw, str):
        raise ValueError("Field 'service' must be a string.")

    # --- source ---
    source_raw = header.get("source", "auto")
    if source_raw is None:
        source_raw = "auto"
    if not isinstance(source_raw, str):
        raise ValueError("Field 'source' must be a string.")

    # --- target ---
    target_raw = header.get("target", "ru")
    if target_raw is None:
        target_raw = "ru"
    if not isinstance(target_raw, str):
        raise ValueError("Field 'target' must be a string.")

    # --- params ---
    params_raw = header.get("params", {})
    if params_raw is None:
        params_raw = {}
    if not isinstance(params_raw, dict):
        raise ValueError("Field 'params' must be an object.")

    # --- texts ---
    texts_raw = header.get("texts")
    if not isinstance(texts_raw, list):
        raise ValueError("Field 'texts' must be an array.")
    if not texts_raw:
        raise ValueError("Field 'texts' must not be empty.")
    texts = [str(text or "") for text in texts_raw]

    try:
        results = ctx.state.machine_translation.translate_batch(
            service=service_raw,
            source=source_raw,
            target=target_raw,
            params=params_raw,
            texts=texts,
        )
    except ValueError as exc:
        raise ValueError(str(exc)) from exc
    except Exception as exc:
        traceback.print_exc()
        raise RuntimeError(str(exc)) from exc

    translated = sum(1 for item in results if bool(item.get("ok")))
    errors = len(results) - translated

    # service is normalized the same way the HTTP handler normalizes it.
    service_normalized = str(service_raw).strip().lower() or "google"

    return (
        {
            "service": service_normalized,
            "translated": translated,
            "errors": errors,
            "results": results,
        },
        b"",
    )


register(METHOD_TRANSLATE_DEEP, _handle_translate_deep)
