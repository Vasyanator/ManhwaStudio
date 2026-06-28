"""
FILE OVERVIEW: modules/new_project/adv_fetch_cli.py
Python daemon for the advanced Selenium-based downloader used by the Rust launcher.

Main items:
- `AdvancedFetchDaemon`: owns the Selenium driver lifecycle and persistent browser profiles.
- `open_url` command: opens a page in the selected browser/profile.
- `fetch` command: collects image candidates from the active page, transfers cookies to
  the active browser tab, downloads images, and stores them in a temporary folder for Rust.
- `fetch_canvas` / `start_intercept` / `stop_intercept`: collect current canvas snapshots or
  run a background canvas capture loop that tracks new unique canvas frames.

Protocol:
- `_handle_command({"command": ...})` performs one command and reports a single
  terminal event (plus interim `progress` events) through `self._emit`.
- This logic is now driven in-process by the unified AI backend via
  `modules/ai_backend/browser/service.py` (`BrowserService`) over the framed IPC
  method `browser.command`; `BrowserService` redirects `_emit` to stream progress
  and capture the terminal event.
- `--daemon` / `main()` keeps the original line-JSON-over-stdio loop as a manual
  debugging fallback; it is no longer launched by the Rust launcher.
"""

from __future__ import annotations

import argparse
import base64
from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import json
import logging
import re
import shutil
import sys
import tempfile
import threading
import traceback
from dataclasses import dataclass
from io import BytesIO
from pathlib import Path
from typing import Any, Optional
from urllib.parse import quote, unquote, urljoin, urlparse, urlunparse

import requests
from PIL import Image
from selenium.common.exceptions import WebDriverException
from selenium.webdriver.common.by import By
from selenium.webdriver.support import expected_conditions as ec
from selenium.webdriver.support.ui import WebDriverWait

PROJECT_ROOT = Path(__file__).resolve().parents[2]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from modules.browser_f import (  # noqa: E402
    build_browser,
    browserlike_headers,
    cleanup_browser_runtime,
    get_origin,
)
from modules.new_project.common import compile_wildcard_prefixes  # noqa: E402
from config import VERSION  # noqa: E402

import time
import random

LOG = logging.getLogger(__name__)
CONTROL_TRANSLATION = {code: None for code in range(0x00, 0x20)} | {0x7F: None}
VERBOSE_DOWNLOAD_LOG = True
EMIT_LOCK = threading.Lock()
DOWNLOAD_METHOD_REQUESTS = "requests"
DOWNLOAD_METHOD_CURRENT_TAB = "current-tab"
DOWNLOAD_METHOD_NEW_TAB = "new-tab"
NEW_TAB_MAX_ATTEMPTS_PER_LINK = 3
NEW_TAB_OPEN_POLL_SECONDS = 1.0
NEW_TAB_OPEN_POLL_INTERVAL_SECONDS = 0.05
NEW_TAB_IMAGE_WAIT_SECONDS = 5.0
BROWSER_FETCH_TIMEOUT_MS = 8000


class SelfClosingNewTabError(RuntimeError):
    """Raised when a candidate opens a browser tab that closes before it can be inspected."""


class NonImagePayloadError(RuntimeError):
    """Raised when a candidate URL returns bytes that are definitely not an image."""


@dataclass
class FetchResult:
    page_url: str
    output_dir: Path
    downloaded_images: int


@dataclass
class CanvasCaptureState:
    stop_event: threading.Event
    lock: threading.Lock
    entries: list[dict[str, Any]]
    hashes: set[str]
    worker: threading.Thread
    page_url: str
    window_handle: str
    error_message: Optional[str] = None
    log_message: Optional[str] = None


@dataclass
class LinkCollectState:
    stop_event: threading.Event
    lock: threading.Lock
    links: list[str]
    seen_links: set[str]
    worker: threading.Thread
    page_url: str
    window_handle: str
    pattern: str
    max_parallel: int
    exclude_site_code_links: bool
    error_message: Optional[str] = None
    log_message: Optional[str] = None


