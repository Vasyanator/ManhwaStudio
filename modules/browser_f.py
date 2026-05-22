"""
FILE OVERVIEW: modules/browser_f.py
Helpers for Selenium browser startup, profile management, and cookie/header transfer.

Main responsibilities:
- detect installed browsers and matching drivers;
- prepare persistent or temporary browser profiles for Selenium sessions;
- create configured Selenium drivers for Firefox/Chrome/Edge/Safari;
- expose browser-like request headers and cookie transfer helpers for downloads.

Key functions:
- build_browser()
- profile_dir_for()
- browserlike_headers()
- transfer_cookies_from_selenium()

Notes:
- Firefox startup uses a dedicated automation profile from `modules/browser_profiles`.
- Volatile Firefox lock/runtime files are cleaned before launch to avoid stale-profile exits.
"""

import logging
import os
import sys
from pathlib import Path
from urllib.parse import urlparse
import tempfile
import shutil

import requests
from PIL import Image

import traceback
import cv2
import numpy as np
import subprocess
#from selenium.webdriver.chrome.service import Service
from selenium.common.exceptions import WebDriverException
from selenium import webdriver
#from selenium.webdriver.chrome.options import Options
from selenium.webdriver.common.by import By
from selenium.webdriver.support.ui import WebDriverWait
from selenium.webdriver.support import expected_conditions as EC
from selenium.webdriver.firefox.options import Options as FFOptions

from selenium.webdriver.chrome.service import Service as ChromeService
from selenium.webdriver.chrome.options import Options as ChromeOptions

from selenium.webdriver.edge.service import Service as EdgeService
from selenium.webdriver.edge.options import Options as EdgeOptions

from selenium.webdriver.safari.options import Options as SafariOptions
from selenium.webdriver.safari.service import Service as SafariService
# =======================
# НАСТРОЙКИ (под себя)
# =======================

LOG = logging.getLogger(__name__)
RUNTIME_PROFILE_SOURCE_MARKER = ".mangafucker_profile_source"

# Теперь куки и весь профиль браузера живут в папке программы.
USE_PERSISTENT_PROFILE = True
CHROME_PROFILE_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "browser_profile")

BATCH_SIZE = 24
THUMB_MAX  = 480
DISPLAY_MAX_SIDE = 3000  # предел по большей стороне для отображения

# Pillow 9/10 совместимость
try:
    RESAMPLE = Image.Resampling.LANCZOS
except AttributeError:
    RESAMPLE = Image.LANCZOS


# =======================
# УТИЛИТЫ
# =======================


def run_cmd_getline(cmd):
    try:
        out = subprocess.check_output(cmd, stderr=subprocess.STDOUT, text=True)
        return out.strip().splitlines()[0] if out else ""
    except Exception:
        return ""

def find_firefox_binary() -> str | None:
    # env override
    env_path = os.environ.get("FIREFOX_BIN")
    if env_path and os.path.isfile(env_path):
        return os.path.realpath(env_path)

    # which
    for name in ["firefox", "firefox-esr"]:
        p = shutil.which(name)
        if p:
            return os.path.realpath(p)

    # частые пути
    for p in [
        "/usr/bin/firefox",
        "/usr/bin/firefox-esr",
        "/snap/bin/firefox",
        "/opt/firefox/firefox",
    ]:
        if os.path.isfile(p) and os.access(p, os.X_OK):
            return os.path.realpath(p)
    return None

def find_chrome_binary() -> str | None:
    env = os.environ.get("CHROME_BIN") or os.environ.get("GOOGLE_CHROME_BIN")
    if env and os.path.isfile(env):
        return os.path.realpath(env)

    cand = []
    if sys.platform.startswith("win"):
        cand += [
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ]
    elif sys.platform.startswith("darwin"):
        cand += ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"]
    else:
        # Linux
        for name in ["google-chrome", "chrome", "chromium", "chromium-browser"]:
            p = shutil.which(name)
            if p:
                return os.path.realpath(p)
        cand += ["/usr/bin/google-chrome", "/usr/bin/chromium", "/snap/bin/chromium"]
    for p in cand:
        if os.path.isfile(p) and os.access(p, os.X_OK):
            return os.path.realpath(p)
    return None


def find_edgebinary() -> str | None:
    env = os.environ.get("EDGE_BIN")
    if env and os.path.isfile(env):
        return os.path.realpath(env)

    cand = []
    if sys.platform.startswith("win"):
        cand += [
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        ]
    elif sys.platform.startswith("darwin"):
        cand += ["/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"]
    else:
        # Linux
        for name in ["microsoft-edge", "microsoft-edge-stable"]:
            p = shutil.which(name)
            if p:
                return os.path.realpath(p)
    for p in cand:
        if os.path.isfile(p) and os.access(p, os.X_OK):
            return os.path.realpath(p)
    return None


