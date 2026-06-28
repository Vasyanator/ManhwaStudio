"""
File: modules/ai_backend/browser/service.py

Purpose:
Host the advanced web-scraping browser session inside the unified AI backend
process and expose it through one IPC method (``browser.command``).

Design:
The two legacy stdio daemons (`AdvancedFetchDaemon` in
``modules/new_project/adv_fetch_cli.py`` and `CloakFetchDaemon` in
``adv_fetch_cloak_cli.py``) already share an identical command contract:
``_handle_command({"command": ...})`` performs the work and reports results by
writing one JSON event per call through ``self._emit``. ``BrowserService`` reuses
that contract unchanged — it instantiates the active backend's daemon, redirects
its ``_emit`` into this service, and calls ``_handle_command`` directly:

- ``progress`` events are forwarded to the per-request IPC ``ProgressEmitter`` so
  the launcher gets live progress frames;
- the single terminal event (``opened`` / ``result`` / ``auto_result`` /
  ``link_collect_started`` / ``intercept_count`` / ...) is captured and returned
  as the IPC response header;
- a raised exception from ``_handle_command`` propagates out so the IPC dispatcher
  reports ``status:"error"``.

Because the backend and launcher always share one machine, downloaded images keep
being handed off as an on-disk directory path + count (the daemons' existing
behaviour); no image bytes travel over IPC.

Notes:
- Selenium / Playwright are imported lazily (only when a browser command first
  runs) so an AI-only backend never pays that import cost and a missing scraping
  dependency cannot break AI startup.
- A single lock serialises browser commands (mirroring the old single stdin loop);
  AI handlers use other services and are unaffected.
- Cancellation: for the commands whose long work polls a ``cancel_file``
  (auto-fetch / deep-intercept), the IPC ``cancel_event`` is bridged to a temp
  cancel file. Other commands run to completion, exactly as the stdio daemons did
  (which could only be cancelled by killing the process).
"""

from __future__ import annotations

import logging
import tempfile
import threading
from pathlib import Path
from typing import Any, Callable, Optional
from uuid import uuid4

LOG = logging.getLogger(__name__)

BACKEND_SELENIUM = "selenium"
BACKEND_CLOAK = "cloak"

# Commands whose long-running work honours a ``cancel_file`` on the daemon side,
# so an IPC ``cancel`` can be bridged into them. Everything else runs to
# completion (matching the old "kill the process to cancel" semantics).
_CANCELABLE_COMMANDS = frozenset(
    {
        "fetch_auto_links",
        "stop_auto_link_collect",
        "stop_deep_intercept",
    }
)

# Daemon events that are not a terminal command result; never captured as one.
_NON_RESULT_EVENTS = frozenset({"progress", "log", "ready"})


