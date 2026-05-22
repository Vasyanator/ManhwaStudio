from __future__ import annotations

import io
import os
import shutil
import subprocess
import tarfile
import tempfile
import traceback
import zipfile
from pathlib import Path
from typing import List, Optional, Tuple

from PIL import Image
from PyQt6 import QtWidgets

from .common import IMG_EXT, RES_PAT, compile_wildcard_fullmatch, sort_key_for_path

try:
    import rarfile  # type: ignore
except Exception:
    rarfile = None

try:
    import py7zr  # type: ignore
except Exception:
    py7zr = None


def list_resource_like_files(folder: str) -> List[str]:
    """Files named resource/resource(1)/resource(2)... with any extension."""
    out = []
    for fn in os.listdir(folder):
        name, _ext = os.path.splitext(fn)
        m = RES_PAT.match(name)
        if not m:
            continue
        path = os.path.join(folder, fn)
        if os.path.isfile(path):
            num = m.group(2)
            num_i = int(num) if num is not None else 0
            out.append((num_i, fn))
    out.sort(key=lambda t: t[0])
    return [os.path.join(folder, fn) for _, fn in out]


def check_saved_webpage(folder: str) -> Optional[Tuple[str, str]]:
    """
    Detect a folder that is a saved webpage and return (html_path, resources_folder).
    """
    folder_path = Path(folder)
    folder_name = folder_path.name
    parent_dir = folder_path.parent

    for suffix in ("_files", "_data"):
        if folder_name.endswith(suffix):
            page_name = folder_name[: -len(suffix)]
            html_path = parent_dir / f"{page_name}.html"
            if html_path.exists() and html_path.is_file():
                return (str(html_path), str(folder_path))
    return None


def parse_saved_webpage_images(html_path: str, resources_folder: str) -> List[str]:
    """
    Parse a saved HTML page and extract local images in appearance order.
    """
    from html.parser import HTMLParser
    from urllib.parse import unquote

    html_dir = Path(html_path).parent
    resources_path = Path(resources_folder)
    resources_name = resources_path.name

    image_paths = []
    seen_paths = set()

    class ImageExtractor(HTMLParser):
        def __init__(self):
            super().__init__()
            self.in_picture = False

        def handle_starttag(self, tag, attrs):
            tag_lower = tag.lower()
            attrs_dict = dict(attrs)

            if tag_lower == "picture":
                self.in_picture = True

            if tag_lower == "img" or (tag_lower == "source" and self.in_picture):
                for attr_name in ("src", "data-src", "srcset"):
                    src = attrs_dict.get(attr_name, "")
                    if not src:
                        continue
                    if attr_name == "srcset":
                        for part in src.split(","):
                            part = part.strip()
                            if part:
                                url = part.split()[0] if part.split() else part
                                self._process_src(url)
                    else:
                        self._process_src(src)

        def handle_endtag(self, tag):
            if tag.lower() == "picture":
                self.in_picture = False

        def _process_src(self, src: str):
            if not src:
                return

            src = unquote(src)
            src_path = None
            clean_src = src.lstrip("./")

            if clean_src.startswith(resources_name + "/") or clean_src.startswith(resources_name + "\\"):
                src_path = html_dir / clean_src
            elif "/" not in clean_src and "\\" not in clean_src:
                src_path = resources_path / clean_src
            else:
                src_path = html_dir / clean_src

            if src_path and src_path.exists() and src_path.is_file():
                abs_path = str(src_path.resolve())
                ext = src_path.suffix.lower()
                if ext in IMG_EXT or ext in (".svg", ".gif"):
                    if abs_path not in seen_paths:
                        seen_paths.add(abs_path)
                        image_paths.append(abs_path)

    try:
        with open(html_path, "r", encoding="utf-8", errors="ignore") as f:
            html_content = f.read()

        parser = ImageExtractor()
        parser.feed(html_content)
    except Exception:
        traceback.print_exc()

    return image_paths


def list_images_sorted(folder: str) -> List[str]:
    files = [
        os.path.join(folder, fn)
        for fn in os.listdir(folder)
        if os.path.splitext(fn)[1].lower() in IMG_EXT
    ]
    try:
        files.sort(key=sort_key_for_path)
    except Exception:
        files.sort()
    return files


