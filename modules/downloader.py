import binascii
import html
import json
import re
import threading
from concurrent.futures import ThreadPoolExecutor, as_completed
from io import BytesIO
from typing import Callable, List, Optional, Tuple
from urllib.parse import parse_qs, urljoin, urlparse

import requests
from bs4 import BeautifulSoup
from PIL import Image
from pycasso import Canvas

# Подставьте свои заголовки, например User-Agent
HEADERS = {
    'User-Agent': 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 '
                  '(KHTML, like Gecko) Chrome/115.0.0.0 Safari/537.36'
}
SUPPORTED_SITES ='''
Поддерживаемые сайты:
comic.naver.com
webtoons.com
m.webtoons.com
mangadex.org
natomanga.com
readcomiconline.li
comicfury.com
hecomicseries.com
kuaikanmanhua.com
bato.to
'''
REQUEST_TIMEOUT = 10


def _merge_headers(extra: Optional[dict[str, str]] = None) -> dict[str, str]:
    merged = dict(HEADERS)
    if extra:
        merged.update(extra)
    return merged


def _normalize_url(url: str, base: str | None = None) -> str:
    if url.startswith("//"):
        return f"https:{url}"
    if base and url.startswith("/"):
        return urljoin(base, url)
    return url


def _download_images_ordered(
    image_urls: List[str],
    *,
    headers: Optional[dict[str, str]] = None,
    progress_callback: Optional[Callable[[str, int, int], None]] = None,
    max_workers: int = 8,
) -> List[Image.Image]:
    """
    Загружает список картинок по URL и возвращает PIL.Image в исходном порядке.
    """
    total = len(image_urls)
    images: List[Optional[Image.Image]] = [None] * total
    downloaded = 0
    lock = threading.Lock()
    merged_headers = _merge_headers(headers)

    def fetch_one(idx: int, img_url: str) -> None:
        nonlocal downloaded
        normalized = _normalize_url(img_url)
        r = requests.get(normalized, headers=merged_headers, stream=True, timeout=REQUEST_TIMEOUT)
        r.raise_for_status()
        img = Image.open(BytesIO(r.content))
        if img.mode != "RGB":
            img = img.convert("RGB")
        with lock:
            images[idx] = img
            downloaded += 1
            if progress_callback:
                progress_callback("download", downloaded, total)

    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        futures = [executor.submit(fetch_one, idx, url) for idx, url in enumerate(image_urls)]
        for f in as_completed(futures):
            _ = f.result()

    if any(img is None for img in images):
        raise RuntimeError("Some images failed to download.")

    return images  # type: ignore[return-value]


def download_webtoon_images(
    url: str,
    progress_callback: Optional[Callable[[str, int, int], None]] = None,
    max_workers: int = 8,
) -> List[Image.Image]:
    """
    Определяет сайт по URL и вызывает конкретную функцию-скачиватель
    для этого сайта.
    """
    parsed = urlparse(url)
    host = parsed.netloc.lower()

    if 'comic.naver.com' in host:
        return comic_naver_downloader(url, progress_callback=progress_callback, max_workers=max_workers)
    if 'webtoons.com' in host or 'm.webtoons.com' in host:
        return webtoons_downloader(url, progress_callback=progress_callback, max_workers=max_workers)
    if 'mangadex.org' in host:
        return mangadex_downloader(url, progress_callback=progress_callback, max_workers=max_workers)
    if 'natomanga.com' in host:
        return natomanga_downloader(url, progress_callback=progress_callback, max_workers=max_workers)
    if 'readcomiconline.li' in host:
        return readcomiconline_downloader(
            url, progress_callback=progress_callback, max_workers=max_workers
        )
    if 'comicfury.com' in host or host.endswith('.thecomicseries.com'):
        return comicfury_downloader(url, progress_callback=progress_callback, max_workers=max_workers)
    if 'kuaikanmanhua.com' in host:
        return kuaikan_downloader(url, progress_callback=progress_callback, max_workers=max_workers)
    if 'bato.to' in host:
        return comic_bato_downloader(url, progress_callback=progress_callback, max_workers=max_workers)
    if 'piccoma.com' in host:
        return piccoma_downloader(url, progress_callback=progress_callback, max_workers=max_workers)

    raise ValueError(f"Unsupported webtoon host: {host!r}")

