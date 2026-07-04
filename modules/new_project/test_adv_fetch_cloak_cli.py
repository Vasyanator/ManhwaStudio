"""
File: modules/new_project/test_adv_fetch_cloak_cli.py

Purpose:
Unit tests for `CloakFetchDaemon`'s active-tab resolution
(`modules/new_project/adv_fetch_cloak_cli.py`).

Main responsibilities:
- `_resolve_active_page` picks the tab with the newest activation timestamp from the
  injected monitor, regardless of tab order — no first-tab / tracked-tab / open-order
  bias;
- ties on timestamp are broken by current visibility;
- a single live real-URL tab is used as-is (no monitor read needed);
- a chosen tab that is already closed triggers a live re-resolve;
- `_active_page_url` raises the standard error when the active tab is blank.

Notes:
Fake pages record `evaluate`/`bring_to_front` and never touch a real browser. `_valid_pages`
is patched to return them, bypassing `_ensure_browser`. The tests avoid pytest fixtures and
expose a `__main__` runner so they pass under both `pytest` and a plain `python3` invocation.
"""

from __future__ import annotations

from typing import Any, Optional

from modules.new_project.adv_fetch_cloak_cli import CloakFetchDaemon


class _FakePage:
    """Minimal Playwright-page stand-in for active-tab resolution tests."""

    def __init__(self, url: str, active_ts: float, visible: bool = False, closed: bool = False) -> None:
        self.url = url
        self._active_ts = active_ts
        self._visible = visible
        self._closed = closed
        self.brought_to_front = 0

    def is_closed(self) -> bool:
        return self._closed

    def evaluate(self, _js: str) -> Any:
        # Only ACTIVE_MONITOR_READ_JS is evaluated across candidate tabs.
        return {"a": self._active_ts, "vis": self._visible}

    def bring_to_front(self) -> None:
        self.brought_to_front += 1

    def on(self, *_args: Any, **_kwargs: Any) -> None:
        pass


def _daemon_with_pages(page_batches: list[list[_FakePage]]) -> CloakFetchDaemon:
    """Build a daemon whose `_valid_pages` yields successive batches per call."""
    daemon = CloakFetchDaemon()
    calls = {"i": 0}

    def fake_valid_pages() -> list[Any]:
        idx = min(calls["i"], len(page_batches) - 1)
        calls["i"] += 1
        return list(page_batches[idx])

    daemon._valid_pages = fake_valid_pages  # type: ignore[assignment]
    return daemon


def test_picks_newest_activation_regardless_of_order() -> None:
    a = _FakePage("https://site/a", active_ts=100.0)
    b = _FakePage("https://site/b", active_ts=300.0)  # most recently activated
    c = _FakePage("https://site/c", active_ts=200.0)
    daemon = _daemon_with_pages([[a, c, b]])  # order deliberately not by timestamp

    chosen = daemon._resolve_active_page("test")

    assert chosen is b
    assert daemon._page is b
    assert b.brought_to_front == 1


def test_visibility_breaks_timestamp_tie() -> None:
    a = _FakePage("https://site/a", active_ts=0.0, visible=False)
    b = _FakePage("https://site/b", active_ts=0.0, visible=True)  # tie on ts, but visible
    daemon = _daemon_with_pages([[a, b]])

    chosen = daemon._resolve_active_page("test")

    assert chosen is b


def test_single_tab_used_as_is() -> None:
    only = _FakePage("https://site/only", active_ts=0.0)
    daemon = _daemon_with_pages([[only]])

    chosen = daemon._resolve_active_page("test")

    assert chosen is only


def test_closed_chosen_tab_triggers_reresolve() -> None:
    dead = _FakePage("https://site/dead", active_ts=999.0, closed=True)
    live = _FakePage("https://site/live", active_ts=10.0)
    # First batch: only the (single) dead tab -> chosen but is_closed -> re-resolve.
    # Second batch: a live tab -> returned.
    daemon = _daemon_with_pages([[dead], [live]])

    chosen = daemon._resolve_active_page("test")

    assert chosen is live


def test_active_page_url_rejects_blank() -> None:
    # A resolver that yields a blank-URL page (filtered out -> no valid -> _require_page).
    daemon = CloakFetchDaemon()
    blank = _FakePage("about:blank", active_ts=0.0)
    daemon._resolve_active_page = lambda reason: blank  # type: ignore[assignment]

    raised: Optional[Exception] = None
    try:
        daemon._active_page_url("test")
    except RuntimeError as exc:
        raised = exc

    assert raised is not None
    assert "CloakBrowser" in str(raised)


if __name__ == "__main__":
    test_picks_newest_activation_regardless_of_order()
    test_visibility_breaks_timestamp_tie()
    test_single_tab_used_as_is()
    test_closed_chosen_tab_triggers_reresolve()
    test_active_page_url_rejects_blank()
    print("all active-tab resolution tests passed")