def _archive_kind(path: str) -> Optional[str]:
    low = path.lower()
    if low.endswith(".tar.gz") or low.endswith(".tgz"):
        return "tar"
    if low.endswith(".tar"):
        return "tar"
    if low.endswith(".zip"):
        return "zip"
    if low.endswith(".rar"):
        return "rar"
    if low.endswith(".7z"):
        return "7z"
    return None


def _sort_archive_paths(paths: List[str]) -> List[str]:
    try:
        return sorted(paths, key=lambda p: sort_key_for_path(os.path.basename(p)))
    except Exception:
        return sorted(paths)


def _pick_archive_images(paths: List[str], window) -> List[str]:
    cleaned = [p.replace("\\", "/").lstrip("./") for p in paths]
    img_paths = [p for p in cleaned if Path(p).suffix.lower() in IMG_EXT]
    if not img_paths:
        return []

    top_dirs = sorted({p.split("/", 1)[0] for p in img_paths if "/" in p})
    if len(top_dirs) > 1:
        QtWidgets.QMessageBox.warning(
            window,
            "Архив",
            "Архив похоже содержит не одну главу. Распакуйте его.",
        )

    root_imgs = [p for p in img_paths if "/" not in p]
    if root_imgs:
        return _sort_archive_paths(root_imgs)

    if top_dirs:
        base = top_dirs[0]
        direct = [p for p in img_paths if p.startswith(base + "/") and p.count("/") == 1]
        if not direct:
            direct = [p for p in img_paths if p.startswith(base + "/")]
        return _sort_archive_paths(direct)

    return _sort_archive_paths(img_paths)


def _open_image_from_fileobj(f) -> Optional[Image.Image]:
    try:
        img = Image.open(f)
        img.load()
        return img.convert("RGB")
    except Exception:
        return None


def _load_images_from_zip(path: str, window) -> List[Image.Image]:
    pil_list: List[Image.Image] = []
    with zipfile.ZipFile(path, "r") as zf:
        names = [n for n in zf.namelist() if not n.endswith("/")]
        picked = _pick_archive_images(names, window)
        for name in picked:
            try:
                with zf.open(name) as f:
                    img = _open_image_from_fileobj(f)
                    if img is not None:
                        pil_list.append(img)
            except Exception:
                pass
    return pil_list


def _load_images_from_tar(path: str, window) -> List[Image.Image]:
    pil_list: List[Image.Image] = []
    with tarfile.open(path, "r:*") as tf:
        members = [m for m in tf.getmembers() if m.isfile()]
        names = [m.name for m in members]
        picked = _pick_archive_images(names, window)
        member_map = {m.name: m for m in members}
        for name in picked:
            member = member_map.get(name)
            if not member:
                continue
            try:
                f = tf.extractfile(member)
                if f:
                    img = _open_image_from_fileobj(f)
                    if img is not None:
                        pil_list.append(img)
            except Exception:
                pass
    return pil_list


def _load_images_from_rar(path: str, window) -> List[Image.Image]:
    pil_list: List[Image.Image] = []
    if rarfile is None:
        QtWidgets.QMessageBox.warning(window, "Архив", "Поддержка RAR не доступна.")
        return pil_list
    with rarfile.RarFile(path) as rf:
        names = [i.filename for i in rf.infolist() if not i.isdir()]
        picked = _pick_archive_images(names, window)
        for name in picked:
            try:
                with rf.open(name) as f:
                    img = _open_image_from_fileobj(f)
                    if img is not None:
                        pil_list.append(img)
            except Exception:
                pass
    return pil_list