def comic_naver_downloader(
    url: str,
    progress_callback: Optional[Callable[[str, int, int], None]] = None,
    max_workers: int = 8,
) -> List[Image.Image]:
    """
    Скачивает все куски эпизода вебтуна с comic.naver.com в виде PIL.Image
    и возвращает их в правильном порядке. Загрузка ведётся параллельно.
    """
    parsed = urlparse(url)
    if parsed.netloc != 'comic.naver.com' or not parsed.path.startswith('/webtoon/detail'):
        raise ValueError("URL must be from 'comic.naver.com/webtoon/detail'")

    params = parse_qs(parsed.query)
    try:
        title_id = params['titleId'][0]
        episode_no = params['no'][0]
    except KeyError:
        raise ValueError("URL must include 'titleId' and 'no' parameters")

    resp = requests.get(url, headers=HEADERS)
    resp.raise_for_status()
    soup = BeautifulSoup(resp.text, 'html.parser')

    img_pattern = re.compile(
        rf"https://image-comic\.pstatic\.net/webtoon/{title_id}/{episode_no}/.+?IMAG(\d+)_(\d+)\.jpg"
    )

    # собираем все совпадения (src, (x, y))
    img_urls: List[Tuple[str, Tuple[int, int]]] = []
    for img in soup.find_all('img', {'src': img_pattern}):
        src = img['src']
        m = img_pattern.match(src)
        if m:
            img_urls.append((src, tuple(map(int, m.groups()))))

    if not img_urls:
        raise RuntimeError("No matching webtoon images found for this Naver episode.")

    # сортировка по (x, y) для корректного порядка
    img_urls.sort(key=lambda t: (t[1][0], t[1][1]))

    total = len(img_urls)
    images: List[Optional[Image.Image]] = [None] * total  # заранее выделяем список под результат

    downloaded = 0
    lock = threading.Lock()

    def fetch_one(idx: int, src: str) -> None:
        nonlocal downloaded
        r = requests.get(src, headers=HEADERS)
        r.raise_for_status()
        img = Image.open(BytesIO(r.content))
        if img.mode != 'RGB':
            img = img.convert('RGB')

        # кладём картинку на своё место и обновляем прогресс
        with lock:
            images[idx] = img
            downloaded += 1
            if progress_callback:
                progress_callback('download', downloaded, total)

    # параллельная загрузка
    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        futures = []
        for idx, (src, _) in enumerate(img_urls):
            futures.append(executor.submit(fetch_one, idx, src))

        # гарантируем, что все исключения из потоков будут подняты здесь
        for f in as_completed(futures):
            _ = f.result()

    # safety check (на всякий случай)
    if any(img is None for img in images):
        raise RuntimeError("Some images failed to download from Naver.")

    return images  # type: ignore[return-value]