class AdvancedFetchDaemon:
    def __init__(self) -> None:
        self._driver = None
        self._browser_name: Optional[str] = None
        self._current_window_handle: Optional[str] = None
        self._tmp_profile_dir: Optional[str] = None
        self._intercept_active = False
        self._canvas_capture: Optional[CanvasCaptureState] = None
        self._link_collect_active = False
        self._link_collect: Optional[LinkCollectState] = None
        self._preferred_download_method: Optional[str] = None
        self._new_tab_attempts_by_link: dict[str, int] = {}

    def run(self) -> int:
        self._emit({"event": "ready", "downloader_version": VERSION})
        for raw_line in sys.stdin:
            line = raw_line.strip()
            if not line:
                continue
            try:
                command = json.loads(line)
                self._handle_command(command)
            except SystemExit:
                raise
            except Exception as exc:  # noqa: BLE001
                self._emit_error(
                    user_message=str(exc) or "Продвинутый выкачиватель завершился с ошибкой.",
                    log_message=f"unexpected daemon error: {type(exc).__name__}: {exc}",
                )
                LOG.exception("Unexpected daemon error")
        self.close()
        return 0

    def close(self) -> None:
        self._stop_link_collect()
        self._stop_canvas_capture()
        if self._driver is not None:
            try:
                self._driver.quit()
            except Exception:  # noqa: BLE001
                LOG.exception("Failed to quit Selenium driver")
            self._driver = None
        if self._tmp_profile_dir:
            try:
                cleanup_browser_runtime(self._browser_name or "", self._tmp_profile_dir)
            except Exception:  # noqa: BLE001
                LOG.exception("Failed to remove temp profile dir")
            self._tmp_profile_dir = None
        self._browser_name = None
        self._current_window_handle = None
        self._intercept_active = False
        self._link_collect_active = False

    def _handle_command(self, command: dict) -> None:
        command_name = str(command.get("command") or "").strip()
        if command_name == "shutdown":
            self.close()
            raise SystemExit(0)
        if command_name == "open_url":
            browser = str(command.get("browser") or "").strip()
            url = _normalize_http_url(str(command.get("url") or ""))
            current_url = self.open_url(browser, url)
            self._emit({"event": "opened", "current_url": current_url})
            return
        if command_name == "fetch":
            browser = str(command.get("browser") or "").strip()
            pattern = str(command.get("pattern") or "").strip()
            max_parallel = int(command.get("max_parallel") or 1)
            result = self.fetch(browser, pattern, max_parallel)
            self._emit(
                {
                    "event": "result",
                    "page_url": result.page_url,
                    "output_dir": str(result.output_dir),
                    "downloaded_images": result.downloaded_images,
                }
            )
            return
        if command_name == "fetch_auto_links":
            browser = str(command.get("browser") or "").strip()
            max_parallel = int(command.get("max_parallel") or 1)
            cancel_file = _optional_cancel_file(command.get("cancel_file"))
            result = self.fetch_auto_links(browser, max_parallel, cancel_file)
            self._emit(
                {
                    "event": "auto_result",
                    "page_url": result["page_url"],
                    "output_dir": result["output_dir"],
                    "downloaded_images": result["downloaded_images"],
                    "items": result["items"],
                }
            )
            return
        if command_name == "start_link_collect":
            browser = str(command.get("browser") or "").strip()
            pattern = str(command.get("pattern") or "").strip()
            max_parallel = int(command.get("max_parallel") or 1)
            current_url = self.start_link_collect(browser, pattern, max_parallel)
            self._emit({"event": "link_collect_started", "current_url": current_url})
            return
        if command_name == "start_auto_link_collect":
            browser = str(command.get("browser") or "").strip()
            max_parallel = int(command.get("max_parallel") or 1)
            current_url = self.start_auto_link_collect(browser, max_parallel)
            self._emit({"event": "link_collect_started", "current_url": current_url})
            return
        if command_name == "stop_link_collect":
            browser = str(command.get("browser") or "").strip()
            result = self.stop_link_collect(browser)
            self._emit(
                {
                    "event": "result",
                    "page_url": result.page_url,
                    "output_dir": str(result.output_dir),
                    "downloaded_images": result.downloaded_images,
                }
            )
            return
        if command_name == "stop_auto_link_collect":
            browser = str(command.get("browser") or "").strip()
            cancel_file = _optional_cancel_file(command.get("cancel_file"))
            result = self.stop_auto_link_collect(browser, cancel_file)
            self._emit(
                {
                    "event": "auto_result",
                    "page_url": result["page_url"],
                    "output_dir": result["output_dir"],
                    "downloaded_images": result["downloaded_images"],
                    "items": result["items"],
                }
            )
            return
        if command_name == "link_collect_status":
            browser = str(command.get("browser") or "").strip()
            found_links = self.link_collect_status(browser)
            self._emit({"event": "link_collect_count", "found_links": found_links})
            return
        if command_name == "fetch_canvas":
            browser = str(command.get("browser") or "").strip()
            result = self.fetch_canvas(browser)
            self._emit(
                {
                    "event": "result",
                    "page_url": result.page_url,
                    "output_dir": str(result.output_dir),
                    "downloaded_images": result.downloaded_images,
                }
            )
            return
        if command_name == "start_intercept":
            browser = str(command.get("browser") or "").strip()
            current_url = self.start_intercept(browser)
            self._emit({"event": "intercept_started", "current_url": current_url})
            return
        if command_name == "stop_intercept":
            browser = str(command.get("browser") or "").strip()
            result = self.stop_intercept(browser)
            self._emit(
                {
                    "event": "result",
                    "page_url": result.page_url,
                    "output_dir": str(result.output_dir),
                    "downloaded_images": result.downloaded_images,
                }
            )
            return
        if command_name == "intercept_status":
            browser = str(command.get("browser") or "").strip()
            found_pages = self.intercept_status(browser)
            self._emit({"event": "intercept_count", "found_pages": found_pages})
            return
        if command_name == "scroll_page":
            self.scroll_page()
            self._emit({"event": "scrolled"})
            return
        raise RuntimeError(f"Unknown command: {command_name}")

    def scroll_page(self) -> None:
        """Scroll the current browser page down and back up to trigger lazy-load content."""
        if self._driver is None:
            raise RuntimeError("Сначала откройте страницу в браузере (open_url).")
        driver = self._driver
        # Read initial scroll height.
        prev_height = driver.execute_script("return document.body.scrollHeight") or 0
        # Scroll down in steps.
        for pct in (0, 40, 80, 100):
            driver.execute_script(
                f"window.scrollTo(0, document.body.scrollHeight * {pct / 100});"
            )
            time.sleep(0.3)
        # Check if page grew (lazy-load) and scroll up/down up to 40 times.
        for _ in range(40):
            new_height = driver.execute_script("return document.body.scrollHeight") or 0
            if new_height <= prev_height:
                break
            prev_height = new_height
            for pct in (100, 80, 40, 0):
                driver.execute_script(
                    f"window.scrollTo(0, document.body.scrollHeight * {pct / 100});"
                )
                time.sleep(0.2)
            for pct in (0, 40, 80, 100):
                driver.execute_script(
                    f"window.scrollTo(0, document.body.scrollHeight * {pct / 100});"
                )
                time.sleep(0.2)

    def open_url(self, browser: str, url: str) -> str:
        self._ensure_browser(browser)
        self._emit_progress("browser", 0, 0)
        assert self._driver is not None
        self._sync_active_browser_tab()
        self._driver.get(url)
        self._remember_current_window_handle()
        self._wait_for_page_ready()
        return str(self._driver.current_url or url)

    def _wait_for_page_ready(self, timeout: float = 30.0) -> None:
        assert self._driver is not None
        try:
            WebDriverWait(self._driver, timeout).until(
                lambda driver: str(driver.current_url or "").strip()
                not in {"", "about:blank", "data:,"}
            )
            WebDriverWait(self._driver, timeout).until(
                lambda driver: str(
                    driver.execute_script("return document.readyState || '';")
                ).lower()
                == "complete"
            )
            WebDriverWait(self._driver, timeout).until(
                lambda driver: bool(
                    driver.execute_script("return document.body !== null;")
                )
            )
        except Exception as exc:  # noqa: BLE001
            current_url = str(self._driver.current_url or "").strip()
            raise RuntimeError(
                "Страница в браузере не успела загрузиться полностью."
                f" Последний URL: {current_url or 'unknown'}"
            ) from exc

    def fetch(self, browser: str, pattern: str, max_parallel: int = 1) -> FetchResult:
        self._ensure_browser(browser)
        assert self._driver is not None
        page_url, _window_handle = self._select_best_fetch_target(pattern)

        try:
            WebDriverWait(self._driver, 10).until(
                ec.presence_of_all_elements_located((By.CSS_SELECTOR, "img, a"))
            )
        except Exception:  # noqa: BLE001
            LOG.debug("Page did not report img/a presence within wait timeout", exc_info=True)

        if not page_url or page_url in {"about:blank", "data:,"}:
            raise RuntimeError("Сначала откройте страницу главы в браузере.")

        self._emit_progress("collect", 0, 0)
        candidates = self._collect_candidates(page_url)
        _debug_log(
            "fetch: collected %d raw candidates from %s",
            len(candidates),
            page_url,
        )
        filtered = self._filter_candidates(candidates, pattern)
        _debug_log(
            "fetch: %d candidates remained after prefilter (pattern=%r)",
            len(filtered),
            pattern,
        )
        return self._download_candidate_links(
            filtered,
            page_url,
            temp_prefix="mangafucker_adv_fetch_",
            max_parallel=max_parallel,
        )

    def fetch_auto_links(
        self,
        browser: str,
        max_parallel: int = 1,
        cancel_file: Optional[Path] = None,
    ) -> dict[str, Any]:
        self._ensure_browser(browser)
        assert self._driver is not None
        page_url, _window_handle = self._select_best_fetch_target("")

        try:
            WebDriverWait(self._driver, 10).until(
                ec.presence_of_all_elements_located((By.CSS_SELECTOR, "img, source, a"))
            )
        except Exception:  # noqa: BLE001
            LOG.debug("Page did not report img/source/a presence within wait timeout", exc_info=True)

        if not page_url or page_url in {"about:blank", "data:,"}:
            raise RuntimeError("Сначала откройте страницу главы в браузере.")

        self._emit_progress("collect", 0, 0)
        candidates = self._collect_auto_candidate_links(page_url)
        _debug_log("auto fetch: %d candidates remained after auto prefilter", len(candidates))
        return self._download_auto_candidate_links(
            candidates,
            page_url,
            temp_prefix="mangafucker_adv_auto_fetch_",
            max_parallel=max_parallel,
            cancel_file=cancel_file,
        )

    def start_link_collect(self, browser: str, pattern: str, max_parallel: int = 1) -> str:
        return self._start_link_collect(
            browser,
            pattern,
            max_parallel,
            exclude_site_code_links=False,
        )

    def start_auto_link_collect(self, browser: str, max_parallel: int = 1) -> str:
        return self._start_link_collect(
            browser,
            "",
            max_parallel,
            exclude_site_code_links=True,
        )

    def _start_link_collect(
        self,
        browser: str,
        pattern: str,
        max_parallel: int,
        exclude_site_code_links: bool,
    ) -> str:
        self._ensure_browser(browser)
        if self._link_collect is not None or self._link_collect_active:
            raise RuntimeError("Сбор ссылок уже запущен.")
        if self._canvas_capture is not None or self._intercept_active:
            raise RuntimeError("Сначала завершите текущий перехват Canvas.")

        page_url, window_handle = self._select_best_fetch_target(pattern)
        collect_stop_event = threading.Event()
        collect_lock = threading.Lock()
        self._link_collect_active = True
        worker = threading.Thread(
            target=self._collect_links_loop,
            args=(collect_stop_event, collect_lock),
            daemon=True,
            name="mangafucker-link-collect",
        )
        self._link_collect = LinkCollectState(
            stop_event=collect_stop_event,
            lock=collect_lock,
            links=[],
            seen_links=set(),
            worker=worker,
            page_url=page_url,
            window_handle=window_handle,
            pattern=pattern,
            max_parallel=max_parallel,
            exclude_site_code_links=exclude_site_code_links,
        )
        self._emit_progress("collect", 0, 0)
        worker.start()
        return page_url

    def stop_link_collect(self, browser: str) -> FetchResult:
        self._ensure_browser(browser)
        collect = self._link_collect
        if not self._link_collect_active or collect is None:
            raise RuntimeError("Сбор ссылок ещё не запущен.")

        self._emit_progress("collect", 0, 0)
        collect.stop_event.set()
        collect.worker.join(timeout=2.5)
        with collect.lock:
            links = list(collect.links)
            error_message = collect.error_message
            log_message = collect.log_message
        self._clear_link_collect_runtime()

        if error_message:
            if log_message:
                LOG.error(log_message)
            raise RuntimeError(error_message)
        return self._download_candidate_links(
            links,
            self._page_url_for_handle(collect.window_handle) or collect.page_url,
            temp_prefix="mangafucker_adv_fetch_collect_",
            max_parallel=collect.max_parallel,
        )

    def stop_auto_link_collect(
        self,
        browser: str,
        cancel_file: Optional[Path] = None,
    ) -> dict[str, Any]:
        self._ensure_browser(browser)
        collect = self._link_collect
        if not self._link_collect_active or collect is None:
            raise RuntimeError("Сбор ссылок ещё не запущен.")

        self._emit_progress("collect", 0, 0)
        collect.stop_event.set()
        collect.worker.join(timeout=2.5)
        with collect.lock:
            links = list(collect.links)
            error_message = collect.error_message
            log_message = collect.log_message
        self._clear_link_collect_runtime()

        if error_message:
            if log_message:
                LOG.error(log_message)
            raise RuntimeError(error_message)
        return self._download_auto_candidate_links(
            links,
            self._page_url_for_handle(collect.window_handle) or collect.page_url,
            temp_prefix="mangafucker_adv_auto_collect_",
            max_parallel=collect.max_parallel,
            cancel_file=cancel_file,
        )

    def link_collect_status(self, browser: str) -> int:
        self._ensure_browser(browser)
        collect = self._link_collect
        if collect is None or not self._link_collect_active:
            return 0
        with collect.lock:
            return len(collect.links)

    def _download_candidate_links(
        self,
        filtered: list[str],
        page_url: str,
        temp_prefix: str,
        max_parallel: int,
    ) -> FetchResult:
        if not filtered:
            raise RuntimeError("Подходящих ссылок не найдено или ничего не скачалось.")

        output_dir = Path(tempfile.mkdtemp(prefix=temp_prefix))
        results = self._download_candidate_links_parallel(
            filtered,
            page_url,
            max_parallel=max_parallel,
        )
        downloaded = 0
        for index, (link, image) in enumerate(zip(filtered, results, strict=True), start=1):
            if image is None:
                continue
            downloaded += 1
            _debug_log(
                "fetch: [%d/%d] success %s -> %dx%d",
                index,
                len(filtered),
                link,
                image.width,
                image.height,
            )
            image.save(output_dir / f"{downloaded:04}.png", format="PNG")

        if downloaded == 0:
            shutil.rmtree(output_dir, ignore_errors=True)
            raise RuntimeError("Подходящих ссылок не найдено или ничего не скачалось.")

        return FetchResult(
            page_url=page_url,
            output_dir=output_dir,
            downloaded_images=downloaded,
        )

    def _download_auto_candidate_links(
        self,
        filtered: list[str],
        page_url: str,
        temp_prefix: str,
        max_parallel: int,
        cancel_file: Optional[Path] = None,
    ) -> dict[str, Any]:
        if not filtered:
            raise RuntimeError("Подходящих ссылок не найдено или ничего не скачалось.")

        output_dir = Path(tempfile.mkdtemp(prefix=temp_prefix))
        items: list[dict[str, Any]] = []
        downloaded = 0
        cancelled = False
        grouped_candidates = [
            (link, _auto_candidate_group_signature(link)) for link in filtered
        ]
        group_failures: dict[str, int] = {}
        group_successes: dict[str, int] = {}
        rejected_groups: set[str] = set()
        _debug_log(
            "auto fetch: prepared %d candidate(s) in %d group(s) before download",
            len(grouped_candidates),
            len({signature for _link, signature in grouped_candidates}),
        )
        for order_index, (link, group_signature) in enumerate(grouped_candidates):
            if _cancel_requested(cancel_file):
                cancelled = True
                _debug_log(
                    "auto fetch: cancel requested after %d downloaded image(s)",
                    downloaded,
                )
                break
            if group_signature in rejected_groups:
                _debug_log(
                    "auto fetch: skip rejected group %s candidate %s",
                    group_signature,
                    _short_link(link),
                )
                continue
            self._emit_progress("download", order_index + 1, len(filtered))
            try:
                image = self._download_image_with_strategy(link, page_url, auto_mode=True)
            except Exception as exc:  # noqa: BLE001
                failures = group_failures.get(group_signature, 0) + 1
                group_failures[group_signature] = failures
                if (
                    failures >= 3
                    and group_successes.get(group_signature, 0) == 0
                ):
                    rejected_groups.add(group_signature)
                    _debug_log(
                        "auto fetch: rejected group %s after %d failed candidate(s)",
                        group_signature,
                        failures,
                    )
                _debug_log(
                    "auto fetch: [%d/%d] failed %s: %s",
                    order_index + 1,
                    len(filtered),
                    link,
                    exc,
                )
                LOG.exception("Failed to download auto candidate %s", link)
                continue
            group_successes[group_signature] = group_successes.get(group_signature, 0) + 1
            downloaded += 1
            file_name = f"{downloaded:04}.png"
            _debug_log(
                "auto fetch: [%d/%d] success %s -> %dx%d",
                order_index + 1,
                len(filtered),
                link,
                image.width,
                image.height,
            )
            image.save(output_dir / file_name, format="PNG")
            items.append(
                {
                    "order": order_index,
                    "url": link,
                    "file_name": file_name,
                    "width": image.width,
                    "height": image.height,
                }
            )

        if downloaded == 0:
            shutil.rmtree(output_dir, ignore_errors=True)
            if cancelled:
                raise RuntimeError("Выкачка остановлена до загрузки первой картинки.")
            raise RuntimeError("Подходящих ссылок не найдено или ничего не скачалось.")

        return {
            "page_url": page_url,
            "output_dir": str(output_dir),
            "downloaded_images": downloaded,
            "items": items,
        }

    def _download_candidate_links_parallel(
        self,
        filtered: list[str],
        page_url: str,
        max_parallel: int,
    ) -> list[Optional[Image.Image]]:
        self._preferred_download_method = None
        results: list[Optional[Image.Image]] = []
        for _link in filtered:
            results.append(None)

        preferred = self._preferred_download_method
        fallback_progress_floor = 0
        if preferred in {None, DOWNLOAD_METHOD_REQUESTS}:
            request_indexes = [
                index
                for index, link in enumerate(filtered)
                if self._can_download_with_requests(link)
            ]
            if request_indexes:
                fallback_progress_floor = len(request_indexes)
                request_results = self._download_candidates_with_requests_parallel(
                    [(index, filtered[index]) for index in request_indexes],
                    page_url,
                    max_parallel=max_parallel,
                    total=len(filtered),
                )
                request_successes = 0
                for index, image in request_results.items():
                    if image is None:
                        continue
                    results[index] = image
                    request_successes += 1
                if request_successes > 0:
                    self._preferred_download_method = DOWNLOAD_METHOD_REQUESTS
                    _debug_log(
                        "download: selected %s method after %d successful image(s)",
                        DOWNLOAD_METHOD_REQUESTS,
                        request_successes,
                    )

        for index, link in enumerate(filtered):
            if results[index] is not None:
                continue
            self._emit_progress(
                "download",
                max(index + 1, fallback_progress_floor),
                len(filtered),
            )
            try:
                results[index] = self._download_image_with_strategy(
                    link,
                    page_url,
                    already_tried={
                        DOWNLOAD_METHOD_REQUESTS
                    }
                    if self._can_download_with_requests(link)
                    else set(),
                )
            except Exception as exc:  # noqa: BLE001
                _debug_log(
                    "fetch: [%d/%d] failed %s: %s",
                    index + 1,
                    len(filtered),
                    link,
                    exc,
                )
                LOG.exception("Failed to download candidate %s", link)
        return results

    def fetch_canvas(self, browser: str) -> FetchResult:
        self._ensure_browser(browser)
        if self._canvas_capture is not None:
            raise RuntimeError("Сначала завершите текущий перехват Canvas.")
        if self._intercept_active:
            raise RuntimeError("Сначала завершите текущий перехват Canvas.")

        page_url, _window_handle = self._select_best_canvas_target()
        self._emit_progress("collect_canvas", 0, 0)
        canvas_entries = self._collect_canvas_entries()
        if not canvas_entries:
            raise RuntimeError("Canvas на текущей странице не найдены.")

        output_dir = Path(tempfile.mkdtemp(prefix="mangafucker_adv_canvas_fetch_"))
        saved_count = self._save_canvas_entries(canvas_entries, output_dir)
        if saved_count == 0:
            shutil.rmtree(output_dir, ignore_errors=True)
            raise RuntimeError("Canvas на текущей странице не удалось сохранить.")

        return FetchResult(
            page_url=page_url,
            output_dir=output_dir,
            downloaded_images=saved_count,
        )

    def start_intercept(self, browser: str) -> str:
        self._ensure_browser(browser)
        if self._canvas_capture is not None or self._intercept_active:
            raise RuntimeError("Перехват Canvas уже запущен.")

        page_url, window_handle = self._select_best_canvas_target()
        self._intercept_active = True
        capture_stop_event = threading.Event()
        capture_lock = threading.Lock()
        worker = threading.Thread(
            target=self._capture_canvas_loop,
            args=(capture_stop_event, capture_lock),
            daemon=True,
            name="mangafucker-canvas-capture",
        )
        self._canvas_capture = CanvasCaptureState(
            stop_event=capture_stop_event,
            lock=capture_lock,
            entries=[],
            hashes=set(),
            worker=worker,
            page_url=page_url,
            window_handle=window_handle,
        )
        self._emit_progress("collect_canvas", 0, 0)
        worker.start()
        return page_url

    def stop_intercept(self, browser: str) -> FetchResult:
        self._ensure_browser(browser)
        capture = self._canvas_capture
        if not self._intercept_active or capture is None:
            raise RuntimeError("Перехват ещё не запущен.")

        self._emit_progress("collect_canvas", 0, 0)
        capture.stop_event.set()
        capture.worker.join(timeout=2.5)
        with capture.lock:
            canvas_entries = list(capture.entries)
            error_message = capture.error_message
            log_message = capture.log_message
        self._clear_intercept_runtime()

        if error_message:
            if log_message:
                LOG.error(log_message)
            raise RuntimeError(error_message)
        if not canvas_entries:
            raise RuntimeError("Во время перехвата не найдено новых Canvas.")

        output_dir = Path(tempfile.mkdtemp(prefix="mangafucker_adv_canvas_intercept_"))
        saved_count = self._save_canvas_entries(canvas_entries, output_dir)
        if saved_count == 0:
            shutil.rmtree(output_dir, ignore_errors=True)
            raise RuntimeError("Не удалось сохранить Canvas из перехвата.")

        return FetchResult(
            page_url=self._page_url_for_handle(capture.window_handle) or capture.page_url,
            output_dir=output_dir,
            downloaded_images=saved_count,
        )

    def intercept_status(self, browser: str) -> int:
        self._ensure_browser(browser)
        capture = self._canvas_capture
        if capture is None or not self._intercept_active:
            return 0
        with capture.lock:
            return len(capture.entries)

    def _ensure_browser(self, browser: str) -> None:
        if not browser:
            raise RuntimeError("Не найден ни один поддерживаемый браузер.")

        if self._driver is not None and self._browser_name == browser:
            try:
                self._sync_active_browser_tab()
                _ = self._driver.current_url
                self._remember_current_window_handle()
                return
            except Exception:  # noqa: BLE001
                # Any failure to talk to the cached driver (closed window, dead
                # chromedriver connection, invalid session) means there is no live
                # browser; drop it so we relaunch below instead of erroring out.
                LOG.warning("Existing Selenium driver became invalid, recreating it")
                self.close()
        elif self._driver is not None:
            self.close()

        self._emit_progress("browser", 0, 0)
        self._driver, self._tmp_profile_dir = build_browser(True, browser)
        self._browser_name = browser
        self._remember_current_window_handle()
        # Anti-bot stealth measures
        self._driver.set_window_size(random.randint(1200, 1920), random.randint(800, 1080))
        self._driver.execute_script("""
Object.defineProperty(navigator, 'webdriver', {
    get: () => undefined,
});
if (window.chrome) {
    window.chrome.runtime = {};
}
""")
        self._stop_link_collect()
        self._stop_canvas_capture()
        self._intercept_active = False
        self._link_collect_active = False

    def _require_page_url(self, default: Optional[str] = None, *, sync: bool = True) -> str:
        if self._driver is None:
            raise RuntimeError("Сначала откройте страницу главы в браузере.")
        if sync:
            self._sync_active_browser_tab()
        page_url = str(self._driver.current_url or default or "").strip()
        self._remember_current_window_handle()
        if not page_url or page_url in {"about:blank", "data:,"}:
            raise RuntimeError("Сначала откройте страницу главы в браузере.")
        return page_url

    def _list_window_handles(self) -> list[str]:
        if self._driver is None:
            return []
        try:
            return [str(handle) for handle in self._driver.window_handles]
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to list browser window handles", exc_info=True)
            return []

    def _switch_to_window_handle(self, handle: str) -> None:
        if self._driver is None:
            raise RuntimeError("Сначала откройте страницу главы в браузере.")
        self._driver.switch_to.window(handle)
        self._current_window_handle = handle

    def _page_url_for_handle(self, handle: str) -> str:
        self._switch_to_window_handle(handle)
        assert self._driver is not None
        return str(self._driver.current_url or "").strip()

    def _count_canvas_entries(self) -> int:
        if self._driver is None:
            return 0
        raw_count = self._driver.execute_script(
            """
            let total = 0;
            const walk = (root) => {
                if (!root || !root.querySelectorAll) {
                    return;
                }
                total += root.querySelectorAll("canvas").length;
                for (const element of root.querySelectorAll("*")) {
                    if (element.shadowRoot) {
                        walk(element.shadowRoot);
                    }
                }
                for (const iframe of root.querySelectorAll("iframe")) {
                    try {
                        if (iframe.contentWindow && iframe.contentWindow.document) {
                            walk(iframe.contentWindow.document);
                        }
                    } catch (_error) {
                    }
                }
            };
            walk(document);
            return total;
            """
        )
        try:
            return max(0, int(raw_count))
        except Exception:  # noqa: BLE001
            return 0

    def _select_best_fetch_target(self, pattern: str) -> tuple[str, str]:
        if self._driver is None:
            raise RuntimeError("Сначала откройте страницу главы в браузере.")
        handles = self._list_window_handles()
        if not handles:
            page_url = self._require_page_url()
            if self._current_window_handle is None:
                raise RuntimeError("Не удалось определить вкладку браузера.")
            return page_url, self._current_window_handle

        ranked: list[tuple[int, int, int, str, str]] = []
        for index, handle in enumerate(handles):
            try:
                page_url = self._page_url_for_handle(handle)
                if not page_url or page_url in {"about:blank", "data:,"}:
                    continue
                candidates = self._collect_candidates(page_url)
                filtered = self._filter_candidates(candidates, pattern)
                ranked.append((len(filtered), len(candidates), -index, handle, page_url))
            except Exception:  # noqa: BLE001
                LOG.debug("Failed to inspect tab %s for fetch target", handle, exc_info=True)

        if not ranked:
            page_url = self._require_page_url()
            if self._current_window_handle is None:
                raise RuntimeError("Не удалось определить вкладку браузера.")
            return page_url, self._current_window_handle

        best_filtered, best_total, _neg_index, best_handle, best_page_url = max(ranked)
        self._switch_to_window_handle(best_handle)
        _debug_log(
            "window select fetch: handle=%s filtered=%d total=%d url=%s pattern=%r",
            best_handle,
            best_filtered,
            best_total,
            best_page_url,
            pattern,
        )
        return best_page_url, best_handle

    def _select_best_canvas_target(self) -> tuple[str, str]:
        if self._driver is None:
            raise RuntimeError("Сначала откройте страницу главы в браузере.")
        handles = self._list_window_handles()
        if not handles:
            page_url = self._require_page_url()
            if self._current_window_handle is None:
                raise RuntimeError("Не удалось определить вкладку браузера.")
            return page_url, self._current_window_handle

        ranked: list[tuple[int, int, str, str]] = []
        for index, handle in enumerate(handles):
            try:
                page_url = self._page_url_for_handle(handle)
                if not page_url or page_url in {"about:blank", "data:,"}:
                    continue
                ranked.append((self._count_canvas_entries(), -index, handle, page_url))
            except Exception:  # noqa: BLE001
                LOG.debug("Failed to inspect tab %s for canvas target", handle, exc_info=True)

        if not ranked:
            page_url = self._require_page_url()
            if self._current_window_handle is None:
                raise RuntimeError("Не удалось определить вкладку браузера.")
            return page_url, self._current_window_handle

        canvas_count, _neg_index, best_handle, best_page_url = max(ranked)
        self._switch_to_window_handle(best_handle)
        _debug_log(
            "window select canvas: handle=%s count=%d url=%s",
            best_handle,
            canvas_count,
            best_page_url,
        )
        return best_page_url, best_handle

    def _remember_current_window_handle(self) -> None:
        if self._driver is None:
            self._current_window_handle = None
            return
        try:
            self._current_window_handle = str(self._driver.current_window_handle)
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to remember current browser window handle", exc_info=True)

    def _inspect_window_handle(self, handle: str) -> Optional[dict[str, Any]]:
        if self._driver is None:
            return None
        try:
            self._driver.switch_to.window(handle)
            details = self._driver.execute_script(
                """
                return {
                    href: String(window.location.href || ""),
                    visibility: String(document.visibilityState || ""),
                    has_focus: Boolean(document.hasFocus && document.hasFocus()),
                    title: String(document.title || ""),
                    ready_state: String(document.readyState || ""),
                };
                """
            )
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to inspect browser window handle %s", handle, exc_info=True)
            return None
        if not isinstance(details, dict):
            return None
        href = str(details.get("href") or "").strip()
        visibility = str(details.get("visibility") or "").strip().lower()
        has_focus = bool(details.get("has_focus"))
        score = 0
        if has_focus:
            score += 100
        if visibility == "visible":
            score += 10
        if href and href not in {"about:blank", "data:,"}:
            score += 1
        if handle == self._current_window_handle:
            score += 2
        return {
            "handle": handle,
            "href": href,
            "visibility": visibility,
            "has_focus": has_focus,
            "score": score,
        }

    def _sync_active_browser_tab(self) -> None:
        if self._driver is None:
            return
        try:
            handles = list(self._driver.window_handles)
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to read browser window handles", exc_info=True)
            return
        if not handles:
            return

        inspected: list[dict[str, Any]] = []
        for handle in handles:
            details = self._inspect_window_handle(handle)
            if details is not None:
                inspected.append(details)
        if not inspected:
            return

        best = max(inspected, key=lambda item: item["score"])
        best_handle = str(best["handle"])
        try:
            self._driver.switch_to.window(best_handle)
            self._current_window_handle = best_handle
            _debug_log(
                "window sync: selected handle=%s focus=%s visibility=%s url=%s",
                best_handle,
                best["has_focus"],
                best["visibility"],
                best["href"],
            )
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to switch to active browser window handle %s", best_handle, exc_info=True)

    def _collect_canvas_entries(self) -> list[dict[str, Any]]:
        if self._driver is None:
            return []
        raw_entries = self._driver.execute_script(
            """
            const entries = [];
            const walk = (root) => {
                if (!root || !root.querySelectorAll) {
                    return;
                }
                for (const canvas of root.querySelectorAll("canvas")) {
                    try {
                        entries.push({
                            index: entries.length,
                            width: Number(canvas.width || 0),
                            height: Number(canvas.height || 0),
                            data: canvas.toDataURL("image/png", 1.0),
                        });
                    } catch (_error) {
                    }
                }
                for (const element of root.querySelectorAll("*")) {
                    if (element.shadowRoot) {
                        walk(element.shadowRoot);
                    }
                }
                for (const iframe of root.querySelectorAll("iframe")) {
                    try {
                        if (iframe.contentWindow && iframe.contentWindow.document) {
                            walk(iframe.contentWindow.document);
                        }
                    } catch (_error) {
                    }
                }
            };
            walk(document);
            return entries;
            """
        )
        if not isinstance(raw_entries, list):
            return []
        filtered_entries: list[dict[str, Any]] = []
        for index, item in enumerate(raw_entries):
            if not isinstance(item, dict):
                continue
            data = item.get("data")
            if not isinstance(data, str) or not data.startswith("data:image/png;base64,"):
                continue
            filtered_entries.append(
                {
                    "index": item.get("index", index),
                    "width": item.get("width", 0),
                    "height": item.get("height", 0),
                    "data": data,
                }
            )
        return filtered_entries

    def _capture_canvas_loop(
        self,
        stop_event: threading.Event,
        capture_lock: threading.Lock,
    ) -> None:
        while not stop_event.is_set():
            try:
                capture = self._canvas_capture
                if capture is None:
                    return
                self._switch_to_window_handle(capture.window_handle)
                canvas_entries = self._collect_canvas_entries()
                added_count = 0
                with capture_lock:
                    for item in canvas_entries:
                        canvas_hash = hashlib.sha256(
                            item["data"].encode("utf-8")
                        ).hexdigest()
                        if canvas_hash in capture.hashes:
                            continue
                        capture.hashes.add(canvas_hash)
                        capture.entries.append(item)
                        added_count += 1
                    total_count = len(capture.entries)
                if added_count > 0:
                    self._emit_progress("collect_canvas", total_count, 0)
                if added_count > 0:
                    _debug_log(
                        "canvas capture: total=%d added=%d",
                        total_count,
                        added_count,
                    )
            except Exception as exc:  # noqa: BLE001
                LOG.exception("Canvas capture loop failed")
                capture = self._canvas_capture
                if capture is not None:
                    with capture_lock:
                        capture.error_message = f"Ошибка перехвата Canvas: {exc}"
                        capture.log_message = (
                            f"canvas capture loop failed: {type(exc).__name__}: {exc}"
                        )
                stop_event.set()
                break
            stop_event.wait(1.0)

    def _collect_links_loop(
        self,
        stop_event: threading.Event,
        collect_lock: threading.Lock,
    ) -> None:
        while not stop_event.is_set():
            try:
                collect = self._link_collect
                if collect is None:
                    return
                self._switch_to_window_handle(collect.window_handle)
                page_url = self._require_page_url(default=collect.page_url, sync=False)
                if collect.exclude_site_code_links:
                    filtered = self._collect_auto_candidate_links(page_url)
                else:
                    filtered = self._filter_candidates(
                        self._collect_candidates(page_url),
                        collect.pattern,
                    )
                added_count = 0
                with collect_lock:
                    for link in filtered:
                        if link in collect.seen_links:
                            continue
                        collect.seen_links.add(link)
                        collect.links.append(link)
                        added_count += 1
                    total_count = len(collect.links)
                if added_count > 0:
                    self._emit_progress("collect", total_count, 0)
                    _debug_log(
                        "link collect: total=%d added=%d pattern=%r",
                        total_count,
                        added_count,
                        collect.pattern,
                    )
            except Exception as exc:  # noqa: BLE001
                LOG.exception("Link collection loop failed")
                collect = self._link_collect
                if collect is not None:
                    with collect_lock:
                        collect.error_message = f"Ошибка фонового сбора ссылок: {exc}"
                        collect.log_message = (
                            f"link collect loop failed: {type(exc).__name__}: {exc}"
                        )
                stop_event.set()
                break
            stop_event.wait(1.0)

    def _save_canvas_entries(
        self,
        canvas_entries: list[dict[str, Any]],
        output_dir: Path,
    ) -> int:
        output_dir.mkdir(parents=True, exist_ok=True)
        saved_count = 0
        total = len(canvas_entries)
        for index, item in enumerate(canvas_entries, start=1):
            self._emit_progress("save_canvas", index, total)
            data_url = item.get("data")
            if not isinstance(data_url, str):
                continue
            _, _, payload = data_url.partition(",")
            if not payload:
                continue
            try:
                image_bytes = base64.b64decode(payload)
            except Exception:  # noqa: BLE001
                LOG.exception("Failed to decode canvas payload")
                continue
            saved_count += 1
            (output_dir / f"{saved_count:04}.png").write_bytes(image_bytes)
        return saved_count

    def _collect_candidates(self, page_url: str) -> list[str]:
        assert self._driver is not None
        self._sync_active_browser_tab()
        raw_candidates = self._driver.execute_script(
            """
            const seen = new Set();
            const out = [];

            const add = (value) => {
                if (typeof value !== "string") {
                    return;
                }
                const normalized = value.trim();
                if (!normalized || seen.has(normalized)) {
                    return;
                }
                seen.add(normalized);
                out.push(normalized);
            };

            const looksLikeUrl = (value) => {
                if (typeof value !== "string") {
                    return false;
                }
                const normalized = value.trim();
                if (!normalized || /\\s/.test(normalized)) {
                    return false;
                }
                if (/^(https?:|file:|data:image\\/|\\/\\/|\\/|\\.\\/|\\.\\.\\/)/i.test(normalized)) {
                    return true;
                }
                return normalized.includes("/") || normalized.includes(".");
            };

            const addUrlish = (value) => {
                if (looksLikeUrl(value)) {
                    add(value);
                }
            };

            const addSrcSet = (value) => {
                if (typeof value !== "string") {
                    return;
                }
                for (const part of value.split(",")) {
                    const token = part.trim().split(/\\s+/)[0] || "";
                    add(token);
                }
            };

            const collectFromRoot = (root) => {
                if (!root || !root.querySelectorAll) {
                    return;
                }
                for (const img of root.querySelectorAll("img")) {
                    add(img.currentSrc || "");
                    add(img.src || "");
                    add(img.getAttribute("src"));
                    add(img.getAttribute("data-src"));
                    add(img.getAttribute("data-lazy-src"));
                    add(img.getAttribute("data-original"));
                    add(img.getAttribute("data-url"));
                    addSrcSet(img.getAttribute("srcset") || "");
                    addSrcSet(img.getAttribute("data-srcset") || "");
                }
                for (const source of root.querySelectorAll("source")) {
                    add(source.src || "");
                    add(source.getAttribute("src"));
                    addSrcSet(source.srcset || "");
                    addSrcSet(source.getAttribute("srcset") || "");
                    addSrcSet(source.getAttribute("data-srcset") || "");
                }
                for (const anchor of root.querySelectorAll("a[href]")) {
                    add(anchor.href || "");
                    add(anchor.getAttribute("href"));
                }
                for (const element of root.querySelectorAll("*")) {
                    if (element.attributes) {
                        for (const attr of element.attributes) {
                            const name = String(attr.name || "").toLowerCase();
                            const value = String(attr.value || "");
                            if (name.includes("srcset")) {
                                addSrcSet(value);
                            } else if (
                                name === "href" ||
                                name === "src" ||
                                name === "poster" ||
                                name === "content" ||
                                name.startsWith("data-")
                            ) {
                                addUrlish(value);
                            }
                        }
                    }
                    const styleValue = element.getAttribute("style") || "";
                    for (const match of styleValue.matchAll(/url\\((['"]?)(.*?)\\1\\)/g)) {
                        addUrlish(match[2] || "");
                    }
                }
            };

            collectFromRoot(document);
            for (const element of document.querySelectorAll("*")) {
                if (element.shadowRoot) {
                    collectFromRoot(element.shadowRoot);
                }
            }
            return out;
            """
        )
        if not isinstance(raw_candidates, list):
            return []
        candidates: list[str] = []
        for item in raw_candidates:
            if not isinstance(item, str):
                continue
            value = item.strip()
            if not value:
                continue
            try:
                candidates.append(urljoin(page_url, value))
            except Exception:  # noqa: BLE001
                continue
        return candidates

    def _collect_auto_candidate_links(self, page_url: str) -> list[str]:
        assert self._driver is not None
        self._sync_active_browser_tab()
        raw_candidates = self._driver.execute_script(AUTO_COLLECT_CANDIDATES_JS)
        if not isinstance(raw_candidates, list):
            return []
        filtered: list[str] = []
        seen: set[str] = set()
        for item in raw_candidates:
            if not isinstance(item, dict):
                continue
            raw_url = item.get("url")
            source = str(item.get("source") or "")
            if not isinstance(raw_url, str):
                continue
            try:
                candidate = urljoin(page_url, raw_url.strip())
            except Exception:  # noqa: BLE001
                continue
            if candidate in seen:
                continue
            allowed, reason = _auto_candidate_allowed(candidate, source)
            if not allowed:
                _debug_log(
                    "auto prefilter: skipped %s from %s: %s",
                    candidate,
                    source,
                    reason,
                )
                continue
            seen.add(candidate)
            filtered.append(candidate)
        return filtered

    def _filter_candidates(self, candidates: list[str], pattern: str) -> list[str]:
        matcher = compile_wildcard_prefixes(pattern) if pattern else None
        filtered: list[str] = []
        seen: set[str] = set()
        for candidate in candidates:
            if candidate in seen:
                _debug_log("prefilter: skip duplicate %s", candidate)
                continue
            if matcher is not None:
                if matcher.search(candidate):
                    seen.add(candidate)
                    filtered.append(candidate)
                    _debug_log("prefilter: matched pattern %s", candidate)
                else:
                    _debug_log("prefilter: skipped by pattern %s", candidate)
                continue
            seen.add(candidate)
            filtered.append(candidate)
            _debug_log("prefilter: accepted %s", candidate)
        return filtered

    def _filter_explicit_site_code_links(self, candidates: list[str]) -> list[str]:
        filtered: list[str] = []
        seen: set[str] = set()
        for candidate in candidates:
            if candidate in seen:
                continue
            if _looks_like_site_code_resource(candidate):
                _debug_log("auto prefilter: skipped site code resource %s", candidate)
                continue
            seen.add(candidate)
            filtered.append(candidate)
        return filtered

    def _clear_intercept_runtime(self) -> None:
        self._intercept_active = False
        self._canvas_capture = None

    def _clear_link_collect_runtime(self) -> None:
        self._link_collect_active = False
        self._link_collect = None

    def _stop_canvas_capture(self) -> None:
        capture = self._canvas_capture
        if capture is None:
            return
        capture.stop_event.set()
        if capture.worker.is_alive():
            capture.worker.join(timeout=2.0)
        self._canvas_capture = None
        self._intercept_active = False

    def _stop_link_collect(self) -> None:
        collect = self._link_collect
        if collect is None:
            return
        collect.stop_event.set()
        if collect.worker.is_alive():
            collect.worker.join(timeout=2.0)
        self._link_collect = None
        self._link_collect_active = False

    def _can_download_with_requests(self, link: str) -> bool:
        parsed = urlparse(link)
        return parsed.scheme in {"http", "https"}

    def _download_candidates_with_requests_parallel(
        self,
        indexed_links: list[tuple[int, str]],
        referer: str,
        max_parallel: int,
        total: int,
    ) -> dict[int, Optional[Image.Image]]:
        if not indexed_links:
            return {}

        assert self._driver is not None
        base_headers = self._browser_request_headers()
        cookies = self._browser_cookie_snapshot()
        worker_count = max(1, min(max_parallel, len(indexed_links)))
        _debug_log(
            "download: trying %s for %d image(s), workers=%d",
            DOWNLOAD_METHOD_REQUESTS,
            len(indexed_links),
            worker_count,
        )

        def fetch_one(index: int, link: str) -> tuple[int, Optional[Image.Image]]:
            try:
                image = self._download_image_with_requests_context(
                    link,
                    referer,
                    base_headers,
                    cookies,
                )
                return index, image
            except Exception as exc:  # noqa: BLE001
                _debug_log(
                    "download: %s failed for %s: %s",
                    DOWNLOAD_METHOD_REQUESTS,
                    _short_link(link),
                    exc,
                )
                return index, None

        results: dict[int, Optional[Image.Image]] = {}
        completed = 0
        if worker_count == 1:
            for index, link in indexed_links:
                results[index] = fetch_one(index, link)[1]
                completed += 1
                self._emit_progress("download", completed, total)
            return results

        with ThreadPoolExecutor(max_workers=worker_count) as executor:
            futures = {
                executor.submit(fetch_one, index, link): index
                for index, link in indexed_links
            }
            for future in as_completed(futures):
                index, image = future.result()
                results[index] = image
                completed += 1
                self._emit_progress("download", completed, total)
        return results

    def _download_image_with_strategy(
        self,
        link: str,
        referer: str,
        already_tried: Optional[set[str]] = None,
        auto_mode: bool = False,
    ) -> Image.Image:
        if link.startswith("data:image/"):
            _debug_log("download: decoding data URL %s", _short_link(link))
            return self._decode_data_image(link)

        errors: list[str] = []
        skipped = already_tried or set()
        for method in self._download_method_order(link, skipped):
            try:
                image = self._download_image_with_method(method, link, referer, auto_mode)
                self._preferred_download_method = method
                _debug_log("download: selected %s method for %s", method, _short_link(link))
                return image
            except Exception as exc:  # noqa: BLE001
                errors.append(f"{method}: {type(exc).__name__}: {exc}")
                _debug_log("download: %s failed for %s: %s", method, _short_link(link), exc)
                if method == DOWNLOAD_METHOD_CURRENT_TAB and isinstance(exc, NonImagePayloadError):
                    skipped.add(DOWNLOAD_METHOD_NEW_TAB)
                    _debug_log(
                        "download: skip %s for %s because browser fetch returned non-image bytes",
                        DOWNLOAD_METHOD_NEW_TAB,
                        _short_link(link),
                    )
                    break

        joined_errors = "; ".join(errors) if errors else "no method available"
        raise RuntimeError(f"Could not download image URL: {link}. Tried methods: {joined_errors}")

    def _download_method_order(self, link: str, skipped: set[str]) -> list[str]:
        methods = [
            DOWNLOAD_METHOD_REQUESTS,
            DOWNLOAD_METHOD_CURRENT_TAB,
            DOWNLOAD_METHOD_NEW_TAB,
        ]
        preferred = self._preferred_download_method
        if preferred in methods:
            methods.remove(preferred)
            methods.insert(0, preferred)
        if not self._can_download_with_requests(link):
            skipped = skipped | {DOWNLOAD_METHOD_REQUESTS}
        attempt_key = _new_tab_attempt_key(link)
        if self._new_tab_attempts_by_link.get(attempt_key, 0) >= NEW_TAB_MAX_ATTEMPTS_PER_LINK:
            skipped = skipped | {DOWNLOAD_METHOD_NEW_TAB}
        return [method for method in methods if method not in skipped]

    def _download_image_with_method(
        self,
        method: str,
        link: str,
        referer: str,
        auto_mode: bool,
    ) -> Image.Image:
        if method == DOWNLOAD_METHOD_REQUESTS:
            return self._download_image_with_requests(link, referer, auto_mode)
        if method == DOWNLOAD_METHOD_CURRENT_TAB:
            return self._download_image_with_current_tab(link, auto_mode)
        if method == DOWNLOAD_METHOD_NEW_TAB:
            return self._download_image_with_new_tab(link, auto_mode)
        raise RuntimeError(f"Unknown download method: {method}")

    def _download_image_with_requests(
        self,
        link: str,
        referer: str,
        auto_mode: bool = False,
    ) -> Image.Image:
        assert self._driver is not None
        return self._download_image_with_requests_context(
            link,
            referer,
            self._browser_request_headers(),
            self._browser_cookie_snapshot(),
            auto_mode=auto_mode,
        )

    def _download_image_with_requests_context(
        self,
        link: str,
        referer: str,
        base_headers: dict[str, str],
        cookies: list[dict[str, Any]],
        auto_mode: bool = False,
    ) -> Image.Image:
        if not self._can_download_with_requests(link):
            raise RuntimeError(f"requests cannot download non-http URL: {link}")
        session = requests.Session()
        for cookie in cookies:
            name = cookie.get("name")
            value = cookie.get("value")
            if not isinstance(name, str) or not isinstance(value, str):
                continue
            cookie_kwargs: dict[str, str] = {
                "path": str(cookie.get("path") or "/"),
            }
            domain = cookie.get("domain")
            if isinstance(domain, str) and domain:
                cookie_kwargs["domain"] = domain
            session.cookies.set(name=name, value=value, **cookie_kwargs)
        headers = self._request_headers_for_link(base_headers, referer, link)
        response = session.get(
            link,
            headers=headers,
            timeout=60,
            allow_redirects=not auto_mode,
        )
        if auto_mode and _is_http_redirect_status(response.status_code):
            raise RuntimeError(f"auto fetch skipped redirect HTTP {response.status_code}")
        if not response.ok:
            raise RuntimeError(f"HTTP {response.status_code}")
        _debug_log(
            "download: requests returned %d bytes for %s",
            len(response.content),
            _short_link(link),
        )
        return _decode_image_bytes(response.content, link)

    def _download_image_with_current_tab(self, link: str, auto_mode: bool = False) -> Image.Image:
        browser_bytes = self._download_bytes_via_browser(link, reject_redirects=auto_mode)
        if browser_bytes is None:
            raise RuntimeError(f"Browser tab could not download image URL: {link}")
        _debug_log(
            "download: current browser tab returned %d bytes for %s",
            len(browser_bytes),
            _short_link(link),
        )
        return _decode_image_bytes(browser_bytes, link)

    def _download_image_with_new_tab(self, link: str, auto_mode: bool = False) -> Image.Image:
        assert self._driver is not None
        self._reserve_new_tab_attempt(link)
        original_handle = self._driver.current_window_handle
        before_handles = set(self._driver.window_handles)
        new_handle: Optional[str] = None
        try:
            direct_navigation = _new_tab_direct_navigation_allowed(link)
            tab_url = link if direct_navigation else "about:blank"
            new_handle = self._open_stable_new_tab(tab_url, before_handles)
            WebDriverWait(self._driver, 20).until(
                lambda driver: str(
                    driver.execute_script("return document.readyState || '';")
                )
                in {"interactive", "complete"}
            )
            browser_bytes = self._download_bytes_via_browser(link, reject_redirects=auto_mode)
            if browser_bytes is not None:
                _debug_log(
                    "download: new browser tab returned %d bytes for %s",
                    len(browser_bytes),
                    _short_link(link),
                )
                return _decode_image_bytes(browser_bytes, link)
            if not direct_navigation:
                raise RuntimeError(
                    "new blank tab could not fetch image bytes; direct image navigation was "
                    "skipped to avoid triggering a browser download"
                )
            WebDriverWait(self._driver, NEW_TAB_IMAGE_WAIT_SECONDS).until(
                lambda driver: bool(
                    driver.execute_script(
                        """
                        const img = document.images && document.images[0];
                        return Boolean(img && img.complete && img.naturalWidth > 0 && img.naturalHeight > 0);
                        """
                    )
                )
            )
            data_url = self._driver.execute_script(
                """
                const img = document.images && document.images[0];
                if (!img || !img.complete || img.naturalWidth <= 0 || img.naturalHeight <= 0) {
                    return null;
                }
                const canvas = document.createElement("canvas");
                canvas.width = img.naturalWidth;
                canvas.height = img.naturalHeight;
                const ctx = canvas.getContext("2d");
                ctx.drawImage(img, 0, 0);
                return canvas.toDataURL("image/png", 1.0);
                """
            )
            if not isinstance(data_url, str) or not data_url.startswith("data:image/"):
                raise RuntimeError("new image tab did not expose a canvas-readable image")
            _debug_log("download: new browser tab captured %s", _short_link(link))
            return self._decode_data_image(data_url)
        except SelfClosingNewTabError:
            raise
        except WebDriverException as exc:
            if self._new_tab_closed_unexpectedly(new_handle):
                raise SelfClosingNewTabError(
                    f"new image tab closed itself before inspection: {link}"
                ) from exc
            raise
        finally:
            try:
                if new_handle is not None and new_handle in self._driver.window_handles:
                    self._driver.switch_to.window(new_handle)
                    self._driver.close()
            finally:
                if original_handle in self._driver.window_handles:
                    self._driver.switch_to.window(original_handle)

    def _reserve_new_tab_attempt(self, link: str) -> None:
        attempt_key = _new_tab_attempt_key(link)
        attempts = self._new_tab_attempts_by_link.get(attempt_key, 0)
        if attempts >= NEW_TAB_MAX_ATTEMPTS_PER_LINK:
            raise RuntimeError(
                f"new-tab download attempt limit reached for URL: {link}"
            )
        self._new_tab_attempts_by_link[attempt_key] = attempts + 1

    def _open_stable_new_tab(self, link: str, before_handles: set[str]) -> str:
        assert self._driver is not None
        opened = self._driver.execute_script(
            "const tab = window.open(arguments[0], '_blank'); return tab !== null;",
            link,
        )
        if opened is False:
            raise SelfClosingNewTabError(f"browser blocked new image tab: {link}")

        deadline = time.monotonic() + NEW_TAB_OPEN_POLL_SECONDS
        while time.monotonic() < deadline:
            new_handles = [
                handle
                for handle in self._driver.window_handles
                if handle not in before_handles
            ]
            if new_handles:
                new_handle = new_handles[-1]
                try:
                    self._driver.switch_to.window(new_handle)
                except WebDriverException as exc:
                    raise SelfClosingNewTabError(
                        f"new image tab closed before switch: {link}"
                    ) from exc
                return new_handle
            time.sleep(NEW_TAB_OPEN_POLL_INTERVAL_SECONDS)

        raise SelfClosingNewTabError(f"new image tab did not stay open: {link}")

    def _new_tab_closed_unexpectedly(self, new_handle: Optional[str]) -> bool:
        if new_handle is None or self._driver is None:
            return False
        try:
            return new_handle not in self._driver.window_handles
        except WebDriverException:
            return True

    def _decode_data_image(self, link: str) -> Image.Image:
        header, _, payload = link.partition(",")
        if not payload:
            raise RuntimeError("Empty data URL")
        if ";base64" in header:
            image_bytes = base64.b64decode(payload)
        else:
            image_bytes = payload.encode("utf-8")
        return _decode_image_bytes(image_bytes, link)

    def _browser_request_headers(self) -> dict[str, str]:
        assert self._driver is not None
        headers = browserlike_headers(self._driver)
        headers.pop("Accept-Encoding", None)
        return {str(key): str(value) for key, value in headers.items()}

    def _browser_cookie_snapshot(self) -> list[dict[str, Any]]:
        assert self._driver is not None
        cookies = self._driver.get_cookies()
        return [cookie for cookie in cookies if isinstance(cookie, dict)]

    def _request_headers_for_link(
        self,
        base_headers: dict[str, str],
        referer: str,
        link: str,
    ) -> dict[str, str]:
        headers = dict(base_headers)
        headers["Referer"] = referer
        try:
            headers["Sec-Fetch-Site"] = (
                "same-origin" if get_origin(referer) == get_origin(link) else "cross-site"
            )
        except Exception:  # noqa: BLE001
            headers["Sec-Fetch-Site"] = "none"
        return headers

    def _download_bytes_via_browser(
        self,
        link: str,
        reject_redirects: bool = False,
    ) -> Optional[bytes]:
        assert self._driver is not None
        result = self._driver.execute_async_script(
            """
            const url = arguments[0];
            const timeoutMs = arguments[1];
            const rejectRedirects = Boolean(arguments[2]);
            const done = arguments[arguments.length - 1];
            (async () => {
                const controller = new AbortController();
                const timeoutId = setTimeout(() => controller.abort(), timeoutMs);
                try {
                    const response = await fetch(url, {
                        credentials: "include",
                        cache: "no-store",
                        redirect: rejectRedirects ? "manual" : "follow",
                        signal: controller.signal,
                    });
                    if (rejectRedirects && (response.type === "opaqueredirect" || response.redirected || (response.status >= 300 && response.status < 400))) {
                        throw new Error(`redirect ${response.status}`);
                    }
                    if (!response.ok) {
                        throw new Error(`HTTP ${response.status}`);
                    }
                    const buffer = await response.arrayBuffer();
                    const bytes = new Uint8Array(buffer);
                    const chunkSize = 0x8000;
                    const parts = [];
                    for (let offset = 0; offset < bytes.length; offset += chunkSize) {
                        parts.push(String.fromCharCode(...bytes.subarray(offset, offset + chunkSize)));
                    }
                    done({ ok: true, data: btoa(parts.join("")) });
                } catch (error) {
                    done({ ok: false, error: String(error) });
                } finally {
                    clearTimeout(timeoutId);
                }
            })();
            """,
            link,
            BROWSER_FETCH_TIMEOUT_MS,
            reject_redirects,
        )
        if not isinstance(result, dict) or not result.get("ok"):
            _debug_log("download: browser fetch failed for %s with result=%r", link, result)
            return None
        payload = result.get("data")
        if not isinstance(payload, str) or not payload:
            _debug_log("download: browser fetch returned empty payload for %s", link)
            return None
        return base64.b64decode(payload)

    def _emit_progress(self, stage: str, current: int, total: int) -> None:
        self._emit({"event": "progress", "stage": stage, "current": current, "total": total})

    def _emit_error(self, user_message: str, log_message: str) -> None:
        self._emit(
            {
                "event": "error",
                "user_message": user_message,
                "log_message": log_message,
            }
        )

    def _emit(self, payload: dict) -> None:
        with EMIT_LOCK:
            sys.stdout.write(json.dumps(payload, ensure_ascii=False) + "\n")
            sys.stdout.flush()


