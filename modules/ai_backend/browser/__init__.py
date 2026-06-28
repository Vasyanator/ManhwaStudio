"""
Package: modules/ai_backend/browser

Hosts the advanced web-scraping browser session (Selenium / CloakBrowser) inside
the unified AI backend process. Previously these ran as separate stdio daemons
(`modules/new_project/adv_fetch_cli.py` and `adv_fetch_cloak_cli.py`); they are
now driven in-process through `BrowserService` and served over the framed IPC
protocol (method ``browser.command``).
"""

from .service import BACKEND_CLOAK, BACKEND_SELENIUM, BrowserService

__all__ = ["BrowserService", "BACKEND_SELENIUM", "BACKEND_CLOAK"]