def find_chromedriver() -> str | None:
    env = os.environ.get("CHROME_DRIVER")
    if env and os.path.isfile(env):
        return os.path.realpath(env)
    p = shutil.which("chromedriver")
    if p:
        return os.path.realpath(p)
    # частые пути
    for p in [
        "/usr/bin/chromedriver", "/usr/local/bin/chromedriver",
        r"C:\WebDriver\bin\chromedriver.exe",
    ]:
        if os.path.isfile(p) and os.access(p, os.X_OK):
            return os.path.realpath(p)
    return None


def find_edgedriver() -> str | None:
    env = os.environ.get("EDGE_DRIVER")
    if env and os.path.isfile(env):
        return os.path.realpath(env)
    p = shutil.which("msedgedriver")
    if p:
        return os.path.realpath(p)
    for p in [
        "/usr/bin/msedgedriver", "/usr/local/bin/msedgedriver",
        r"C:\WebDriver\bin\msedgedriver.exe",
    ]:
        if os.path.isfile(p) and os.access(p, os.X_OK):
            return os.path.realpath(p)
    return None


def safari_available() -> bool:
    # На macOS нужен встроенный safaridriver
    if not sys.platform.startswith("darwin"):
        return False
    return bool(shutil.which("safaridriver"))


def get_available_browsers() -> dict[str, str]:
    """
    Возвращает { 'Chrome': <path>, 'Firefox': <path>, 'Edge': <path>, 'Safari': <path or 'safaridriver'> }
    Только доступные на системе варианты.
    """
    out: dict[str, str] = {}

    ff = find_firefox_binary()
    if ff:
        out["Firefox"] = ff

    ch = find_chrome_binary()
    if ch:
        out["Chrome"] = ch

    ed = find_edgebinary()
    if ed:
        out["Edge"] = ed

    if safari_available():
        # сам бинарь Safari нам не нужен, достаточно наличия safaridriver
        out["Safari"] = "/usr/bin/safaridriver"

    return out


# === NEW: профили и утилиты ===
PROFILE_ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "browser_profiles")

def profile_dir_for(browser_name: str) -> str:
    slug = browser_name.strip().lower()
    return os.path.join(PROFILE_ROOT, f"{slug}_profile")

def find_geckodriver() -> str | None:
    env_path = os.environ.get("GECKO_DRIVER")
    if env_path and os.path.isfile(env_path):
        return os.path.realpath(env_path)

    p = shutil.which("geckodriver")
    if p:
        return os.path.realpath(p)

    for p in [
        "/usr/bin/geckodriver",
        "/usr/local/bin/geckodriver",
        "/opt/geckodriver/geckodriver",
    ]:
        if os.path.isfile(p) and os.access(p, os.X_OK):
            return os.path.realpath(p)
    return None


def get_firefox_versions(ff_path: str | None, drv_path: str | None) -> tuple[str, str]:
    ff_v  = run_cmd_getline([ff_path, "--version"]) if ff_path else ""
    drv_v = run_cmd_getline([drv_path, "--version"]) if drv_path else ""
    return ff_v, drv_v
FIREFOX_PROFILE_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "browser_profile_firefox")

def ensure_profile_dir(path: str):
    os.makedirs(path, exist_ok=True)


def prepare_firefox_profile_dir(path: str) -> None:
    """
    Remove volatile Firefox runtime files that can prevent Selenium from opening
    the dedicated automation profile after an unclean shutdown.
    """
    profile_path = Path(path)
    profile_path.mkdir(parents=True, exist_ok=True)
    volatile_entries = (
        ".parentlock",
        "lock",
        "parent.lock",
        "MarionetteActivePort",
        "WebDriverBiDiServer.json",
    )
    for entry_name in volatile_entries:
        entry_path = profile_path / entry_name
        try:
            if entry_path.is_symlink() or entry_path.is_file():
                entry_path.unlink()
            elif entry_path.is_dir():
                shutil.rmtree(entry_path, ignore_errors=False)
        except FileNotFoundError:
            continue
        except Exception:
            LOG.warning("Failed to remove Firefox profile runtime entry: %s", entry_path, exc_info=True)