def comic_bato_downloader(
    url: str,
    progress_callback: Optional[Callable[[str, int, int], None]] = None,
    max_workers: int = 8,
) -> List[Image.Image]:
    """
    Скачивает все страницы главы с bato.to и возвращает их как список PIL.Image.
    Используется актуальная раскопка списка картинок из компонента Astro.
    """
    try:
        resp = requests.get(url, headers=_merge_headers(), timeout=REQUEST_TIMEOUT)
        resp.raise_for_status()
    except requests.HTTPError as e:
        if e.response is not None and e.response.status_code == 503:
            raise RuntimeError(
                f"bato.to returned 503 Service Unavailable for URL: {url}. "
                f"Try again later or use a different mirror."
            ) from e
        raise
    except Exception as e:
        raise RuntimeError(f"Failed to load bato.to page: {e}") from e

    soup = BeautifulSoup(resp.content.decode("utf-8"), "html.parser")

    # новый вариант: astro-island с пропсом imageFiles
    image_urls: List[str] = []
    astro = soup.select_one('astro-island[component-url^="/_astro/ImageList"]')
    if astro and astro.get("props"):
        try:
            data = json.loads(html.unescape(astro["props"]))
            files = data.get("imageFiles")
            if isinstance(files, list) and len(files) >= 2:
                for entry in json.loads(files[1]):
                    if isinstance(entry, list) and len(entry) > 1:
                        image_urls.append(entry[1])
        except Exception:
            image_urls = []

    # старый запасной вариант: переменная imgHttps в скрипте
    if not image_urls:
        for script in soup.find_all("script"):
            text = script.string or script.text or ""
            if "imgHttps" in text:
                match = re.search(r"imgHttps\s*=\s*(\[[^\]]*\])\s*;", text)
                if match:
                    try:
                        image_urls = json.loads(match.group(1))
                    except json.JSONDecodeError:
                        pass
                    break

    if not image_urls:
        raise RuntimeError(f"No image URLs found on bato.to page: {url}")

    return _download_images_ordered(
        image_urls,
        headers={"Referer": "https://bato.to/"},
        progress_callback=progress_callback,
        max_workers=max_workers,
    )

def piccoma_downloader(
    url: str,
    progress_callback: Optional[Callable[[str, int, int], None]] = None,
    max_workers: int = 8,
) -> List[Image.Image]:
    """
    Скачивает все страницы главы Piccoma (JP/FR) и возвращает список PIL.Image.
    Поддерживает manga и smartoon, регион определяется по url (/web/ - JP, /fr/ - FR).
    """
    region = _piccoma_region(url)
    if region not in ("jp", "fr"):
        raise ValueError("Piccoma URL must be jp.piccoma.com/web/viewer/... или piccoma.com/fr/viewer/...")

    # собираем метаданные и список ссылок на изображения
    if region == "jp":
        pdata = _piccoma_jp_pdata(url)
        get_checksum = _piccoma_jp_checksum
    else:
        pdata = _piccoma_fr_pdata(url)
        get_checksum = _piccoma_fr_checksum

    images_urls = pdata["img"]
    if not images_urls:
        raise RuntimeError("Piccoma: не удалось собрать ссылки на страницы главы.")

    seed = _piccoma_seed(images_urls[0], get_checksum)
    scrambled = seed.isupper()

    total = len(images_urls)
    images: List[Optional[Image.Image]] = [None] * total
    downloaded = 0
    lock = threading.Lock()

    def fetch_one(idx: int, img_url: str) -> None:
        nonlocal downloaded
        r = requests.get(img_url, headers={**HEADERS, "Referer": "https://piccoma.com/"}, stream=True)
        r.raise_for_status()

        if scrambled:
            decoded = Canvas(r.raw, (50, 50), dd(seed)).export(mode="scramble", format="png").getvalue()
            img = Image.open(BytesIO(decoded))
        else:
            img = Image.open(BytesIO(r.content))

        if img.mode != "RGB":
            img = img.convert("RGB")

        with lock:
            images[idx] = img
            downloaded += 1
            if progress_callback:
                progress_callback("download", downloaded, total)

    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        futures = [executor.submit(fetch_one, idx, img_url) for idx, img_url in enumerate(images_urls)]
        for f in as_completed(futures):
            _ = f.result()

    if any(img is None for img in images):
        raise RuntimeError("Some images failed to download from Piccoma.")

    return images  # type: ignore[return-value]


def webtoons_downloader(
    url: str,
    progress_callback: Optional[Callable[[str, int, int], None]] = None,
    max_workers: int = 8,
) -> List[Image.Image]:
    ctx = _parse_webtoons_context(url)
    headers = {"Referer": "https://www.webtoons.com/"}

    if ctx["mode"] == "viewer":
        chapter_url = url
    else:
        if not ctx["title_no"]:
            raise ValueError("Webtoons: не удалось определить title_no из ссылки.")
        chapters = _webtoons_chapter_urls(ctx)
        if not chapters:
            raise RuntimeError("Webtoons: не удалось получить список эпизодов.")
        chapter_url = chapters[-1]

    image_urls = _webtoons_image_urls(chapter_url)
    if not image_urls:
        raise RuntimeError("Webtoons: не удалось найти изображения в эпизоде.")
    return _download_images_ordered(
        image_urls,
        headers=headers,
        progress_callback=progress_callback,
        max_workers=max_workers,
    )


