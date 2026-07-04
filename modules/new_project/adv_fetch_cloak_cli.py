"""
FILE OVERVIEW: modules/new_project/adv_fetch_cloak_cli.py
CloakBrowser/Playwright daemon for the advanced downloader used by the Rust launcher.

Main items:
- `CloakFetchDaemon`: owns a persistent CloakBrowser context and active Playwright page.
- Browser commands keep the same line-oriented JSON protocol as `adv_fetch_cli.py`.
- Image fetching prioritizes bytes already seen by the page, then page-context `fetch()`,
  DOM image readback, a temporary page, and finally an HTTP requests fallback with browser cookies.
- Canvas commands read `canvas.toDataURL()` from the active page and open shadow roots, and emit
  detailed diagnostics for canvas geometry, export status, decoded PNG dimensions, and black or
  transparent payloads.
- Deep intercept captures bytes from every layer (network/CDP, canvas readback, screenshots, plus
  observe-only page hooks for `URL.createObjectURL` and `OffscreenCanvas.convertToBlob` so DRM and
  descramble sites are covered) and finalizes them through a content pipeline: blank (single-colour)
  frames are dropped globally, per-element repeats collapsed by stable WeakMap id, records clustered
  by perceptual hash so one page seen through several layers is one page, the highest-fidelity
  representative is kept per cluster, pages are ordered by DOM/geometry/URL signals, and size-outlier
  pages are flagged as probable junk.

Protocol:
- `_handle_command({"command": ...})` runs one command and reports a single
  terminal event (plus interim `progress` events) through `self._emit`, the same
  contract as the Selenium helper.
- Driven in-process by the unified AI backend via
  `modules/ai_backend/browser/service.py` (`BrowserService`) over the framed IPC
  method `browser.command`; the `--daemon` stdin loop remains only as a manual fallback.
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
import time
import traceback
from dataclasses import dataclass, field
from io import BytesIO
from pathlib import Path
from typing import Any, Callable, Optional
from urllib.parse import quote, unquote, unquote_to_bytes, urljoin, urlparse, urlunparse

import requests
from PIL import Image

PROJECT_ROOT = Path(__file__).resolve().parents[2]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from config import VERSION  # noqa: E402
from modules.browser_f import get_origin  # noqa: E402
from modules.new_project.common import compile_wildcard_prefixes  # noqa: E402

LOG = logging.getLogger(__name__)
CONTROL_TRANSLATION = {code: None for code in range(0x00, 0x20)} | {0x7F: None}
EMIT_LOCK = threading.Lock()
VERBOSE_DOWNLOAD_LOG = True
BROWSER_FETCH_TIMEOUT_MS = 8000
DOWNLOAD_METHOD_MEMORY = "page-memory"
DOWNLOAD_METHOD_CURRENT_PAGE = "current-page"
DOWNLOAD_METHOD_DOM_IMAGE = "dom-image"
DOWNLOAD_METHOD_NEW_PAGE = "new-page"
DOWNLOAD_METHOD_REQUESTS = "requests"
# Max Hamming distance between dHash values for two captures to be treated as the
# same visual page during deep-intercept content clustering.
DEEP_CAPTURE_PHASH_MERGE_DISTANCE = 5
# Captures whose smaller side is below this many pixels are flagged as probable
# junk (icons, sprites, UI chrome) in the deep-intercept review.
DEEP_CAPTURE_MIN_PAGE_DIM = 64


class NonImagePayloadError(RuntimeError):
    """Raised when candidate bytes are definitely not an image."""


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
    page_url: str
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
    pattern: str
    max_parallel: int
    exclude_site_code_links: bool
    error_message: Optional[str] = None
    log_message: Optional[str] = None


@dataclass
class DeepCaptureState:
    stop_event: threading.Event
    lock: threading.Lock
    entries: list[dict[str, Any]]
    hashes: set[str]
    page_url: str
    output_dir: Path
    raw_dir: Path
    # Cumulative document order of page images/canvases, refreshed on every drain so a
    # page's reading order is recorded while its elements are still in the DOM (early
    # pages survive virtual-scroll recycling). Each entry is a ("image", url) or
    # ("canvas", str(weakmap_id)) key; once present a key keeps its slot and is never
    # moved or removed when it later leaves the page. Guarded by ``lock``.
    dom_order_keys: list[tuple[str, str]] = field(default_factory=list)


@dataclass
class DeepCaptureDomOrder:
    """Document-order index of capturable elements read once at stop time.

    `url_to_index` maps a resolved image URL (`<img>`/`<source>`) to its position in
    DOM traversal order; `element_to_index` maps a canvas WeakMap id to the same order
    space. Used to sort captured pages by their order of appearance in the page rather
    than by network arrival order or URL-embedded numbers.
    """
    url_to_index: dict[str, int]
    element_to_index: dict[int, int]


def _run_inline(fn: Callable[..., Any], *args: Any, **kwargs: Any) -> Any:
    """Passthrough browser-thread hook: run ``fn`` on the caller's thread.

    Default for standalone use (the ``run()`` stdin loop), where the main thread
    both owns the browser and drives every command. The in-process
    ``BrowserService`` overrides ``_run_on_browser_thread`` to marshal onto its
    single owner thread instead.
    """
    return fn(*args, **kwargs)


def _is_target_closed(exc: BaseException) -> bool:
    """True if ``exc`` is Playwright's "target/page/context closed" error.

    Playwright does not re-export ``TargetClosedError`` from ``sync_api``, and the
    daemon must not import Playwright at module load (it stays lazy for AI-only
    startup). So resolve the class lazily and fall back to a message match if the
    class is somehow unavailable.
    """
    try:
        from playwright._impl._errors import TargetClosedError

        if isinstance(exc, TargetClosedError):
            return True
    except Exception:  # noqa: BLE001 - Playwright not importable; use message match
        pass
    message = str(exc)
    return "has been closed" in message or "Target page, context or browser" in message


class CloakFetchDaemon:
    def __init__(self) -> None:
        self._context = None
        self._page = None
        self._profile_dir = PROJECT_ROOT / "modules" / "browser_profiles" / "cloak_profile"
        self._page_lock = threading.RLock()
        self._canvas_capture: Optional[CanvasCaptureState] = None
        self._intercept_active = False
        self._link_collect: Optional[LinkCollectState] = None
        self._link_collect_active = False
        self._deep_capture: Optional[DeepCaptureState] = None
        self._deep_capture_active = False
        self._deep_capture_script_installed = False
        self._context_response_listener_installed = False
        self._context_page_diagnostics_installed = False
        self._preferred_download_method: Optional[str] = None
        self._response_bodies: dict[str, tuple[bytes, str]] = {}
        self._response_cache_attached: set[int] = set()
        self._response_lock = threading.Lock()
        self._pending_responses: list[Any] = []
        self._pending_response_lock = threading.Lock()
        self._cdp_session: Optional[Any] = None
        self._cdp_response_meta: dict[str, dict[str, str]] = {}
        self._pending_cdp_finished: list[str] = []
        self._cdp_lock = threading.Lock()
        self._canvas_diag_seen_hashes: set[str] = set()
        self._page_diagnostics_attached: set[int] = set()
        # Whether the observe-only active-tab monitor context init script is installed.
        # The monitor timestamps each tab's last foreground transition so the active
        # tab can be resolved live, with no memory of past/first tabs. See
        # `_install_active_monitor_context` and `_resolve_active_page`.
        self._active_monitor_installed = False
        # Hook to run Playwright-touching work on the browser-owner thread. Default
        # passthrough keeps standalone behaviour; the in-process BrowserService
        # overrides it so the background link-collect loop's page calls run on the
        # one thread that owns the browser context (Playwright is thread-affine).
        self._run_on_browser_thread: Callable[..., Any] = _run_inline

    def run(self) -> int:
        self._emit({"event": "ready", "downloader_version": VERSION})
        for raw_line in sys.stdin:
            line = raw_line.strip()
            if not line:
                continue
            try:
                self._handle_command(json.loads(line))
            except SystemExit:
                raise
            except Exception as exc:  # noqa: BLE001
                self._emit_error(
                    user_message=str(exc) or "CloakBrowser-выкачиватель завершился с ошибкой.",
                    log_message=f"unexpected cloak daemon error: {type(exc).__name__}: {exc}",
                )
                LOG.exception("Unexpected cloak daemon error")
        self.close()
        return 0

    def close(self) -> None:
        self._stop_link_collect()
        self._stop_canvas_capture()
        self._clear_deep_capture_runtime()
        if self._context is not None:
            try:
                self._context.close()
            except Exception:  # noqa: BLE001
                LOG.exception("Failed to close CloakBrowser context")
        self._context = None
        self._page = None
        self._cdp_session = None
        self._cdp_response_meta.clear()
        self._pending_cdp_finished.clear()
        self._deep_capture_script_installed = False
        self._context_response_listener_installed = False
        self._context_page_diagnostics_installed = False
        self._active_monitor_installed = False
        self._page_diagnostics_attached.clear()
        self._intercept_active = False
        self._link_collect_active = False
        self._deep_capture_active = False

    def _handle_command(self, command: dict[str, Any]) -> None:
        command_name = str(command.get("command") or "").strip()
        if command_name == "shutdown":
            self.close()
            raise SystemExit(0)
        if command_name == "open_url":
            url = _normalize_http_url(str(command.get("url") or ""))
            current_url = self.open_url(url)
            self._emit({"event": "opened", "current_url": current_url})
            return
        if command_name == "fetch":
            result = self.fetch(
                str(command.get("pattern") or "").strip(),
                int(command.get("max_parallel") or 1),
            )
            self._emit_result("result", result)
            return
        if command_name == "fetch_auto_links":
            cancel_file = _optional_cancel_file(command.get("cancel_file"))
            result = self.fetch_auto_links(
                int(command.get("max_parallel") or 1),
                cancel_file,
            )
            self._emit({"event": "auto_result", **result})
            return
        if command_name == "start_link_collect":
            current_url = self.start_link_collect(
                str(command.get("pattern") or "").strip(),
                int(command.get("max_parallel") or 1),
                exclude_site_code_links=False,
            )
            self._emit({"event": "link_collect_started", "current_url": current_url})
            return
        if command_name == "start_auto_link_collect":
            current_url = self.start_link_collect(
                "",
                int(command.get("max_parallel") or 1),
                exclude_site_code_links=True,
            )
            self._emit({"event": "link_collect_started", "current_url": current_url})
            return
        if command_name == "stop_link_collect":
            self._emit_result("result", self.stop_link_collect(auto=False))
            return
        if command_name == "stop_auto_link_collect":
            cancel_file = _optional_cancel_file(command.get("cancel_file"))
            result = self.stop_auto_link_collect(cancel_file)
            self._emit({"event": "auto_result", **result})
            return
        if command_name == "link_collect_status":
            self._emit({"event": "link_collect_count", "found_links": self.link_collect_status()})
            return
        if command_name == "fetch_canvas":
            self._emit_result("result", self.fetch_canvas())
            return
        if command_name == "start_intercept":
            current_url = self.start_intercept()
            self._emit({"event": "intercept_started", "current_url": current_url})
            return
        if command_name == "stop_intercept":
            self._emit_result("result", self.stop_intercept())
            return
        if command_name == "intercept_status":
            self._emit({"event": "intercept_count", "found_pages": self.intercept_status()})
            return
        if command_name == "start_deep_intercept":
            current_url = self.start_deep_intercept()
            self._emit({"event": "intercept_started", "current_url": current_url})
            return
        if command_name == "deep_intercept_status":
            status = self.deep_intercept_status()
            self._emit(
                {
                    "event": "intercept_count",
                    "found_pages": status["total"],
                    "found_canvases": status["canvases"],
                    "found_images": status["images"],
                }
            )
            return
        if command_name == "stop_deep_intercept":
            cancel_file = _optional_cancel_file(command.get("cancel_file"))
            result = self.stop_deep_intercept(cancel_file)
            self._emit({"event": "auto_result", **result})
            return
        if command_name == "scroll_page":
            self.scroll_page()
            self._emit({"event": "scrolled"})
            return
        raise RuntimeError(f"Unknown command: {command_name}")

    def open_url(self, url: str) -> str:
        self._ensure_browser()
        self._emit_progress("browser", 0, 0)
        with self._page_lock:
            page = self._require_page()
            page.goto(url, wait_until="domcontentloaded", timeout=60_000)
            try:
                page.wait_for_load_state("networkidle", timeout=15_000)
            except Exception:  # noqa: BLE001
                LOG.debug("CloakBrowser page did not reach networkidle", exc_info=True)
            return str(page.url or url)

    def scroll_page(self) -> None:
        with self._page_lock:
            page = self._require_page()
            prev_height = int(page.evaluate("() => document.body ? document.body.scrollHeight : 0") or 0)
            for pct in (0, 40, 80, 100):
                page.evaluate("(pct) => window.scrollTo(0, document.body.scrollHeight * pct / 100)", pct)
                page.wait_for_timeout(300)
            for _ in range(40):
                new_height = int(page.evaluate("() => document.body ? document.body.scrollHeight : 0") or 0)
                if new_height <= prev_height:
                    break
                prev_height = new_height
                for pct in (100, 80, 40, 0, 40, 80, 100):
                    page.evaluate("(pct) => window.scrollTo(0, document.body.scrollHeight * pct / 100)", pct)
                    page.wait_for_timeout(200)

    def fetch(self, pattern: str, max_parallel: int = 1) -> FetchResult:
        page_url = self._active_page_url("fetch")
        self._emit_progress("collect", 0, 0)
        candidates = self._collect_candidates(page_url)
        filtered = self._filter_candidates(candidates, pattern)
        return self._download_candidate_links(
            filtered,
            page_url,
            temp_prefix="mangafucker_adv_cloak_fetch_",
            max_parallel=max_parallel,
        )

    def fetch_auto_links(
        self,
        max_parallel: int = 1,
        cancel_file: Optional[Path] = None,
    ) -> dict[str, Any]:
        page_url = self._active_page_url("auto")
        self._emit_progress("collect", 0, 0)
        candidates = self._collect_auto_candidate_links(page_url)
        return self._download_auto_candidate_links(
            candidates,
            page_url,
            temp_prefix="mangafucker_adv_cloak_auto_fetch_",
            max_parallel=max_parallel,
            cancel_file=cancel_file,
        )

    def start_link_collect(
        self,
        pattern: str,
        max_parallel: int,
        *,
        exclude_site_code_links: bool,
    ) -> str:
        self._ensure_browser()
        if self._link_collect is not None or self._link_collect_active:
            raise RuntimeError("Сбор ссылок уже запущен.")
        if self._canvas_capture is not None or self._intercept_active:
            raise RuntimeError("Сначала завершите текущий перехват Canvas.")

        page_url = self._active_page_url("collect")
        stop_event = threading.Event()
        collect_lock = threading.Lock()
        worker = threading.Thread(
            target=self._collect_links_loop,
            args=(stop_event, collect_lock),
            daemon=True,
            name="mangafucker-cloak-link-collect",
        )
        self._link_collect = LinkCollectState(
            stop_event=stop_event,
            lock=collect_lock,
            links=[],
            seen_links=set(),
            worker=worker,
            page_url=page_url,
            pattern=pattern,
            max_parallel=max_parallel,
            exclude_site_code_links=exclude_site_code_links,
        )
        self._link_collect_active = True
        self._emit_progress("collect", 0, 0)
        worker.start()
        return page_url

    def stop_link_collect(self, *, auto: bool) -> FetchResult:
        collect = self._finish_link_collect()
        if auto:
            raise RuntimeError("Internal error: use stop_auto_link_collect for auto mode.")
        return self._download_candidate_links(
            collect.links,
            self._current_url_or(collect.page_url),
            temp_prefix="mangafucker_adv_cloak_collect_",
            max_parallel=collect.max_parallel,
        )

    def stop_auto_link_collect(self, cancel_file: Optional[Path] = None) -> dict[str, Any]:
        collect = self._finish_link_collect()
        return self._download_auto_candidate_links(
            collect.links,
            self._current_url_or(collect.page_url),
            temp_prefix="mangafucker_adv_cloak_auto_collect_",
            max_parallel=collect.max_parallel,
            cancel_file=cancel_file,
        )

    def link_collect_status(self) -> int:
        collect = self._link_collect
        if collect is None or not self._link_collect_active:
            return 0
        with collect.lock:
            return len(collect.links)

    def fetch_canvas(self) -> FetchResult:
        self._ensure_browser()
        if self._canvas_capture is not None or self._intercept_active:
            raise RuntimeError("Сначала завершите текущий перехват Canvas.")
        self._reset_canvas_diagnostics()
        page_url = self._active_page_url("canvas")
        self._emit_progress("collect_canvas", 0, 0)
        canvas_entries = self._collect_canvas_entries()
        if not canvas_entries:
            raise RuntimeError("Canvas на текущей странице не найдены.")
        output_dir = Path(tempfile.mkdtemp(prefix="mangafucker_adv_cloak_canvas_fetch_"))
        saved_count = self._save_canvas_entries(canvas_entries, output_dir)
        if saved_count == 0:
            shutil.rmtree(output_dir, ignore_errors=True)
            raise RuntimeError("Canvas на текущей странице не удалось сохранить.")
        return FetchResult(page_url=page_url, output_dir=output_dir, downloaded_images=saved_count)

    def start_intercept(self) -> str:
        self._ensure_browser()
        if self._canvas_capture is not None or self._intercept_active:
            raise RuntimeError("Перехват Canvas уже запущен.")
        self._reset_canvas_diagnostics()
        page_url = self._active_page_url("intercept")
        stop_event = threading.Event()
        capture_lock = threading.Lock()
        self._canvas_capture = CanvasCaptureState(
            stop_event=stop_event,
            lock=capture_lock,
            entries=[],
            hashes=set(),
            page_url=page_url,
        )
        self._intercept_active = True
        self._emit_progress("collect_canvas", 0, 0)
        return page_url

    def stop_intercept(self) -> FetchResult:
        capture = self._canvas_capture
        if capture is None or not self._intercept_active:
            raise RuntimeError("Перехват ещё не запущен.")
        self._emit_progress("collect_canvas", 0, 0)
        capture.stop_event.set()
        self._capture_canvas_updates_once(capture, capture.lock)
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
        output_dir = Path(tempfile.mkdtemp(prefix="mangafucker_adv_cloak_canvas_intercept_"))
        saved_count = self._save_canvas_entries(canvas_entries, output_dir)
        if saved_count == 0:
            shutil.rmtree(output_dir, ignore_errors=True)
            raise RuntimeError("Не удалось сохранить Canvas из перехвата.")
        return FetchResult(
            page_url=self._current_url_or(capture.page_url),
            output_dir=output_dir,
            downloaded_images=saved_count,
        )

    def intercept_status(self) -> int:
        capture = self._canvas_capture
        if capture is None or not self._intercept_active:
            return 0
        self._capture_canvas_updates_once(capture, capture.lock)
        with capture.lock:
            return len(capture.entries)

    def start_deep_intercept(self) -> str:
        self._ensure_browser()
        if self._deep_capture is not None or self._deep_capture_active:
            raise RuntimeError("Глубокий перехват уже запущен.")
        if self._link_collect is not None or self._link_collect_active:
            raise RuntimeError("Сначала завершите текущий сбор ссылок.")
        if self._canvas_capture is not None or self._intercept_active:
            raise RuntimeError("Сначала завершите текущий перехват Canvas.")

        output_dir = Path(tempfile.mkdtemp(prefix="mangafucker_adv_cloak_deep_"))
        raw_dir = output_dir / "_raw"
        raw_dir.mkdir(parents=True, exist_ok=True)
        stop_event = threading.Event()
        self._deep_capture = DeepCaptureState(
            stop_event=stop_event,
            lock=threading.Lock(),
            entries=[],
            hashes=set(),
            page_url="",
            output_dir=output_dir,
            raw_dir=raw_dir,
        )
        self._deep_capture_active = True

        def prepare(page: Any) -> None:
            # Install the observe-only capture hooks before the reload so the reloaded
            # document is captured from its first byte.
            self._install_deep_capture_init_script(page)
            self._attach_context_response_cache()
            self._attach_response_cache(page)
            self._attach_cdp_network_capture(page)
            self._emit_progress("browser", 0, 0)

        # Resolve the active tab and reload it, re-resolving if the user closes/switches
        # the tab mid-reload. Roll back the "started" state on any failure so a retry is
        # not permanently rejected with "Глубокий перехват уже запущен."
        try:
            page = self._reload_capture_page(prepare)
        except Exception:
            self._clear_deep_capture_runtime()
            raise
        page_url = str(page.url or "").strip()
        capture = self._deep_capture
        if capture is not None:
            capture.page_url = page_url
        self._emit_progress("collect", 0, 0)
        return page_url

    def _reload_capture_page(self, prepare: Callable[[Any], None]) -> Any:
        """Resolve the active tab, run ``prepare(page)`` on it, and reload it.

        Re-resolves the active tab and retries (bounded) if the tab is closed mid-reload
        because the user switched/closed tabs. Raises the standard "open a chapter" error
        if the active tab has no real URL, and re-raises any non-close failure unchanged.
        Returns the reloaded, still-live page.
        """
        last_exc: Optional[Exception] = None
        for _ in range(3):
            page = self._resolve_active_page("deep")
            if page.is_closed():
                continue
            try:
                with self._page_lock:
                    page_url = str(page.url or "").strip()
                    if not page_url or page_url in {"about:blank", "data:,"}:
                        raise RuntimeError("Сначала откройте страницу главы в CloakBrowser.")
                    prepare(page)
                    page.reload(wait_until="domcontentloaded", timeout=60_000)
                    try:
                        page.wait_for_load_state("networkidle", timeout=10_000)
                    except Exception:  # noqa: BLE001
                        LOG.debug("Deep capture page did not reach networkidle", exc_info=True)
                return page
            except Exception as exc:  # noqa: BLE001
                if not _is_target_closed(exc):
                    raise
                last_exc = exc
                LOG.debug(
                    "active tab closed during deep-capture reload; re-resolving", exc_info=True
                )
        if last_exc is not None:
            raise last_exc
        raise RuntimeError("Активная вкладка закрылась во время запуска перехвата.")

    def deep_intercept_status(self) -> dict[str, int]:
        """Live count of what deep capture has gathered so far, split by kind.

        Returns the raw captured-payload totals (not the deduped/clustered page count
        produced at stop): ``canvases`` are canvas-element captures, ``images`` is
        everything else (plain ``<img>`` reads, network bytes, blob/descramble exports),
        and ``total`` is their sum.
        """
        capture = self._deep_capture
        if capture is None or not self._deep_capture_active:
            return {"total": 0, "canvases": 0, "images": 0}
        self._capture_deep_updates_once()
        with capture.lock:
            canvases = 0
            images = 0
            for entry in capture.entries:
                if _deep_capture_is_canvas_source(str(entry.get("source") or "")):
                    canvases += 1
                else:
                    images += 1
            return {"total": canvases + images, "canvases": canvases, "images": images}

    def stop_deep_intercept(self, cancel_file: Optional[Path] = None) -> dict[str, Any]:
        capture = self._deep_capture
        if capture is None or not self._deep_capture_active:
            raise RuntimeError("Глубокий перехват ещё не запущен.")

        self._emit_progress("collect", 0, 0)
        capture.stop_event.set()
        self._capture_deep_updates_once()
        self._settle_deep_image_reads()
        self._accumulate_deep_dom_order()
        self._capture_visible_canvas_screenshots_if_needed(capture)
        dom_order = self._finalize_dom_capture_order(capture)
        with capture.lock:
            entries = list(capture.entries)
            page_url = self._current_url_or(capture.page_url)
            output_dir = capture.output_dir

        self._clear_deep_capture_runtime()
        return self._build_auto_result_from_deep_entries(
            entries, page_url, output_dir, cancel_file, dom_order
        )

    def _read_dom_order_keys(self) -> list[tuple[str, str]]:
        """Read the current document order of page images/canvases as ordered keys."""
        try:
            with self._page_lock:
                raw = self._require_page().evaluate(COLLECT_DOM_IMAGE_ORDER_JS)
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to read DOM image order during deep capture", exc_info=True)
            return []
        return _deep_capture_dom_keys_from_raw(raw)

    def _accumulate_deep_dom_order(self) -> None:
        """Record the page's current document order, in first-seen (chronological) order.

        Called on every drain so a page's reading order is captured while its elements are
        still attached, then preserved after a virtual-scroll reader recycles them out of
        the DOM. Accumulation is plain first-seen append (no anchor-based insertion), which
        keeps groups in reading order even when consecutive polls catch non-overlapping
        windows. The live order is reconciled with the current DOM at stop.
        """
        capture = self._deep_capture
        if capture is None or not self._deep_capture_active:
            return
        current_keys = self._read_dom_order_keys()
        if not current_keys:
            return
        with capture.lock:
            seen = set(capture.dom_order_keys)
            _append_first_seen_keys(capture.dom_order_keys, seen, current_keys)

    def _finalize_dom_capture_order(self, capture: DeepCaptureState) -> DeepCaptureDomOrder:
        """Build the document-order index, prepending pages that left the DOM by stop."""
        stop_keys = self._read_dom_order_keys()
        with capture.lock:
            seen_keys = list(capture.dom_order_keys)
        keys = _combine_dom_order(seen_keys, stop_keys)
        url_to_index: dict[str, int] = {}
        element_to_index: dict[int, int] = {}
        for order, (kind, value) in enumerate(keys):
            if kind == "image":
                if value and value not in url_to_index:
                    url_to_index[value] = order
            elif kind == "canvas":
                try:
                    element_id = int(value)
                except (TypeError, ValueError):
                    continue
                if element_id > 0 and element_id not in element_to_index:
                    element_to_index[element_id] = order
        _debug_log(
            "cloak deep capture: finalized DOM order for %d image url(s), %d canvas element(s)",
            len(url_to_index),
            len(element_to_index),
        )
        return DeepCaptureDomOrder(url_to_index, element_to_index)

    def _ensure_browser(self) -> None:
        if self._context is not None and self._browser_session_alive():
            return
        if self._context is not None:
            # The cached context is dead (the user closed CloakBrowser); tear it down
            # so a fresh one launches below instead of reusing a disconnected session.
            self.close()
        self._emit_progress("browser", 0, 0)
        try:
            from cloakbrowser import launch_persistent_context
        except ImportError as exc:
            raise RuntimeError(
                "CloakBrowser не установлен в Python-окружении. Установите пакет: pip install cloakbrowser"
            ) from exc

        self._profile_dir.mkdir(parents=True, exist_ok=True)
        self._context = launch_persistent_context(
            str(self._profile_dir),
            headless=False,
            humanize=True,
            viewport={"width": 1280, "height": 900},
            service_workers="block",
        )
        self._attach_context_page_diagnostics()
        self._page = self._context.new_page()
        self._attach_page_diagnostics(self._page)
        # Install the active-tab monitor now (browser is foreground at launch), so
        # every subsequent user tab switch is recorded and the active tab can be
        # resolved live without any first-tab/tracked-tab memory.
        self._install_active_monitor_context()
        self._stop_link_collect()
        self._stop_canvas_capture()
        self._intercept_active = False
        self._link_collect_active = False
        self._deep_capture_active = False

    def _browser_session_alive(self) -> bool:
        """Report whether the cached CloakBrowser context still has a usable page.

        When the user closes CloakBrowser (or the process dies) Playwright marks every
        page of the context closed, so the absence of any live page is a reliable
        "session is dead" signal. If only the active tab was closed but the context is
        still alive, adopt the most recently opened live page so commands keep working.
        """
        context = self._context
        if context is None:
            return False
        try:
            live_pages = [page for page in context.pages if not page.is_closed()]
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to inspect CloakBrowser context liveness", exc_info=True)
            return False
        if not live_pages:
            return False
        if self._page is None or self._page.is_closed():
            self._page = live_pages[-1]
            self._attach_page_diagnostics(self._page)
        return True

    def _require_page(self):
        self._ensure_browser()
        if self._page is None:
            raise RuntimeError("Сначала откройте страницу главы в CloakBrowser.")
        if getattr(self._page, "is_closed", lambda: False)():
            pages = [page for page in self._context.pages if not page.is_closed()]
            self._page = pages[-1] if pages else self._context.new_page()
            self._attach_page_diagnostics(self._page)
        return self._page

    def _attach_response_cache(self, page: Any) -> None:
        page_id = id(page)
        if page_id in self._response_cache_attached:
            return
        try:
            page.on("response", self._remember_response_body)
            self._response_cache_attached.add(page_id)
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to attach response cache listener", exc_info=True)

    def _attach_context_page_diagnostics(self) -> None:
        if self._context is None:
            return
        if not self._context_page_diagnostics_installed:
            try:
                self._context.on("page", self._attach_page_diagnostics)
                self._context_page_diagnostics_installed = True
            except Exception:  # noqa: BLE001
                LOG.debug("Failed to attach context page diagnostics listener", exc_info=True)
        try:
            for page in self._context.pages:
                if not page.is_closed():
                    self._attach_page_diagnostics(page)
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to attach diagnostics to existing context pages", exc_info=True)

    def _attach_page_diagnostics(self, page: Any) -> None:
        page_id = id(page)
        if page_id in self._page_diagnostics_attached:
            return
        try:
            page.on("console", self._emit_page_console_diagnostic)
            page.on("pageerror", self._emit_page_error_diagnostic)
            self._page_diagnostics_attached.add(page_id)
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to attach page diagnostics listeners", exc_info=True)

    def _emit_page_console_diagnostic(self, message: Any) -> None:
        try:
            message_type = str(getattr(message, "type", "") or "").lower()
            if message_type not in {"warning", "warn", "error"}:
                return
            text = str(getattr(message, "text", "") or "").strip()
            location = getattr(message, "location", None)
            location_text = _format_console_location(location)
            level = "error" if message_type == "error" else "warn"
            suffix = f" ({location_text})" if location_text else ""
            self._emit_log(level, f"web page console {message_type}: {text}{suffix}")
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to emit page console diagnostic", exc_info=True)

    def _emit_page_error_diagnostic(self, error: Any) -> None:
        try:
            text = str(error or "").strip() or repr(error)
            self._emit_log("error", f"web page error: {text}")
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to emit page error diagnostic", exc_info=True)

    def _attach_context_response_cache(self) -> None:
        if self._context is None or self._context_response_listener_installed:
            return
        try:
            self._context.on("response", self._remember_response_body)
            self._context_response_listener_installed = True
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to attach context response cache listener", exc_info=True)

    def _remember_response_body(self, response: Any) -> None:
        if not self._deep_capture_active:
            return
        with self._pending_response_lock:
            self._pending_responses.append(response)

    def _drain_pending_response_bodies(self) -> None:
        with self._pending_response_lock:
            pending = self._pending_responses
            self._pending_responses = []
        for response in pending:
            self._cache_response_body_on_owner_thread(response)
        self._drain_pending_cdp_response_bodies()

    def _attach_cdp_network_capture(self, page: Any) -> None:
        if self._context is None:
            return
        try:
            session = self._context.new_cdp_session(page)
            session.on("Network.responseReceived", self._remember_cdp_response_meta)
            session.on("Network.loadingFinished", self._remember_cdp_loading_finished)
            session.send("Network.enable")
            self._cdp_session = session
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to attach CDP network capture", exc_info=True)

    def _remember_cdp_response_meta(self, payload: dict[str, Any]) -> None:
        if not self._deep_capture_active:
            return
        try:
            request_id = str(payload.get("requestId") or "")
            response = payload.get("response")
            if not request_id or not isinstance(response, dict):
                return
            headers = response.get("headers") if isinstance(response.get("headers"), dict) else {}
            content_type = str(
                response.get("mimeType")
                or headers.get("content-type")
                or headers.get("Content-Type")
                or ""
            )
            with self._cdp_lock:
                self._cdp_response_meta[request_id] = {
                    "url": str(response.get("url") or ""),
                    "content_type": content_type,
                }
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to remember CDP response metadata", exc_info=True)

    def _remember_cdp_loading_finished(self, payload: dict[str, Any]) -> None:
        if not self._deep_capture_active:
            return
        request_id = str(payload.get("requestId") or "")
        if not request_id:
            return
        with self._cdp_lock:
            self._pending_cdp_finished.append(request_id)

    def _drain_pending_cdp_response_bodies(self) -> None:
        session = self._cdp_session
        if session is None:
            return
        with self._cdp_lock:
            request_ids = self._pending_cdp_finished
            self._pending_cdp_finished = []
            metadata = dict(self._cdp_response_meta)
        for request_id in request_ids:
            meta = metadata.get(request_id)
            if meta is None:
                continue
            content_type = meta.get("content_type", "")
            response_url = meta.get("url", "")
            is_image_candidate = content_type.startswith("image/") or re.search(
                r"\.(png|jpe?g|webp|avif|gif|bmp|tiff?)(\?|$)",
                response_url,
                re.I,
            )
            if not is_image_candidate:
                continue
            try:
                result = session.send("Network.getResponseBody", {"requestId": request_id})
            except Exception:  # noqa: BLE001
                continue
            body_value = result.get("body") if isinstance(result, dict) else None
            if not isinstance(body_value, str) or not body_value:
                continue
            try:
                body = (
                    base64.b64decode(body_value)
                    if bool(result.get("base64Encoded"))
                    else body_value.encode("utf-8")
                )
            except Exception:  # noqa: BLE001
                LOG.debug("Failed to decode CDP response body", exc_info=True)
                continue
            self._remember_deep_capture_bytes(
                source="cdp-network",
                url=response_url,
                body=body,
                content_type=content_type,
            )
            if is_image_candidate:
                with self._response_lock:
                    self._response_bodies[response_url] = (body, content_type)

    def _cache_response_body_on_owner_thread(self, response: Any) -> None:
        try:
            content_type = str(response.headers.get("content-type", ""))
            if not response.ok:
                return
            response_url = str(response.url or "")
            is_image_candidate = content_type.startswith("image/") or re.search(
                r"\.(png|jpe?g|webp|avif|gif|bmp|tiff?)(\?|$)",
                response_url,
                re.I,
            )
            if not is_image_candidate:
                return
            body = response.body()
            if not body:
                return
            self._remember_deep_capture_bytes(
                source="network",
                url=response_url,
                body=body,
                content_type=content_type,
            )
            if not is_image_candidate:
                return
            with self._response_lock:
                self._response_bodies[response_url] = (body, content_type)
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to cache CloakBrowser response body", exc_info=True)

    def _install_deep_capture_init_script(self, page: Any) -> None:
        if self._context is None:
            return
        if not self._deep_capture_script_installed:
            try:
                self._context.add_init_script(DEEP_CAPTURE_INIT_JS)
                self._deep_capture_script_installed = True
            except Exception:  # noqa: BLE001
                LOG.debug("Failed to install deep capture context init script", exc_info=True)
        if page is not None:
            try:
                page.add_init_script(DEEP_CAPTURE_INIT_JS)
            except Exception:  # noqa: BLE001
                LOG.debug("Failed to install deep capture page init script", exc_info=True)

    def _remember_deep_capture_bytes(
        self,
        *,
        source: str,
        url: str,
        body: bytes,
        content_type: str,
        metadata: Optional[dict[str, Any]] = None,
    ) -> None:
        capture = self._deep_capture
        if capture is None or not self._deep_capture_active or not body:
            return
        digest = hashlib.sha256(body).hexdigest()
        with capture.lock:
            if digest in capture.hashes:
                return
            capture.hashes.add(digest)
            raw_name = f"{len(capture.entries) + 1:06}_{digest[:16]}.bin"
            raw_path = capture.raw_dir / raw_name
            raw_path.write_bytes(body)
            capture.entries.append(
                {
                    "order": len(capture.entries),
                    "source": source,
                    "url": url,
                    "content_type": content_type,
                    "sha256": digest,
                    "raw_path": str(raw_path),
                    "size": len(body),
                    "metadata": metadata or {},
                }
            )

    def _drain_deep_capture_page_events(self) -> None:
        capture = self._deep_capture
        if capture is None or not self._deep_capture_active:
            return
        with self._page_lock:
            try:
                raw_events = self._require_page().evaluate(DRAIN_DEEP_CAPTURE_JS)
            except Exception:  # noqa: BLE001
                LOG.debug("Failed to drain deep capture page events", exc_info=True)
                return
        if not isinstance(raw_events, list):
            return
        for item in raw_events:
            if not isinstance(item, dict):
                continue
            payload = item.get("data")
            if not isinstance(payload, str) or not payload:
                continue
            try:
                body = base64.b64decode(payload)
            except Exception:  # noqa: BLE001
                LOG.debug("Failed to decode deep capture page payload", exc_info=True)
                continue
            self._remember_deep_capture_bytes(
                source=str(item.get("source") or "page"),
                url=str(item.get("url") or ""),
                body=body,
                content_type=str(item.get("content_type") or ""),
                metadata=_deep_capture_item_metadata(item),
            )

    def _capture_deep_element_snapshots(self) -> None:
        capture = self._deep_capture
        if capture is None or not self._deep_capture_active:
            return
        with self._page_lock:
            try:
                raw_events = self._require_page().evaluate(COLLECT_DEEP_ELEMENT_SNAPSHOTS_JS)
            except Exception:  # noqa: BLE001
                LOG.debug("Failed to collect deep capture element snapshots", exc_info=True)
                return
        if not isinstance(raw_events, list):
            return
        for item in raw_events:
            if not isinstance(item, dict):
                continue
            data_url = item.get("data")
            if not isinstance(data_url, str) or not data_url.startswith("data:"):
                continue
            try:
                body, content_type = _decode_data_url_bytes(data_url)
            except Exception:  # noqa: BLE001
                LOG.debug("Failed to decode deep capture element snapshot", exc_info=True)
                continue
            self._remember_deep_capture_bytes(
                source=str(item.get("source") or "element"),
                url=str(item.get("url") or ""),
                body=body,
                content_type=content_type,
                metadata=_deep_capture_item_metadata(item),
            )

    def _capture_deep_updates_once(self) -> None:
        self._drain_pending_response_bodies()
        self._drain_deep_capture_page_events()
        self._capture_deep_element_snapshots()
        self._accumulate_deep_dom_order()

    def _settle_deep_image_reads(self) -> None:
        """Give in-flight `<img>` reads kicked off by the page scan time to resolve.

        ``scanImages`` fetches each rendered image asynchronously, so the very last
        scan (and any reads still pending when the user stops) only land in the page
        buffer a moment later. Drain a few more times with short waits so a fast
        start -> stop still captures every plain <img> on the page.
        """
        for _ in range(6):
            if not self._deep_capture_active:
                break
            time.sleep(0.15)
            self._drain_deep_capture_page_events()

    def _capture_visible_canvas_screenshots_if_needed(self, capture: DeepCaptureState) -> None:
        with self._page_lock:
            try:
                handles = self._require_page().query_selector_all("canvas")
            except Exception:  # noqa: BLE001
                LOG.debug("Failed to query canvas elements for deep capture screenshots", exc_info=True)
                return
            for index, handle in enumerate(handles):
                try:
                    box = handle.bounding_box()
                    if not box or box.get("width", 0) <= 1 or box.get("height", 0) <= 1:
                        continue
                    body = handle.screenshot(type="png")
                except Exception:  # noqa: BLE001
                    LOG.debug("Failed to screenshot canvas %d during deep capture", index, exc_info=True)
                    continue
                self._remember_deep_capture_bytes(
                    source="canvas-screenshot",
                    url=f"{self._current_url_or(capture.page_url)}#canvas-{index}",
                    body=body,
                    content_type="image/png",
                    metadata={"dom_order": index, "element": "canvas"},
                )

    def _decode_deep_capture_records(
        self,
        entries: list[dict[str, Any]],
        cancel_file: Optional[Path],
    ) -> list[dict[str, Any]]:
        """Decode captured payloads into scored records, dropping exact duplicates.

        Each record keeps the original entry, decoded RGB image, capture order, a
        blank-frame flag, and a perceptual hash used later for content clustering.
        Cancellation via `cancel_file` stops decoding early but keeps what was decoded.
        """
        records: list[dict[str, Any]] = []
        image_hashes: set[str] = set()
        total = len(entries)
        for index, entry in enumerate(entries):
            if _cancel_requested(cancel_file):
                break
            self._emit_progress("download", index + 1, total)
            raw_path_value = entry.get("raw_path")
            if not isinstance(raw_path_value, str):
                continue
            raw_path = Path(raw_path_value)
            try:
                body = raw_path.read_bytes()
                image = _decode_image_bytes(body, str(entry.get("url") or raw_path))
            except Exception:  # noqa: BLE001
                continue
            image_digest = _image_exact_digest(image)
            if image_digest in image_hashes:
                _debug_log(
                    "cloak deep decode: [%d/%d] exact-duplicate dropped source=%s %dx%d url=%s",
                    index + 1,
                    total,
                    str(entry.get("source") or "?"),
                    image.width,
                    image.height,
                    _short_link(str(entry.get("url") or "")),
                )
                continue
            image_hashes.add(image_digest)
            stats = _blank_stats(image)
            blank = _image_looks_blank(image)
            _debug_log(
                (
                    "cloak deep decode: [%d/%d] source=%s %dx%d lo=%.0f hi=%.0f "
                    "dark=%.4f light=%.4f blank=%s phash=%016x url=%s"
                ),
                index + 1,
                total,
                str(entry.get("source") or "?"),
                image.width,
                image.height,
                stats["lo"],
                stats["hi"],
                stats["dark_frac"],
                stats["light_frac"],
                blank,
                _image_dhash(image),
                _short_link(str(entry.get("url") or "")),
            )
            records.append(
                {
                    "entry": entry,
                    "image": image,
                    "source_index": index,
                    "blank": blank,
                    "phash": _image_dhash(image),
                }
            )
        return records

    def _build_auto_result_from_deep_entries(
        self,
        entries: list[dict[str, Any]],
        page_url: str,
        output_dir: Path,
        cancel_file: Optional[Path],
        dom_order: Optional[DeepCaptureDomOrder] = None,
    ) -> dict[str, Any]:
        """Turn captured deep-intercept payloads into an ordered auto-review result.

        Pipeline: decode payloads, drop blank (single-colour) frames globally so an
        off-screen never-rendered canvas can never become a black page, collapse
        repeated frames of the same DOM element, cluster the rest by visual content
        so one page captured through several layers (network bytes, canvas readback,
        screenshot) becomes a single page, pick the highest-fidelity representative
        per cluster, order pages by DOM/geometry/URL signals, and flag size-outlier
        pages as probable junk for the review UI.

        Raises RuntimeError if nothing decodable or only blank frames were captured.
        """
        records = self._decode_deep_capture_records(entries, cancel_file)
        if not records:
            shutil.rmtree(output_dir, ignore_errors=True)
            raise RuntimeError("Глубокий перехват не нашёл декодируемых изображений.")

        decoded_count = len(records)
        records = _drop_blank_deep_records(records)
        dropped_blank = decoded_count - len(records)
        if dropped_blank > 0:
            _debug_log("cloak deep capture: dropped %d blank (near-uniform) frame(s)", dropped_blank)
        if not records:
            shutil.rmtree(output_dir, ignore_errors=True)
            raise RuntimeError("Глубокий перехват нашёл только пустые (одноцветные) кадры.")

        collapsed = _collapse_deep_capture_dom_updates(records)
        clusters = _cluster_deep_records_by_content(collapsed)
        representatives = [_select_cluster_representative(cluster) for cluster in clusters]
        representatives.sort(
            key=lambda record: _deep_capture_sort_key(
                record["entry"], record["image"], record["source_index"], dom_order
            )
        )
        _assign_deep_capture_confidence(representatives)
        _debug_log(
            "cloak deep capture: %d payload(s) -> %d decoded -> %d blank dropped -> %d page(s) (%d flagged as probable junk)",
            len(entries),
            decoded_count,
            dropped_blank,
            len(representatives),
            sum(1 for record in representatives if record.get("probable_junk")),
        )
        for page_index, record in enumerate(representatives):
            image = record["image"]
            stats = _blank_stats(image)
            _debug_log(
                "cloak deep page: #%d source=%s %dx%d dark=%.4f light=%.4f probable_junk=%s",
                page_index + 1,
                str(record["entry"].get("source") or "?"),
                image.width,
                image.height,
                stats["dark_frac"],
                stats["light_frac"],
                bool(record.get("probable_junk")),
            )

        items: list[dict[str, Any]] = []
        downloaded = 0
        for record in representatives:
            entry = record["entry"]
            image = record["image"]
            downloaded += 1
            file_name = f"{downloaded:04}.png"
            image.save(output_dir / file_name, format="PNG")
            source = str(entry.get("source") or "unknown")
            original_url = str(entry.get("url") or "").strip()
            url = _deep_capture_review_url(downloaded, source, original_url)
            items.append(
                {
                    "order": downloaded - 1,
                    "url": url,
                    "file_name": file_name,
                    "width": image.width,
                    "height": image.height,
                    "probable_junk": bool(record.get("probable_junk")),
                }
            )
        return {
            "page_url": page_url,
            "output_dir": str(output_dir),
            "downloaded_images": downloaded,
            "items": items,
        }

    def _current_url_or(self, default: str) -> str:
        with self._page_lock:
            page = self._require_page()
            current_url = str(page.url or "").strip()
            return current_url if current_url and current_url not in {"about:blank", "data:,"} else default

    def _valid_pages(self) -> list[Any]:
        self._ensure_browser()
        return [page for page in self._context.pages if not page.is_closed()]

    def _install_active_monitor_context(self) -> None:
        """Install the observe-only active-tab monitor once per context.

        Adds `ACTIVE_MONITOR_JS` as a context init script (runs on every future
        document load, after CloakBrowser's own stealth scripts) and seeds any tabs
        already open at launch (restored persistent-profile tabs never get the init
        script because they are not navigated). The monitor only adds passive
        listeners and one `window` object — no prototype patching — so the anti-detect
        layer is untouched. Idempotent via `_active_monitor_installed`.
        """
        if self._context is None or self._active_monitor_installed:
            return
        try:
            self._context.add_init_script(ACTIVE_MONITOR_JS)
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to install active-tab monitor init script", exc_info=True)
            return
        self._active_monitor_installed = True
        try:
            existing = list(self._context.pages)
        except Exception:  # noqa: BLE001
            existing = []
        for page in existing:
            try:
                if not page.is_closed():
                    page.evaluate(ACTIVE_MONITOR_JS)
            except Exception:  # noqa: BLE001
                LOG.debug("Failed to seed active-tab monitor on existing page", exc_info=True)

    def _resolve_active_page(self, reason: str) -> Any:
        """Resolve the tab the user currently has in front — live, with no memory.

        Ranks live real-URL tabs purely by the active-tab monitor's last-foreground
        timestamp (`window.__mfActiveMonitor`), which persists through OS-defocus and
        window occlusion because it records the last *transition* rather than the
        instantaneous `visibilityState`/`hasFocus()` (both of which read false while
        the Studio window is focused). Tie-break: current `visibilityState==='visible'`.
        There is deliberately NO fallback to a tracked page, first/most-recently-opened
        tab, or the initially opened URL. Degenerate cases: exactly one live real-URL
        tab is used as-is; none present falls through to `_require_page` (which raises
        the standard "open a chapter" error). Re-resolves if the chosen tab is already
        closed. Sets `self._page` as a cache for incidental callers and returns the page.
        """
        with self._page_lock:
            for _ in range(3):
                valid = [
                    page
                    for page in self._valid_pages()
                    if str(page.url or "").strip() not in {"", "about:blank", "data:,"}
                ]
                if not valid:
                    return self._require_page()
                if len(valid) == 1:
                    chosen = valid[0]
                    basis = "only"
                else:
                    scored: list[tuple[float, bool, Any]] = []
                    for page in valid:
                        active_ts = 0.0
                        visible = False
                        try:
                            info = page.evaluate(ACTIVE_MONITOR_READ_JS)
                        except Exception:  # noqa: BLE001
                            info = None
                        if isinstance(info, dict):
                            try:
                                active_ts = float(info.get("a") or 0)
                            except (TypeError, ValueError):
                                active_ts = 0.0
                            visible = bool(info.get("vis"))
                        scored.append((active_ts, visible, page))
                    # Rank by last-active timestamp, then current visibility. The key
                    # returns only comparable scalars so Page objects are never compared.
                    best = max(scored, key=lambda item: (item[0], 1 if item[1] else 0))
                    chosen = best[2]
                    basis = "active" if best[0] > 0 else ("visible" if best[1] else "ambiguous")
                if chosen.is_closed():
                    continue
                self._page = chosen
                self._attach_page_diagnostics(chosen)
                try:
                    chosen.bring_to_front()
                except Exception:  # noqa: BLE001
                    LOG.debug("Failed to bring active page to front", exc_info=True)
                _debug_log(
                    "cloak active tab (%s): basis=%s url=%s of %d valid",
                    reason,
                    basis,
                    str(chosen.url or ""),
                    len(valid),
                )
                return chosen
            return self._require_page()

    def _active_page_url(self, reason: str) -> str:
        """Resolve the active tab and return its real URL, or raise the standard error.

        Shared entry point for every non-deep download mode (fetch / auto / link
        collect / canvas) so they all target the same tab the user is viewing.
        """
        page = self._resolve_active_page(reason)
        page_url = str(page.url or "").strip()
        if not page_url or page_url in {"about:blank", "data:,"}:
            raise RuntimeError("Сначала откройте страницу главы в CloakBrowser.")
        return page_url

    def _collect_canvas_entries(self) -> list[dict[str, Any]]:
        with self._page_lock:
            raw_entries = self._require_page().evaluate(COLLECT_CANVAS_JS)
        if not isinstance(raw_entries, list):
            _debug_log("cloak canvas collect: JS returned %s instead of list", type(raw_entries).__name__)
            return []
        entries: list[dict[str, Any]] = []
        self._log_canvas_diag_once(
            f"canvas-collect-count:{len(raw_entries)}",
            "cloak canvas collect: inspected %d canvas node(s)",
            len(raw_entries),
        )
        for index, item in enumerate(raw_entries):
            if not isinstance(item, dict):
                self._log_canvas_diag_once(
                    f"non-dict:{index}:{type(item).__name__}",
                    "cloak canvas collect: item %d is %s, skipped",
                    index,
                    type(item).__name__,
                )
                continue
            data = item.get("data")
            valid_png_data_url = isinstance(data, str) and data.startswith("data:image/png;base64,")
            self._log_canvas_diagnostic(index, item, valid_png_data_url)
            if not valid_png_data_url:
                continue
            entries.append(
                {
                    "index": item.get("index", index),
                    "width": item.get("width", 0),
                    "height": item.get("height", 0),
                    "css_width": item.get("css_width", 0),
                    "css_height": item.get("css_height", 0),
                    "device_pixel_ratio": item.get("device_pixel_ratio", 0),
                    "visible": item.get("visible", False),
                    "viewport_overlap": item.get("viewport_overlap", False),
                    "data": data,
                }
            )
        self._log_canvas_diag_once(
            f"canvas-collect-accepted:{len(entries)}:{len(raw_entries)}",
            "cloak canvas collect: accepted %d/%d PNG data URL(s)",
            len(entries),
            len(raw_entries),
        )
        return entries

    def _collect_candidates(self, page_url: str) -> list[str]:
        with self._page_lock:
            raw_candidates = self._require_page().evaluate(COLLECT_CANDIDATES_JS)
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
        with self._page_lock:
            raw_candidates = self._require_page().evaluate(AUTO_COLLECT_CANDIDATES_JS)
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
                    "cloak auto prefilter: skipped %s from %s: %s",
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
                continue
            if matcher is not None and not matcher.search(candidate):
                continue
            seen.add(candidate)
            filtered.append(candidate)
        return filtered

    def _filter_explicit_site_code_links(self, candidates: list[str]) -> list[str]:
        filtered: list[str] = []
        seen: set[str] = set()
        for candidate in candidates:
            if candidate in seen or _looks_like_site_code_resource(candidate):
                continue
            seen.add(candidate)
            filtered.append(candidate)
        return filtered

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
        results = self._download_candidate_links_parallel(filtered, page_url, max_parallel)
        downloaded = 0
        for index, (link, image) in enumerate(zip(filtered, results, strict=True), start=1):
            if image is None:
                continue
            downloaded += 1
            _debug_log("cloak fetch: [%d/%d] success %s -> %dx%d", index, len(filtered), link, image.width, image.height)
            image.save(output_dir / f"{downloaded:04}.png", format="PNG")
        if downloaded == 0:
            shutil.rmtree(output_dir, ignore_errors=True)
            raise RuntimeError("Подходящих ссылок не найдено или ничего не скачалось.")
        return FetchResult(page_url=page_url, output_dir=output_dir, downloaded_images=downloaded)

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
            "cloak auto fetch: using cancellable page-first strategy for %d image(s) in %d group(s), requested_parallel=%d",
            len(grouped_candidates),
            len({signature for _link, signature in grouped_candidates}),
            max_parallel,
        )
        for order_index, (link, group_signature) in enumerate(grouped_candidates):
            if _cancel_requested(cancel_file):
                cancelled = True
                _debug_log(
                    "cloak auto fetch: cancel requested after %d downloaded image(s)",
                    downloaded,
                )
                break
            if group_signature in rejected_groups:
                _debug_log(
                    "cloak auto fetch: skip rejected group %s candidate %s",
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
                        "cloak auto fetch: rejected group %s after %d failed candidate(s)",
                        group_signature,
                        failures,
                    )
                _debug_log(
                    "cloak auto fetch: [%d/%d] failed %s: %s",
                    order_index + 1,
                    len(filtered),
                    link,
                    exc,
                )
                LOG.exception("Failed to download cloak auto candidate %s", link)
                continue
            group_successes[group_signature] = group_successes.get(group_signature, 0) + 1
            downloaded += 1
            file_name = f"{downloaded:04}.png"
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
        results: list[Optional[Image.Image]] = [None for _link in filtered]
        _debug_log(
            "cloak download: using page-first strategy for %d image(s), requested_parallel=%d",
            len(filtered),
            max_parallel,
        )
        for index, link in enumerate(filtered):
            if results[index] is not None:
                continue
            self._emit_progress("download", index + 1, len(filtered))
            try:
                results[index] = self._download_image_with_strategy(link, page_url)
            except Exception as exc:  # noqa: BLE001
                _debug_log("cloak fetch: [%d/%d] failed %s: %s", index + 1, len(filtered), link, exc)
                LOG.exception("Failed to download cloak candidate %s", link)
        return results

    def _download_candidates_with_requests_parallel(
        self,
        indexed_links: list[tuple[int, str]],
        referer: str,
        max_parallel: int,
        total: int,
    ) -> dict[int, Optional[Image.Image]]:
        base_headers = self._browser_request_headers()
        cookies = self._browser_cookie_snapshot()
        worker_count = max(1, min(max_parallel, len(indexed_links)))

        def fetch_one(index: int, link: str) -> tuple[int, Optional[Image.Image]]:
            try:
                return index, self._download_image_with_requests_context(link, referer, base_headers, cookies)
            except Exception as exc:  # noqa: BLE001
                _debug_log("cloak requests failed for %s: %s", _short_link(link), exc)
                return index, None

        results: dict[int, Optional[Image.Image]] = {}
        completed = 0
        with ThreadPoolExecutor(max_workers=worker_count) as executor:
            futures = {executor.submit(fetch_one, index, link): index for index, link in indexed_links}
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
        auto_mode: bool = False,
    ) -> Image.Image:
        if link.startswith("data:image/"):
            return self._decode_data_image(link)
        errors: list[str] = []
        for method in self._download_method_order(link):
            try:
                image = self._download_image_with_method(method, link, referer, auto_mode)
                self._preferred_download_method = method
                return image
            except Exception as exc:  # noqa: BLE001
                errors.append(f"{method}: {type(exc).__name__}: {exc}")
                if method == DOWNLOAD_METHOD_CURRENT_PAGE and isinstance(exc, NonImagePayloadError):
                    break
        raise RuntimeError(f"Could not download image URL: {link}. Tried methods: {'; '.join(errors)}")

    def _download_method_order(self, link: str) -> list[str]:
        methods = [
            DOWNLOAD_METHOD_MEMORY,
            DOWNLOAD_METHOD_CURRENT_PAGE,
            DOWNLOAD_METHOD_DOM_IMAGE,
            DOWNLOAD_METHOD_NEW_PAGE,
            DOWNLOAD_METHOD_REQUESTS,
        ]
        preferred = self._preferred_download_method
        with self._response_lock:
            has_response_memory = bool(self._response_bodies)
        if not has_response_memory:
            methods.remove(DOWNLOAD_METHOD_MEMORY)
        if preferred in methods:
            methods.remove(preferred)
            methods.insert(0, preferred)
        if not self._can_download_with_requests(link):
            methods.remove(DOWNLOAD_METHOD_REQUESTS)
        return methods

    def _download_image_with_method(
        self,
        method: str,
        link: str,
        referer: str,
        auto_mode: bool,
    ) -> Image.Image:
        if method == DOWNLOAD_METHOD_MEMORY:
            return self._download_image_from_response_memory(link)
        if method == DOWNLOAD_METHOD_CURRENT_PAGE:
            return _decode_image_bytes(
                self._download_bytes_via_page_fetch(link, auto_mode),
                link,
            )
        if method == DOWNLOAD_METHOD_DOM_IMAGE:
            return self._download_image_from_dom_image(link)
        if method == DOWNLOAD_METHOD_NEW_PAGE:
            return self._download_image_with_new_page(link, auto_mode)
        if method == DOWNLOAD_METHOD_REQUESTS:
            return self._download_image_with_requests(link, referer, auto_mode)
        raise RuntimeError(f"Unknown download method: {method}")

    def _download_image_from_response_memory(self, link: str) -> Image.Image:
        self._drain_pending_response_bodies()
        with self._response_lock:
            cached = self._response_bodies.get(link)
        if cached is None:
            raise RuntimeError(f"URL was not present in page response memory: {link}")
        body, _content_type = cached
        return _decode_image_bytes(body, link)

    def _download_bytes_via_page_fetch(self, link: str, auto_mode: bool = False) -> bytes:
        with self._page_lock:
            result = self._require_page().evaluate(
                PAGE_FETCH_BYTES_JS,
                {
                    "url": link,
                    "timeoutMs": BROWSER_FETCH_TIMEOUT_MS,
                    "rejectRedirects": auto_mode,
                },
            )
        if not isinstance(result, dict) or not result.get("ok"):
            raise RuntimeError(f"page fetch failed: {result}")
        payload = result.get("data")
        if not isinstance(payload, str) or not payload:
            raise RuntimeError("page fetch returned empty payload")
        return base64.b64decode(payload)

    def _download_image_from_dom_image(self, link: str) -> Image.Image:
        with self._page_lock:
            result = self._require_page().evaluate(DOM_IMAGE_TO_DATA_URL_JS, link)
        if not isinstance(result, dict) or not result.get("ok"):
            raise RuntimeError(f"DOM image readback failed: {result}")
        data_url = result.get("data")
        if not isinstance(data_url, str) or not data_url.startswith("data:image/"):
            raise RuntimeError("DOM image readback returned no image")
        return self._decode_data_image(data_url)

    def _download_image_with_new_page(
        self,
        link: str,
        auto_mode: bool = False,
    ) -> Image.Image:
        with self._page_lock:
            page = self._context.new_page()
            self._attach_page_diagnostics(page)
            try:
                if not auto_mode and urlparse(link).scheme in {"http", "https", "file"}:
                    page.goto(link, wait_until="domcontentloaded", timeout=30_000)
                    try:
                        page.wait_for_load_state("networkidle", timeout=5_000)
                    except Exception:  # noqa: BLE001
                        pass
                result = page.evaluate(
                    PAGE_FETCH_BYTES_JS,
                    {
                        "url": link,
                        "timeoutMs": BROWSER_FETCH_TIMEOUT_MS,
                        "rejectRedirects": auto_mode,
                    },
                )
                if isinstance(result, dict) and result.get("ok") and isinstance(result.get("data"), str):
                    return _decode_image_bytes(base64.b64decode(result["data"]), link)
                data_result = page.evaluate(DOM_IMAGE_TO_DATA_URL_JS, link)
                if isinstance(data_result, dict) and data_result.get("ok"):
                    data_url = data_result.get("data")
                    if isinstance(data_url, str) and data_url.startswith("data:image/"):
                        return self._decode_data_image(data_url)
                raise RuntimeError(f"new page could not read image bytes: {result}")
            finally:
                page.close()

    def _download_image_with_requests(
        self,
        link: str,
        referer: str,
        auto_mode: bool = False,
    ) -> Image.Image:
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
        session = requests.Session()
        for cookie in cookies:
            name = cookie.get("name")
            value = cookie.get("value")
            if not isinstance(name, str) or not isinstance(value, str):
                continue
            kwargs: dict[str, str] = {"path": str(cookie.get("path") or "/")}
            domain = cookie.get("domain")
            if isinstance(domain, str) and domain:
                kwargs["domain"] = domain
            session.cookies.set(name=name, value=value, **kwargs)
        response = session.get(
            link,
            headers=self._request_headers_for_link(base_headers, referer, link),
            timeout=60,
            allow_redirects=not auto_mode,
        )
        if auto_mode and _is_http_redirect_status(response.status_code):
            raise RuntimeError(f"auto fetch skipped redirect HTTP {response.status_code}")
        if not response.ok:
            raise RuntimeError(f"HTTP {response.status_code}")
        return _decode_image_bytes(response.content, link)

    def _browser_request_headers(self) -> dict[str, str]:
        with self._page_lock:
            ua = self._require_page().evaluate("() => navigator.userAgent || ''")
        return {
            "User-Agent": str(ua),
            "Accept": "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
            "Accept-Language": "ru,en;q=0.9",
            "Connection": "keep-alive",
            "Sec-Fetch-Dest": "image",
            "Sec-Fetch-Mode": "no-cors",
            "Sec-Fetch-Site": "same-origin",
        }

    def _browser_cookie_snapshot(self) -> list[dict[str, Any]]:
        self._ensure_browser()
        try:
            return [cookie for cookie in self._context.cookies() if isinstance(cookie, dict)]
        except Exception:  # noqa: BLE001
            LOG.debug("Failed to snapshot CloakBrowser cookies", exc_info=True)
            return []

    def _request_headers_for_link(
        self,
        base_headers: dict[str, str],
        referer: str,
        link: str,
    ) -> dict[str, str]:
        headers = dict(base_headers)
        headers["Referer"] = referer
        try:
            headers["Sec-Fetch-Site"] = "same-origin" if get_origin(referer) == get_origin(link) else "cross-site"
        except Exception:  # noqa: BLE001
            headers["Sec-Fetch-Site"] = "none"
        return headers

    def _can_download_with_requests(self, link: str) -> bool:
        return urlparse(link).scheme in {"http", "https"}

    def _decode_data_image(self, link: str) -> Image.Image:
        image_bytes, _content_type = _decode_data_url_bytes(link)
        return _decode_image_bytes(image_bytes, link)

    def _collect_links_loop(self, stop_event: threading.Event, collect_lock: threading.Lock) -> None:
        while not stop_event.is_set():
            try:
                collect = self._link_collect
                if collect is None:
                    return
                # This loop runs on its own worker thread, but the calls below drive
                # Playwright (page.url / page.evaluate). Marshal them onto the browser
                # owner thread so they don't trip the greenlet cross-thread error.
                # `_filter_candidates` is pure Python and stays off the owner thread.
                page_url = self._run_on_browser_thread(self._current_url_or, collect.page_url)
                if collect.exclude_site_code_links:
                    filtered = self._run_on_browser_thread(
                        self._collect_auto_candidate_links, page_url
                    )
                else:
                    candidates = self._run_on_browser_thread(
                        self._collect_candidates, page_url
                    )
                    filtered = self._filter_candidates(candidates, collect.pattern)
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
            except Exception as exc:  # noqa: BLE001
                LOG.exception("Cloak link collection loop failed")
                collect = self._link_collect
                if collect is not None:
                    with collect_lock:
                        collect.error_message = f"Ошибка фонового сбора ссылок: {exc}"
                        collect.log_message = f"cloak link collect loop failed: {type(exc).__name__}: {exc}"
                stop_event.set()
                break
            stop_event.wait(1.0)

    def _capture_canvas_updates_once(
        self,
        capture: CanvasCaptureState,
        capture_lock: threading.Lock,
    ) -> None:
        if capture.stop_event.is_set() and not self._intercept_active:
            return
        canvas_entries = self._collect_canvas_entries()
        added_count = 0
        with capture_lock:
            for item in canvas_entries:
                canvas_hash = hashlib.sha256(item["data"].encode("utf-8")).hexdigest()
                if canvas_hash in capture.hashes:
                    continue
                capture.hashes.add(canvas_hash)
                capture.entries.append(item)
                added_count += 1
                self._log_canvas_capture_added(item, len(capture.entries), canvas_hash)
            total_count = len(capture.entries)
        if added_count > 0:
            self._emit_progress("collect_canvas", total_count, 0)

    def _finish_link_collect(self) -> LinkCollectState:
        collect = self._link_collect
        if collect is None or not self._link_collect_active:
            raise RuntimeError("Сбор ссылок ещё не запущен.")
        self._emit_progress("collect", 0, 0)
        collect.stop_event.set()
        collect.worker.join(timeout=2.5)
        self._clear_link_collect_runtime()
        if collect.error_message:
            if collect.log_message:
                LOG.error(collect.log_message)
            raise RuntimeError(collect.error_message)
        return collect

    def _save_canvas_entries(self, canvas_entries: list[dict[str, Any]], output_dir: Path) -> int:
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
                _debug_log(
                    "cloak canvas save: entry %d/%d index=%s failed base64 decode payload_chars=%d",
                    index,
                    total,
                    item.get("index", "?"),
                    len(payload),
                )
                continue
            saved_count += 1
            output_path = output_dir / f"{saved_count:04}.png"
            output_path.write_bytes(image_bytes)
            self._log_saved_canvas_payload(index, total, item, image_bytes, output_path)
        return saved_count

    def _reset_canvas_diagnostics(self) -> None:
        self._canvas_diag_seen_hashes.clear()

    def _log_canvas_diagnostic(self, fallback_index: int, item: dict[str, Any], valid_png_data_url: bool) -> None:
        data = item.get("data")
        data_prefix = data[:32] if isinstance(data, str) else ""
        signature = hashlib.sha256(
            json.dumps(
                {
                    "index": item.get("index", fallback_index),
                    "width": item.get("width"),
                    "height": item.get("height"),
                    "css_width": item.get("css_width"),
                    "css_height": item.get("css_height"),
                    "visible": item.get("visible"),
                    "viewport_overlap": item.get("viewport_overlap"),
                    "export_error": item.get("export_error"),
                    "data_type": type(data).__name__,
                    "data_prefix": data_prefix,
                    "data_length": len(data) if isinstance(data, str) else 0,
                },
                sort_keys=True,
                ensure_ascii=False,
            ).encode("utf-8")
        ).hexdigest()
        self._log_canvas_diag_once(
            signature,
            (
                "cloak canvas inspect: index=%s backing=%sx%s css=%.1fx%.1f rect=(%.1f,%.1f %.1fx%.1f) "
                "dpr=%.3g display=%s visibility=%s opacity=%s visible=%s viewport_overlap=%s "
                "connected=%s data_type=%s data_chars=%d valid_png=%s export_error=%s data_prefix=%s"
            ),
            item.get("index", fallback_index),
            item.get("width", 0),
            item.get("height", 0),
            _float_value(item.get("css_width")),
            _float_value(item.get("css_height")),
            _float_value(item.get("rect_x")),
            _float_value(item.get("rect_y")),
            _float_value(item.get("rect_width")),
            _float_value(item.get("rect_height")),
            _float_value(item.get("device_pixel_ratio"), default=0.0),
            item.get("display", ""),
            item.get("visibility", ""),
            item.get("opacity", ""),
            bool(item.get("visible")),
            bool(item.get("viewport_overlap")),
            bool(item.get("connected")),
            type(data).__name__,
            len(data) if isinstance(data, str) else 0,
            valid_png_data_url,
            item.get("export_error", ""),
            data_prefix,
        )
        if valid_png_data_url:
            return
        self._log_canvas_diag_once(
            f"reject:{signature}",
            "cloak canvas reject: index=%s reason=%s",
            item.get("index", fallback_index),
            _canvas_reject_reason(item),
        )

    def _log_canvas_capture_added(self, item: dict[str, Any], captured_count: int, canvas_hash: str) -> None:
        _debug_log(
            (
                "cloak canvas intercept: captured new frame #%d hash=%s index=%s backing=%sx%s "
                "css=%.1fx%.1f visible=%s viewport_overlap=%s data_chars=%d"
            ),
            captured_count,
            canvas_hash[:16],
            item.get("index", "?"),
            item.get("width", 0),
            item.get("height", 0),
            _float_value(item.get("css_width")),
            _float_value(item.get("css_height")),
            bool(item.get("visible")),
            bool(item.get("viewport_overlap")),
            len(str(item.get("data") or "")),
        )

    def _log_saved_canvas_payload(
        self,
        source_index: int,
        total: int,
        item: dict[str, Any],
        image_bytes: bytes,
        output_path: Path,
    ) -> None:
        try:
            with Image.open(BytesIO(image_bytes)) as image:
                rgba = image.convert("RGBA")
                extrema = rgba.getextrema()
                alpha_range = extrema[3]
                rgb_ranges = extrema[:3]
                all_rgb_zero = all(channel == (0, 0) for channel in rgb_ranges)
                all_alpha_zero = alpha_range == (0, 0)
                _debug_log(
                    (
                        "cloak canvas save: entry=%d/%d saved=%s bytes=%d dom_index=%s "
                        "dom_backing=%sx%s decoded=%dx%d mode=%s rgba_extrema=%s "
                        "all_rgb_zero=%s all_alpha_zero=%s"
                    ),
                    source_index,
                    total,
                    output_path.name,
                    len(image_bytes),
                    item.get("index", "?"),
                    item.get("width", 0),
                    item.get("height", 0),
                    image.width,
                    image.height,
                    image.mode,
                    extrema,
                    all_rgb_zero,
                    all_alpha_zero,
                )
        except Exception as exc:  # noqa: BLE001
            _debug_log(
                "cloak canvas save: entry=%d/%d saved=%s bytes=%d failed PIL decode: %s: %s",
                source_index,
                total,
                output_path.name,
                len(image_bytes),
                type(exc).__name__,
                exc,
            )

    def _log_canvas_diag_once(self, signature: str, message: str, *args: object) -> None:
        if signature in self._canvas_diag_seen_hashes:
            return
        self._canvas_diag_seen_hashes.add(signature)
        _debug_log(message, *args)

    def _clear_intercept_runtime(self) -> None:
        self._intercept_active = False
        self._canvas_capture = None

    def _clear_link_collect_runtime(self) -> None:
        self._link_collect_active = False
        self._link_collect = None

    def _clear_deep_capture_runtime(self) -> None:
        capture = self._deep_capture
        if capture is not None:
            capture.stop_event.set()
        self._deep_capture_active = False
        self._deep_capture = None
        # Release the per-capture CDP session so a retry/next cycle attaches a fresh
        # one instead of leaking the previous session (`_attach_cdp_network_capture`
        # creates a new session each start and has no idempotency guard). Callers run
        # on the browser-owner thread, so detaching here is thread-safe.
        session = self._cdp_session
        self._cdp_session = None
        if session is not None:
            try:
                session.detach()
            except Exception:  # noqa: BLE001 - best-effort teardown
                LOG.debug("Failed to detach CDP session on deep-capture clear", exc_info=True)
        with self._pending_response_lock:
            self._pending_responses = []
        with self._response_lock:
            self._response_bodies.clear()
        with self._cdp_lock:
            self._pending_cdp_finished = []
            self._cdp_response_meta.clear()

    def _stop_canvas_capture(self) -> None:
        capture = self._canvas_capture
        if capture is None:
            return
        capture.stop_event.set()
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

    def _emit_result(self, event: str, result: FetchResult) -> None:
        self._emit(
            {
                "event": event,
                "page_url": result.page_url,
                "output_dir": str(result.output_dir),
                "downloaded_images": result.downloaded_images,
            }
        )

    def _emit_progress(self, stage: str, current: int, total: int) -> None:
        self._emit({"event": "progress", "stage": stage, "current": current, "total": total})

    def _emit_error(self, user_message: str, log_message: str) -> None:
        self._emit({"event": "error", "user_message": user_message, "log_message": log_message})

    def _emit_log(self, level: str, message: str) -> None:
        self._emit({"event": "log", "level": level, "message": message})

    def _emit(self, payload: dict[str, Any]) -> None:
        with EMIT_LOCK:
            sys.stdout.write(json.dumps(payload, ensure_ascii=False) + "\n")
            sys.stdout.flush()


# Observe-only instrumentation installed into every page/frame before scripts run.
# It never replaces page behaviour: each wrapper calls the native function, returns its
# real result, swallows its own errors, is idempotent, and spoofs `toString` so it reads
# as native. It targets the image-delivery categories seen across manga sites:
#   A. direct network images   -> handled outside JS (response/CDP capture)
#   B. visible <canvas> readers -> handled by element snapshots; WebGL needs the
#      preserveDrawingBuffer override below so read-back is not blank
# Active-tab monitor: an observe-only script installed into every page (context init
# script + one-time seed of tabs already open at launch). It timestamps the last time a
# tab entered the foreground so `_resolve_active_page` can pick the tab the user is
# actually viewing without any first-tab/tracked-tab memory. Instantaneous
# `document.visibilityState`/`document.hasFocus()` are unreliable here — when the user
# clicks the Studio window the Chromium window loses focus and (when occluded) reports
# every tab hidden. Recording the last *transition* survives that: the tab last brought
# to the foreground keeps the newest timestamp. STRICTLY observe-only — only passive
# listeners and one `window` object, no prototype patching — so CloakBrowser stealth is
# untouched. Idempotent via the `window.__mfActiveMonitor` sentinel.
ACTIVE_MONITOR_JS = """
(() => {
  if (window.__mfActiveMonitor) return;
  const state = { lastVisible: 0, lastInteract: 0 };
  window.__mfActiveMonitor = state;
  const stampVisible = () => {
    try { if (document.visibilityState === 'visible') state.lastVisible = Date.now(); }
    catch (e) {}
  };
  const stampInteract = () => { state.lastInteract = Date.now(); };
  stampVisible();
  try {
    document.addEventListener('visibilitychange', stampVisible, { capture: true, passive: true });
    window.addEventListener('focus', stampInteract, { capture: true, passive: true });
    document.addEventListener('pointerdown', stampInteract, { capture: true, passive: true });
    document.addEventListener('keydown', stampInteract, { capture: true, passive: true });
  } catch (e) {}
})();
"""

# Reader for the active-tab monitor. Installs the monitor if missing (defensive — a tab
# that somehow escaped the init script still gets listeners for next time), then returns
# `{ a: <last foreground timestamp>, vis: <currently visible> }`. Ranked descending.
ACTIVE_MONITOR_READ_JS = """
() => {
  if (!window.__mfActiveMonitor) {
    try {
      const state = { lastVisible: 0, lastInteract: 0 };
      window.__mfActiveMonitor = state;
      const stampVisible = () => {
        try { if (document.visibilityState === 'visible') state.lastVisible = Date.now(); }
        catch (e) {}
      };
      const stampInteract = () => { state.lastInteract = Date.now(); };
      stampVisible();
      document.addEventListener('visibilitychange', stampVisible, { capture: true, passive: true });
      window.addEventListener('focus', stampInteract, { capture: true, passive: true });
      document.addEventListener('pointerdown', stampInteract, { capture: true, passive: true });
      document.addEventListener('keydown', stampInteract, { capture: true, passive: true });
    } catch (e) {}
  }
  const s = window.__mfActiveMonitor || { lastVisible: 0, lastInteract: 0 };
  let vis = false;
  try { vis = document.visibilityState === 'visible'; } catch (e) {}
  return { a: Math.max(s.lastVisible || 0, s.lastInteract || 0), vis: vis };
}
"""

#   C. OffscreenCanvas.convertToBlob descramblers -> captured here
#   D. decrypted bytes -> Blob -> URL.createObjectURL -> <img> (DRM) -> captured here
# Captured bytes are queued on `window.__mfDeepCapture.entries` as raw base64 and drained
# by DRAIN_DEEP_CAPTURE_JS. A WeakMap assigns each canvas a stable id that survives DOM
# recycling in virtual-scroll readers, so the same element collapses to one page.
DEEP_CAPTURE_INIT_JS = """
(() => {
  if (window.__mfDeepCaptureInstalled) return;
  window.__mfDeepCaptureInstalled = true;

  const state = window.__mfDeepCapture || (window.__mfDeepCapture = {
    entries: [],
    seen: new Set(),
    idMap: new WeakMap(),
    nextId: 1,
    nativeCanvasToDataURL: HTMLCanvasElement.prototype.toDataURL,
  });

  // Stable per-element id that survives DOM-node recycling in virtual-scroll readers.
  state.elementId = function (element) {
    if (!element) return 0;
    let id = state.idMap.get(element);
    if (id === undefined) { id = state.nextId++; state.idMap.set(element, id); }
    return id;
  };

  const MAX_ENTRIES = 4096;
  const pushBytes = (source, url, base64, contentType) => {
    if (typeof base64 !== "string" || !base64) return;
    if (state.entries.length >= MAX_ENTRIES) return;
    const key = `${source}|${url}|${base64.length}|${base64.slice(0, 64)}`;
    if (state.seen.has(key)) return;
    state.seen.add(key);
    state.entries.push({ source, url: String(url || ""), data: base64, content_type: contentType || "", metadata: {} });
  };

  const captureBlob = (source, blob, url) => {
    try {
      if (!(blob instanceof Blob)) return;
      if (!blob.size) return;
      const type = String(blob.type || "");
      if (type && !type.startsWith("image/")) return;
      const reader = new FileReader();
      reader.onload = () => {
        try {
          const result = String(reader.result || "");
          const comma = result.indexOf(",");
          if (comma > 0) pushBytes(source, url || "", result.slice(comma + 1), type || "image/png");
        } catch (_) {}
      };
      reader.readAsDataURL(blob);
    } catch (_) {}
  };

  // E: plain <img> tags (http(s)/blob:/data:) are read straight from the page so they
  // show up live in the capture count, not only when their network response happens to
  // be observed. fetch() reuses the browser cache and carries the page session cookies,
  // so authorized images decode without a second sign-in; cross-origin images without
  // CORS come back opaque (size 0) and are skipped here, but the network layer still
  // captures those. Each resolved src is fetched once per session.
  state.fetchedImg = state.fetchedImg || new Set();
  const captureImgSrc = (src) => {
    try {
      if (typeof src !== "string") return;
      const url = src.trim();
      if (!url || !/^(https?:|blob:|data:|file:)/i.test(url)) return;
      if (state.fetchedImg.has(url)) return;
      state.fetchedImg.add(url);
      fetch(url, { credentials: "include", cache: "force-cache" })
        .then((response) => {
          if (!response || !response.ok || response.type === "opaque") {
            throw new Error("img fetch not usable");
          }
          return response.blob();
        })
        .then((blob) => captureBlob("img-element", blob, url))
        .catch(() => {});
    } catch (_) {}
  };
  state.scanImages = function () {
    const walk = (root) => {
      if (!root || !root.querySelectorAll) return;
      for (const img of root.querySelectorAll("img")) {
        try { captureImgSrc(img.currentSrc || img.src || ""); } catch (_) {}
      }
      for (const element of root.querySelectorAll("*")) {
        if (element.shadowRoot) walk(element.shadowRoot);
      }
      for (const iframe of root.querySelectorAll("iframe")) {
        try {
          if (iframe.contentWindow && iframe.contentWindow.document) {
            walk(iframe.contentWindow.document);
          }
        } catch (_) {}
      }
    };
    try { walk(document); } catch (_) {}
  };

  const spoof = (wrapper, native) => {
    try { wrapper.toString = () => native.toString(); } catch (_) {}
  };

  // B/WebGL: retain the drawing buffer so toDataURL read-back is not blank.
  try {
    const nativeGetContext = HTMLCanvasElement.prototype.getContext;
    const wrappedGetContext = function (type, attrs) {
      try {
        if (type === "webgl" || type === "webgl2" || type === "experimental-webgl") {
          attrs = Object.assign({}, attrs || {}, { preserveDrawingBuffer: true });
        }
      } catch (_) {}
      return nativeGetContext.call(this, type, attrs);
    };
    spoof(wrappedGetContext, nativeGetContext);
    HTMLCanvasElement.prototype.getContext = wrappedGetContext;
  } catch (_) {}

  // D: decrypted/descrambled images frequently surface only as blob: object URLs.
  try {
    const nativeCreateObjectURL = URL.createObjectURL;
    const wrappedCreateObjectURL = function (obj) {
      const url = nativeCreateObjectURL.call(this, obj);
      try { if (obj instanceof Blob) captureBlob("createObjectURL", obj, url); } catch (_) {}
      return url;
    };
    spoof(wrappedCreateObjectURL, nativeCreateObjectURL);
    URL.createObjectURL = wrappedCreateObjectURL;
  } catch (_) {}

  // C: descramblers assemble tiles on an OffscreenCanvas that never enters the DOM.
  try {
    if (typeof OffscreenCanvas !== "undefined" && OffscreenCanvas.prototype.convertToBlob) {
      const nativeConvertToBlob = OffscreenCanvas.prototype.convertToBlob;
      const wrappedConvertToBlob = function (...args) {
        const promise = nativeConvertToBlob.apply(this, args);
        try { Promise.resolve(promise).then((blob) => captureBlob("offscreen-convertToBlob", blob, "")).catch(() => {}); } catch (_) {}
        return promise;
      };
      spoof(wrappedConvertToBlob, nativeConvertToBlob);
      OffscreenCanvas.prototype.convertToBlob = wrappedConvertToBlob;
    }
  } catch (_) {}
})();
"""

DRAIN_DEEP_CAPTURE_JS = """
() => {
  const state = window.__mfDeepCapture;
  if (!state || !Array.isArray(state.entries)) return [];
  // Kick off reads of any newly rendered <img> tags; their bytes arrive
  // asynchronously and are returned by a later drain (so they are counted live).
  try { if (state.scanImages) state.scanImages(); } catch (_) {}
  const entries = state.entries.splice(0, state.entries.length);
  return entries;
}
"""


COLLECT_DEEP_ELEMENT_SNAPSHOTS_JS = """
() => {
  const state = window.__mfDeepCapture;
  if (!state) return [];
  const out = [];
  const seen = new Set();
  let canvasOrder = 0;

  const addDataUrl = (source, url, data, metadata) => {
    if (typeof data !== "string" || !data.startsWith("data:")) return;
    const key = `${source}|${url}|${data.length}|${data.slice(0, 96)}`;
    if (seen.has(key)) return;
    seen.add(key);
    out.push({source, url: String(url || ""), data, metadata: metadata || {}});
  };

  const walk = (root, label) => {
    if (!root || !root.querySelectorAll) return;
    const nodes = Array.from(root.querySelectorAll("*"));
    nodes.forEach((element) => {
      try {
        if (element.shadowRoot) walk(element.shadowRoot, `${label}-shadow-${canvasOrder}`);
        if (String(element.tagName || "").toLowerCase() !== "canvas") return;
        const canvas = element;
        const order = canvasOrder;
        canvasOrder += 1;
        if (!canvas.width || !canvas.height) return;
        const nativeToDataURL = state.nativeCanvasToDataURL || HTMLCanvasElement.prototype.toDataURL;
        const data = nativeToDataURL.call(canvas, "image/png", 1.0);
        const rect = canvas.getBoundingClientRect();
        const absoluteTop = Math.round(rect.top + (window.scrollY || 0));
        const absoluteLeft = Math.round(rect.left + (window.scrollX || 0));
        const url = `${location.href}#${label}-canvas-${order}-${canvas.width}x${canvas.height}-${absoluteTop}-${absoluteLeft}`;
        addDataUrl("canvas-native", url, data, {
          dom_order: order,
          element_id: (state.elementId ? state.elementId(canvas) : 0),
          element: "canvas",
          width: Number(canvas.width || 0),
          height: Number(canvas.height || 0),
          top: absoluteTop,
          left: absoluteLeft,
        });
      } catch (_) {}
    });

    for (const iframe of root.querySelectorAll("iframe")) {
      try {
        if (iframe.contentWindow && iframe.contentWindow.document) {
          walk(iframe.contentWindow.document, `${label}-iframe`);
        }
      } catch (_) {}
    }
  };

  walk(document, "document");
  return out;
}
"""


# Reads the document-order position of every capturable element so deep capture can
# sort pages by their order of appearance in the page (the reliable reading order for
# plain-<img> sites) instead of by network arrival or URL-embedded numbers. Emits image
# URLs (all candidate attributes share one slot) and canvas WeakMap ids, walking shadow
# roots and same-origin iframes in document order.
COLLECT_DOM_IMAGE_ORDER_JS = """
() => {
  const state = window.__mfDeepCapture;
  const out = [];
  const seenUrls = new Set();
  let order = 0;
  const addUrl = (slot, value) => {
    if (typeof value !== "string") return;
    const url = value.trim();
    if (!url || seenUrls.has(url)) return;
    seenUrls.add(url);
    out.push({ order: slot, kind: "image", url });
  };
  const walk = (root) => {
    if (!root || !root.querySelectorAll) return;
    for (const node of root.querySelectorAll("*")) {
      const tag = String(node.tagName || "").toLowerCase();
      if (tag === "img") {
        const slot = order++;
        addUrl(slot, node.currentSrc || "");
        addUrl(slot, node.src || "");
        addUrl(slot, node.getAttribute("src") || "");
        addUrl(slot, node.getAttribute("data-src") || "");
      } else if (tag === "source") {
        const slot = order++;
        addUrl(slot, node.src || "");
        addUrl(slot, node.getAttribute("src") || "");
      } else if (tag === "canvas") {
        out.push({ order: order++, kind: "canvas", element_id: (state && state.elementId ? state.elementId(node) : 0) });
      }
      if (node.shadowRoot) walk(node.shadowRoot);
    }
    for (const iframe of root.querySelectorAll("iframe")) {
      try { if (iframe.contentWindow && iframe.contentWindow.document) walk(iframe.contentWindow.document); } catch (_) {}
    }
  };
  walk(document);
  return out;
}
"""


COLLECT_CANVAS_JS = """
() => {
  const entries = [];
  const walk = (root) => {
    if (!root || !root.querySelectorAll) return;
    for (const canvas of root.querySelectorAll("canvas")) {
      const style = window.getComputedStyle(canvas);
      const rect = canvas.getBoundingClientRect();
      let data = null;
      let exportError = "";
      try {
        data = canvas.toDataURL("image/png", 1.0);
      } catch (error) {
        exportError = error && error.message ? String(error.message) : String(error);
      }
      entries.push({
        index: entries.length,
        width: Number(canvas.width || 0),
        height: Number(canvas.height || 0),
        css_width: Number(rect.width || 0),
        css_height: Number(rect.height || 0),
        rect_x: Number(rect.x || 0),
        rect_y: Number(rect.y || 0),
        rect_width: Number(rect.width || 0),
        rect_height: Number(rect.height || 0),
        device_pixel_ratio: Number(window.devicePixelRatio || 1),
        display: String(style.display || ""),
        visibility: String(style.visibility || ""),
        opacity: String(style.opacity || ""),
        visible: Boolean(
          canvas.isConnected &&
          canvas.width > 0 &&
          canvas.height > 0 &&
          rect.width > 0 &&
          rect.height > 0 &&
          style.display !== "none" &&
          style.visibility !== "hidden" &&
          Number(style.opacity || 1) !== 0
        ),
        viewport_overlap: Boolean(
          rect.bottom > 0 &&
          rect.right > 0 &&
          rect.top < window.innerHeight &&
          rect.left < window.innerWidth
        ),
        connected: Boolean(canvas.isConnected),
        data: data,
        export_error: exportError,
      });
    }
    for (const element of root.querySelectorAll("*")) {
      if (element.shadowRoot) walk(element.shadowRoot);
    }
  };
  walk(document);
  return entries;
}
"""

COLLECT_CANDIDATES_JS = """
() => {
  const seen = new Set();
  const out = [];
  const add = (value) => {
    if (typeof value !== "string") return;
    const normalized = value.trim();
    if (!normalized || seen.has(normalized)) return;
    seen.add(normalized);
    out.push(normalized);
  };
  const looksLikeUrl = (value) => {
    if (typeof value !== "string") return false;
    const normalized = value.trim();
    if (!normalized || /\\s/.test(normalized)) return false;
    if (/^(https?:|file:|blob:|data:image\\/|\\/\\/|\\/|\\.\\/|\\.\\.\\/)/i.test(normalized)) return true;
    return normalized.includes("/") || normalized.includes(".");
  };
  const addUrlish = (value) => { if (looksLikeUrl(value)) add(value); };
  const addSrcSet = (value) => {
    if (typeof value !== "string") return;
    for (const part of value.split(",")) add(part.trim().split(/\\s+/)[0] || "");
  };
  const collectFromRoot = (root) => {
    if (!root || !root.querySelectorAll) return;
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
          if (name.includes("srcset")) addSrcSet(value);
          else if (name === "href" || name === "src" || name === "poster" || name === "content" || name.startsWith("data-")) addUrlish(value);
        }
      }
      const styleValue = element.getAttribute("style") || "";
      for (const match of styleValue.matchAll(/url\\((['"]?)(.*?)\\1\\)/g)) addUrlish(match[2] || "");
      if (element.shadowRoot) collectFromRoot(element.shadowRoot);
    }
  };
  collectFromRoot(document);
  return out;
}
"""

AUTO_COLLECT_CANDIDATES_JS = """
() => {
  const seen = new Set();
  const out = [];
  const add = (value, source) => {
    if (typeof value !== "string") return;
    const normalized = value.trim();
    if (!normalized || seen.has(`${source}\\n${normalized}`)) return;
    seen.add(`${source}\\n${normalized}`);
    out.push({url: normalized, source});
  };
  const looksLikeUrl = (value) => {
    if (typeof value !== "string") return false;
    const normalized = value.trim();
    if (!normalized || /\\s/.test(normalized)) return false;
    if (/^(https?:|file:|blob:|data:image\\/|\\/\\/|\\/|\\.\\/|\\.\\.\\/)/i.test(normalized)) return true;
    return normalized.includes("/") || normalized.includes(".");
  };
  const addUrlish = (value, source) => { if (looksLikeUrl(value)) add(value, source); };
  const addSrcSet = (value, source) => {
    if (typeof value !== "string") return;
    for (const part of value.split(",")) addUrlish(part.trim().split(/\\s+/)[0] || "", source);
  };
  const collectFromRoot = (root) => {
    if (!root || !root.querySelectorAll) return;
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
          if (name.includes("srcset")) addSrcSet(value, `generic.${name}`);
          else if (name === "src" || name === "poster" || name === "content" || name.startsWith("data-")) addUrlish(value, `generic.${name}`);
        }
      }
      const styleValue = element.getAttribute("style") || "";
      for (const match of styleValue.matchAll(/url\\((['"]?)(.*?)\\1\\)/g)) addUrlish(match[2] || "", "css.url");
      if (element.shadowRoot) collectFromRoot(element.shadowRoot);
    }
  };
  collectFromRoot(document);
  return out;
}
"""

PAGE_FETCH_BYTES_JS = """
async ({url, timeoutMs, rejectRedirects}) => {
  const controller = new AbortController();
  const timeoutId = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(url, {
      credentials: "include",
      cache: "no-store",
      redirect: rejectRedirects ? "manual" : "follow",
      referrer: location.href,
      referrerPolicy: "strict-origin-when-cross-origin",
      headers: {
        "Accept": "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
      },
      signal: controller.signal,
    });
    if (rejectRedirects && (response.type === "opaqueredirect" || response.redirected || (response.status >= 300 && response.status < 400))) {
      throw new Error(`redirect ${response.status}`);
    }
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const buffer = await response.arrayBuffer();
    const bytes = new Uint8Array(buffer);
    const parts = [];
    for (let offset = 0; offset < bytes.length; offset += 0x8000) {
      parts.push(String.fromCharCode(...bytes.subarray(offset, offset + 0x8000)));
    }
    return {ok: true, data: btoa(parts.join(""))};
  } catch (error) {
    return {ok: false, error: String(error)};
  } finally {
    clearTimeout(timeoutId);
  }
}
"""

DOM_IMAGE_TO_DATA_URL_JS = """
async (url) => {
  const matches = [...document.images].filter((img) => {
    const values = [img.currentSrc || "", img.src || "", img.getAttribute("src") || ""];
    return values.includes(url);
  });
  const img = matches[0];
  if (!img) return {ok: false, error: "matching DOM image not found"};
  if (!img.complete || img.naturalWidth <= 0 || img.naturalHeight <= 0) {
    await new Promise((resolve, reject) => {
      img.addEventListener("load", resolve, {once: true});
      img.addEventListener("error", () => reject(new Error("image load failed")), {once: true});
      setTimeout(() => reject(new Error("image load timeout")), 5000);
    });
  }
  try {
    const canvas = document.createElement("canvas");
    canvas.width = img.naturalWidth;
    canvas.height = img.naturalHeight;
    canvas.getContext("2d").drawImage(img, 0, 0);
    return {ok: true, data: canvas.toDataURL("image/png", 1.0)};
  } catch (error) {
    return {ok: false, error: String(error)};
  }
}
"""


def _normalize_http_url(raw: str) -> str:
    value = (raw or "").translate(CONTROL_TRANSLATION).strip().replace("\\", "/")
    if not value:
        raise ValueError("Введите ссылку на страницу.")
    has_scheme = re.match(r"^[a-zA-Z][a-zA-Z0-9+.\-]*://", value) is not None
    if not has_scheme and (
        value.startswith("www.") or re.match(r"^[\w\-\.]+\.[a-zA-Z]{2,}(/|$)", value)
    ):
        value = "https://" + value
    parsed = urlparse(value)
    if parsed.scheme not in ("http", "https", "file"):
        raise ValueError("Поддерживаются ссылки http(s) и file://")
    if parsed.scheme in ("http", "https") and not parsed.netloc:
        raise ValueError("В адресе отсутствует домен (host).")
    return urlunparse(
        (
            parsed.scheme,
            parsed.netloc,
            quote(parsed.path or "/", safe="/%:@&=+$,;~*'()"),
            parsed.params,
            parsed.query.replace(" ", "%20"),
            parsed.fragment.replace(" ", "%20"),
        )
    )


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
    with EMIT_LOCK:
        sys.stdout.write(json.dumps({"event": "log", "level": "info", "message": formatted}, ensure_ascii=False) + "\n")
        sys.stdout.flush()


def _format_console_location(location: Any) -> str:
    if not isinstance(location, dict):
        return ""
    url = str(location.get("url") or "").strip()
    line = location.get("lineNumber")
    column = location.get("columnNumber")
    parts = [url] if url else []
    if isinstance(line, int) and line > 0:
        if isinstance(column, int) and column > 0:
            parts.append(f"{line}:{column}")
        else:
            parts.append(str(line))
    return ":".join(parts)


def _float_value(value: Any, default: float = 0.0) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def _canvas_reject_reason(item: dict[str, Any]) -> str:
    data = item.get("data")
    if item.get("export_error"):
        return f"toDataURL error: {item.get('export_error')}"
    if not isinstance(data, str):
        return f"toDataURL returned {type(data).__name__}"
    if not data:
        return "toDataURL returned empty string"
    if not data.startswith("data:image/png;base64,"):
        return f"unexpected data URL prefix: {data[:80]}"
    return "unknown reject reason"


def _deep_capture_resolved_dom_index(
    entry: dict[str, Any],
    url: str,
    dom_order: Optional[DeepCaptureDomOrder],
) -> Optional[int]:
    """Resolve a capture's position in page document order, if known.

    Canvas captures resolve through their WeakMap `element_id`; image/network captures
    resolve through their real URL. Returns `None` when no document-order index applies.
    """
    if dom_order is None:
        return None
    metadata = entry.get("metadata")
    if isinstance(metadata, dict):
        element_id = metadata.get("element_id")
        try:
            if element_id is not None and int(element_id) in dom_order.element_to_index:
                return dom_order.element_to_index[int(element_id)]
        except (TypeError, ValueError):
            pass
    if url and url in dom_order.url_to_index:
        return dom_order.url_to_index[url]
    return None


def _deep_capture_sort_key(
    entry: dict[str, Any],
    image: Image.Image,
    fallback_index: int,
    dom_order: Optional[DeepCaptureDomOrder] = None,
) -> tuple[int, int, int, int, int]:
    url = str(entry.get("url") or "")
    dom_index = _deep_capture_resolved_dom_index(entry, url, dom_order)
    dom_walk_order = _deep_capture_dom_order(entry)
    absolute_top = _deep_capture_canvas_top(url)
    sequence = _deep_capture_url_sequence(url)
    source = str(entry.get("source") or "")
    source_rank = _deep_capture_source_rank(source)
    # Document order is the reliable reading order; fall back to per-snapshot DOM walk
    # order, then on-page geometry, then URL-embedded page numbers, then capture order.
    if dom_index is not None:
        primary_rank = 0
        primary_value = dom_index
    elif dom_walk_order is not None:
        primary_rank = 1
        primary_value = dom_walk_order
    elif absolute_top is not None:
        primary_rank = 2
        primary_value = absolute_top
    elif sequence is not None:
        primary_rank = 3
        primary_value = sequence
    else:
        primary_rank = 4
        primary_value = int(entry.get("order") or fallback_index)
    area_rank = -int(image.width * image.height)
    return (primary_rank, primary_value, source_rank, fallback_index, area_rank)


def _deep_capture_item_metadata(item: dict[str, Any]) -> dict[str, Any]:
    metadata = item.get("metadata")
    if not isinstance(metadata, dict):
        return {}
    safe: dict[str, Any] = {}
    for key in ("dom_order", "element_id", "element", "width", "height", "top", "left", "context", "worker"):
        value = metadata.get(key)
        if isinstance(value, (str, int, float, bool)):
            safe[key] = value
    return safe


def _deep_capture_dom_order(entry: dict[str, Any]) -> Optional[int]:
    metadata = entry.get("metadata")
    if not isinstance(metadata, dict):
        return None
    value = metadata.get("dom_order")
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _deep_capture_element_key(entry: dict[str, Any]) -> Optional[tuple[str, int]]:
    """Return a stable identity key for the DOM element a capture came from.

    Prefers the WeakMap `element_id` (survives DOM-node recycling in virtual-scroll
    readers), then falls back to `dom_order`. Returns `None` for non-DOM captures
    (network bytes, blob/offscreen exports), which therefore never collapse together.
    """
    metadata = entry.get("metadata")
    if isinstance(metadata, dict):
        element_id = metadata.get("element_id")
        try:
            if element_id is not None and int(element_id) > 0:
                return ("element_id", int(element_id))
        except (TypeError, ValueError):
            pass
    dom_order = _deep_capture_dom_order(entry)
    if dom_order is not None:
        return ("dom_order", dom_order)
    return None


def _drop_blank_deep_records(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Drop single-colour (blank/black/white) frames so they never become pages.

    Blank frames come from canvases that were not rendered while off-screen; an
    element whose only captured frames are blank is removed entirely instead of
    being saved as a black page.
    """
    return [record for record in records if not record.get("blank")]


def _cluster_deep_records_by_content(
    records: list[dict[str, Any]],
) -> list[list[dict[str, Any]]]:
    """Group records that show the same page into clusters via perceptual hash.

    Uses union-find over `phash`; records within `DEEP_CAPTURE_PHASH_MERGE_DISTANCE`
    Hamming distance are treated as the same visual page even if they arrived through
    different capture layers. Returns one list of records per cluster.
    """
    count = len(records)
    parent = list(range(count))

    def find(node: int) -> int:
        root = node
        while parent[root] != root:
            root = parent[root]
        while parent[node] != root:
            parent[node], node = root, parent[node]
        return root

    def union(left: int, right: int) -> None:
        left_root, right_root = find(left), find(right)
        if left_root != right_root:
            parent[max(left_root, right_root)] = min(left_root, right_root)

    for i in range(count):
        for j in range(i + 1, count):
            distance = _phash_distance(records[i]["phash"], records[j]["phash"])
            if distance <= DEEP_CAPTURE_PHASH_MERGE_DISTANCE:
                union(i, j)

    clusters: dict[int, list[dict[str, Any]]] = {}
    for index, record in enumerate(records):
        clusters.setdefault(find(index), []).append(record)
    return list(clusters.values())


def _select_cluster_representative(cluster: list[dict[str, Any]]) -> dict[str, Any]:
    """Pick the best record in a content cluster.

    Prefers non-blank frames, higher-fidelity sources, larger area, and later
    captures via `_deep_capture_record_preference`.
    """
    return max(cluster, key=_deep_capture_record_preference)


def _assign_deep_capture_confidence(records: list[dict[str, Any]]) -> None:
    """Flag size-outlier records as probable junk for the review UI in place.

    Sets `probable_junk=True` on records whose smaller side is below
    `DEEP_CAPTURE_MIN_PAGE_DIM` or whose area is far below the median page area;
    such records are page outliers (icons, sprites, UI chrome), not manga pages.
    """
    if not records:
        return
    areas = sorted(record["image"].width * record["image"].height for record in records)
    median_area = areas[len(areas) // 2]
    for record in records:
        image = record["image"]
        area = image.width * image.height
        min_dim = min(image.width, image.height)
        record["probable_junk"] = bool(
            min_dim < DEEP_CAPTURE_MIN_PAGE_DIM or area * 4 < median_area
        )


def _collapse_deep_capture_dom_updates(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Keep one best frame per DOM element, passing non-element captures through.

    Records are keyed by `_deep_capture_element_key` (stable WeakMap id, then
    `dom_order`); within a key the most-preferred frame wins. Captures without an
    element key (network bytes, blob/offscreen exports) pass through untouched and
    are later merged by visual content in `_cluster_deep_records_by_content`.
    """
    by_element: dict[tuple[str, int], dict[str, Any]] = {}
    passthrough: list[dict[str, Any]] = []
    for record in records:
        entry = record.get("entry")
        if not isinstance(entry, dict):
            passthrough.append(record)
            continue
        element_key = _deep_capture_element_key(entry)
        if element_key is None:
            passthrough.append(record)
            continue
        previous = by_element.get(element_key)
        if previous is None or _deep_capture_record_preference(record) >= _deep_capture_record_preference(previous):
            by_element[element_key] = record
    collapsed = [*by_element.values(), *passthrough]
    removed = len(records) - len(collapsed)
    if removed > 0:
        _debug_log(
            "cloak deep capture: collapsed %d DOM update duplicate(s), kept latest frame per element",
            removed,
        )
    return collapsed


def _deep_capture_record_preference(record: dict[str, Any]) -> tuple[int, int, int, int]:
    entry = record.get("entry")
    source = str(entry.get("source") or "") if isinstance(entry, dict) else ""
    image = record.get("image")
    area = int(getattr(image, "width", 0) or 0) * int(getattr(image, "height", 0) or 0)
    nonblank = 0 if bool(record.get("blank")) else 1
    source_quality = -_deep_capture_source_rank(source)
    source_index = int(record.get("source_index") or 0)
    return (nonblank, source_quality, area, source_index)


def _deep_capture_url_sequence(url: str) -> Optional[int]:
    parsed = urlparse(url)
    value = unquote(f"{parsed.path}?{parsed.query}")
    numbers = [
        int(match.group(0))
        for match in re.finditer(r"(?<![A-Za-z])\d{1,3}(?![A-Za-z])", value)
    ]
    candidates = [number for number in numbers if 0 < number <= 500]
    if not candidates:
        return None
    return min(candidates)


def _deep_capture_canvas_top(url: str) -> Optional[int]:
    fragment = urlparse(url).fragment
    match = re.search(r"-(?P<top>-?\d+)-(?P<left>-?\d+)$", fragment)
    if match is None:
        return None
    try:
        return int(match.group("top"))
    except ValueError:
        return None


def _deep_capture_dom_keys_from_raw(raw: Any) -> list[tuple[str, str]]:
    """Parse COLLECT_DOM_IMAGE_ORDER_JS output into ordered, de-duplicated DOM keys.

    Keys are ("image", url) for `<img>`/`<source>` candidate URLs and ("canvas",
    str(weakmap_id)) for canvases, in document order. Multiple URL variants of one
    `<img>` (currentSrc/src/data-src) appear as adjacent keys so any of them can match a
    captured entry's URL.
    """
    keys: list[tuple[str, str]] = []
    seen: set[tuple[str, str]] = set()
    if not isinstance(raw, list):
        return keys
    for item in raw:
        if not isinstance(item, dict):
            continue
        kind = item.get("kind")
        if kind == "image":
            url = item.get("url")
            if not isinstance(url, str) or not url:
                continue
            key = ("image", url)
        elif kind == "canvas":
            try:
                element_id = int(item.get("element_id"))
            except (TypeError, ValueError):
                continue
            if element_id <= 0:
                continue
            key = ("canvas", str(element_id))
        else:
            continue
        if key in seen:
            continue
        seen.add(key)
        keys.append(key)
    return keys


def _append_first_seen_keys(
    order: list[tuple[str, str]], seen: set[tuple[str, str]], current: list[tuple[str, str]]
) -> None:
    """Append keys from `current` not seen before, in encounter order, in place.

    Plain chronological first-seen accumulation: while reading top-to-bottom, each poll's
    window is appended after the previous one, so a fast scroll that skips between
    non-overlapping windows still keeps groups in reading order (anchor-based insertion
    would instead drop a non-overlapping group at the front and scramble the sequence).
    """
    for key in current:
        if key in seen:
            continue
        seen.add(key)
        order.append(key)


def _combine_dom_order(
    seen_keys: list[tuple[str, str]], stop_keys: list[tuple[str, str]]
) -> list[tuple[str, str]]:
    """Final document order: pages gone from the DOM, then the live stop-time order.

    `stop_keys` (current DOM document order at stop) is the reliable order for everything
    still on the page. Keys seen earlier but absent at stop are the pages that scrolled
    out of a virtual-scroll reader (typically the first ones, loaded right after reload);
    they are prepended in first-seen order so they keep their reading position instead of
    falling back to capture/network arrival order. Pages still present keep the stop order
    untouched, so the common case is identical to a single stop-time read.
    """
    stop_set = set(stop_keys)
    vanished = [key for key in seen_keys if key not in stop_set]
    return vanished + stop_keys


def _deep_capture_is_canvas_source(source: str) -> bool:
    """Whether a capture came from a `<canvas>` element (native readback or screenshot).

    Everything else (plain `<img>`, network/CDP bytes, blob/offscreen descramble
    exports) is counted as a regular image in the live deep-intercept status.
    """
    return source.lower().startswith("canvas-")


def _deep_capture_source_rank(source: str) -> int:
    normalized = source.lower()
    if normalized.startswith("canvas-native"):
        return 0
    if normalized.startswith("canvas-screenshot"):
        return 1
    # Descrambled/decrypted final images (offscreen exports, blob object URLs) are the
    # real page bytes on DRM/descramble sites, so they rank above raw network payloads.
    if normalized.startswith(
        ("offscreen", "createobjecturl", "blob", "data-url", "fetch", "xhr", "image")
    ):
        return 2
    if normalized.startswith("network"):
        return 3
    return 4


def _deep_capture_review_url(order: int, source: str, original_url: str) -> str:
    fragment_parts = [f"source={quote(source, safe='')}"]
    if original_url:
        fragment_parts.append(f"original={quote(original_url, safe='')}")
    return f"deep-capture://capture/page/{order:04}.png#{'&'.join(fragment_parts)}"


def _decode_data_url_bytes(data_url: str) -> tuple[bytes, str]:
    header, _, payload = data_url.partition(",")
    if not payload or not header.startswith("data:"):
        raise RuntimeError("Empty data URL")
    content_type = header[5:].split(";", 1)[0]
    if ";base64" in header:
        return base64.b64decode(payload), content_type
    return unquote_to_bytes(payload), content_type


def _decode_image_bytes(image_bytes: bytes, source: str) -> Image.Image:
    if not _looks_like_supported_image(image_bytes):
        raise NonImagePayloadError(f"Downloaded content does not look like an image: {source}")
    image = Image.open(BytesIO(image_bytes)).convert("RGB")
    if image.width <= 0 or image.height <= 0:
        raise RuntimeError(f"Invalid downloaded image from {source}")
    return image


def _image_exact_digest(image: Image.Image) -> str:
    normalized = image.convert("RGB")
    return hashlib.sha256(
        f"{normalized.width}x{normalized.height}:".encode("ascii") + normalized.tobytes()
    ).hexdigest()


def _blank_stats(image: Image.Image, *, near: int = 4) -> dict[str, float]:
    """Return blank-frame diagnostics: luminance extrema and near-black/near-white fractions.

    `near` is how many extreme grayscale bins count as black (`0..near-1`) or white
    (`256-near..255`). Used both by `_image_looks_blank` and for debug logging so the
    log shows why a frame was or was not treated as blank.
    """
    gray = image.convert("L")
    lo, hi = gray.getextrema()
    histogram = gray.histogram()
    total = sum(histogram) or 1
    dark = sum(histogram[:near]) / total
    light = sum(histogram[256 - near:]) / total
    return {"lo": float(lo), "hi": float(hi), "dark_frac": dark, "light_frac": light}


def _image_looks_blank(image: Image.Image, *, dominant_fraction: float = 0.999) -> bool:
    """Report whether an image is an empty (near-uniform black or white) frame.

    Unlike an exact-uniformity check, this tolerates a small fraction of stray pixels
    (antialiased edges, compositor noise) so a near-black canvas screenshot is still
    recognised as blank: `True` when at least `dominant_fraction` of pixels are
    near-black or near-white. Real manga pages (white background plus line art and
    screentones) never reach that fraction.
    """
    stats = _blank_stats(image)
    return stats["dark_frac"] >= dominant_fraction or stats["light_frac"] >= dominant_fraction


def _image_dhash(image: Image.Image, hash_size: int = 8) -> int:
    """Compute a size-invariant difference hash (dHash) of an image.

    Returns a `hash_size*hash_size`-bit integer comparing horizontal neighbours of a
    grayscale thumbnail; lets the same page captured through different layers match
    regardless of resolution. `hash_size` must be >= 1.
    """
    gray = image.convert("L").resize((hash_size + 1, hash_size), Image.LANCZOS)
    # mode "L" packs one byte per pixel, row-major, so raw bytes match getdata().
    pixels = gray.tobytes()
    bits = 0
    bit_index = 0
    row_stride = hash_size + 1
    for row in range(hash_size):
        row_start = row * row_stride
        for col in range(hash_size):
            if pixels[row_start + col] > pixels[row_start + col + 1]:
                bits |= 1 << bit_index
            bit_index += 1
    return bits


def _phash_distance(left: int, right: int) -> int:
    """Hamming distance between two dHash values (number of differing bits)."""
    return bin(left ^ right).count("1")


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
    return image_bytes.startswith(b"RIFF") and image_bytes[8:12] == b"WEBP"


def _looks_like_site_code_resource(link: str) -> bool:
    parsed = urlparse(link)
    path = (parsed.path or "").lower()
    if parsed.scheme in {"http", "https"} and path in {"", "/"} and not parsed.query:
        return True
    return path.endswith((".js", ".mjs", ".css", ".map", ".wasm"))


def _short_link(link: str, limit: int = 120) -> str:
    return link if len(link) <= limit else link[: limit - 3] + "..."


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
    daemon = CloakFetchDaemon()
    try:
        return daemon.run()
    except SystemExit as exc:
        return int(exc.code or 0)
    except Exception as exc:  # noqa: BLE001
        traceback.print_exc()
        daemon._emit_error(
            user_message="CloakBrowser-выкачиватель завершился с ошибкой.",
            log_message=f"fatal cloak helper error: {type(exc).__name__}: {exc}",
        )
        return 1
    finally:
        daemon.close()


if __name__ == "__main__":
    raise SystemExit(main())