def _clone_tree(src: Path, dst: Path, ignore_names: set[str]) -> None:
    if not src.exists():
        dst.mkdir(parents=True, exist_ok=True)
        return
    for entry in src.iterdir():
        if entry.name in ignore_names:
            continue
        target = dst / entry.name
        if entry.is_dir():
            shutil.copytree(entry, target, dirs_exist_ok=True)
        else:
            shutil.copy2(entry, target)


def create_firefox_runtime_profile(persistent: bool) -> tuple[str, str | None]:
    """
    Firefox works more reliably when Selenium starts from a throwaway runtime copy
    of the automation profile instead of the live persistent directory itself.
    """
    if persistent:
        source_dir = Path(profile_dir_for("firefox"))
        ensure_profile_dir(str(source_dir))
        prepare_firefox_profile_dir(str(source_dir))

        runtime_root = Path(PROFILE_ROOT) / "firefox_runtime"
        runtime_root.mkdir(parents=True, exist_ok=True)
        runtime_dir = Path(tempfile.mkdtemp(prefix="selenium_ff_profile_", dir=runtime_root))
        ignore_names = {
            ".parentlock",
            "lock",
            "parent.lock",
            "MarionetteActivePort",
            "WebDriverBiDiServer.json",
            "compatibility.ini",
            "startupCache",
            "cache2",
            "shader-cache",
            "minidumps",
            "crashes",
        }
        _clone_tree(source_dir, runtime_dir, ignore_names)
        (runtime_dir / RUNTIME_PROFILE_SOURCE_MARKER).write_text(
            str(source_dir),
            encoding="utf-8",
        )
        prepare_firefox_profile_dir(str(runtime_dir))
        return str(runtime_dir), str(runtime_dir)

    runtime_root = Path(PROFILE_ROOT) / "firefox_runtime"
    runtime_root.mkdir(parents=True, exist_ok=True)
    runtime_dir = tempfile.mkdtemp(prefix="selenium_ff_profile_", dir=runtime_root)
    prepare_firefox_profile_dir(runtime_dir)
    return runtime_dir, runtime_dir


def cleanup_browser_runtime(browser_name: str, runtime_dir: str | None) -> None:
    browser = (browser_name or "").strip().lower()
    if not runtime_dir:
        return

    runtime_path = Path(runtime_dir)
    try:
        if browser == "firefox":
            marker_path = runtime_path / RUNTIME_PROFILE_SOURCE_MARKER
            if marker_path.is_file():
                source_dir = Path(marker_path.read_text(encoding="utf-8").strip())
                if source_dir:
                    source_dir.mkdir(parents=True, exist_ok=True)
                    prepare_firefox_profile_dir(str(runtime_path))
                    ignore_names = {
                        RUNTIME_PROFILE_SOURCE_MARKER,
                        ".parentlock",
                        "lock",
                        "parent.lock",
                        "MarionetteActivePort",
                        "WebDriverBiDiServer.json",
                    }
                    _clone_tree(runtime_path, source_dir, ignore_names)
                    prepare_firefox_profile_dir(str(source_dir))
    except Exception:
        LOG.warning("Failed to sync browser runtime profile: %s", runtime_dir, exc_info=True)
    finally:
        shutil.rmtree(runtime_dir, ignore_errors=True)

def get_origin(url: str) -> str:
    u = urlparse(url)
    return f"{u.scheme}://{u.netloc}"