def _normalize_http_url(raw: str) -> str:
    value = (raw or "").translate(CONTROL_TRANSLATION).strip().replace("\\", "/")
    if not value:
        raise ValueError("Введите ссылку на страницу.")

    has_scheme = re.match(r"^[a-zA-Z][a-zA-Z0-9+.\-]*://", value) is not None
    if not has_scheme:
        if value.startswith("www.") or re.match(r"^[\w\-\.]+\.[a-zA-Z]{2,}(/|$)", value):
            value = "https://" + value

    parsed = urlparse(value)
    if parsed.scheme not in ("http", "https", "file"):
        raise ValueError("Поддерживаются ссылки http(s) и file://")
    if parsed.scheme in ("http", "https") and not parsed.netloc:
        raise ValueError("В адресе отсутствует домен (host).")

    safe_path = quote(parsed.path or "/", safe="/%:@&=+$,;~*'()")
    safe_query = parsed.query.replace(" ", "%20")
    safe_fragment = parsed.fragment.replace(" ", "%20")
    return urlunparse(
        (
            parsed.scheme,
            parsed.netloc,
            safe_path,
            parsed.params,
            safe_query,
            safe_fragment,
        )
    )


AUTO_COLLECT_CANDIDATES_JS = """
const seen = new Set();
const out = [];

const add = (value, source) => {
    if (typeof value !== "string") {
        return;
    }
    const normalized = value.trim();
    if (!normalized || seen.has(`${source}\\n${normalized}`)) {
        return;
    }
    seen.add(`${source}\\n${normalized}`);
    out.push({url: normalized, source});
};

const looksLikeUrl = (value) => {
    if (typeof value !== "string") {
        return false;
    }
    const normalized = value.trim();
    if (!normalized || /\\s/.test(normalized)) {
        return false;
    }
    if (/^(https?:|file:|blob:|data:image\\/|\\/\\/|\\/|\\.\\/|\\.\\.\\/)/i.test(normalized)) {
        return true;
    }
    return normalized.includes("/") || normalized.includes(".");
};

const addUrlish = (value, source) => {
    if (looksLikeUrl(value)) {
        add(value, source);
    }
};

const addSrcSet = (value, source) => {
    if (typeof value !== "string") {
        return;
    }
    for (const part of value.split(",")) {
        const token = part.trim().split(/\\s+/)[0] || "";
        addUrlish(token, source);
    }
};

const collectFromRoot = (root) => {
    if (!root || !root.querySelectorAll) {
        return;
    }
    for (const img of root.querySelectorAll("img")) {
        addUrlish(img.currentSrc || "", "img.currentSrc");
        addUrlish(img.src || "", "img.src");
        addUrlish(img.getAttribute("src"), "img.src");
        addUrlish(img.getAttribute("data-src"), "img.data-src");
        addUrlish(img.getAttribute("data-lazy-src"), "img.data-lazy-src");
        addUrlish(img.getAttribute("data-original"), "img.data-original");
        addUrlish(img.getAttribute("data-url"), "img.data-url");
        addSrcSet(img.getAttribute("srcset") || "", "img.srcset");
        addSrcSet(img.getAttribute("data-srcset") || "", "img.data-srcset");
    }
    for (const source of root.querySelectorAll("source")) {
        addUrlish(source.src || "", "source.src");
        addUrlish(source.getAttribute("src"), "source.src");
        addSrcSet(source.srcset || "", "source.srcset");
        addSrcSet(source.getAttribute("srcset") || "", "source.srcset");
        addSrcSet(source.getAttribute("data-srcset") || "", "source.data-srcset");
    }
    for (const media of root.querySelectorAll("video[poster], object[data], embed[src]")) {
        addUrlish(media.getAttribute("poster"), "poster");
        addUrlish(media.getAttribute("data"), "media.data");
        addUrlish(media.getAttribute("src"), "media.src");
    }
    for (const anchor of root.querySelectorAll("a[href]")) {
        addUrlish(anchor.href || "", "anchor.href");
        addUrlish(anchor.getAttribute("href"), "anchor.href");
    }
    for (const element of root.querySelectorAll("*")) {
        if (element.attributes) {
            for (const attr of element.attributes) {
                const name = String(attr.name || "").toLowerCase();
                const value = String(attr.value || "");
                if (name.includes("srcset")) {
                    addSrcSet(value, `generic.${name}`);
                } else if (
                    name === "src" ||
                    name === "poster" ||
                    name === "content" ||
                    name.startsWith("data-")
                ) {
                    addUrlish(value, `generic.${name}`);
                }
            }
        }
        const styleValue = element.getAttribute("style") || "";
        for (const match of styleValue.matchAll(/url\\((['"]?)(.*?)\\1\\)/g)) {
            addUrlish(match[2] || "", "css.url");
        }
        if (element.shadowRoot) {
            collectFromRoot(element.shadowRoot);
        }
    }
};

collectFromRoot(document);
return out;
"""


