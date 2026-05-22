from __future__ import annotations

import os
import platform
import re
import shutil
import threading
import traceback
from io import BytesIO
from urllib.parse import urljoin, urlparse, urlunparse, quote

import requests
from PIL import Image
from PyQt6 import QtCore, QtWidgets
from selenium.common.exceptions import WebDriverException
from selenium.webdriver.common.by import By
from selenium.webdriver.support import expected_conditions as EC
from selenium.webdriver.support.ui import WebDriverWait

from modules.browser_f import (
    build_browser,
    browserlike_headers,
    cleanup_browser_runtime,
    get_origin,
    transfer_cookies_from_selenium,
)

from .common import compile_wildcard_prefixes

try:
    from modules.downloader import download_webtoon_images, SUPPORTED_SITES

    _HAS_DOWNLOADER = True
except Exception:
    traceback.print_exc()
    _HAS_DOWNLOADER = False
    SUPPORTED_SITES = ""

_DEFAULT_LINK_PREFIX = "https://page-edge.kakao.com/sdownload/resource"

_CONTROL = {c: None for c in range(0x00, 0x20)} | {0x7F: None}


def _normalize_http_url(raw: str) -> str:
    s = str(raw or "")
    s = s.translate(_CONTROL).strip()
    s = s.replace("\\", "/")

    if re.match(r"^[a-zA-Z]:/[^?]*", s):
        try:
            from pathlib import Path

            return Path(s).as_uri()
        except Exception:
            pass

    has_scheme = re.match(r"^[a-zA-Z][a-zA-Z0-9+.\-]*://", s) is not None
    if not has_scheme:
        if s.startswith("www."):
            s = "https://" + s
        elif re.match(r"^[\w\-\.]+\.[a-zA-Z]{2,}(/|$)", s):
            s = "https://" + s

    p = urlparse(s)

    if p.scheme not in ("http", "https", "file"):
        raise ValueError("Поддерживаются ссылки http(s) и file://")
    if p.scheme in ("http", "https") and not p.netloc:
        raise ValueError("В адресе отсутствует домен (host).")

    safe_path = quote(p.path or "/", safe="/%:@&=+$,;~*'()")
    safe_query = p.query.replace(" ", "%20")
    safe_frag = p.fragment.replace(" ", "%20")

    return urlunparse((p.scheme, p.netloc, safe_path, p.params, safe_query, safe_frag))


def detect_available_browsers() -> list[str]:
    """
    Return available browsers from: ['Chrome', 'Firefox', 'Edge', 'Safari'].
    """
    system = platform.system()
    found = {}

    def _find_chrome():
        for env in ("CHROME_BIN", "GOOGLE_CHROME_BIN"):
            p = os.environ.get(env)
            if p and os.path.isfile(p):
                return p
        for name in ("google-chrome", "chrome", "chromium", "chromium-browser"):
            p = shutil.which(name)
            if p:
                return p
        if system == "Windows":
            for p in (
                r"C:\Program Files\Google\Chrome\Application\chrome.exe",
                r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            ):
                if os.path.isfile(p):
                    return p
        elif system == "Darwin":
            p = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
            if os.path.isfile(p):
                return p
        else:
            for p in ("/usr/bin/google-chrome", "/usr/bin/chromium", "/snap/bin/chromium"):
                if os.path.isfile(p):
                    return p
        return None

    def _find_firefox():
        for env in ("FIREFOX_BIN",):
            p = os.environ.get(env)
            if p and os.path.isfile(p):
                return p
        for name in ("firefox", "firefox-esr"):
            p = shutil.which(name)
            if p:
                return p
        for p in ("/usr/bin/firefox", "/usr/bin/firefox-esr", "/snap/bin/firefox", "/opt/firefox/firefox"):
            if os.path.isfile(p):
                return p
        if system == "Windows":
            for p in (
                r"C:\Program Files\Mozilla Firefox\firefox.exe",
                r"C:\Program Files (x86)\Mozilla Firefox\firefox.exe",
            ):
                if os.path.isfile(p):
                    return p
        elif system == "Darwin":
            p = "/Applications/Firefox.app/Contents/MacOS/firefox"
            if os.path.isfile(p):
                return p
        return None

    def _find_edge():
        for env in ("EDGE_BIN",):
            p = os.environ.get(env)
            if p and os.path.isfile(p):
                return p
        if system == "Windows":
            for p in (
                r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
                r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            ):
                if os.path.isfile(p):
                    return p
        elif system == "Darwin":
            p = "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"
            if os.path.isfile(p):
                return p
        else:
            for name in ("microsoft-edge", "microsoft-edge-stable"):
                p = shutil.which(name)
                if p:
                    return p
        return None

    def _safari_available():
        return system == "Darwin" and shutil.which("safaridriver") is not None

    if _find_chrome():
        found["Chrome"] = True
    if _find_firefox():
        found["Firefox"] = True
    if _find_edge():
        found["Edge"] = True
    if _safari_available():
        found["Safari"] = True

    order = ["Chrome", "Firefox", "Edge", "Safari"]
    return [b for b in order if b in found]


