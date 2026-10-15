"""
File: modules/ai_backend/browser/test_browser_service.py

Purpose:
Unit tests for `BrowserService`'s thread-affinity and event-routing contracts
(`modules/ai_backend/browser/service.py`).

Main responsibilities:
- every browser command (`_handle_command`) runs on ONE dedicated owner thread,
  never the calling (dispatcher pool) thread;
- that owner thread is reused across sequential commands (so Playwright's
  greenlet, bound to the thread that launched the browser, is never switched);
- `close()` tears the daemon down on the same owner thread;
- the daemon receives the `_run_on_browser_thread` hook, and calling it marshals
  work onto the owner thread (so the daemon's background loop can pin its
  Playwright calls too);
- daemon `log` events reach the Python logger at their own level and are never
  taken as the command result; an `error` event's `log_message` is logged while
  its `user_message` becomes the raised error.

Notes:
A fake daemon records `threading.get_ident()` for each call, so the tests need no
real browser. The tests avoid pytest fixtures and expose a `__main__` runner, so
they pass under both `pytest` and a plain `python3` invocation.
"""

from __future__ import annotations

import logging
import threading

from modules.ai_backend.browser import service as service_module
from modules.ai_backend.browser.service import BrowserService


class _FakeDaemon:
    """Records the thread each daemon call runs on; no real browser involved."""

    def __init__(self) -> None:
        self.command_threads: list[int] = []
        self.close_threads: list[int] = []
        # Set by BrowserService._ensure_daemon; a real daemon defaults it to a
        # passthrough. Present here so the attribute exists before injection.
        self._emit = None
        self._run_on_browser_thread = None

    def _handle_command(self, command: dict) -> None:
        self.command_threads.append(threading.get_ident())

    def close(self) -> None:
        self.close_threads.append(threading.get_ident())


def _service_with_fake() -> tuple[BrowserService, _FakeDaemon]:
    """Return a service whose daemon build is replaced by a shared fake."""
    service = BrowserService()
    fake = _FakeDaemon()
    # Patch the daemon factory so no Selenium/Playwright import happens and the
    # same fake is inspected after dispatch.
    service._build_daemon = lambda backend: fake  # type: ignore[assignment]
    return service, fake


def test_commands_run_on_single_owner_thread_not_caller() -> None:
    service, fake = _service_with_fake()
    caller_thread = threading.get_ident()

    service.dispatch({"command": "open_url"}, progress_emitter=None, cancel_event=None)
    service.dispatch({"command": "start_intercept"}, progress_emitter=None, cancel_event=None)

    assert len(fake.command_threads) == 2
    owner = fake.command_threads[0]
    # Never the dispatcher/caller thread.
    assert owner != caller_thread
    # Same owner thread reused across commands (greenlet affinity preserved).
    assert fake.command_threads[1] == owner


def test_hook_marshals_background_work_onto_owner_thread() -> None:
    service, fake = _service_with_fake()
    service.dispatch({"command": "open_url"}, progress_emitter=None, cancel_event=None)
    owner = fake.command_threads[0]

    # Simulate the daemon's background link-collect loop (its own worker thread)
    # marshalling a Playwright call through the injected hook: it must run on the
    # owner thread, not the loop thread.
    seen: dict[str, int] = {}

    def loop_worker() -> None:
        seen["loop"] = threading.get_ident()
        seen["ran_on"] = fake._run_on_browser_thread(threading.get_ident)

    t = threading.Thread(target=loop_worker)
    t.start()
    t.join(timeout=5.0)

    assert not t.is_alive()
    assert seen["ran_on"] == owner
    assert seen["loop"] != owner


def test_close_runs_on_owner_thread() -> None:
    service, fake = _service_with_fake()

    service.dispatch({"command": "open_url"}, progress_emitter=None, cancel_event=None)
    owner = fake.command_threads[0]

    service.close()

    assert fake.close_threads == [owner]


def test_hook_injected_and_bound_to_service() -> None:
    service, fake = _service_with_fake()

    service.dispatch({"command": "open_url"}, progress_emitter=None, cancel_event=None)

    # The service redirected the daemon's emitter and pinned its browser-thread hook.
    assert fake._emit == service._sink
    assert fake._run_on_browser_thread == service._run_on_browser_thread


class _RecordingHandler(logging.Handler):
    """Collects `(levelno, message)` of every record the service logger emits."""

    def __init__(self) -> None:
        super().__init__(level=logging.DEBUG)
        self.records: list[tuple[int, str]] = []

    def emit(self, record: logging.LogRecord) -> None:
        self.records.append((record.levelno, record.getMessage()))


def _dispatch_emitting(events: list[dict]) -> tuple[_RecordingHandler, object]:
    """Dispatch one command whose fake daemon emits `events`; return the log and the outcome."""
    service, fake = _service_with_fake()

    def handle(_command: dict) -> None:
        for event in events:
            fake._emit(event)

    fake._handle_command = handle  # type: ignore[method-assign]
    handler = _RecordingHandler()
    logger = service_module.LOG
    previous_level = logger.level
    logger.addHandler(handler)
    logger.setLevel(logging.DEBUG)
    try:
        try:
            outcome: object = service.dispatch({"command": "fetch"}, progress_emitter=None, cancel_event=None)
        except RuntimeError as exc:
            outcome = exc
    finally:
        logger.removeHandler(handler)
        logger.setLevel(previous_level)
        service.close()
    return handler, outcome


def test_log_events_reach_the_python_logger_with_their_level() -> None:
    handler, outcome = _dispatch_emitting(
        [
            {"event": "log", "level": "warn", "message": "web page console warning: x"},
            {"event": "log", "level": "error", "message": "web page error: y"},
            {"event": "log", "level": "odd", "message": "z"},
            {"event": "result", "count": 1},
        ]
    )
    assert handler.records == [
        (logging.WARNING, "browser selenium: web page console warning: x"),
        (logging.ERROR, "browser selenium: web page error: y"),
        (logging.INFO, "browser selenium: z"),
    ]
    # A log event is never mistaken for the terminal result.
    assert outcome == {"event": "result", "count": 1}


def test_error_event_logs_its_log_message_and_raises_the_user_message() -> None:
    handler, outcome = _dispatch_emitting(
        [{"event": "error", "user_message": "Не удалось.", "log_message": "driver crashed: boom"}]
    )
    assert isinstance(outcome, RuntimeError)
    assert str(outcome) == "Не удалось."
    assert len(handler.records) == 1
    level, message = handler.records[0]
    assert level == logging.ERROR
    assert "driver crashed: boom" in message


if __name__ == "__main__":
    test_commands_run_on_single_owner_thread_not_caller()
    test_hook_marshals_background_work_onto_owner_thread()
    test_close_runs_on_owner_thread()
    test_hook_injected_and_bound_to_service()
    test_log_events_reach_the_python_logger_with_their_level()
    test_error_event_logs_its_log_message_and_raises_the_user_message()
    print("all browser_service tests passed")