def _optional_cancel_file(raw: Any) -> Optional[Path]:
    if not isinstance(raw, str) or not raw.strip():
        return None
    return Path(raw)


def _cancel_requested(cancel_file: Optional[Path]) -> bool:
    return cancel_file is not None and cancel_file.is_file()


def _auto_candidate_allowed(link: str, source: str) -> tuple[bool, str]:
    value = link.strip()
    if not value:
        return False, "empty URL"
    lower = value.lower()
    if lower.startswith("data:"):
        if lower.startswith("data:image/"):
            return True, "image data URL"
        return False, "non-image data URL"
    parsed = urlparse(value)
    scheme = parsed.scheme.lower()
    if scheme and scheme not in {"http", "https", "file", "blob"}:
        return False, f"unsupported scheme {scheme}"
    if _looks_like_site_code_resource(value):
        return False, "site code/static resource"
    if _looks_like_text_or_page_resource(value):
        return False, "text or page-like resource"
    if _source_is_strong_image_context(source):
        return True, "image context"
    if source.lower().startswith("anchor."):
        return True, "anchor candidate requires response validation"
    if _url_has_image_signal(value):
        return True, "image URL signal"
    return False, "weak non-image candidate"


def _auto_candidate_group_signature(url: str) -> str:
    without_fragment = url.split("#", 1)[0]
    without_query, query = (
        without_fragment.split("?", 1)
        if "?" in without_fragment
        else (without_fragment, "")
    )
    without_scheme = (
        without_query.split("://", 1)[1]
        if "://" in without_query
        else without_query
    )
    host, path = (
        without_scheme.split("/", 1)
        if "/" in without_scheme
        else (without_scheme, "")
    )
    parts = [f"h:{_host_signature(host)}", f"p:{_path_signature(path)}"]
    if query:
        parts.append(f"q:{_query_signature(query)}")
    return "|".join(parts)