def to_pil_list(obj):
    if obj is None:
        return []
    out = []
    if isinstance(obj, list):
        import numpy as np

        for it in obj:
            try:
                if hasattr(it, "width") and hasattr(it, "height"):
                    im = it.convert("RGB") if hasattr(it, "convert") else it
                elif isinstance(it, np.ndarray):
                    a = it
                    if a.ndim == 2:
                        a = np.stack([a, a, a], axis=-1)
                    if a.dtype != np.uint8:
                        a = (np.clip(a, 0, 1) * 255).astype("uint8")
                    im = Image.fromarray(a).convert("RGB")
                elif isinstance(it, str) and os.path.isfile(it):
                    im = Image.open(it).convert("RGB")
                else:
                    continue
                if im.width > 0 and im.height > 0:
                    out.append(im)
            except Exception:
                pass
    return out


def on_download(window) -> None:
    url = (window.edNaver.text() or "").strip()
    if not url:
        QtWidgets.QMessageBox.warning(window, "Внимание", "Вставьте ссылку на главу Naver")
        return
    if not _HAS_DOWNLOADER:
        QtWidgets.QMessageBox.critical(
            window,
            "Недоступно",
            "Модуль downloader не найден. Реализуйте download_webtoon_images или подключите модуль.",
        )
        return

    window.btnDownload.setEnabled(False)
    window._set_progress("Загрузка…", 0, 0, pulse=True)

    def worker():
        def cb(step, cur, total):
            QtCore.QMetaObject.invokeMethod(
                window,
                "_set_progress",
                QtCore.Qt.ConnectionType.QueuedConnection,
                QtCore.Q_ARG(str, "Загрузка" if step == "download" else str(step)),
                QtCore.Q_ARG(int, cur),
                QtCore.Q_ARG(int, total),
                QtCore.Q_ARG(bool, False),
            )

        try:
            try:
                np_images = download_webtoon_images(url, progress_callback=cb)
            except TypeError:
                np_images = download_webtoon_images(url)
            pil_list = to_pil_list(np_images)
        except Exception as e:
            traceback.print_exc()
            QtWidgets.QMessageBox.critical(window, "Ошибка скачивания", f"{type(e).__name__}: {e}")
            pil_list = []
        QtCore.QMetaObject.invokeMethod(
            window,
            "_finish_download",
            QtCore.Qt.ConnectionType.QueuedConnection,
            QtCore.Q_ARG(object, pil_list),
        )

    threading.Thread(target=worker, daemon=True).start()


def ensure_browser(window) -> None:
    if getattr(window, "_driver", None) is not None:
        try:
            _ = window._driver.current_url
            return
        except WebDriverException:
            try:
                window._driver.quit()
            except Exception:
                pass
            cleanup_browser_runtime(getattr(window, "cmbBrowser").currentText(), getattr(window, "_tmp_profile_dir", None))
            window._driver = None
            window._tmp_profile_dir = None
    browser = (window.cmbBrowser.currentText() or "").strip()
    if not browser:
        raise RuntimeError("Не найден ни один поддерживаемый браузер.")
    window._driver, window._tmp_profile_dir = build_browser(True, browser)


