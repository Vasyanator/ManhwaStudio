"""
FILE OVERVIEW: modules/new_project/test_adv_fetch_cli.py
Focused tests for the advanced Selenium downloader helpers.

Main items:
- verifies browser-scoped URL routing without requiring a real Selenium driver.
- verifies image-byte decoding for the browser-context path.
"""

from __future__ import annotations

from io import BytesIO
from typing import Optional
from unittest import TestCase, main
from unittest.mock import patch

from PIL import Image

from modules.new_project.adv_fetch_cli import AdvancedFetchDaemon


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

    def _emit_progress(self, stage: str, current: int, total: int) -> None:
        return


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


if __name__ == "__main__":
    main()
