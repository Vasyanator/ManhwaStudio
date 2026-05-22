from __future__ import annotations

from typing import List, Dict, Any
import traceback

from PyQt6.QtCore import QSignalBlocker
from PyQt6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QFormLayout,
    QFrame,
    QHBoxLayout,
    QLineEdit,
    QMessageBox,
    QWidget,
)

from config import EASYOCR_DIR
from ..utils import _numpy_from_qimage
from ..panels.ocr_langs import EASYOCR_FULL_LANGUAGES, EASYOCR_MAIN_LANGUAGES
from .base import OcrEngineBase


class EasyOcrEngine(OcrEngineBase):
    key = "easyocr"
    title = "EasyOCR"
    checkbox_label = "EasyOCR"

    def __init__(self, canvas):
        super().__init__(canvas)
        self.langs_text = "ko"
        self.show_full_langs = False
        self.gpu = False
        self.detail = 1
        self.paragraph = False
        self._reader = None

    def _ai_device_str(self) -> str:
        dev = getattr(self.canvas, "ai_device", None)
        if dev is None:
            return "cpu"
        return str(dev).strip().lower()

    def _gpu_from_ai_device(self) -> bool:
        dev = self._ai_device_str()
        return dev == "cuda" or dev.startswith("cuda:")

    # --- UI -----------------------------------------------------
    def build_ui(self, parent: QWidget, on_change) -> QWidget:
        self._on_change = on_change

        root = QFrame(parent)
        lay = QFormLayout(root)
        lay.setContentsMargins(0, 0, 0, 0)

        # выбор из списка
        lang_row = QHBoxLayout()
        self.lang_combo = QComboBox()
        self.lang_combo.setMinimumWidth(150)
        self.chk_full_langs = QCheckBox("Показать полный список языков")
        lang_row.addWidget(self.lang_combo)
        lang_row.addWidget(self.chk_full_langs)
        lang_row.addStretch(1)
        lay.addRow("Выбор языка:", lang_row)

        # ручной ввод
        self.ed_langs = QLineEdit(self.langs_text)
        lay.addRow("Языки:", self.ed_langs)

        self.chk_gpu = QCheckBox("GPU")
        self.gpu = self._gpu_from_ai_device()
        self.chk_gpu.setChecked(self.gpu)
        self.chk_gpu.setEnabled(False)
        self.chk_gpu.setToolTip("Управляется глобальной настройкой ИИ-устройства.")
        lay.addRow("", self.chk_gpu)

        self.chk_full_langs.stateChanged.connect(self._rebuild_language_list)
        self.chk_full_langs.stateChanged.connect(self._handle_change)
        self.lang_combo.currentTextChanged.connect(self._on_language_selected)
        self.ed_langs.textChanged.connect(self._handle_change)
        self.chk_gpu.stateChanged.connect(self._handle_change)

        self._rebuild_language_list()
        self._apply_state_to_ui()
        return root

    def _apply_state_to_ui(self):
        if not hasattr(self, "ed_langs"):
            return
        with QSignalBlocker(self.ed_langs):
            self.ed_langs.setText(self.langs_text)
        with QSignalBlocker(self.chk_full_langs):
            self.chk_full_langs.setChecked(self.show_full_langs)
        with QSignalBlocker(self.chk_gpu):
            self.gpu = self._gpu_from_ai_device()
            self.chk_gpu.setChecked(self.gpu)
        self._rebuild_language_list()

    def _lang_dict(self):
        return EASYOCR_FULL_LANGUAGES if self.chk_full_langs.isChecked() else EASYOCR_MAIN_LANGUAGES

    def _rebuild_language_list(self):
        if not hasattr(self, "lang_combo"):
            return
        lang_dict = self._lang_dict()
        current_text = self.lang_combo.currentText()
        with QSignalBlocker(self.lang_combo):
            self.lang_combo.clear()
            for code, name in lang_dict.items():
                self.lang_combo.addItem(f"{name} ({code})", code)
        idx = self.lang_combo.findText(current_text)
        if idx >= 0:
            self.lang_combo.setCurrentIndex(idx)

    def _on_language_selected(self):
        if self.lang_combo.currentIndex() < 0:
            return
        code = self.lang_combo.currentData()
        if not code:
            return
        langs = self._parse_langs()
        if code not in langs:
            langs.append(code)
        self.ed_langs.setText(", ".join(langs))

    def _parse_langs(self) -> List[str]:
        return [s.strip() for s in self.ed_langs.text().split(",") if s.strip()]

    def _handle_change(self):
        self.read_ui_state()
        if self._on_change:
            self._on_change()

    # --- Сохранение / загрузка ----------------------------------
    def read_ui_state(self):
        self.langs_text = self.ed_langs.text().strip()
        self.show_full_langs = self.chk_full_langs.isChecked()
        self.gpu = self._gpu_from_ai_device()
        if hasattr(self, "chk_gpu"):
            with QSignalBlocker(self.chk_gpu):
                self.chk_gpu.setChecked(self.gpu)
        self.reset()

    def validate(self) -> bool:
        langs = self._parse_langs()
        invalid = [lang for lang in langs if lang and lang not in EASYOCR_FULL_LANGUAGES]
        if invalid:
            if self._ui:
                QMessageBox.warning(
                    self._ui,
                    "Языки OCR",
                    f"Коды языков отсутствуют в списке EasyOCR: {', '.join(invalid)}",
                )
            return False
        return True

    def save_settings(self) -> Dict[str, Any]:
        return {
            "langs": self.langs_text,
            "gpu": self.gpu,
            "full_langs": self.show_full_langs,
        }

    def load_settings(self, data: Dict[str, Any]):
        if not isinstance(data, dict):
            return
        lang_raw = data.get("langs", self.langs_text)
        if isinstance(lang_raw, (list, tuple)):
            lang_raw = ", ".join(str(x) for x in lang_raw)
        self.langs_text = str(lang_raw) if lang_raw is not None else self.langs_text
        self.show_full_langs = bool(data.get("full_langs", self.show_full_langs))
        self.gpu = self._gpu_from_ai_device()
        self.reset()
        self._apply_state_to_ui()

    # --- Загрузка и работа --------------------------------------
    def _load_impl(self) -> bool:
        try:
            import easyocr  # type: ignore
        except Exception as e:
            print("[EasyOCR] not installed:", e)
            return False

        langs = self._parse_langs() or ["en"]
        try:
            try:
                self._reader = easyocr.Reader(
                    langs,
                    model_storage_directory=EASYOCR_DIR,
                    download_enabled=True,
                    gpu=self.gpu,
                )
            except Exception:
                traceback.print_exc()
                self._reader = easyocr.Reader(
                    langs,
                    model_storage_directory=EASYOCR_DIR,
                    download_enabled=True,
                    gpu=False,
                )
        except Exception as e:
            print("[EasyOCR] init failed:", e)
            self._reader = None
            return False
        return True

    def warmup(self):
        if not self._reader:
            return
        try:
            import numpy as np  # type: ignore

            dummy = np.zeros((8, 8, 3), dtype="uint8")
            self._reader.readtext(dummy, detail=0, paragraph=False)
        except Exception:
            traceback.print_exc()

    def recognize(self, qimage, join_newlines: bool, reflect_strings: bool) -> str:
        if not self.ensure_loaded() or not self._reader:
            return ""
        try:
            import numpy as np  # noqa: F401
        except Exception as e:
            print("[EasyOCR] numpy is required:", e)
            return ""

        img_np = _numpy_from_qimage(qimage)

        try:
            res = self._reader.readtext(
                img_np,
                detail=self.detail,
                paragraph=self.paragraph,
            )
            texts: List[str] = []
            if self.detail == 0:
                texts = list(map(str, res))
            else:
                texts = [t[1] for t in res]
            if reflect_strings:
                texts = list(reversed(texts))
            s = "\n".join(texts) if join_newlines else " ".join(texts)
            return s.strip()
        except Exception:
            traceback.print_exc()
            return ""