def mangadex_downloader(
    url: str,
    progress_callback: Optional[Callable[[str, int, int], None]] = None,
    max_workers: int = 8,
) -> List[Image.Image]:
    chapter_id = _mangadex_chapter_id_from_url(url)
    if not chapter_id:
        manga_id = _mangadex_manga_id_from_url(url)
        if not manga_id:
            raise ValueError("MangaDex URL должен вести на главу или тайтл.")
        chapter_id = _mangadex_pick_latest_chapter(manga_id)
        if not chapter_id:
            raise RuntimeError("MangaDex: не удалось найти главы для скачивания.")

    image_urls = _mangadex_chapter_images(chapter_id)
    return _download_images_ordered(
        image_urls,
        progress_callback=progress_callback,
        max_workers=max_workers,
    )


def natomanga_downloader(
    url: str,
    progress_callback: Optional[Callable[[str, int, int], None]] = None,
    max_workers: int = 8,
) -> List[Image.Image]:
    chapter_url = url
    if "chapter" not in urlparse(url).path:
        chapters = _natomanga_chapter_urls(url)
        if not chapters:
            raise RuntimeError("NatoManga: не удалось получить список глав.")
        chapter_url = chapters[-1]

    image_urls = _natomanga_image_urls(chapter_url)
    if not image_urls:
        raise RuntimeError("NatoManga: не удалось найти картинки в главе.")
    return _download_images_ordered(
        image_urls,
        progress_callback=progress_callback,
        max_workers=max_workers,
    )


def readcomiconline_downloader(
    url: str,
    progress_callback: Optional[Callable[[str, int, int], None]] = None,
    max_workers: int = 8,
) -> List[Image.Image]:
    parsed = urlparse(url)
    parts = [p for p in parsed.path.split("/") if p]

    if len(parts) < 3:
        raise ValueError("ReadComicOnline URL должен содержать Comic/<name>/<chapter>.")

    if len(parts) == 2:  # передали только ссылку на комикс
        comic_id = parts[1]
        chapters = _readcomiconline_chapter_urls(comic_id)
        if not chapters:
            raise RuntimeError("ReadComicOnline: не удалось получить список глав.")
        chapter_url = chapters[-1]
    else:
        chapter_url = url

    image_urls = _readcomiconline_image_urls(chapter_url)
    if not image_urls:
        raise RuntimeError("ReadComicOnline: не удалось найти картинки в главе.")
    return _download_images_ordered(
        image_urls,
        progress_callback=progress_callback,
        max_workers=max_workers,
    )


def comicfury_downloader(
    url: str,
    progress_callback: Optional[Callable[[str, int, int], None]] = None,
    max_workers: int = 8,
) -> List[Image.Image]:
    comic_id = _comicfury_id(url)
    if not comic_id:
        raise ValueError("ComicFury: не удалось определить идентификатор комикса.")

    parsed = urlparse(url)
    if "/read/" in parsed.path and "/comics/" in parsed.path:
        chapter_url = url
    else:
        chapters = _comicfury_chapter_urls(comic_id)
        if not chapters:
            raise RuntimeError("ComicFury: не удалось получить список глав.")
        chapter_url = chapters[-1]

    image_urls = _comicfury_image_urls(chapter_url)
    if not image_urls:
        raise RuntimeError("ComicFury: не удалось найти изображения в главе.")
    return _download_images_ordered(
        image_urls,
        progress_callback=progress_callback,
        max_workers=max_workers,
    )