def _load_images_from_7z(path: str, window) -> List[Image.Image]:
    pil_list: List[Image.Image] = []
    if py7zr is None:
        seven_zip = shutil.which("7z") or shutil.which("7za")
        if not seven_zip:
            QtWidgets.QMessageBox.warning(window, "Архив", "Поддержка 7z не доступна.")
            return pil_list
        with tempfile.TemporaryDirectory() as tmpdir:
            try:
                subprocess.run(
                    [seven_zip, "x", "-y", f"-o{tmpdir}", path],
                    check=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
            except Exception:
                QtWidgets.QMessageBox.warning(window, "Архив", "Не удалось распаковать 7z архив.")
                return pil_list
            return _load_images_from_extracted_dir(tmpdir, window)

    with py7zr.SevenZipFile(path, "r") as zf:
        names = zf.getnames()
        picked = _pick_archive_images(names, window)
        if not picked:
            return pil_list
        data_map = zf.read(picked)
        for name in picked:
            data = data_map.get(name)
            if data is None:
                continue
            try:
                if hasattr(data, "read"):
                    data = data.read()
                img = Image.open(io.BytesIO(data))
                img.load()
                pil_list.append(img.convert("RGB"))
            except Exception:
                pass
    return pil_list


def _load_images_from_extracted_dir(root_dir: str, window) -> List[Image.Image]:
    images = list_images_sorted(root_dir)
    if not images:
        subdirs = sorted(
            [p for p in os.listdir(root_dir) if os.path.isdir(os.path.join(root_dir, p))]
        )
        if len(subdirs) > 1:
            QtWidgets.QMessageBox.warning(
                window,
                "Архив",
                "Архив похоже содержит не одну главу. Распакуйте его.",
            )
        if subdirs:
            base = os.path.join(root_dir, subdirs[0])
            images = list_images_sorted(base)
            if not images:
                for root, _dirs, files in os.walk(base):
                    for fn in files:
                        if os.path.splitext(fn)[1].lower() in IMG_EXT:
                            images.append(os.path.join(root, fn))
    pil_list = []
    for p in images:
        try:
            pil_list.append(Image.open(p).convert("RGB"))
        except Exception:
            pass
    return pil_list


def _load_images_from_archive(path: str, window) -> List[Image.Image]:
    kind = _archive_kind(path)
    if kind == "zip":
        return _load_images_from_zip(path, window)
    if kind == "tar":
        return _load_images_from_tar(path, window)
    if kind == "rar":
        return _load_images_from_rar(path, window)
    if kind == "7z":
        return _load_images_from_7z(path, window)
    return []


def filter_width_outliers(pil_list, tolerance: float = 0.5):
    if not pil_list:
        return pil_list, 0, None
    widths = [im.width for im in pil_list if getattr(im, "width", None)]
    if len(widths) < 3:
        return pil_list, 0, None
    import statistics

    med = int(statistics.median(widths))
    lo = int(med * (1.0 - tolerance))
    hi = int(med * (1.0 + tolerance))
    kept = [im for im in pil_list if lo <= im.width <= hi]
    removed = len(pil_list) - len(kept)
    return kept, removed, (med, lo, hi)


def on_open_folder(window) -> None:
    folder = QtWidgets.QFileDialog.getExistingDirectory(window, "Выберите папку с изображениями")
    if not folder:
        return

    webpage_info = check_saved_webpage(folder)
    if webpage_info:
        html_path, resources_folder = webpage_info
        window._set_progress("Парсинг сохранённой веб-страницы…", 0, 0, pulse=True)
        try:
            paths = parse_saved_webpage_images(html_path, resources_folder)
            if paths:
                pil_list = []
                for p in paths:
                    try:
                        pil_list.append(Image.open(p).convert("RGB"))
                    except Exception:
                        pass

                if pil_list:
                    window._set_progress(
                        f"Загружено из веб-страницы: {len(pil_list)} изображений", 0, 0
                    )
                    window._set_images(pil_list)
                    return
                window._set_progress("", 0, 0)
            else:
                window._set_progress("", 0, 0)
        except Exception:
            traceback.print_exc()
            window._set_progress("", 0, 0)

    paths = []
    try:
        by_ext = [
            os.path.join(folder, fn)
            for fn in os.listdir(folder)
            if os.path.splitext(fn)[1].lower() in IMG_EXT
        ]

        extra_pat = (window.edExtraNames.text() or "").strip()
        extra = []
        if extra_pat:
            rx = compile_wildcard_fullmatch(extra_pat)
            if rx:
                for fn in os.listdir(folder):
                    full = os.path.join(folder, fn)
                    if not os.path.isfile(full):
                        continue
                    if rx.search(fn):
                        extra.append(full)

        paths = list(dict.fromkeys(by_ext + extra))

        if not paths:
            paths = list_resource_like_files(folder)

        try:
            paths.sort(key=sort_key_for_path)
        except Exception:
            paths.sort()

    except Exception as e:
        QtWidgets.QMessageBox.critical(window, "Ошибка", str(e))
        return

    pil_list = []
    for p in paths:
        try:
            pil_list.append(Image.open(p).convert("RGB"))
        except Exception:
            pass

    if not pil_list:
        QtWidgets.QMessageBox.information(window, "Пусто", "Не удалось открыть изображения из папки")
        return
    window._set_images(pil_list)


def _resources_folder_for_html(html_path: str) -> Optional[str]:
    html_file = Path(html_path)
    stem = html_file.stem
    for suffix in ("_files", "_data"):
        cand = html_file.parent / f"{stem}{suffix}"
        if cand.exists() and cand.is_dir():
            return str(cand)
    return None


def _load_images_from_html(html_path: str, window) -> List[Image.Image]:
    resources_folder = _resources_folder_for_html(html_path) or str(Path(html_path).parent)
    window._set_progress("Парсинг сохранённой веб-страницы…", 0, 0, pulse=True)
    try:
        paths = parse_saved_webpage_images(html_path, resources_folder)
    finally:
        window._set_progress("", 0, 0)

    pil_list = []
    for p in paths:
        try:
            pil_list.append(Image.open(p).convert("RGB"))
        except Exception:
            pass
    return pil_list


def _ask_replace_or_append(window) -> Optional[str]:
    if not getattr(window, "_current_images_pil", None):
        return "replace"
    box = QtWidgets.QMessageBox(window)
    box.setWindowTitle("Холст не пустой")
    box.setIcon(QtWidgets.QMessageBox.Icon.Question)
    box.setText("Холст уже содержит изображения. Что сделать?")
    btn_replace = box.addButton("Заменить", QtWidgets.QMessageBox.ButtonRole.AcceptRole)
    btn_append = box.addButton("Добавить в конец", QtWidgets.QMessageBox.ButtonRole.ActionRole)
    box.addButton("Отмена", QtWidgets.QMessageBox.ButtonRole.RejectRole)
    box.exec()
    if box.clickedButton() == btn_replace:
        return "replace"
    if box.clickedButton() == btn_append:
        return "append"
    return None


def on_open_file(window) -> None:
    filters = [
        "Изображения (*.png *.jpg *.jpeg *.bmp *.webp *.tif *.tiff)",
        "HTML (*.html *.htm)",
        "Архивы (*.zip *.rar *.7z *.tar *.tar.gz *.tgz)",
        "Все файлы (*.*)",
    ]
    path, _ = QtWidgets.QFileDialog.getOpenFileName(
        window,
        "Выберите файл",
        "",
        ";;".join(filters),
    )
    if not path:
        return

    ext = Path(path).suffix.lower()

    if ext in (".html", ".htm"):
        pil_list = _load_images_from_html(path, window)
        if not pil_list:
            QtWidgets.QMessageBox.information(window, "Пусто", "Не удалось открыть изображения из HTML")
            return
        window._set_images(pil_list)
        return

    if _archive_kind(path):
        pil_list = _load_images_from_archive(path, window)
        if not pil_list:
            QtWidgets.QMessageBox.information(window, "Пусто", "Не удалось найти изображения в архиве")
            return
        window._set_images(pil_list)
        return

    if ext in IMG_EXT:
        try:
            img = Image.open(path).convert("RGB")
        except Exception as e:
            QtWidgets.QMessageBox.critical(window, "Ошибка", str(e))
            return
        action = _ask_replace_or_append(window)
        if action == "replace":
            window._set_images([img])
        elif action == "append":
            current = list(getattr(window, "_current_images_pil", []) or [])
            current.append(img)
            window._current_images_pil = current
            window._opened_images_pil = list(current)
            window.viewer.set_images(window._current_images_pil)
        return

    QtWidgets.QMessageBox.information(window, "Формат", "Этот формат файла пока не поддерживается.")
