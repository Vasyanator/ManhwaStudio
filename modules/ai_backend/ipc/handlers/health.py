"""
File: modules/ai_backend/ipc/handlers/health.py

Methods hosted here:
    health  — pull the current backend health snapshot (METHOD_HEALTH)

Registration pattern — to add a new method in this module (or any sibling
module), call ``register`` with the method-name constant and the handler
function:

    from ..registry import register
    from ..protocol import METHOD_HEALTH

    @register(METHOD_HEALTH)
    def _handle_health(ctx, header, blob, cancel_event):
        ...

Or equivalently:

    register(METHOD_HEALTH, _handle_health)

Both forms are supported.  ``register`` is a thin decorator-compatible
wrapper around ``METHOD_HANDLERS[method] = handler``.
"""

from __future__ import annotations

import threading
from typing import Any

from ..protocol import METHOD_HEALTH
from ..registry import HandlerContext, register


def _handle_health(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`health`: return the current health snapshot as inline response fields.

    Mirrors ``GET /health``: the snapshot dict (already maintained by the health
    worker) is returned verbatim as the response header fields.  No blob.
    """
    snapshot = ctx.get_health_snapshot()
    return dict(snapshot), b""


register(METHOD_HEALTH, _handle_health)