def kuaikan_downloader(
    url: str,
    progress_callback: Optional[Callable[[str, int, int], None]] = None,
    max_workers: int = 8,
) -> List[Image.Image]:
    if "/web/topic/" in url:
        chapters = _kuaikan_chapter_urls(url)
        if not chapters:
            raise RuntimeError("Kuaikan: не удалось получить список глав.")
        chapter_url = chapters[-1]
    else:
        chapter_url = url

    image_urls = _kuaikan_image_urls(chapter_url)
    if not image_urls:
        raise RuntimeError("Kuaikan: не удалось найти картинки в главе.")
    return _download_images_ordered(
        image_urls,
        progress_callback=progress_callback,
        max_workers=max_workers,
    )

def _piccoma_region(url: str) -> str:
    parsed = urlparse(url)
    path = parsed.path
    if "/fr/" in path:
        return "fr"
    if "/web/" in path or "/viewer/" in path:
        return "jp"
    return ""

def _piccoma_seed(first_img_url: str, checksum_func: Callable[[str], str]) -> str:
    checksum = checksum_func(first_img_url)
    key = " ".join(parse_qs(first_img_url).get("expires", []))
    for num in key:
        if num.isdigit() and int(num) != 0:
            checksum = checksum[-int(num):] + checksum[: len(checksum) - int(num)]
    return checksum

def _piccoma_jp_checksum(img_url: str) -> str:
    return img_url.split("/")[-2]

def _piccoma_fr_checksum(img_url: str) -> str:
    return " ".join(parse_qs(img_url).get("q", []))

def _piccoma_jp_pdata(url: str) -> dict:
    resp = requests.get(url, headers={**HEADERS, "Referer": "https://piccoma.com/"})
    resp.raise_for_status()
    html = resp.text
    soup = BeautifulSoup(html, "html.parser")

    title_tag = soup.find("title")
    title = ""
    if title_tag and "｜" in title_tag.text:
        title = title_tag.text.split("｜")[1]

    script_text = ""
    for script in soup.find_all("script"):
        text = script.string or script.text or ""
        if "pdata" in text:
            script_text = text
            break

    if not script_text:
        raise RuntimeError("Piccoma JP: не удалось найти блок pdata на странице.")

    ep_match = re.search(r"'title'\s*:\s*'([^']+)'", script_text)
    ep_title = ep_match.group(1).strip() if ep_match else ""

    # ссылки на картинки в виде :'//...'
    images = ["https:" + match for match in re.findall(r"(?<=:')[^']+(?=')", script_text)]

    return {"title": title, "ep_title": ep_title, "img": images}

def _piccoma_fr_pdata(url: str) -> dict:
    # строим API-url как в оригинальном клиенте: /_next/data/<build>/viewer/<product>/<episode>.json
    base = "https://piccoma.com/fr"
    resp = requests.get(base, headers={**HEADERS, "Referer": "https://piccoma.com/"})
    resp.raise_for_status()

    soup = BeautifulSoup(resp.text, "html.parser")
    script = soup.find("script", {"id": "__NEXT_DATA__"})
    if not script or not script.text:
        raise RuntimeError("Piccoma FR: не удалось получить buildId.")
    build_id = json.loads(script.text)["buildId"]

    parts = [p for p in urlparse(url).path.split("/") if p]
    try:
        product_id, episode_id = parts[-2], parts[-1]
    except Exception as e:
        raise ValueError("Piccoma FR URL должен быть вида .../viewer/<product>/<episode>") from e

    api_url = f"{base}/_next/data/{build_id}/viewer/{product_id}/{episode_id}.json?productId={product_id}&episodeId={episode_id}"
    data_resp = requests.get(api_url, headers={**HEADERS, "Referer": "https://piccoma.com/"})
    data_resp.raise_for_status()
    page = data_resp.json()["pageProps"]["initialState"]

    product = page["productDetail"]["productDetail"]["product"]
    authors = " ".join([author["name"] for author in product["authors"]])
    title = f"{product['title']} ({authors})"
    ep_title = page["viewer"]["pData"]["title"]
    images = [img["path"] for img in page["viewer"]["pData"]["img"]]

    return {"title": title, "ep_title": ep_title, "img": images}