def _host_signature(host: str) -> str:
    labels = [part for part in host.split(".") if part]
    if len(labels) <= 2:
        return ".".join(label.lower() for label in labels)
    return ".".join(
        label.lower() if index + 2 >= len(labels) else _token_signature(label)
        for index, label in enumerate(labels)
    )


def _path_signature(path: str) -> str:
    return "/".join(
        _path_segment_signature(part)
        for part in path.split("/")
        if part
    )


def _path_segment_signature(segment: str) -> str:
    lower = segment.lower()
    if "." in lower:
        stem, ext = lower.rsplit(".", 1)
        if stem and ext and len(ext) <= 5:
            return f"{_token_signature(stem)}.{ext}"
    return _token_signature(lower)


def _query_signature(query: str) -> str:
    pairs: list[str] = []
    for part in query.split("&"):
        if not part:
            continue
        key, value = part.split("=", 1) if "=" in part else (part, "")
        pairs.append(f"{key.lower()}={_token_signature(value)}")
    pairs.sort()
    return "&".join(pairs)


def _token_signature(token: str) -> str:
    value = token.strip().lower()
    if not value:
        return "{}"
    if value.isdigit():
        return "{num}"
    if len(value) >= 8 and all(ch in "0123456789abcdef" for ch in value):
        return "{hex}"
    if _is_uuid_like(value):
        return "{uuid}"
    has_ascii_digit = any(ch.isdigit() for ch in value)
    has_ascii_alpha = any("a" <= ch <= "z" for ch in value)
    if has_ascii_digit and has_ascii_alpha:
        return "{id}"
    if has_ascii_alpha and len(value) <= 3:
        return "{short-alpha}"
    if len(value) >= 16 and all(_is_url_safe_token_char(ch) for ch in value):
        return "{token}"
    return value


