"""
FILE OVERVIEW: modules/psd_import.py
Qt window for importing PSD layers (single files/folders/archives) into project chapters.

Main items:
- `PSDArchiveViewer`: loads PSD/ZIP/RAR, previews layers, maps them to "Исходник/Клин".
- Project save block writes into projects root from `user_config.json` (`General.projects_dir`).
"""

import io
import os
import re
import sys
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from PyQt6.QtCore import Qt
from PyQt6.QtGui import QImage, QPixmap
from PyQt6.QtWidgets import (
    QApplication, QMainWindow, QWidget, QFileDialog, QVBoxLayout, QHBoxLayout,
    QPushButton, QLabel, QSplitter, QScrollArea, QMessageBox,
    QTableWidget, QTableWidgetItem, QComboBox
)

from config import get_projects_root
from psd_tools import PSDImage
from psd_tools.api.layers import PixelLayer


PAGE_RE = re.compile(r"(?i)(?:^|/)(\d+)\.psd$")  # .../001.psd -> 1


def pil_to_qimage(pil_img):
    if pil_img.mode not in ("RGBA", "RGB"):
        pil_img = pil_img.convert("RGBA")
    else:
        pil_img = pil_img.copy()

    if pil_img.mode == "RGBA":
        fmt = QImage.Format.Format_RGBA8888
        data = pil_img.tobytes("raw", "RGBA")
        qimg = QImage(data, pil_img.width, pil_img.height, 4 * pil_img.width, fmt)
    else:
        fmt = QImage.Format.Format_RGB888
        data = pil_img.tobytes("raw", "RGB")
        qimg = QImage(data, pil_img.width, pil_img.height, 3 * pil_img.width, fmt)

    return qimg.copy()


def try_import_rarfile():
    try:
        import rarfile  # type: ignore
        return rarfile
    except Exception:
        return None


def extract_page_from_name(name: str) -> Optional[int]:
    m = PAGE_RE.search(name.replace("\\", "/"))
    if not m:
        return None
    try:
        return int(m.group(1))
    except Exception:
        return None


def iter_leaf_layers(psd_or_group, prefix=""):
    # Возвращает только "листья" (не группы) как независимые картинки.
    for layer in psd_or_group:
        if layer.is_group():
            yield from iter_leaf_layers(layer, prefix=prefix + (layer.name or "(group)") + "/")
        elif isinstance(layer, PixelLayer):
            yield prefix, layer


@dataclass
class RowData:
    file_name: str           # 001.psd
    page: int                # 1
    layer_title: str         # "Group/Layer"
    layer_obj: object        # psd_tools layer
    size: Tuple[int, int]    # (w, h)
    typ: str                 # "Не импортировать" | "Исходник" | "Клин"
    # Для отката при проверках:
    prev_page: int
    prev_typ: str
    preview: Optional[QPixmap] = None
    preview_error: Optional[str] = None


