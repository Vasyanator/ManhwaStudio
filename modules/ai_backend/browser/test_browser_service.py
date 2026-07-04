"""
File: modules/ai_backend/browser/test_browser_service.py

Purpose:
Unit tests for `BrowserService`'s thread-affinity contract
(`modules/ai_backend/browser/service.py`).

Main responsibilities:
- every browser command (`_handle_command`) runs on ONE dedicated owner thread,
  never the calling (dispatcher pool) thread;
- that owner thread is reused across sequential commands (so Playwright's
  greenlet, bound to the thread that launched the browser, is never switched);
- `close()` tears the daemon down on the same owner thread;
- the daemon receives the `_run_on_browser_thread` hook, and calling it marshals
  work onto the owner thread (so the daemon's background loop can pin its
  Playwright calls too).

Notes:
A fake daemon records `threading.get_ident()` for each call, so the tests need no
real browser. The tests avoid pytest fixtures and expose a `__main__` runner, so
they pass under both `pytest` and a plain `python3` invocation.
"""

from __future__ import annotations

import threading

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


if __name__ == "__main__":
    test_commands_run_on_single_owner_thread_not_caller()
    test_hook_marshals_background_work_onto_owner_thread()
    test_close_runs_on_owner_thread()
    test_hook_injected_and_bound_to_service()
    print("all browser_service thread-affinity tests passed")
