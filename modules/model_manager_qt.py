# -*- coding: utf-8 -*-
"""
Менеджер моделей ИИ для ManhwaStudio (PyQt6 версия).

Состояния (глобальные):
- "Нужно установить модели ИИ"   (красный)
- "Не все модели ИИ установлены" (жёлтый)
- "Модели ИИ установлены"        (зелёный)

Секции окна: PaddleOCR / EasyOCR / Удаление с фото (advimman/lama).
"""

from __future__ import annotations
import os
import threading
import traceback
from typing import Callable, Optional, Dict, Literal

from PyQt6.QtCore import QObject, pyqtSignal, Qt, QTimer
from PyQt6.QtWidgets import (
    QDialog, QVBoxLayout, QHBoxLayout, QLabel, QPushButton,
    QScrollArea, QWidget, QFrame, QGroupBox, QMessageBox
)
from PyQt6.QtGui import QColor, QPainter, QPen, QBrush
from ui_new.theme import apply_theme
from config import (
    AOT_DIR,
    LAMA_DIR,
    LAMA_MPE_DIR,
    TEXT_DETECTOR_DIR,
    TEXT_DETECTOR_ONNX_DIR,
)
import zipfile
import tempfile
import shutil
import requests
import re
from urllib.parse import urljoin, urlparse, parse_qs, urlencode, urlunparse, parse_qsl
# === Конфигурация PaddleOCR ===
PD_OCR_MODELS: Dict[str, str] = {
    "Корейский": "PaddlePaddle/korean_PP-OCRv5_mobile_rec",
    "Китайский/Японский/Английский": "PaddlePaddle/PP-OCRv5_server_rec",
}
LAMA_DL = "https://drive.google.com/uc?export=download&id=11RbsVSav3O-fReBsPHBE1nn8kcFIMnKp"
LAMA_MPE_DL = 'https://github.com/zyddnys/manga-image-translator/releases/download/beta-0.3/inpainting_lama_mpe.ckpt'
AOT_DL = "https://github.com/zyddnys/manga-image-translator/releases/download/beta-0.3/inpainting.ckpt"
TEXT_DETECTOR_DL = "https://github.com/zyddnys/manga-image-translator/releases/download/beta-0.3/comictextdetector.pt"
TEXT_DETECTOR_ONNX_DL = "https://github.com/zyddnys/manga-image-translator/releases/download/beta-0.3/comictextdetector.pt.onnx"
# === Цвета индикаторов ===
COLOR_RED = "#f44336"
COLOR_YELLOW = "#ff9800"
COLOR_GREEN = "#4caf50"
COLOR_GREY = "#9e9e9e"

# === Типы состояний модели ===
ModelState = Literal["missing", "downloading", "present"]


def _models_root() -> str:
    home = os.path.expanduser("~")
    return os.path.join(home, ".paddlex", "official_models")


def _repo_local_dir(repo_id: str) -> str:
    """Локальная папка модели: ~/.paddlex/official_models/<repo_name>/"""
    repo_name = repo_id.split("/", 1)[-1]
    return os.path.join(_models_root(), repo_name)


def _dir_has_files(path: str) -> bool:
    if not os.path.isdir(path):
        return False
    for _, _, files in os.walk(path):
        if files:
            return True
    return False


class StatusIndicator(QWidget):
    """Круглый индикатор состояния."""

    def __init__(self, parent=None, size: int = 14):
        super().__init__(parent)
        self._color = QColor(COLOR_GREY)
        self._size = size
        self.setFixedSize(size, size)

    def set_color(self, color: str):
        self._color = QColor(color)
        self.update()

    def paintEvent(self, event):
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        painter.setPen(QPen(self._color, 1))
        painter.setBrush(QBrush(self._color))
        margin = 2
        painter.drawEllipse(margin, margin, self._size - 2*margin, self._size - 2*margin)


