"""
File: modules/ai_backend/ipc/handlers/browser.py

Method hosted here:
    browser.command — drive the in-process Selenium / CloakBrowser session
                      (METHOD_BROWSER_COMMAND).

The request header carries the legacy advanced-download command object under the
``payload`` field, e.g.::

    {"payload": {"command": "open_url", "browser": "chrome", "url": "https://..."}}

``BrowserService.dispatch`` runs it (streaming ``progress`` frames via the
per-request emitter) and returns the daemon's terminal event dict, which becomes
the response header (fields such as ``current_url`` / ``output_dir`` /
``downloaded_images`` / ``found_links`` / ``found_pages`` / ``items``). No
response blob: downloaded images are handed off as the on-disk ``output_dir``.
"""

from __future__ import annotations

import threading
from typing import Any

from ..protocol import METHOD_BROWSER_COMMAND
from ..registry import HandlerContext, Interrupted, register


@register(METHOD_BROWSER_COMMAND)
def _handle_browser_command(
    ctx: HandlerContext,
    header: dict[str, Any],
    blob: bytes,
    cancel_event: threading.Event,
) -> tuple[dict[str, Any], bytes]:
    """`browser.command`: run one advanced-download command in the shared session."""
    payload = header.get("payload")
    if not isinstance(payload, dict):
        raise ValueError("browser.command requires an object 'payload' field.")

    result = ctx.state.browser.dispatch(payload, ctx.progress_emitter, cancel_event)

    if cancel_event.is_set():
        raise Interrupted("browser.command canceled.")
    if not isinstance(result, dict):
        result = {"event": "ok"}
    return result, b""