def _parse_webtoons_context(url: str) -> dict[str, str | int]:
    parsed = urlparse(url)
    query = parse_qs(parsed.query)
    title_no = int(query.get("title_no", ["0"])[0] or 0)
    title_path = "/".join(url.split("/")[3:6])
    webtoon_type = "canvas" if "/canvas/" in url or "/challenge/" in url else "webtoon"
    mode = "viewer" if "viewer" in parsed.path else "list"
    return {"title_no": title_no, "title_path": title_path, "type": webtoon_type, "mode": mode}


def _webtoons_chapter_urls(ctx: dict[str, str | int]) -> List[str]:
    api_url = (
        f"https://m.webtoons.com/api/v1/{ctx['type']}/{ctx['title_no']}/episodes?pageSize=2000"
    )
    res = requests.get(
        api_url, headers=_merge_headers({"Referer": "https://webtoons.com/"}), timeout=REQUEST_TIMEOUT
    ).json()
    episodes = (res.get("result") or {}).get("episodeList") or []
    return [f"https://www.webtoons.com{ep['viewerLink']}" for ep in episodes]


def _webtoons_image_urls(chapter_url: str) -> List[str]:
    resp = requests.get(
        chapter_url,
        headers=_merge_headers({"Referer": "https://webtoons.com/"}),
        timeout=REQUEST_TIMEOUT,
    )
    resp.raise_for_status()
    soup = BeautifulSoup(resp.text, "html.parser")
    urls: List[str] = []
    for img in soup.select("div#_imageList > img"):
        src = img.get("data-url") or img.get("data-src") or img.get("src")
        if src:
            urls.append(src)
    return urls


def _mangadex_chapter_id_from_url(url: str) -> str:
    parts = [p for p in urlparse(url).path.split("/") if p]
    if "chapter" in parts:
        try:
            return parts[parts.index("chapter") + 1]
        except Exception:
            return ""
    return ""


def _mangadex_manga_id_from_url(url: str) -> str:
    parts = [p for p in urlparse(url).path.split("/") if p]
    if "title" in parts:
        try:
            return parts[parts.index("title") + 1]
        except Exception:
            return ""
    return ""


def _mangadex_pick_latest_chapter(manga_id: str) -> str:
    def _fetch(limit_lang: bool) -> dict:
        lang_param = "&translatedLanguage[]=en" if limit_lang else ""
        feed_url = (
            f"https://api.mangadex.org/manga/{manga_id}/feed?limit=1{lang_param}"
            "&order[volume]=desc&order[chapter]=desc"
        )
        return requests.get(feed_url, headers=_merge_headers(), timeout=REQUEST_TIMEOUT).json()

    data = _fetch(True)
    if not data.get("data"):
        data = _fetch(False)

    if data.get("data"):
        return data["data"][0]["id"]
    return ""


def _mangadex_chapter_images(chapter_id: str) -> List[str]:
    resp = requests.get(
        f"https://api.mangadex.org/at-home/server/{chapter_id}",
        headers=_merge_headers(),
        timeout=REQUEST_TIMEOUT,
    )
    if resp.status_code == 404:
        raise RuntimeError("Эта глава MangaDex недоступна для скачивания.")
    resp.raise_for_status()
    data = resp.json()
    base = data["baseUrl"]
    chapter_hash = data["chapter"]["hash"]
    return [f"{base}/data/{chapter_hash}/{p}" for p in data["chapter"]["data"]]


def _natomanga_chapter_urls(series_url: str) -> List[str]:
    resp = requests.get(series_url, headers=_merge_headers(), timeout=REQUEST_TIMEOUT)
    resp.raise_for_status()
    soup = BeautifulSoup(resp.text, "html.parser")
    chapters = [a["href"] for a in soup.select(".chapter-list a") if a.get("href")]
    chapters.reverse()
    return chapters


def _natomanga_image_urls(chapter_url: str) -> List[str]:
    resp = requests.get(chapter_url, headers=_merge_headers(), timeout=REQUEST_TIMEOUT)
    resp.raise_for_status()
    soup = BeautifulSoup(resp.text, "html.parser")
    images: List[str] = []
    for img in soup.find_all("img"):
        src = img.get("src")
        if not src or src.startswith("https://natomanga.com"):
            continue
        images.append(src)
    return images