class ModelRowWidget(QWidget):
    """Строка модели с индикатором, названием и кнопкой."""

    def __init__(self, name: str, parent=None):
        super().__init__(parent)
        self._name = name

        layout = QHBoxLayout(self)
        layout.setContentsMargins(8, 4, 8, 4)
        layout.setSpacing(8)

        # Название модели
        self.lbl_name = QLabel(name)
        self.lbl_name.setMinimumWidth(220)
        layout.addWidget(self.lbl_name)

        # Индикатор
        self.indicator = StatusIndicator(self)
        layout.addWidget(self.indicator)

        # Статус текст
        self.lbl_status = QLabel("Нет")
        self.lbl_status.setMinimumWidth(100)
        layout.addWidget(self.lbl_status)

        # Кнопка
        self.btn_action = QPushButton("Скачать")
        self.btn_action.setFixedWidth(100)
        layout.addWidget(self.btn_action)

        layout.addStretch()

    def set_state(self, state: ModelState):
        color_map = {
            "missing": COLOR_RED,
            "downloading": COLOR_YELLOW,
            "present": COLOR_GREEN,
        }
        text_map = {
            "missing": "Нет",
            "downloading": "Качается…",
            "present": "Готово",
        }

        self.indicator.set_color(color_map[state])
        self.lbl_status.setText(text_map[state])
        self.btn_action.setEnabled(state == "missing")