def adv_open_in_browser(window) -> None:
    url_raw = (window.edAdvUrl.text() or "").strip()
    if not url_raw:
        QtWidgets.QMessageBox.warning(window, "Внимание", "Введите ссылку на страницу.")
        return
    try:
        ensure_browser(window)
        url = _normalize_http_url(url_raw)
        window._driver.get(url)
    except Exception as e:
        traceback.print_exc()
        QtWidgets.QMessageBox.critical(window, "Браузер", str(e))


def adv_fetch_start(window) -> None:
    url = (window.edAdvUrl.text() or "").strip()
    pat = (window.edAdvPat.text() or "").strip()
    if not url:
        QtWidgets.QMessageBox.warning(window, "Внимание", "Введите ссылку на страницу.")
        return

    window.btnAdvFetch.setEnabled(False)
    window._set_progress("Поиск ссылок…", 0, 0, pulse=True)
    window.setCursor(QtCore.Qt.CursorShape.BusyCursor)

    def worker():
        pil_list = []
        err_text = None
        try:
            ensure_browser(window)
            try:
                WebDriverWait(window._driver, 10).until(
                    EC.presence_of_all_elements_located((By.CSS_SELECTOR, "img, a"))
                )
            except Exception:
                pass

            page_url = window._driver.current_url
            page_origin = get_origin(page_url)

            imgs = window._driver.find_elements(By.TAG_NAME, "img")
            anchors = window._driver.find_elements(By.TAG_NAME, "a")
            cand = []
            for el in imgs:
                try:
                    s = el.get_attribute("src") or ""
                    if s:
                        cand.append(s)
                except Exception:
                    pass
            for el in anchors:
                try:
                    h = el.get_attribute("href") or ""
                    if h:
                        cand.append(h)
                except Exception:
                    pass

            cand_abs = []
            for c in cand:
                try:
                    cand_abs.append(urljoin(page_url, c))
                except Exception:
                    continue

            matcher = compile_wildcard_prefixes(pat) if pat else None
            seen = set()
            cand_f = []
            if matcher:
                for c in cand_abs:
                    if c in seen:
                        continue
                    if matcher.search(c):
                        seen.add(c)
                        cand_f.append(c)
            else:
                for c in cand_abs:
                    if c in seen:
                        continue
                    if re.search(r"\.(?:jpe?g|png|webp)(?:\?|$)", c, re.I):
                        seen.add(c)
                        cand_f.append(c)

            if cand_f:
                sess = requests.Session()
                headers = browserlike_headers(window._driver)
                transfer_cookies_from_selenium(window._driver, sess)
                total = len(cand_f)
                for i, link in enumerate(cand_f, 1):
                    try:
                        h = dict(headers)
                        h["Referer"] = page_origin + "/"
                        r = sess.get(link, headers=h, timeout=60)
                        if not r.ok:
                            continue
                        im = Image.open(BytesIO(r.content)).convert("RGB")
                        if im.width > 0 and im.height > 0:
                            pil_list.append(im)
                    except Exception:
                        pass
                    finally:
                        QtCore.QMetaObject.invokeMethod(
                            window,
                            "_set_progress",
                            QtCore.Qt.ConnectionType.QueuedConnection,
                            QtCore.Q_ARG(str, "Загрузка"),
                            QtCore.Q_ARG(int, i),
                            QtCore.Q_ARG(int, total),
                            QtCore.Q_ARG(bool, False),
                        )

        except Exception as e:
            traceback.print_exc()
            err_text = str(e)

        QtCore.QMetaObject.invokeMethod(
            window,
            "_finish_adv",
            QtCore.Qt.ConnectionType.QueuedConnection,
            QtCore.Q_ARG(object, pil_list),
            QtCore.Q_ARG(object, err_text),
        )

    threading.Thread(target=worker, daemon=True).start()


__all__ = [
    "SUPPORTED_SITES",
    "_DEFAULT_LINK_PREFIX",
    "_HAS_DOWNLOADER",
    "detect_available_browsers",
    "on_download",
    "to_pil_list",
    "ensure_browser",
    "adv_open_in_browser",
    "adv_fetch_start",
]