def _readcomiconline_chapter_urls(comic_id: str) -> List[str]:
    resp = requests.get(
        f"https://readcomiconline.li/Comic/{comic_id}", headers=_merge_headers(), timeout=REQUEST_TIMEOUT
    )
    resp.raise_for_status()
    soup = BeautifulSoup(resp.text, "html.parser")
    chapters = [
        f"https://readcomiconline.li{e['href']}"
        for e in soup.select("ul.list > li > div > a")
        if e.get("href")
    ]
    return list(reversed(chapters))


def _readcomiconline_image_urls(chapter_url: str) -> List[str]:
    text = requests.get(chapter_url, headers=_merge_headers(), timeout=REQUEST_TIMEOUT).text
    images: List[str] = []
    start = 0
    while (index := text.find("lstImages.push(", start)) != -1:
        s_index = index + len("lstImages.push(") + 1
        e_index = text.find(");", s_index) - 1
        images.append(_readcomiconline_beau(text[s_index:e_index]))
        start = e_index
    return images


def _readcomiconline_beau(url: str) -> str:
    url = url.replace("_x236", "d").replace("_x945", "g")

    if url.startswith("https"):
        return url

    url, sep, rest = url.partition("?")
    containsS0 = "=s0" in url
    url = url[: -3 if containsS0 else -6]
    url = url[4:22] + url[25:]
    url = url[0:-6] + url[-2:]
    url = binascii.a2b_base64(url).decode()
    url = url[0:13] + url[17:]
    url = url[0:-2] + ("=s0" if containsS0 else "=s1600")
    return f"https://2.bp.blogspot.com/{url}{sep}{rest}"


def _comicfury_id(url: str) -> str:
    parsed = urlparse(url)
    if parsed.netloc == "comicfury.com":
        if parsed.path.startswith("/read/"):
            parts = [p for p in parsed.path.split("/") if p]
            if len(parts) >= 2:
                return parts[1]
        return parse_qs(parsed.query).get("url", [""])[0]
    if parsed.netloc.endswith(".thecomicseries.com"):
        return parsed.netloc.split(".")[0]
    return ""


def _comicfury_chapter_urls(comic_id: str) -> List[str]:
    resp = requests.get(
        f"https://comicfury.com/read/{comic_id}/archive",
        headers=_merge_headers(),
        timeout=REQUEST_TIMEOUT,
    )
    resp.raise_for_status()
    soup = BeautifulSoup(resp.text, "html.parser")
    return [
        urljoin("https://comicfury.com", a["href"])
        for a in soup.select("a:has(.archive-chapter)")
        if a.get("href")
    ]