def build_browser(persistent: bool, browser: str):
    browser = (browser or "").strip().lower()
    tmp_dir = None

    if os.environ.get("XDG_SESSION_TYPE") == "wayland":
        os.environ["MOZ_ENABLE_WAYLAND"] = "1"

    if browser == "firefox":
        opts = FFOptions()
        ff_bin = find_firefox_binary()
        if not ff_bin:
            raise RuntimeError("Не найден Firefox. Укажи FIREFOX_BIN или установи firefox/firefox-esr.")
        opts.binary_location = ff_bin

        profile_dir, tmp_dir = create_firefox_runtime_profile(persistent)
        opts.add_argument("-profile")
        opts.add_argument(profile_dir)

        opts.set_preference("dom.webnotifications.enabled", False)
        opts.set_preference("dom.push.enabled", False)
        opts.set_preference("browser.startup.page", 0)
        opts.set_preference("browser.startup.homepage", "about:blank")
        opts.set_preference("startup.homepage_welcome_url", "about:blank")
        opts.set_preference("browser.startup.homepage_override.mstone", "ignore")

        gecko_log_path = (
            Path(tmp_dir) / "geckodriver.log"
            if tmp_dir
            else Path(tempfile.mkdtemp(prefix="selenium_ff_driver_")) / "geckodriver.log"
        )
        service = webdriver.firefox.service.Service(log_output=str(gecko_log_path))
        try:
            driver = webdriver.Firefox(service=service, options=opts)
        except Exception as exc:
            log_tail = ""
            try:
                if gecko_log_path.is_file():
                    log_lines = gecko_log_path.read_text(
                        encoding="utf-8",
                        errors="replace",
                    ).splitlines()
                    if log_lines:
                        log_tail = "\n".join(log_lines[-20:])
            except Exception:
                LOG.warning("Failed to read geckodriver log: %s", gecko_log_path, exc_info=True)
            cleanup_browser_runtime(browser, tmp_dir)
            raise RuntimeError(
                "Не удалось запустить Firefox Selenium-сессию."
                + (f"\nGeckodriver log:\n{log_tail}" if log_tail else "")
            ) from exc
        driver.set_window_size(1280, 900)
        return driver, tmp_dir

    if browser == "chrome":
        opts = ChromeOptions()
        ch_bin = find_chrome_binary()
        if not ch_bin:
            raise RuntimeError("Не найден Chrome/Chromium. Укажи CHROME_BIN или установи браузер.")
        opts.binary_location = ch_bin

        if persistent:
            pdir = profile_dir_for("chrome")
            os.makedirs(pdir, exist_ok=True)
            opts.add_argument(f"--user-data-dir={pdir}")
        else:
            tmp_dir = tempfile.mkdtemp(prefix="selenium_ch_profile_")
            opts.add_argument(f"--user-data-dir={tmp_dir}")

        opts.add_argument("--no-first-run")
        opts.add_argument("--no-default-browser-check")
        if sys.platform.startswith("linux"):
            opts.add_argument("--disable-dev-shm-usage")

        driver = webdriver.Chrome(options=opts)  # без Service(...)
        driver.set_window_size(1280, 900)
        return driver, tmp_dir

    if browser == "edge":
        opts = EdgeOptions()
        ed_bin = find_edgebinary()
        if not ed_bin:
            raise RuntimeError("Не найден Microsoft Edge. Укажи EDGE_BIN или установи Edge.")
        opts.binary_location = ed_bin

        if persistent:
            pdir = profile_dir_for("edge")
            os.makedirs(pdir, exist_ok=True)
            opts.add_argument(f"--user-data-dir={pdir}")
        else:
            tmp_dir = tempfile.mkdtemp(prefix="selenium_edge_profile_")
            opts.add_argument(f"--user-data-dir={tmp_dir}")

        driver = webdriver.Edge(options=opts)  # без Service(...)
        driver.set_window_size(1280, 900)
        return driver, tmp_dir

    if browser == "safari":
        if not safari_available():
            raise RuntimeError("Safari/safaridriver недоступен на этой системе. Попробуйте выполнить команду safaridriver --enable")
        # Для Safari драйвер встроен в macOS. Один раз выполните:
        #   safaridriver --enable
        # (и включите Remote Automation в Safari → Develop) 
        # После этого — просто:
        driver = webdriver.Safari(options=SafariOptions())
        driver.set_window_size(1280, 900)
        return driver, None

    raise RuntimeError(f"Неизвестный браузер: {browser}. Ожидаются: Firefox/Chrome/Edge/Safari.")

def transfer_cookies_from_selenium(driver: webdriver.Chrome, sess: requests.Session):
    # Переносим текущее состояние кук из профиля браузера в requests
    sess.cookies.clear()
    for c in driver.get_cookies():
        sess.cookies.set(
            name=c.get("name"),
            value=c.get("value"),
            domain=c.get("domain"),
            path=c.get("path", "/")
        )

def browserlike_headers(driver: webdriver.Chrome) -> dict:
    ua = driver.execute_script("return navigator.userAgent")
    return {
        "User-Agent": ua,
        "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        "Accept-Language": "ru,en;q=0.9",
        "Accept-Encoding": "gzip, deflate, br",
        "Connection": "keep-alive",
        "Sec-Fetch-Dest": "image",
        "Sec-Fetch-Mode": "no-cors",
        "Sec-Fetch-Site": "same-origin",
    }

def pil_to_bgr(img: Image.Image) -> np.ndarray:
    arr = np.array(img.convert("RGB"))
    return cv2.cvtColor(arr, cv2.COLOR_RGB2BGR)

def bgr_to_pil(arr: np.ndarray) -> Image.Image:
    rgb = cv2.cvtColor(arr, cv2.COLOR_BGR2RGB)
    return Image.fromarray(rgb)