class PSDArchiveViewer(QMainWindow):
    TYPE_OPTIONS = ["Не импортировать", "Исходник", "Клин"]

    def __init__(self, projects_dir: Optional[str] = None):
        super().__init__()
        self.setWindowTitle("Импорт из архива с psd")
        self.showMaximized()

        self.projects_dir = Path(projects_dir or get_projects_root()).resolve()
        self.psd_docs: List[PSDImage] = []
        self.rows: List[RowData] = []
        self.available_pages: List[int] = []

        # --- Top bar
        open_btn = QPushButton("Открыть PSD/ZIP/RAR…")
        open_btn.clicked.connect(self.open_any)

        open_dir_btn = QPushButton("Открыть папку с psd")
        open_dir_btn.clicked.connect(self.open_psd_folder)

        swap_btn = QPushButton("Поменять фон и клин")
        swap_btn.clicked.connect(self.swap_source_and_clean)

        self.info_label = QLabel("Файл не выбран")
        self.info_label.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)

        top = QWidget()
        top_l = QHBoxLayout(top)
        top_l.setContentsMargins(8, 8, 8, 8)
        top_l.addWidget(open_btn)
        top_l.addWidget(open_dir_btn)
        top_l.addWidget(swap_btn)
        top_l.addWidget(self.info_label, 1)

        # --- Left: table of "images" (each layer)
        self.table = QTableWidget(0, 4)
        self.table.setHorizontalHeaderLabels(["Картинка", "Размер", "Страница", "Тип"])
        self.table.setSelectionBehavior(QTableWidget.SelectionBehavior.SelectRows)
        self.table.setSelectionMode(QTableWidget.SelectionMode.SingleSelection)
        self.table.itemSelectionChanged.connect(self.on_row_selected)
        self.table.horizontalHeader().setStretchLastSection(False)
        self.table.horizontalHeader().setDefaultAlignment(Qt.AlignmentFlag.AlignLeft)

        # --- Save block
        save_block = QWidget()
        save_l = QVBoxLayout(save_block)
        save_l.setContentsMargins(8, 8, 8, 8)
        save_l.setSpacing(6)
        save_title = QLabel("Сохранить главу")
        save_title.setStyleSheet("font-weight: bold;")

        self.title_cb = QComboBox()
        self.title_cb.setEditable(True)
        self.title_cb.currentTextChanged.connect(self.on_title_changed)
        self.chapter_cb = QComboBox()
        self.chapter_cb.setEditable(True)

        save_btn = QPushButton("Сохранить")
        save_btn.clicked.connect(self.save_chapter)

        save_l.addWidget(save_title)
        save_l.addWidget(QLabel("Тайтл"))
        save_l.addWidget(self.title_cb)
        save_l.addWidget(QLabel("Глава"))
        save_l.addWidget(self.chapter_cb)
        save_l.addWidget(save_btn)

        left_panel = QWidget()
        left_l = QVBoxLayout(left_panel)
        left_l.setContentsMargins(0, 0, 0, 0)
        left_l.setSpacing(4)
        left_l.addWidget(self.table, 1)
        left_l.addWidget(save_block, 0)

        # --- Right: scrollable image
        self.image_label = QLabel("Откройте PSD/архив и выберите слой")
        self.image_label.setAlignment(Qt.AlignmentFlag.AlignTop | Qt.AlignmentFlag.AlignLeft)
        self.image_label.setScaledContents(False)

        self.scroll = QScrollArea()
        self.scroll.setWidgetResizable(True)
        self.scroll.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self.scroll.setWidget(self.image_label)

        splitter = QSplitter()
        splitter.addWidget(left_panel)
        splitter.addWidget(self.scroll)
        splitter.setSizes([360, 1140])  # даём превью больше базовой ширины
        splitter.setStretchFactor(0, 0)
        splitter.setStretchFactor(1, 1)

        central = QWidget()
        lay = QVBoxLayout(central)
        lay.setContentsMargins(0, 0, 0, 0)
        lay.addWidget(top)
        lay.addWidget(splitter, 1)
        self.setCentralWidget(central)

        # защита от рекурсивных сигналов при откатах
        self._block_validate = False
        self._load_titles()

    # -------------------------
    # Loading
    # -------------------------
    def open_any(self):
        paths, _ = QFileDialog.getOpenFileNames(
            self,
            "Выберите PSD / ZIP / RAR (архив можно только один)",
            os.fspath(self.projects_dir),
            "Photoshop / Archives (*.psd *.psb *.zip *.rar);;All Files (*.*)",
        )
        if not paths:
            return

        files = [Path(p) for p in paths]
        psd_files = [p for p in files if p.suffix.lower() in [".psd", ".psb"]]
        archive_files = [p for p in files if p.suffix.lower() in [".zip", ".rar"]]
        unsupported = [p for p in files if p.suffix.lower() not in [".psd", ".psb", ".zip", ".rar"]]

        if unsupported:
            QMessageBox.warning(
                self,
                "Неподдерживаемый формат",
                "Выбраны неподдерживаемые файлы:\n" + "\n".join(str(p.name) for p in unsupported),
            )
            return

        if archive_files:
            if len(archive_files) > 1 or psd_files:
                QMessageBox.warning(
                    self,
                    "Ограничение выбора",
                    "Можно выбрать либо несколько PSD/PSB, либо только один архив (ZIP/RAR).",
                )
                return
            archive_path = archive_files[0]
            self._open_archive(archive_path)
            return

        if not psd_files:
            QMessageBox.warning(self, "Открытие", "Не выбраны PSD/PSB файлы.")
            return

        self._open_psd_files(psd_files)

    def open_psd_folder(self):
        folder = QFileDialog.getExistingDirectory(
            self,
            "Выберите папку с PSD",
            os.fspath(self.projects_dir),
        )
        if not folder:
            return

        root = Path(folder)
        psd_files = sorted(
            [p for p in root.rglob("*") if p.is_file() and p.suffix.lower() in [".psd", ".psb"]],
            key=lambda p: p.as_posix().lower(),
        )

        if not psd_files:
            QMessageBox.information(self, "Пусто", "В выбранной папке PSD/PSB файлы не найдены.")
            return

        self._open_psd_files(psd_files, src_desc=f"{root.name} (папка)")

    def _open_archive(self, archive_path: Path):
        suffix = archive_path.suffix.lower()

        try:
            if suffix == ".zip":
                docs, src_desc = self._load_from_zip(str(archive_path))
            elif suffix == ".rar":
                docs, src_desc = self._load_from_rar(str(archive_path))
            else:
                raise ValueError("Неподдерживаемый формат.")
        except Exception as e:
            QMessageBox.critical(self, "Ошибка", f"Не удалось открыть:\n{e}")
            return

        self._set_loaded_docs(docs, src_desc)

    def set_projects_dir(self, projects_dir: str) -> None:
        self.projects_dir = Path(projects_dir).resolve()
        self._load_titles()

    def _open_psd_files(self, psd_files: List[Path], src_desc: Optional[str] = None):
        docs: List[PSDImage] = []
        errors: List[str] = []

        sorted_files = sorted(psd_files, key=lambda p: p.as_posix().lower())
        for psd_file in sorted_files:
            try:
                docs.extend(self._load_single_psd(str(psd_file)))
            except Exception as e:
                errors.append(f"{psd_file.name}: {e}")

        if errors:
            QMessageBox.warning(self, "Открытие PSD", "Некоторые файлы не открыты:\n" + "\n".join(errors))

        if src_desc is None:
            if len(sorted_files) == 1:
                src_desc = sorted_files[0].name
            else:
                src_desc = f"Выбрано PSD: {len(sorted_files)}"

        self._set_loaded_docs(docs, src_desc)

    def _set_loaded_docs(self, docs: List[PSDImage], src_desc: str):
        if not docs:
            QMessageBox.information(self, "Пусто", "PSD файлы не найдены.")
            return

        self.psd_docs = docs
        self._build_rows_from_docs()
        self._populate_table()
        self.info_label.setText(f"{src_desc} — PSD: {len(self.psd_docs)}; слоёв (картинок): {len(self.rows)}")

        # Показать первый ряд (если есть)
        if self.rows:
            self.table.selectRow(0)

    def _load_single_psd(self, filepath: str) -> List[PSDImage]:
        psd = PSDImage.open(filepath)
        # Если файл назван 001.psd — возьмём страницу из имени, иначе 1
        page = extract_page_from_name(Path(filepath).name) or 1
        psd._page_hint = page  # type: ignore[attr-defined]
        psd._file_hint = Path(filepath).name  # type: ignore[attr-defined]
        return [psd]

    def _load_from_zip(self, filepath: str) -> Tuple[List[PSDImage], str]:
        docs: List[PSDImage] = []
        with zipfile.ZipFile(filepath, "r") as zf:
            names = [n for n in zf.namelist() if extract_page_from_name(n) is not None]
            names.sort(key=lambda n: extract_page_from_name(n) or 0)

            for name in names:
                page = extract_page_from_name(name)
                if page is None:
                    continue
                data = zf.read(name)
                psd = PSDImage.open(io.BytesIO(data))
                psd._page_hint = page  # type: ignore[attr-defined]
                psd._file_hint = Path(name).name  # type: ignore[attr-defined]
                docs.append(psd)

        return docs, Path(filepath).name

    def _load_from_rar(self, filepath: str) -> Tuple[List[PSDImage], str]:
        rarfile = try_import_rarfile()
        if rarfile is None:
            raise RuntimeError("RAR не поддерживается: установите пакет 'rarfile' и утилиту unrar/bsdtar.")

        docs: List[PSDImage] = []
        with rarfile.RarFile(filepath) as rf:  # type: ignore
            names = [n for n in rf.namelist() if extract_page_from_name(n) is not None]
            names.sort(key=lambda n: extract_page_from_name(n) or 0)

            for name in names:
                page = extract_page_from_name(name)
                if page is None:
                    continue
                data = rf.read(name)
                psd = PSDImage.open(io.BytesIO(data))
                psd._page_hint = page  # type: ignore[attr-defined]
                psd._file_hint = Path(name).name  # type: ignore[attr-defined]
                docs.append(psd)

        return docs, Path(filepath).name

    def _build_rows_from_docs(self):
        self.rows.clear()

        pages = []
        for psd in self.psd_docs:
            page = getattr(psd, "_page_hint", None)
            if page is None:
                page = 1
                psd._page_hint = 1  # type: ignore[attr-defined]
            pages.append(page)

        self.available_pages = sorted(set(pages))

        for psd in self.psd_docs:
            file_name = getattr(psd, "_file_hint", "unknown.psd")
            page = int(getattr(psd, "_page_hint", 1))

            doc_rows: List[RowData] = []
            for prefix, layer in iter_leaf_layers(psd):
                title = (prefix + (layer.name or "(без имени)")).strip()
                # size: предпочтительно bbox; fallback на doc size
                try:
                    bbox = layer.bbox
                    w = int(bbox.width)
                    h = int(bbox.height)
                    if w <= 0 or h <= 0:
                        w, h = int(psd.width), int(psd.height)
                except Exception:
                    w, h = int(psd.width), int(psd.height)

                # Автозначения:
                typ = "Не импортировать"
                doc_rows.append(RowData(
                    file_name=file_name,
                    page=page,
                    layer_title=title,
                    layer_obj=layer,
                    size=(w, h),
                    typ=typ,
                    prev_page=page,
                    prev_typ=typ
                ))
            doc_rows.reverse()  # Photoshop часто хранит снизу вверх; разворачиваем для привычного порядка
            self._auto_assign_types(doc_rows)
            self.rows.extend(doc_rows)
        self._prepare_previews()

    def _auto_assign_types(self, doc_rows: List[RowData]) -> None:
        """
        Автоматически выставляем тип, если ровно два слоя на странице одинакового размера:
        нижний -> Исходник, верхний -> Клин.
        """
        rows_by_page: Dict[int, List[int]] = {}
        for idx, rd in enumerate(doc_rows):
            rows_by_page.setdefault(rd.page, []).append(idx)

        for page_indices in rows_by_page.values():
            size_buckets: Dict[Tuple[int, int], List[int]] = {}
            for idx in page_indices:
                size_buckets.setdefault(doc_rows[idx].size, []).append(idx)

            candidate_pairs = [idxs for idxs in size_buckets.values() if len(idxs) == 2]
            # Если больше одной пары, не гадаем, чтобы не нарушать уникальность по странице.
            if len(candidate_pairs) != 1:
                continue

            upper_idx, lower_idx = sorted(candidate_pairs[0])  # порядок слоёв: верхний -> нижний
            upper_row = doc_rows[upper_idx]
            lower_row = doc_rows[lower_idx]

            upper_row.typ = "Клин"
            upper_row.prev_typ = "Клин"
            lower_row.typ = "Исходник"
            lower_row.prev_typ = "Исходник"

    def swap_source_and_clean(self):
        """Поменять местами Исходник и Клин на каждой странице."""
        if not self.rows:
            return

        self._block_validate = True
        try:
            page_indices: Dict[int, Dict[str, int]] = {}
            for idx, rd in enumerate(self.rows):
                if rd.typ in ("Исходник", "Клин"):
                    page_indices.setdefault(rd.page, {})[rd.typ] = idx

            for indices in page_indices.values():
                if "Исходник" in indices and "Клин" in indices:
                    src_idx = indices["Исходник"]
                    clean_idx = indices["Клин"]

                    src_row = self.rows[src_idx]
                    clean_row = self.rows[clean_idx]

                    src_row.typ = "Клин"
                    src_row.prev_typ = "Клин"
                    clean_row.typ = "Исходник"
                    clean_row.prev_typ = "Исходник"

                    # Обновляем UI комбобоксов и текст
                    src_cb: QComboBox = self.table.cellWidget(src_idx, 3)  # type: ignore
                    clean_cb: QComboBox = self.table.cellWidget(clean_idx, 3)  # type: ignore
                    if src_cb:
                        src_cb.setCurrentText("Клин")
                    if clean_cb:
                        clean_cb.setCurrentText("Исходник")
            self._refresh_row_text(src_idx)
            self._refresh_row_text(clean_idx)
        finally:
            self._block_validate = False

    # -------------------------
    # Projects / save chapter
    # -------------------------
    def _load_titles(self):
        self.title_cb.clear()
        if self.projects_dir.exists():
            titles = sorted([p.name for p in self.projects_dir.iterdir() if p.is_dir()])
            self.title_cb.addItems(titles)
        # Установить главы для текущего текста, если есть
        self.on_title_changed(self.title_cb.currentText())

    def on_title_changed(self, title: str):
        self.chapter_cb.clear()
        if not title:
            return
        title_path = self.projects_dir / title
        if title_path.exists():
            chapters = sorted([p.name for p in title_path.iterdir() if p.is_dir()])
            self.chapter_cb.addItems(chapters)

    def save_chapter(self):
        title = self.title_cb.currentText().strip()
        chapter = self.chapter_cb.currentText().strip()

        if not title or not chapter:
            QMessageBox.warning(self, "Сохранение", "Укажите тайтл и главу.")
            return

        title_path = self.projects_dir / title
        chapter_path = title_path / chapter
        src_path = chapter_path / "src"
        clean_path = chapter_path / "clean_layers"

        try:
            src_path.mkdir(parents=True, exist_ok=True)
            clean_path.mkdir(parents=True, exist_ok=True)
        except Exception as e:
            QMessageBox.critical(self, "Сохранение", f"Не удалось создать папки:\n{e}")
            return

        pages = sorted(set(rd.page for rd in self.rows))
        saved_pages = 0
        errors: List[str] = []

        for page in pages:
            page_rows = [rd for rd in self.rows if rd.page == page]
            src = next((r for r in page_rows if r.typ == "Исходник"), None)
            clean = next((r for r in page_rows if r.typ == "Клин"), None)

            if not src and not clean:
                continue

            fname = f"{saved_pages:03d}.png"
            if src:
                try:
                    self._save_layer_image(src, src_path / fname)
                except Exception as e:
                    errors.append(f"Страница {page} (Исходник): {e}")

            if clean:
                try:
                    self._save_layer_image(clean, clean_path / fname)
                except Exception as e:
                    errors.append(f"Страница {page} (Клин): {e}")

            saved_pages += 1

        if errors:
            QMessageBox.warning(self, "Сохранение", "Сохранение завершено с ошибками:\n" + "\n".join(errors))
        else:
            QMessageBox.information(self, "Сохранение", f"Сохранено страниц: {saved_pages}")

    def _save_layer_image(self, rd: RowData, path: Path):
        img = rd.layer_obj.composite()
        if img is None:
            raise RuntimeError("Нечего сохранять (пустой слой).")
        img = img.convert("RGBA")
        img.save(path, format="PNG")

    # -------------------------
    # Table + validation
    # -------------------------
    def _populate_table(self):
        self.table.setRowCount(0)

        for i, rd in enumerate(self.rows):
            self.table.insertRow(i)

            # Col 0: text "filename: layer"
            c0 = QTableWidgetItem(self._format_row_title(rd))
            c0.setFlags(c0.flags() ^ Qt.ItemFlag.ItemIsEditable)
            self.table.setItem(i, 0, c0)

            # Col 1: size
            c1 = QTableWidgetItem(f"{rd.size[0]}×{rd.size[1]}")
            c1.setFlags(c1.flags() ^ Qt.ItemFlag.ItemIsEditable)
            self.table.setItem(i, 1, c1)

            # Col 2: page combo
            page_cb = QComboBox()
            for p in self.available_pages:
                page_cb.addItem(str(p), userData=p)
            # set current
            idx = page_cb.findData(rd.page)
            page_cb.setCurrentIndex(idx if idx >= 0 else 0)
            page_cb.currentIndexChanged.connect(lambda _idx, row=i: self.on_page_changed(row))
            self.table.setCellWidget(i, 2, page_cb)

            # Col 3: type combo
            type_cb = QComboBox()
            for t in self.TYPE_OPTIONS:
                type_cb.addItem(t)
            type_cb.setCurrentText(rd.typ)
            type_cb.currentIndexChanged.connect(lambda _idx, row=i: self.on_type_changed(row))
            self.table.setCellWidget(i, 3, type_cb)

        self.table.resizeColumnsToContents()
        # сделать первую колонку заметно уже
        # self.table.setColumnWidth(0, 180)

    def _format_row_title(self, rd: RowData) -> str:
        return f"{rd.file_name}: {rd.layer_title}"

    def _recompute_page_constraints(self) -> Tuple[Dict[int, Tuple[int, int]], Dict[int, Dict[str, int]]]:
        """
        Возвращает:
          page_size: page -> (w,h) (первый встретившийся импортируемый)
          page_type_owner: page -> {"Исходник": row_index, "Клин": row_index}
        """
        page_size: Dict[int, Tuple[int, int]] = {}
        page_type_owner: Dict[int, Dict[str, int]] = {}

        for idx, rd in enumerate(self.rows):
            if rd.typ == "Не импортировать":
                continue

            if rd.page not in page_size:
                page_size[rd.page] = rd.size

            if rd.page not in page_type_owner:
                page_type_owner[rd.page] = {}

            if rd.typ in ("Исходник", "Клин"):
                if rd.typ not in page_type_owner[rd.page]:
                    page_type_owner[rd.page][rd.typ] = idx

        return page_size, page_type_owner

    def on_page_changed(self, row: int):
        if self._block_validate:
            return

        rd = self.rows[row]
        page_cb: QComboBox = self.table.cellWidget(row, 2)  # type: ignore
        new_page = int(page_cb.currentData())
        old_page = rd.page

        if new_page == old_page:
            return

        # Проверка размера: нельзя назначить страницу, если размеры конфликтуют с уже назначенными на эту страницу (кроме "Не импортировать")
        page_size, page_type_owner = self._recompute_page_constraints()

        # Поскольку rd.page уже ещё старый в rows, проверяем как будто мы переносим:
        # 1) если rd тип "Не импортировать", размер-проверка не нужна (не участвует)
        if rd.typ != "Не импортировать":
            target_size = page_size.get(new_page)
            if target_size is not None and target_size != rd.size:
                self._rollback_page(row, old_page, f"Нельзя назначить страницу {new_page}: размер {rd.size[0]}×{rd.size[1]} "
                                                 f"не совпадает с уже назначенными на странице {new_page} ({target_size[0]}×{target_size[1]}).")
                return

            # 2) если тип Исходник/Клин, проверяем уникальность на новой странице
            if rd.typ in ("Исходник", "Клин"):
                owners = page_type_owner.get(new_page, {})
                if rd.typ in owners:
                    self._rollback_page(row, old_page, f"На странице {new_page} уже есть '{rd.typ}'.")
                    return

        # OK
        rd.prev_page = old_page
        rd.page = new_page
        self._refresh_row_text(row)

        # После смены страницы: если rd тип Исходник/Клин и он был единственным на старой странице — ок.
        # Ничего дополнительно делать не нужно.

    def on_type_changed(self, row: int):
        if self._block_validate:
            return

        rd = self.rows[row]
        type_cb: QComboBox = self.table.cellWidget(row, 3)  # type: ignore
        new_type = type_cb.currentText()
        old_type = rd.typ

        if new_type == old_type:
            return

        # Проверка уникальности "Исходник"/"Клин" на странице
        page_size, page_type_owner = self._recompute_page_constraints()

        # Если новый тип = Исходник/Клин, то:
        if new_type in ("Исходник", "Клин"):
            owners = page_type_owner.get(rd.page, {})
            # Если уже занят другим рядом — запрет
            if new_type in owners and owners[new_type] != row:
                self._rollback_type(row, old_type, f"На странице {rd.page} уже есть '{new_type}'.")
                return

            # Также если размеры на странице уже установлены (по импортируемым) — должны совпадать
            target_size = page_size.get(rd.page)
            if target_size is not None and target_size != rd.size:
                self._rollback_type(row, old_type, f"Нельзя назначить '{new_type}' на страницу {rd.page}: размер {rd.size[0]}×{rd.size[1]} "
                                                  f"не совпадает с уже назначенными на странице {rd.page} ({target_size[0]}×{target_size[1]}).")
                return

        # Если новый тип = "Не импортировать" — ограничений нет.
        rd.prev_typ = old_type
        rd.typ = new_type
        self._refresh_row_text(row)

    def _refresh_row_text(self, row: int):
        rd = self.rows[row]
        item = self.table.item(row, 0)
        if item:
            item.setText(self._format_row_title(rd))

    def _rollback_page(self, row: int, old_page: int, message: str):
        QMessageBox.warning(self, "Ограничение страницы", message)
        self._block_validate = True
        try:
            rd = self.rows[row]
            rd.page = old_page
            page_cb: QComboBox = self.table.cellWidget(row, 2)  # type: ignore
            idx = page_cb.findData(old_page)
            if idx >= 0:
                page_cb.setCurrentIndex(idx)
            self._refresh_row_text(row)
        finally:
            self._block_validate = False

    def _rollback_type(self, row: int, old_type: str, message: str):
        QMessageBox.warning(self, "Ограничение типа", message)
        self._block_validate = True
        try:
            rd = self.rows[row]
            rd.typ = old_type
            type_cb: QComboBox = self.table.cellWidget(row, 3)  # type: ignore
            type_cb.setCurrentText(old_type)
            self._refresh_row_text(row)
        finally:
            self._block_validate = False

    # -------------------------
    # Preview
    # -------------------------
    def _prepare_previews(self):
        """
        Рендерим все слои сразу и держим QPixmap в памяти,
        чтобы переключение строк было моментальным.
        """
        for rd in self.rows:
            rd.preview = None
            rd.preview_error = None
            try:
                img = rd.layer_obj.composite()
                if img is None:
                    rd.preview_error = "Нечего отображать (пустой/корректирующий слой)."
                    continue
                qimg = pil_to_qimage(img)
                rd.preview = QPixmap.fromImage(qimg)
            except Exception as e:
                rd.preview_error = f"Не удалось отрендерить слой: {e}"

    def on_row_selected(self):
        sel = self.table.selectedItems()
        if not sel:
            return
        row = sel[0].row()
        if row < 0 or row >= len(self.rows):
            return

        rd = self.rows[row]
        if rd.preview is not None:
            pix = rd.preview
            self.image_label.setPixmap(pix)
            self.image_label.setMinimumSize(pix.size())
            self.image_label.adjustSize()
            self.image_label.setToolTip(self._format_row_title(rd))
        elif rd.preview_error:
            self.image_label.setText(rd.preview_error)
        else:
            self.image_label.setText("Нечего отображать.")


def main():
    app = QApplication(sys.argv)
    w = PSDArchiveViewer()
    w.show()
    sys.exit(app.exec())


if __name__ == "__main__":
    main()