def _comicfury_image_urls(chapter_url: str) -> List[str]:
    soup = BeautifulSoup(
        requests.get(chapter_url, headers=_merge_headers(), timeout=REQUEST_TIMEOUT).text,
        "html.parser",
    )
    pages = soup.select(".archive-comics > a")
    if not pages:
        return []
    first_page_id = pages[0]["href"].split("/")[-1]
    subscribe_el = soup.select_one(".webcomic-subscribe")
    if not subscribe_el or not subscribe_el.get("href"):
        return []
    comic_id = subscribe_el["href"].split("=")[-1]

    page_list_urls = soup.select("div.archive-pages .vfpage")
    index = None
    if page_list_urls:
        for i, el in enumerate(page_list_urls):
            if i == 0:
                continue
            if "vfpagecurrent" in el.get("class", []):
                index = i - 1
                break

    if index is None:
        num_pages = len(pages)
    else:
        soup3 = BeautifulSoup(
            requests.get(
                urljoin("https://comicfury.com", page_list_urls[index]["href"]),
                headers=_merge_headers(),
                timeout=REQUEST_TIMEOUT,
            ).text,
            "html.parser",
        )
        num_pages = len(pages) * (index + 1) + len(soup3.select(".archive-comics > a"))

    soup4 = BeautifulSoup(
        requests.get(
            urljoin("https://comicfury.com", pages[0]["href"]),
            headers=_merge_headers(),
            timeout=REQUEST_TIMEOUT,
        ).text,
        "html.parser",
    )
    first_img_el = soup4.select_one(".is--comic-content img")
    if not first_img_el or not first_img_el.get("src"):
        return []
    first_page = _normalize_url(first_img_el["src"], "https://comicfury.com")

    page_id = first_page_id
    all_images: List[str] = [first_page]
    while len(all_images) < num_pages:
        data = requests.get(
            f"https://comicfury.com/api.php?url=webcomic/id/{comic_id}/comicid/{page_id}/getonsitereadercomics",
            headers=_merge_headers(),
            timeout=REQUEST_TIMEOUT,
        ).json()
        if not data.get("status") or data.get("error_code"):
            raise RuntimeError(
                "ComicFury did not give an expected response. "
                "Please try again or проверить ссылку."
            )
        page_id = data["data"]["newLastComicId"]

        soup2 = BeautifulSoup(data["data"]["html"], "html.parser")
        cur_images = [
            _normalize_url(img["src"], "https://comicfury.com")
            for img in soup2.select(".is--comic-content img")
            if img.get("src")
        ]
        all_images.extend(cur_images)
        if data["data"].get("endsAtLastComic"):
            break
    return all_images


def _kuaikan_chapter_urls(topic_url: str) -> List[str]:
    resp = requests.get(topic_url, headers=_merge_headers(), timeout=REQUEST_TIMEOUT)
    resp.raise_for_status()
    text = resp.text
    soup = BeautifulSoup(text, "html.parser")

    try:
        array_start = text.index("Array(")
        next_https = text.index('"https:', array_start)
        previous_quote = text.rindex('"', array_start, next_https)
        prev_prev_quote = text.rindex('"', array_start, previous_quote)
        end = text.index("{}", next_https)
        last_comma = text.rindex(",", next_https, end)
        constructed = f"[{text[prev_prev_quote:last_comma]}]"
        strings: list[str | int] = json.loads(constructed)

        first_chapter_id = int(soup.select_one("a.firstBtn")["data-href"].split("/")[-1])
        strings.insert(0, first_chapter_id)
    except Exception:
        return []

    chapters: List[str] = []
    for i, string in enumerate(strings):
        if isinstance(string, int) and string >= 459:
            title = strings[i + 1] if i + 1 < len(strings) else f"Chapter {string}"
            chapters.append(f"https://www.kuaikanmanhua.com/webs/comic-next/{string}")
    return chapters


def _kuaikan_image_urls(chapter_url: str) -> List[str]:
    text = requests.get(chapter_url, headers=_merge_headers(), timeout=REQUEST_TIMEOUT).text
    try:
        js_start = text.index("config:{_app:")
        next_sign = text.index("?sign=", js_start)
        prev_quote = text.rindex('"', js_start, next_sign)
        last_sign = text.rindex("?sign=", js_start)
        last_comma = text.index(",", last_sign)
        constructed = f"[{text[prev_quote:last_comma]}]"
        strings: list[str] = json.loads(constructed)
        return [s for s in strings if isinstance(s, str)]
    except Exception:
        return []


def dd(input_string: str) -> str:
    """
    Локальная копия алгоритма перемешивания ключа (из test/pyccoma/pyccoma/dd.py).
    """
    result_bytearray = bytearray()
    for index, byte in enumerate(bytes(input_string, "utf-8")):
        if index < 3:
            byte = byte + (1 - 2 * (byte % 2))
        elif 2 < index < 6 or index == 8:
            pass
        elif index < 10:
            byte = byte + (1 - 2 * (byte % 2))
        elif 12 < index < 15 or index == 16:
            byte = byte + (1 - 2 * (byte % 2))
        elif index == len(input_string[:-1]) or index == len(input_string[:-2]):
            byte = byte + (1 - 2 * (byte % 2))
        else:
            pass
        result_bytearray.append(byte)
    return str(result_bytearray, "utf-8")