def _is_uuid_like(value: str) -> bool:
    return [len(part) for part in value.split("-")] == [8, 4, 4, 4, 12]


def _is_url_safe_token_char(ch: str) -> bool:
    return ch.isascii() and (ch.isalnum() or ch in {"-", "_", "~"})


def _source_is_strong_image_context(source: str) -> bool:
    source = source.lower()
    return source.startswith(("img.", "source.", "css.", "poster", "media."))


def _url_has_image_signal(link: str) -> bool:
    parsed = urlparse(link)
    path = unquote(parsed.path or "").lower()
    query = unquote(parsed.query or "").lower()
    if path.endswith(
        (
            ".jpg",
            ".jpeg",
            ".png",
            ".webp",
            ".gif",
            ".bmp",
            ".tif",
            ".tiff",
            ".avif",
        )
    ):
        return True
    combined = f"{path}?{query}"
    image_tokens = (
        "image",
        "img",
        "photo",
        "picture",
        "thumbnail",
        "thumb",
        "cover",
        "scan",
        "sdownload",
        "resource",
    )
    if any(token in combined for token in image_tokens):
        return True
    return any(
        key in query
        for key in (
            "src=",
            "image=",
            "img=",
            "url=",
            "file=",
            "path=",
            "resource=",
        )
    )


