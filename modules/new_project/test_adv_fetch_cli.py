"""
File: modules/new_project/test_adv_fetch_cli.py

Purpose:
Browser-free unit tests for the Selenium daemon `AdvancedFetchDaemon`
(`modules/new_project/adv_fetch_cli.py`), covering the browser-session lifecycle only.

Main responsibilities:
- `_is_dead_session` separates a dead WebDriver session (closed browser, dead
  chromedriver, vanished window) from an ordinary navigation failure, including through
  a `raise ... from exc` chain, because `_wait_for_page_ready` re-raises a dead driver as
  a friendly `RuntimeError`;
- `open_url` drops the driver and retries the navigation exactly once when the session
  died between the liveness probe and `driver.get` (the check-then-act race);
- every other navigation error propagates with no retry and no relaunch, so a mistyped
  link never costs the user their browser session;
- `_debug_log` / `_warn_log` write each line exactly once: through `LOG` when hosted
  in-process, as a stdout JSON `log` event in the `--daemon` fallback.

Notes:
No browser and no chromedriver are involved: `build_browser` / `cleanup_browser_runtime`
are swapped for fakes on the module object, and a fake driver stands in for WebDriver.
Unlike the CloakBrowser daemon, the Selenium liveness probe (`driver.current_url`) was
already a real round-trip, so there is no staleness bug to cover here — only the race.
The tests avoid pytest fixtures and expose a `__main__` runner, matching
`test_adv_fetch_cloak_cli.py`, so they pass under both `pytest` and a plain `python3` run.
"""

from __future__ import annotations

import io
import json
import logging
import sys
from contextlib import contextmanager
from typing import Any, Iterator, Optional

from selenium.common.exceptions import (
    InvalidSessionIdException,
    NoSuchWindowException,
    WebDriverException,
)

from modules.new_project import adv_fetch_cli
from modules.new_project import common as common_module
from modules.new_project.adv_fetch_cli import AdvancedFetchDaemon, _is_dead_session


class _FakeDriver:
    """Minimal WebDriver stand-in for the `open_url` lifecycle tests.

    `window_handles` is deliberately empty so `_sync_active_browser_tab` early-returns
    without touching `switch_to`; `execute_script` answers the two readiness probes of
    `_wait_for_page_ready` and swallows the stealth script.
    """

    def __init__(self, *, get_errors: Optional[list[Exception]] = None) -> None:
        self.current_url = "about:blank"
        self.window_handles: list[str] = []
        self.current_window_handle = "w1"
        self.get_calls: list[str] = []
        self.quit_calls = 0
        self._get_errors = list(get_errors or [])

    def get(self, url: str) -> None:
        self.get_calls.append(url)
        if self._get_errors:
            raise self._get_errors.pop(0)
        self.current_url = url

    def execute_script(self, script: str, *_args: Any) -> Any:
        if "readyState" in script:
            return "complete"
        if "document.body" in script:
            return True
        return None

    def set_window_size(self, _width: int, _height: int) -> None:
        pass

    def quit(self) -> None:
        self.quit_calls += 1


@contextmanager
def _stub_browser_launch(drivers: list[_FakeDriver]) -> Iterator[list[_FakeDriver]]:
    """Serve `drivers` from `build_browser` and neutralise profile cleanup.

    Restores both module attributes on exit. Raises if the daemon asks for more browsers
    than the test provided, which is how "relaunched more than once" is caught.
    """
    launched: list[_FakeDriver] = []
    pending = list(drivers)

    def fake_build_browser(_headful: bool, _browser: str) -> tuple[_FakeDriver, str]:
        if not pending:
            raise AssertionError("the daemon launched more browsers than the test allows")
        driver = pending.pop(0)
        launched.append(driver)
        return driver, ""

    original_build = adv_fetch_cli.build_browser
    original_cleanup = adv_fetch_cli.cleanup_browser_runtime
    adv_fetch_cli.build_browser = fake_build_browser  # type: ignore[assignment]
    adv_fetch_cli.cleanup_browser_runtime = lambda *_args, **_kwargs: None  # type: ignore[assignment]
    try:
        yield launched
    finally:
        adv_fetch_cli.build_browser = original_build  # type: ignore[assignment]
        adv_fetch_cli.cleanup_browser_runtime = original_cleanup  # type: ignore[assignment]


def test_dead_session_is_distinguished_from_a_navigation_failure() -> None:
    assert _is_dead_session(InvalidSessionIdException("invalid session id")) is True
    assert _is_dead_session(NoSuchWindowException("no such window: target window closed")) is True
    assert _is_dead_session(WebDriverException("disconnected: not connected to DevTools")) is True
    # A page that simply failed to load is NOT a dead session: relaunching the browser
    # for a mistyped link would throw away the user's session.
    assert _is_dead_session(WebDriverException("unknown error: net::ERR_NAME_NOT_RESOLVED")) is False
    assert _is_dead_session(RuntimeError("Страница в браузере не успела загрузиться полностью.")) is False


