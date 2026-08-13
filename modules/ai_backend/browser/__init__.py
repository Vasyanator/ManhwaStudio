"""
Package: modules/ai_backend/browser

Hosts the advanced web-scraping browser session (Selenium / CloakBrowser) inside
the AI backend process: `BrowserService` drives the daemon classes from
`modules/new_project/` (`adv_fetch_cli.py`, `adv_fetch_cloak_cli.py`) in-process
and serves them over the framed IPC protocol under the single method
``browser.command``. See `MODULE_README.md` for the thread-affinity contract.

Unlike the other sub-packages of `modules/ai_backend`, this one re-exports its
service. That is safe only because `service.py` imports Selenium/Playwright
lazily; adding an eager heavy import there means removing this re-export first.
"""

from .service import BACKEND_CLOAK, BACKEND_SELENIUM, BrowserService

__all__ = ["BrowserService", "BACKEND_SELENIUM", "BACKEND_CLOAK"]