def _looks_like_text_or_page_resource(link: str) -> bool:
    parsed = urlparse(link)
    path = unquote(parsed.path or "").lower()
    return path.endswith(
        (
            ".html",
            ".htm",
            ".shtml",
            ".xhtml",
            ".txt",
            ".text",
            ".json",
            ".xml",
            ".csv",
            ".tsv",
            ".md",
            ".markdown",
            ".yaml",
            ".yml",
            ".ini",
            ".log",
            ".svg",
        )
    )


def _is_http_redirect_status(status_code: int) -> bool:
    return 300 <= status_code < 400


def _debug_log(message: str, *args: object) -> None:
    if not VERBOSE_DOWNLOAD_LOG:
        return
    LOG.info(message, *args)
    try:
        formatted = message % args if args else message
    except Exception:  # noqa: BLE001
        formatted = f"{message} | args={args!r}"
    _emit_daemon_log("info", formatted)


def _emit_daemon_log(level: str, message: str) -> None:
    with EMIT_LOCK:
        sys.stdout.write(
            json.dumps(
                {
                    "event": "log",
                    "level": level,
                    "message": message,
                },
                ensure_ascii=False,
            )
            + "\n"
        )
        sys.stdout.flush()


def _decode_image_bytes(image_bytes: bytes, source: str) -> Image.Image:
    if not _looks_like_supported_image(image_bytes):
        _debug_log(
            "decode: rejected non-image payload from %s, first-bytes=%s",
            _short_link(source),
            image_bytes[:16].hex(" "),
        )
        raise NonImagePayloadError(f"Downloaded content does not look like an image: {source}")
    image = Image.open(BytesIO(image_bytes)).convert("RGB")
    if image.width <= 0 or image.height <= 0:
        raise RuntimeError(f"Invalid downloaded image from {source}")
    _debug_log(
        "decode: accepted %s as image %dx%d",
        _short_link(source),
        image.width,
        image.height,
    )
    return image