class BrowserService:
    """In-process owner of the Selenium / CloakBrowser scraping session."""

    def __init__(self) -> None:
        self._lock = threading.RLock()
        self._backend = BACKEND_SELENIUM
        self._daemon: Optional[Any] = None
        # Per-dispatch slots, set under the lock; read by ``_sink`` (which may also
        # run on a daemon background thread, where ``_active_emitter`` is ``None``
        # so stray background progress is harmlessly dropped).
        self._active_emitter: Any = None
        self._captured: Optional[dict[str, Any]] = None

    # -- IPC entry point ----------------------------------------------------

    def dispatch(
        self,
        command: dict[str, Any],
        progress_emitter: Any,
        cancel_event: Optional[threading.Event],
    ) -> dict[str, Any]:
        """Run one ``browser.command`` and return its terminal event dict.

        ``command`` is the legacy advanced-download command object, e.g.
        ``{"command": "fetch", "browser": "chrome", "pattern": "...", ...}``.
        Raises on failure (the dispatcher maps that to ``status:"error"``).
        """
        name = str((command or {}).get("command") or "").strip()
        if not name:
            raise ValueError("browser.command requires a non-empty 'command' field.")

        with self._lock:
            if name in ("shutdown", "close"):
                self._close_daemon()
                return {"event": "closed"}
            if name == "set_backend":
                self._set_backend(self._normalize_backend(command.get("backend")))
                return {"event": "backend_set", "backend": self._backend}
            if name == "version":
                return {"event": "version", "downloader_version": self._version()}

            daemon = self._ensure_daemon()
            cancel_cleanup = self._install_cancel_bridge(command, name, cancel_event)
            self._active_emitter = progress_emitter
            self._captured = None
            try:
                daemon._handle_command(command)
                captured = self._captured
            finally:
                self._active_emitter = None
                self._captured = None
                if cancel_cleanup is not None:
                    cancel_cleanup()

            if captured is not None and captured.get("event") == "error":
                raise RuntimeError(
                    captured.get("user_message")
                    or "Браузерный выкачиватель завершился с ошибкой."
                )
            return captured if captured is not None else {"event": name}

    def close(self) -> None:
        """Close the live browser session (called on backend shutdown)."""
        with self._lock:
            self._close_daemon()

    def health(self) -> dict[str, Any]:
        with self._lock:
            return {
                "status": "ready" if self._daemon is not None else "idle",
                "backend": self._backend,
            }

    # -- internals ----------------------------------------------------------

    def _sink(self, payload: dict[str, Any]) -> None:
        """Replacement for the daemon's ``_emit``: stream progress, capture result."""
        event = payload.get("event")
        if event == "progress":
            emitter = self._active_emitter
            if emitter is not None:
                try:
                    emitter.emit(
                        {
                            "stage": str(payload.get("stage") or ""),
                            "current": int(payload.get("current") or 0),
                            "total": int(payload.get("total") or 0),
                        }
                    )
                except Exception:  # noqa: BLE001 - progress is best-effort
                    LOG.debug("Failed to forward browser progress frame", exc_info=True)
            return
        if event in _NON_RESULT_EVENTS:
            return
        # Terminal result event (opened / result / auto_result / *_count / ...),
        # or a defensively-emitted error event.
        self._captured = dict(payload)

    def _ensure_daemon(self) -> Any:
        if self._daemon is None:
            self._daemon = self._build_daemon(self._backend)
            # Redirect the daemon's stdout JSON emitter into this service so
            # progress streams over IPC and terminal events become return values.
            self._daemon._emit = self._sink
        return self._daemon

    def _build_daemon(self, backend: str) -> Any:
        # Lazy import: selenium / playwright load only on first browser use.
        if backend == BACKEND_CLOAK:
            from modules.new_project.adv_fetch_cloak_cli import CloakFetchDaemon

            return CloakFetchDaemon()
        from modules.new_project.adv_fetch_cli import AdvancedFetchDaemon

        return AdvancedFetchDaemon()

    def _set_backend(self, backend: str) -> None:
        if backend == self._backend and self._daemon is not None:
            return
        self._close_daemon()
        self._backend = backend

    def _close_daemon(self) -> None:
        daemon = self._daemon
        self._daemon = None
        if daemon is not None:
            try:
                daemon.close()
            except Exception:  # noqa: BLE001 - shutdown best-effort
                LOG.exception("Failed to close browser daemon")

    @staticmethod
    def _normalize_backend(raw: Any) -> str:
        value = str(raw or "").strip().lower()
        return BACKEND_CLOAK if value == BACKEND_CLOAK else BACKEND_SELENIUM

    @staticmethod
    def _version() -> str:
        try:
            from config import VERSION

            return str(VERSION)
        except Exception:  # noqa: BLE001
            return ""

    def _install_cancel_bridge(
        self,
        command: dict[str, Any],
        name: str,
        cancel_event: Optional[threading.Event],
    ) -> Optional[Callable[[], None]]:
        """Bridge an IPC ``cancel_event`` to a ``cancel_file`` for auto commands."""
        if cancel_event is None or name not in _CANCELABLE_COMMANDS:
            return None
        cancel_path = Path(tempfile.gettempdir()) / f"ms_browser_cancel_{uuid4().hex}"
        command["cancel_file"] = str(cancel_path)
        stop = threading.Event()

        def watch() -> None:
            while not stop.is_set():
                if cancel_event.is_set():
                    try:
                        cancel_path.write_text("cancel", encoding="utf-8")
                    except Exception:  # noqa: BLE001
                        LOG.debug("Failed to write browser cancel file", exc_info=True)
                    return
                if stop.wait(0.2):
                    return

        watcher = threading.Thread(target=watch, daemon=True, name="ms-browser-cancel")
        watcher.start()

        def cleanup() -> None:
            stop.set()
            try:
                cancel_path.unlink()
            except FileNotFoundError:
                pass
            except Exception:  # noqa: BLE001
                LOG.debug("Failed to remove browser cancel file", exc_info=True)

        return cleanup
