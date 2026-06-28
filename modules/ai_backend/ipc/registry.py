"""
File: modules/ai_backend/ipc/registry.py

Purpose:
Method registry for the v2 IPC protocol: maps a ``method`` name to a handler
that runs the request and returns ``(response_header_fields, response_blob)``.
The dispatcher looks the handler up here, runs it on a worker thread, and
frames the result.

Main responsibilities:
- define the handler contract: ``handler(ctx, header, blob, cancel_event)``
  returning ``(dict, bytes)``; the returned dict holds the inline result fields
  (the dispatcher adds ``kind``/``status``/``id``), and the bytes are the
  response blob (empty for methods that return no image);
- expose ``METHOD_HANDLERS``, ``register``, and ``get_handler`` as the
  extension point that handler group modules use;
- import the ``handlers`` package once so every group module self-registers
  its methods into ``METHOD_HANDLERS`` at import time.

Notes:
A handler may raise to signal a request failure; the dispatcher converts the
exception into a ``response{status:"error"}``.  A handler that observes
``cancel_event`` and returns mid-flight should raise ``Interrupted`` so the
dispatcher emits ``response{status:"interrupted"}``.
"""

from __future__ import annotations

import threading
from dataclasses import dataclass
from typing import Any, Callable, Protocol


class Interrupted(Exception):
    """Raised by a handler that stopped early because its cancel event was set.

    The dispatcher maps this to a terminal ``response{status:"interrupted"}``.
    """


class _HealthSnapshotProvider(Protocol):
    def __call__(self) -> dict[str, Any]: ...


@dataclass
class HandlerContext:
    """Everything a handler needs that is not part of the request frame.

    Attributes:
        state: the shared backend ``AppState`` (the same service objects the
            framed IPC handlers use, e.g. ``state.manga_ocr``).  Typed ``Any`` so
            this module does not import ``server.py`` (avoids a heavy/circular
            import; the real ``AppState`` is passed at runtime).
        events: the ``EventBus`` for publishing server-initiated events.
        get_health_snapshot: returns the current health snapshot dict (mirrors
            ``server._get_health_snapshot``), injected so this module needs no
            ``server.py`` import.
        progress_emitter: optional per-REQUEST streaming hook. The dispatcher
            builds a fresh ``ProgressEmitter`` bound to one request's ``id`` and
            attaches it to a per-request *copy* of this context (never the shared
            connection ctx), so streaming handlers (e.g. SDXL) can push
            ``progress{id}`` frames without two concurrent requests on the same
            connection clobbering each other's emitter. ``None`` for
            non-streaming handlers, which never read it.
    """

    state: Any
    events: Any
    get_health_snapshot: _HealthSnapshotProvider
    progress_emitter: Any = None


# A handler runs one request.  It receives the request header, the request
# blob, and a per-id cancel Event it must observe for long work.  It returns
# the inline response fields (merged into the response header) and the
# response blob.
Handler = Callable[
    [HandlerContext, dict[str, Any], bytes, threading.Event],
    tuple[dict[str, Any], bytes],
]

# ============================================================================
# METHOD REGISTRY
# ----------------------------------------------------------------------------
# The single source of truth for registered handlers.  Do NOT populate this
# dict directly here — add handlers in the appropriate module under handlers/.
# ``hello`` is NOT a method; it is handled inline by the dispatcher handshake
# and intentionally absent from this table.
# ============================================================================
METHOD_HANDLERS: dict[str, Handler] = {}


def register(method: str, handler: Handler | None = None):
    """Register (or override) the handler for ``method``.

    Usable in two equivalent forms:

    Plain call::

        register(METHOD_OCR_MANGA, _handle_ocr_manga)

    Decorator::

        @register(METHOD_OCR_MANGA)
        def _handle_ocr_manga(ctx, header, blob, cancel_event):
            ...

    When called with two arguments the handler is registered immediately and
    the handler function is returned.  When called with only ``method``
    (decorator form) a one-argument decorator is returned.
    """
    if handler is None:
        # Decorator form: @register(METHOD_X)
        def _decorator(fn: Handler) -> Handler:
            METHOD_HANDLERS[method] = fn
            return fn
        return _decorator
    # Plain call: register(METHOD_X, fn)
    METHOD_HANDLERS[method] = handler
    return handler


def get_handler(method: str) -> Handler | None:
    """Return the handler for ``method``, or None if it is not implemented."""
    return METHOD_HANDLERS.get(method)


# ============================================================================
# HANDLER GROUP IMPORTS
# ----------------------------------------------------------------------------
# This is the ONLY shared touch-point for parallel group agents.  Each group
# module in handlers/ self-registers its methods via register() at import
# time.  Importing the package once here is sufficient to wire them all.
# Future agents: do NOT add imports here — add ONE import line in
# handlers/__init__.py instead, keeping this file stable.
# ============================================================================
from . import handlers  # noqa: F401, E402 — side-effect: registers all groups
