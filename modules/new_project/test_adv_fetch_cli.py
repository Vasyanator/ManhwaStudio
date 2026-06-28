"""
FILE OVERVIEW: modules/new_project/test_adv_fetch_cli.py
Focused tests for the advanced Selenium downloader helpers.

Main items:
- verifies browser-scoped URL routing without requiring a real Selenium driver.
- verifies image-byte decoding for the browser-context path.
- verifies self-closing new-tab candidates fail fast.
"""

from __future__ import annotations

from io import BytesIO
from pathlib import Path
import sys
import types
from typing import Optional
from unittest import TestCase, main
from unittest.mock import patch

from PIL import Image

PROJECT_ROOT = Path(__file__).resolve().parents[2]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

new_project_package = types.ModuleType("modules.new_project")
new_project_package.__path__ = [str(Path(__file__).resolve().parent)]  # type: ignore[attr-defined]
sys.modules.setdefault("modules.new_project", new_project_package)

from modules.new_project.adv_fetch_cli import (
    AdvancedFetchDaemon,
    NonImagePayloadError,
    SelfClosingNewTabError,
    _new_tab_direct_navigation_allowed,
)


def _png_bytes() -> bytes:
    buffer = BytesIO()
    Image.new("RGB", (2, 3), (255, 0, 0)).save(buffer, format="PNG")
    return buffer.getvalue()


class BrowserDownloadDaemon(AdvancedFetchDaemon):
    def __init__(self, image_bytes: bytes) -> None:
        super().__init__()
        self.image_bytes = image_bytes

    def _download_bytes_via_browser(self, link: str) -> Optional[bytes]:
        if link.startswith(("blob:", "https://")):
            return self.image_bytes
        return None

    def _download_candidates_with_requests_parallel(
        self,
        indexed_links: list[tuple[int, str]],
        referer: str,
        max_parallel: int,
        total: int,
    ) -> dict[int, Optional[Image.Image]]:
        return {index: None for index, _link in indexed_links}

    def _emit_progress(self, stage: str, current: int, total: int) -> None:
        return


class CurrentTabHtmlDaemon(AdvancedFetchDaemon):
    def _download_image_with_current_tab(self, link: str) -> Image.Image:
        raise NonImagePayloadError(f"Downloaded content does not look like an image: {link}")

    def _download_image_with_new_tab(self, link: str) -> Image.Image:
        raise AssertionError(f"new-tab should not be tried for non-image browser payload: {link}")


class _FakeSwitchTo:
    def window(self, handle: str) -> None:
        return


class _SelfClosingTabDriver:
    current_window_handle = "main"
    switch_to = _FakeSwitchTo()

    def __init__(self) -> None:
        self.open_attempts = 0

    @property
    def window_handles(self) -> list[str]:
        return ["main"]

    def execute_script(self, _script: str, _link: str) -> bool:
        self.open_attempts += 1
        return True