class ModelManagerQt(QObject):
    """
    Менеджер моделей ИИ (PyQt6 версия).

    Публичный контракт:
    - get_status() -> dict(text, color)
    - open_manager(parent) -> None
    - statusChanged: pyqtSignal(dict)
    """

    statusChanged = pyqtSignal(dict)

    def __init__(self, parent=None) -> None:
        super().__init__(parent)

        # Состояния PaddleOCR по моделям
        self._pd_states: Dict[str, ModelState] = {}
        self._lama_state: ModelState = "missing"
        self._lama_mpe_state: ModelState = "missing"
        self._aot_state: ModelState = "missing"
        self._text_detector_state: ModelState = "missing"

        # Виджеты строк (для обновления UI)
        self._pd_row_widgets: Dict[str, ModelRowWidget] = {}
        self._lama_row_widget: Optional[ModelRowWidget] = None
        self._lama_mpe_row_widget: Optional[ModelRowWidget] = None
        self._aot_row_widget: Optional[ModelRowWidget] = None
        self._text_detector_row_widget: Optional[ModelRowWidget] = None

        # Диалог менеджера
        self._dialog: Optional[QDialog] = None

        # Начальная проверка наличия моделей
        self._ensure_dirs()
        self._init_states()
        apply_theme()
        # Сообщим о глобальном статусе
        QTimer.singleShot(0, self._emit_global)

    # ---------- Публичный API ----------
    def get_status(self) -> dict:
        # # PaddleOCR
        # count_total_pd = len(PD_OCR_MODELS)
        # count_present_pd = sum(1 for k in PD_OCR_MODELS if self._pd_states.get(k) == "present")

        # LaMA/AOT: считаем как отдельные «модели». Paddle временно не учитываем.
        lama_present = 1 if self._lama_state == "present" else 0
        lama_mpe_present = 1 if self._lama_mpe_state == "present" else 0
        aot_present = 1 if self._aot_state == "present" else 0
        text_detector_present = 1 if self._text_detector_state == "present" else 0
        count_total = 4
        count_present = lama_present + lama_mpe_present + aot_present + text_detector_present

        if count_present == 0:
            return {"text": "Нужно установить модели ИИ", "color": COLOR_RED}
        elif count_present < count_total:
            return {"text": "Не все модели ИИ установлены", "color": COLOR_YELLOW}
        else:
            return {"text": "Модели ИИ установлены", "color": COLOR_GREEN}

    def open_manager(self, parent=None) -> None:
        if self._dialog is not None and self._dialog.isVisible():
            self._dialog.raise_()
            self._dialog.activateWindow()
            return

        self._dialog = QDialog(parent)
        self._dialog.setWindowTitle("Менеджер моделей ИИ")
        self._dialog.resize(700, 520)
        self._dialog.setMinimumSize(600, 400)

        main_layout = QVBoxLayout(self._dialog)
        main_layout.setContentsMargins(10, 10, 10, 10)

        # Скроллируемая область
        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)

        scroll_content = QWidget()
        scroll_layout = QVBoxLayout(scroll_content)
        scroll_layout.setSpacing(10)

        # --- Секция: PaddleOCR ---
        # pd_group = QGroupBox("PaddleOCR")
        # pd_layout = QVBoxLayout(pd_group)
        # pd_layout.setSpacing(4)

        # # Заголовок
        # header = QWidget()
        # header_layout = QHBoxLayout(header)
        # header_layout.setContentsMargins(8, 6, 8, 2)

        # lbl_model = QLabel("Модель")
        # lbl_model.setMinimumWidth(220)
        # lbl_model.setStyleSheet("font-weight: bold;")
        # header_layout.addWidget(lbl_model)

        # # Пустой индикатор для выравнивания
        # header_layout.addSpacing(14 + 8)

        # lbl_status = QLabel("Статус")
        # lbl_status.setMinimumWidth(100)
        # lbl_status.setStyleSheet("font-weight: bold;")
        # header_layout.addWidget(lbl_status)

        # lbl_action = QLabel("Действие")
        # lbl_action.setStyleSheet("font-weight: bold;")
        # header_layout.addWidget(lbl_action)

        # header_layout.addStretch()
        # pd_layout.addWidget(header)

        # # Разделитель
        # separator = QFrame()
        # separator.setFrameShape(QFrame.Shape.HLine)
        # separator.setFrameShadow(QFrame.Shadow.Sunken)
        # pd_layout.addWidget(separator)

        # # Строки моделей
        # for display_name, repo_id in PD_OCR_MODELS.items():
        #     row = ModelRowWidget(display_name, pd_group)
        #     row.set_state(self._pd_states.get(display_name, "missing"))
        #     row.btn_action.clicked.connect(
        #         lambda checked, n=display_name, r=repo_id: self._download_pd_model(n, r)
        #     )
        #     self._pd_row_widgets[display_name] = row
        #     pd_layout.addWidget(row)

        # scroll_layout.addWidget(pd_group)

        # --- Секция: EasyOCR (заглушка) ---
        ez_group = QGroupBox("EasyOCR/PaddleOCR")
        ez_layout = QVBoxLayout(ez_group)
        ez_layout.addWidget(QLabel("Нужные ИИ модели будут загружены автоматически при использовании."))
        scroll_layout.addWidget(ez_group)

        # --- Секция: Удаление с фото (advimman/lama) ---
        lama_group = QGroupBox("Удаление под маской (advimman/lama)")
        lama_layout = QVBoxLayout(lama_group)

        lama_row = ModelRowWidget("big-lama", lama_group)
        lama_row.set_state(self._lama_state)
        lama_row.btn_action.clicked.connect(self._download_lama)
        self._lama_row_widget = lama_row
        lama_layout.addWidget(lama_row)

        scroll_layout.addWidget(lama_group)

        # --- Секция: Text Detector ---
        text_detector_group = QGroupBox("Детектор текста")
        text_detector_layout = QVBoxLayout(text_detector_group)

        text_detector_row = ModelRowWidget("comictextdetector.pt (+ .onnx)", text_detector_group)
        text_detector_row.set_state(self._text_detector_state)
        text_detector_row.btn_action.clicked.connect(self._download_text_detector)
        self._text_detector_row_widget = text_detector_row
        text_detector_layout.addWidget(text_detector_row)

        scroll_layout.addWidget(text_detector_group)

        # --- Секция: LaMA MPE ---
        lama_mpe_group = QGroupBox("LaMA MPE (не такая умная, но чуть лучше с аниме)")
        lama_mpe_layout = QVBoxLayout(lama_mpe_group)

        lama_mpe_row = ModelRowWidget("inpainting_lama_mpe.ckpt", lama_mpe_group)
        lama_mpe_row.set_state(self._lama_mpe_state)
        lama_mpe_row.btn_action.clicked.connect(self._download_lama_mpe)
        self._lama_mpe_row_widget = lama_mpe_row
        lama_mpe_layout.addWidget(lama_mpe_row)

        scroll_layout.addWidget(lama_mpe_group)

        # --- Секция: AOT Inpaint ---
        aot_group = QGroupBox("AOT Inpaint (самая маленькая)")
        aot_layout = QVBoxLayout(aot_group)

        aot_row = ModelRowWidget("inpainting.ckpt", aot_group)
        aot_row.set_state(self._aot_state)
        aot_row.btn_action.clicked.connect(self._download_aot)
        self._aot_row_widget = aot_row
        aot_layout.addWidget(aot_row)

        scroll_layout.addWidget(aot_group)

        scroll_layout.addStretch()
        scroll.setWidget(scroll_content)
        main_layout.addWidget(scroll)

        # Кнопка закрытия
        btn_close = QPushButton("Закрыть")
        btn_close.clicked.connect(self._dialog.close)
        main_layout.addWidget(btn_close, alignment=Qt.AlignmentFlag.AlignRight)

        self._dialog.show()

    # ---------- Внутреннее ----------
    def _ensure_dirs(self) -> None:
        os.makedirs(_models_root(), exist_ok=True)
        for path in (LAMA_DIR, LAMA_MPE_DIR, AOT_DIR, TEXT_DETECTOR_DIR, TEXT_DETECTOR_ONNX_DIR):
            os.makedirs(path, exist_ok=True)

    def _init_states(self) -> None:
        # for name, repo in PD_OCR_MODELS.items():
        #     local = _repo_local_dir(repo)
        #     self._pd_states[name] = "present" if _dir_has_files(local) else "missing"
        self._init_lama_state()
        self._init_lama_mpe_state()
        self._init_aot_state()
        self._init_text_detector_state()

    def _emit_global(self) -> None:
        self.statusChanged.emit(self.get_status())

    def _update_pd_row_ui(self, display_name: str) -> None:
        if display_name in self._pd_row_widgets:
            state = self._pd_states.get(display_name, "missing")
            self._pd_row_widgets[display_name].set_state(state)
        self._emit_global()

    def _update_lama_row_ui(self) -> None:
        if self._lama_row_widget:
            self._lama_row_widget.set_state(self._lama_state)
        self._emit_global()

    def _update_lama_mpe_row_ui(self) -> None:
        if self._lama_mpe_row_widget:
            self._lama_mpe_row_widget.set_state(self._lama_mpe_state)
        self._emit_global()

    def _update_aot_row_ui(self) -> None:
        if self._aot_row_widget:
            self._aot_row_widget.set_state(self._aot_state)
        self._emit_global()

    def _update_text_detector_row_ui(self) -> None:
        if self._text_detector_row_widget:
            self._text_detector_row_widget.set_state(self._text_detector_state)
        self._emit_global()

    def _download_pd_model(self, display_name: str, repo_id: str) -> None:
        # Переводим строку в состояние "downloading"
        self._pd_states[display_name] = "downloading"
        self._update_pd_row_ui(display_name)

        def worker():
            try:
                try:
                    from huggingface_hub import snapshot_download
                except Exception:
                    raise RuntimeError(
                        "Не установлен huggingface_hub. Установите пакет: pip install huggingface_hub"
                    )

                local_dir = _repo_local_dir(repo_id)
                os.makedirs(local_dir, exist_ok=True)

                # Скачиваем без симлинков
                snapshot_download(
                    repo_id=repo_id,
                    local_dir=local_dir,
                    local_dir_use_symlinks=False,
                    ignore_patterns=None,
                    resume_download=True,
                )

                # Проверим наличие файлов
                if _dir_has_files(local_dir):
                    self._pd_states[display_name] = "present"
                else:
                    self._pd_states[display_name] = "missing"

            except Exception as e:
                self._pd_states[display_name] = "missing"
                msg = f"Не удалось скачать {display_name} ({repo_id}).\n\n{e}"
                self._post_to_ui(lambda: self._show_error("Ошибка загрузки", msg))
                traceback.print_exc()
            finally:
                self._post_to_ui(lambda: self._update_pd_row_ui(display_name))

        threading.Thread(target=worker, daemon=True).start()

    def _download_lama(self) -> None:
        """Скачать big-lama.zip с Google Drive в LAMA_DIR, распаковать."""
        self._lama_state = "downloading"
        self._update_lama_row_ui()

        def worker():
            zip_path = None
            tmp_dir = None
            try:
                os.makedirs(LAMA_DIR, exist_ok=True)
                zip_path = os.path.join(LAMA_DIR, "big-lama.zip")

                # 1) Скачиваем с обходом confirm
                self._gdrive_download_large(LAMA_DL, zip_path)

                # 2) Проверяем, что это действительно zip
                if not zipfile.is_zipfile(zip_path):
                    try:
                        with open(zip_path, "rb") as f:
                            head = f.read(200)
                    except Exception:
                        head = b""
                    raise RuntimeError(
                        "Файл, полученный от Google Drive, не является ZIP. "
                        "Частая причина — страница-предупреждение/квота. "
                        f"Первые байты: {head[:50]!r}"
                    )

                # 3) Распаковка в temp и перенос нужных файлов
                tmp_dir = tempfile.mkdtemp(prefix="lama_unzip_")
                with zipfile.ZipFile(zip_path, "r") as zf:
                    namelist = zf.namelist()

                    # config.yaml
                    cfg_member = "big-lama/config.yaml"
                    if cfg_member not in namelist:
                        raise RuntimeError("В архиве нет big-lama/config.yaml")
                    zf.extract(cfg_member, tmp_dir)
                    shutil.move(os.path.join(tmp_dir, cfg_member), os.path.join(LAMA_DIR, "config.yaml"))

                    # models/**
                    prefix = "big-lama/models/"
                    to_extract = [n for n in namelist if n.startswith(prefix)]
                    if not to_extract:
                        raise RuntimeError("В архиве нет папки big-lama/models")
                    for n in to_extract:
                        zf.extract(n, tmp_dir)

                    src_models = os.path.join(tmp_dir, "big-lama", "models")
                    dst_models = os.path.join(LAMA_DIR, "models")
                    if os.path.isdir(dst_models):
                        shutil.rmtree(dst_models)
                    shutil.move(src_models, dst_models)

                # 4) Успех → пересчитать состояние
                self._init_lama_state()

            except Exception as e:
                self._lama_state = "missing"
                self._post_to_ui(lambda: self._show_error("Ошибка загрузки LaMA", str(e)))
                traceback.print_exc()
            finally:
                # zip удаляем только при успехе
                if self._lama_state == "present":
                    try:
                        if zip_path and os.path.isfile(zip_path):
                            os.remove(zip_path)
                    except Exception:
                        pass
                try:
                    if tmp_dir and os.path.isdir(tmp_dir):
                        shutil.rmtree(tmp_dir, ignore_errors=True)
                except Exception:
                    pass
                self._post_to_ui(self._update_lama_row_ui)

        threading.Thread(target=worker, daemon=True).start()

    def _download_lama_mpe(self) -> None:
        """Скачать lama_mpe checkpoint."""
        self._lama_mpe_state = "downloading"
        self._update_lama_mpe_row_ui()

        def worker():
            dst = os.path.join(LAMA_MPE_DIR, "inpainting_lama_mpe.ckpt")
            try:
                self._download_file(LAMA_MPE_DL, dst)
                self._init_lama_mpe_state()
            except Exception as e:
                self._lama_mpe_state = "missing"
                self._post_to_ui(lambda: self._show_error("Ошибка загрузки LaMA MPE", str(e)))
                traceback.print_exc()
            finally:
                self._post_to_ui(self._update_lama_mpe_row_ui)

        threading.Thread(target=worker, daemon=True).start()

    def _download_aot(self) -> None:
        """Скачать AOT checkpoint."""
        self._aot_state = "downloading"
        self._update_aot_row_ui()

        def worker():
            dst = os.path.join(AOT_DIR, "inpainting.ckpt")
            try:
                self._download_file(AOT_DL, dst)
                self._init_aot_state()
            except Exception as e:
                self._aot_state = "missing"
                self._post_to_ui(lambda: self._show_error("Ошибка загрузки AOT", str(e)))
                traceback.print_exc()
            finally:
                self._post_to_ui(self._update_aot_row_ui)

        threading.Thread(target=worker, daemon=True).start()

    def _download_text_detector(self) -> None:
        """Скачать две модели детектора текста."""
        self._text_detector_state = "downloading"
        self._update_text_detector_row_ui()

        def worker():
            dst1 = os.path.join(TEXT_DETECTOR_DIR, "comictextdetector.pt")
            dst2 = os.path.join(TEXT_DETECTOR_ONNX_DIR, "comictextdetector.pt.onnx")
            try:
                self._download_file(TEXT_DETECTOR_DL, dst1)
                self._download_file(TEXT_DETECTOR_ONNX_DL, dst2)
                self._init_text_detector_state()
            except Exception as e:
                self._text_detector_state = "missing"
                self._post_to_ui(lambda: self._show_error("Ошибка загрузки Text Detector", str(e)))
                traceback.print_exc()
            finally:
                self._post_to_ui(self._update_text_detector_row_ui)

        threading.Thread(target=worker, daemon=True).start()

    def _init_lama_state(self) -> None:
        """Проверяем наличие big-lama."""
        try:
            cfg = os.path.join(LAMA_DIR, "config.yaml")
            models = os.path.join(LAMA_DIR, "models")
            exists = os.path.isfile(cfg) and os.path.isdir(models) and _dir_has_files(models)
            self._lama_state = "present" if exists else "missing"
        except Exception:
            self._lama_state = "missing"

    def _init_lama_mpe_state(self) -> None:
        """Проверяем наличие чекпойнта LaMA MPE."""
        try:
            ckpt = os.path.join(LAMA_MPE_DIR, "inpainting_lama_mpe.ckpt")
            exists = os.path.isfile(ckpt) and os.path.getsize(ckpt) > 0
            self._lama_mpe_state = "present" if exists else "missing"
        except Exception:
            self._lama_mpe_state = "missing"

    def _init_aot_state(self) -> None:
        """Проверяем наличие чекпойнта AOT."""
        try:
            ckpt = os.path.join(AOT_DIR, "inpainting.ckpt")
            exists = os.path.isfile(ckpt) and os.path.getsize(ckpt) > 0
            self._aot_state = "present" if exists else "missing"
        except Exception:
            self._aot_state = "missing"

    def _init_text_detector_state(self) -> None:
        """Проверяем наличие моделей ComicTextDetector (.pt и .onnx)."""
        try:
            ckpt1 = os.path.join(TEXT_DETECTOR_DIR, "comictextdetector.pt")
            ckpt2 = os.path.join(TEXT_DETECTOR_ONNX_DIR, "comictextdetector.pt.onnx")
            exists = os.path.isfile(ckpt1) and os.path.getsize(ckpt1) > 0 and os.path.isfile(ckpt2) and os.path.getsize(ckpt2) > 0
            self._text_detector_state = "present" if exists else "missing"
        except Exception:
            self._text_detector_state = "missing"

    def _post_to_ui(self, fn: Callable[[], None]) -> None:
        """Выполнить fn в главном UI-потоке через QTimer."""
        QTimer.singleShot(0, fn)

    def _show_error(self, title: str, message: str) -> None:
        """Показать диалог ошибки."""
        QMessageBox.critical(self._dialog, title, message)

    def _download_file(self, url: str, dst_path: str) -> None:
        """Скачать файл по прямой ссылке в dst_path."""
        os.makedirs(os.path.dirname(dst_path), exist_ok=True)
        tmp_path = dst_path + ".part"
        try:
            with requests.get(url, stream=True, allow_redirects=True) as r:
                r.raise_for_status()
                with open(tmp_path, "wb") as f:
                    for chunk in r.iter_content(chunk_size=1 << 20):
                        if chunk:
                            f.write(chunk)
            os.replace(tmp_path, dst_path)
        finally:
            if os.path.isfile(tmp_path) and not os.path.isfile(dst_path):
                try:
                    os.remove(tmp_path)
                except Exception:
                    pass

    def _gdrive_download_large(self, url: str, dst_path: str) -> None:
        """Скачивание крупных файлов с Google Drive с обходом предупреждения."""

        def _save_stream(resp, path):
            with open(path, "wb") as f:
                for chunk in resp.iter_content(chunk_size=1 << 20):
                    if chunk:
                        f.write(chunk)

        def _looks_like_file(resp) -> bool:
            if "Content-Disposition" in resp.headers:
                return True
            ctype = resp.headers.get("Content-Type", "")
            if "zip" in ctype or "octet-stream" in ctype:
                return True
            return False

        sess = requests.Session()

        # Извлечь file_id из URL
        file_id = self._drive_file_id_from_url(url)
        if not file_id:
            first = sess.get(url, allow_redirects=True)
        else:
            base_url = f"https://drive.google.com/uc?export=download&id={file_id}"
            first = sess.get(base_url, allow_redirects=True)

        first.raise_for_status()

        # Если сразу файл — сохраняем
        if _looks_like_file(first):
            _save_stream(first, dst_path)
            return

        # Иначе у нас HTML: пробуем найти confirm-токен
        html = first.text

        # Новый случай: drive.usercontent.google.com/download
        parsed = urlparse(first.url)
        if parsed.netloc.endswith("usercontent.google.com") and parsed.path.endswith("/download"):
            q = dict(parse_qsl(parsed.query))
            q["confirm"] = q.get("confirm", "t")
            confirm_url = urlunparse(parsed._replace(query=urlencode(q)))
            r2 = sess.get(confirm_url, stream=True, allow_redirects=True)
            r2.raise_for_status()
            if _looks_like_file(r2):
                _save_stream(r2, dst_path)
                return
            r3 = sess.get(urlunparse(parsed), stream=True, allow_redirects=True)
            r3.raise_for_status()
            if _looks_like_file(r3):
                _save_stream(r3, dst_path)
                return

        # Попробуем вытащить href-ссылку
        m_href = re.search(r'href="(/uc\?export=download[^"]+)"', html)
        if m_href:
            confirm_url = urljoin("https://docs.google.com", m_href.group(1))
            resp3 = sess.get(confirm_url, stream=True, allow_redirects=True)
            resp3.raise_for_status()
            if not _looks_like_file(resp3):
                raise RuntimeError("Google Drive не отдал файл (получен HTML).")
            _save_stream(resp3, dst_path)
            return

        # Запасной путь
        if file_id:
            resp4 = sess.get(
                f"https://drive.google.com/uc?export=download&id={file_id}",
                stream=True, allow_redirects=True
            )
            resp4.raise_for_status()
            if not _looks_like_file(resp4):
                raise RuntimeError("Не удалось получить zip с Google Drive.")
            _save_stream(resp4, dst_path)
            return

        raise RuntimeError("Не удалось получить ссылку скачивания Google Drive.")

    def _drive_file_id_from_url(self, url: str, html: str = "") -> Optional[str]:
        # из ?id=...
        try:
            q = parse_qs(urlparse(url).query)
            if "id" in q and q["id"]:
                return q["id"][0]
        except Exception:
            pass
        # из /file/d/<id>/
        m = re.search(r"/file/d/([A-Za-z0-9_-]+)", url)
        if m:
            return m.group(1)
        # из hidden input
        m = re.search(r'name="id"\s+value="([^"]+)"', html)
        if m:
            return m.group(1)
        return None