def _looks_like_supported_image(image_bytes: bytes) -> bool:
    if len(image_bytes) < 12:
        return False
    if image_bytes.startswith(b"\xff\xd8\xff"):
        return True
    if image_bytes.startswith(b"\x89PNG\r\n\x1a\n"):
        return True
    if image_bytes.startswith((b"GIF87a", b"GIF89a")):
        return True
    if image_bytes.startswith(b"BM"):
        return True
    if image_bytes.startswith((b"II*\x00", b"MM\x00*")):
        return True
    if image_bytes.startswith(b"RIFF") and image_bytes[8:12] == b"WEBP":
        return True
    return False


def _new_tab_attempt_key(link: str) -> str:
    parsed = urlparse(link)
    if not parsed.scheme or not parsed.netloc:
        return unquote(link)
    return urlunparse(
        (
            parsed.scheme.lower(),
            parsed.netloc.lower(),
            unquote(parsed.path or ""),
            parsed.params,
            parsed.query,
            "",
        )
    )


def _new_tab_direct_navigation_allowed(link: str) -> bool:
    parsed = urlparse(link)
    return parsed.scheme.lower() not in {"http", "https"}


def _looks_like_site_code_resource(link: str) -> bool:
    parsed = urlparse(link)
    path = (parsed.path or "").lower()
    if parsed.scheme in {"http", "https"} and path in {"", "/"} and not parsed.query:
        return True
    code_suffixes = (
        ".js",
        ".mjs",
        ".css",
        ".map",
        ".wasm",
    )
    return path.endswith(code_suffixes)


def _short_link(link: str, limit: int = 120) -> str:
    if len(link) <= limit:
        return link
    return link[: limit - 3] + "..."


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--daemon", action="store_true")
    return parser.parse_args()


def main() -> int:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")
    args = parse_args()
    if not args.daemon:
        print("This helper is intended to run with --daemon.", file=sys.stderr)
        return 2

    daemon = AdvancedFetchDaemon()
    try:
        return daemon.run()
    except SystemExit as exc:
        return int(exc.code or 0)
    except Exception as exc:  # noqa: BLE001
        traceback.print_exc()
        daemon._emit_error(
            user_message="Продвинутый выкачиватель завершился с ошибкой.",
            log_message=f"fatal helper error: {type(exc).__name__}: {exc}",
        )
        return 1
    finally:
        daemon.close()


if __name__ == "__main__":
    raise SystemExit(main())