class AdvancedFetchCliTests(TestCase):
    def test_downloads_use_browser_context_for_all_links(self) -> None:
        daemon = BrowserDownloadDaemon(_png_bytes())

        with (
            patch("time.sleep", return_value=None),
            patch("modules.new_project.adv_fetch_cli.VERBOSE_DOWNLOAD_LOG", False),
        ):
            results = daemon._download_candidate_links_parallel(
                [
                    "https://example.test/image.png",
                    "blob:https://example.test/image-id",
                ],
                "https://example.test/chapter",
                max_parallel=8,
            )

        self.assertEqual(len(results), 2)
        self.assertTrue(all(result is not None for result in results))
        for result in results:
            assert result is not None
            self.assertEqual(result.size, (2, 3))

    def test_new_tab_candidate_is_skipped_when_tab_does_not_stay_open(self) -> None:
        daemon = AdvancedFetchDaemon()
        driver = _SelfClosingTabDriver()
        daemon._driver = driver

        with (
            patch("time.sleep", return_value=None),
            patch("modules.new_project.adv_fetch_cli.NEW_TAB_OPEN_POLL_SECONDS", 0.0),
        ):
            with self.assertRaises(SelfClosingNewTabError):
                daemon._download_image_with_new_tab("blob:https://example.test/self-closing")

        self.assertEqual(driver.open_attempts, 1)

    def test_new_tab_candidate_is_not_opened_more_than_three_times_per_link(self) -> None:
        daemon = AdvancedFetchDaemon()
        driver = _SelfClosingTabDriver()
        daemon._driver = driver
        link = "blob:https://example.test/self-closing"

        with (
            patch("time.sleep", return_value=None),
            patch("modules.new_project.adv_fetch_cli.NEW_TAB_OPEN_POLL_SECONDS", 0.0),
        ):
            for _ in range(3):
                with self.assertRaises(SelfClosingNewTabError):
                    daemon._download_image_with_new_tab(link)
            with self.assertRaisesRegex(RuntimeError, "attempt limit"):
                daemon._download_image_with_new_tab(link)

        self.assertEqual(driver.open_attempts, 3)

    def test_new_tab_attempt_limit_uses_decoded_url_key(self) -> None:
        daemon = AdvancedFetchDaemon()
        driver = _SelfClosingTabDriver()
        daemon._driver = driver
        encoded = "blob:https://rawotaku.com/read/%E5%88%97%E5%BC%B7/ja/chapter-2-raw/"
        decoded = "blob:https://rawotaku.com/read/列強/ja/chapter-2-raw/"

        with (
            patch("time.sleep", return_value=None),
            patch("modules.new_project.adv_fetch_cli.NEW_TAB_OPEN_POLL_SECONDS", 0.0),
        ):
            for link in (encoded, decoded, encoded):
                with self.assertRaises(SelfClosingNewTabError):
                    daemon._download_image_with_new_tab(link)
            with self.assertRaisesRegex(RuntimeError, "attempt limit"):
                daemon._download_image_with_new_tab(decoded)

        self.assertEqual(driver.open_attempts, 3)

    def test_new_tab_direct_navigation_is_disabled_for_http_urls(self) -> None:
        self.assertFalse(_new_tab_direct_navigation_allowed("https://example.test/page.jpg"))
        self.assertFalse(_new_tab_direct_navigation_allowed("http://example.test/page.jpg"))
        self.assertTrue(_new_tab_direct_navigation_allowed("blob:https://example.test/page-id"))
        self.assertTrue(_new_tab_direct_navigation_allowed("data:image/png;base64,AA=="))

    def test_new_tab_is_not_tried_after_current_tab_returns_non_image_bytes(self) -> None:
        daemon = CurrentTabHtmlDaemon()

        with patch("modules.new_project.adv_fetch_cli.VERBOSE_DOWNLOAD_LOG", False):
            with self.assertRaisesRegex(RuntimeError, "current-tab"):
                daemon._download_image_with_strategy(
                    "https://rawotaku.com/read/列強戦線/ja/chapter-2-raw/",
                    "https://rawotaku.com/read/列強戦線/ja/chapter-4-raw/",
                    already_tried={"requests"},
                )

    def test_auto_prefilter_skips_site_code_and_bare_origins(self) -> None:
        daemon = AdvancedFetchDaemon()

        with patch("modules.new_project.adv_fetch_cli.VERBOSE_DOWNLOAD_LOG", False):
            filtered = daemon._filter_explicit_site_code_links(
                [
                    "https://example.test/assets/app.js",
                    "https://fonts.gstatic.com",
                    "https://cdnjs.cloudflare.com/",
                    "https://cdn.example.test/images/page",
                    "https://cdn.example.test/images/page",
                ]
            )

        self.assertEqual(filtered, ["https://cdn.example.test/images/page"])


if __name__ == "__main__":
    main()