def test_dead_session_is_found_through_the_cause_chain() -> None:
    # `_wait_for_page_ready` wraps whatever went wrong in a friendly RuntimeError.
    try:
        try:
            raise InvalidSessionIdException("invalid session id")
        except InvalidSessionIdException as exc:
            raise RuntimeError("Страница в браузере не успела загрузиться полностью.") from exc
    except RuntimeError as wrapped:
        assert _is_dead_session(wrapped) is True


def test_open_url_relaunches_and_retries_once_on_a_dead_session() -> None:
    # The browser dies between the `driver.current_url` liveness probe and `driver.get`.
    doomed = _FakeDriver(get_errors=[InvalidSessionIdException("invalid session id")])
    fresh = _FakeDriver()
    daemon = AdvancedFetchDaemon()
    daemon._driver = doomed
    daemon._browser_name = "chrome"

    with _stub_browser_launch([fresh]) as launched:
        result = daemon.open_url("chrome", "https://site/new")

    assert result == "https://site/new"
    assert doomed.get_calls == ["https://site/new"]  # one failed attempt
    assert doomed.quit_calls == 1  # the dead driver was really disposed
    assert launched == [fresh]  # exactly one relaunch
    assert fresh.get_calls == ["https://site/new"]  # exactly ONE retry
    assert daemon._driver is fresh


def test_open_url_propagates_other_errors_without_retry() -> None:
    boom = WebDriverException("unknown error: net::ERR_NAME_NOT_RESOLVED")
    driver = _FakeDriver(get_errors=[boom])
    daemon = AdvancedFetchDaemon()
    daemon._driver = driver
    daemon._browser_name = "chrome"

    raised: Optional[Exception] = None
    # No driver is offered: a relaunch attempt would fail the test loudly.
    with _stub_browser_launch([]):
        try:
            daemon.open_url("chrome", "https://bad/link")
        except WebDriverException as exc:
            raised = exc

    assert raised is boom
    assert driver.get_calls == ["https://bad/link"]  # no retry
    assert driver.quit_calls == 0  # no teardown
    assert daemon._driver is driver


class _LogRecorder(logging.Handler):
    """Collects `(levelno, message)` of every record `adv_fetch_cli.LOG` emits."""

    def __init__(self) -> None:
        super().__init__(level=logging.DEBUG)
        self.records: list[tuple[int, str]] = []

    def emit(self, record: logging.LogRecord) -> None:
        self.records.append((record.levelno, record.getMessage()))


def _capture_diagnostics(daemon_mode: bool) -> tuple[list[tuple[int, str]], str]:
    """Run one `_debug_log` and one `_warn_log`; return the log records and stdout text."""
    recorder = _LogRecorder()
    logger = adv_fetch_cli.LOG
    previous_level = logger.level
    previous_mode = common_module._daemon_stdout_log
    previous_stdout = sys.stdout
    stdout = io.StringIO()
    logger.addHandler(recorder)
    logger.setLevel(logging.DEBUG)
    common_module._daemon_stdout_log = daemon_mode
    sys.stdout = stdout
    try:
        adv_fetch_cli._debug_log("download: selected %s method", "requests")
        adv_fetch_cli._warn_log("selenium session: %s", "relaunching")
    finally:
        sys.stdout = previous_stdout
        common_module._daemon_stdout_log = previous_mode
        logger.removeHandler(recorder)
        logger.setLevel(previous_level)
    return recorder.records, stdout.getvalue()


def test_in_process_diagnostics_go_through_the_logger_only() -> None:
    records, stdout = _capture_diagnostics(daemon_mode=False)
    assert records == [
        (logging.INFO, "download: selected requests method"),
        (logging.WARNING, "selenium session: relaunching"),
    ]
    assert stdout == ""


def test_daemon_diagnostics_go_to_stdout_json_only() -> None:
    records, stdout = _capture_diagnostics(daemon_mode=True)
    assert records == []
    assert [json.loads(line) for line in stdout.splitlines()] == [
        {"event": "log", "level": "info", "message": "download: selected requests method"},
        {"event": "log", "level": "warn", "message": "selenium session: relaunching"},
    ]


if __name__ == "__main__":
    test_dead_session_is_distinguished_from_a_navigation_failure()
    test_dead_session_is_found_through_the_cause_chain()
    test_open_url_relaunches_and_retries_once_on_a_dead_session()
    test_open_url_propagates_other_errors_without_retry()
    test_in_process_diagnostics_go_through_the_logger_only()
    test_daemon_diagnostics_go_to_stdout_json_only()
    print("all Selenium browser-session lifecycle and diagnostics tests passed")
