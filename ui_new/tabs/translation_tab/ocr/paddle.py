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

from ..utils import _numpy_from_qimage
from ..panels.ocr_langs import PADDLEOCR_FULL_LANGUAGES, PADDLEOCR_MAIN_LANGUAGES
from .base import OcrEngineBase


class PaddleOcrEngine(OcrEngineBase):
    key = "paddle"
    title = "PaddleOCR"
    checkbox_label = "PaddleOCR"

    def __init__(self, canvas):
        super().__init__(canvas)
        self.lang_text = "ko"
        self.show_full_langs = False
        self.gpu = False
        self._reader = None
        self._cv2 = None

    # --- UI -----------------------------------------------------
    def build_ui(self, parent: QWidget, on_change) -> QWidget:
        self._on_change = on_change

        root = QFrame(parent)
        lay = QFormLayout(root)
        lay.setContentsMargins(0, 0, 0, 0)

        lang_row = QHBoxLayout()
        self.lang_combo = QComboBox()
        self.lang_combo.setMinimumWidth(150)
        self.chk_full_langs = QCheckBox("Показать полный список языков")
        lang_row.addWidget(self.lang_combo)
        lang_row.addWidget(self.chk_full_langs)
        lang_row.addStretch(1)
        lay.addRow("Выбор языка:", lang_row)

        self.ed_lang = QLineEdit(self.lang_text)
        lay.addRow("Язык:", self.ed_lang)

        self.chk_gpu = QCheckBox("GPU")
        self.chk_gpu.setChecked(self.gpu)
        lay.addRow("", self.chk_gpu)

        self.chk_full_langs.stateChanged.connect(self._rebuild_language_list)
        self.chk_full_langs.stateChanged.connect(self._handle_change)
        self.lang_combo.currentTextChanged.connect(self._on_language_selected)
        self.ed_lang.textChanged.connect(self._handle_change)
        self.chk_gpu.stateChanged.connect(self._handle_change)

        self._rebuild_language_list()
        self._apply_state_to_ui()
        return root

    def _apply_state_to_ui(self):
        if not hasattr(self, "ed_lang"):
            return
        with QSignalBlocker(self.ed_lang):
            self.ed_lang.setText(self.lang_text)
        with QSignalBlocker(self.chk_full_langs):
            self.chk_full_langs.setChecked(self.show_full_langs)
        with QSignalBlocker(self.chk_gpu):
            self.chk_gpu.setChecked(self.gpu)
        self._rebuild_language_list()

    def _lang_dict(self):
        return PADDLEOCR_FULL_LANGUAGES if self.chk_full_langs.isChecked() else PADDLEOCR_MAIN_LANGUAGES

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
        self.ed_lang.setText(str(code))

    def _handle_change(self):
        self.read_ui_state()
        if self._on_change:
            self._on_change()

    # --- Сохранение / загрузка ----------------------------------
    def read_ui_state(self):
        self.lang_text = self.ed_lang.text().strip()
        self.show_full_langs = self.chk_full_langs.isChecked()
        self.gpu = self.chk_gpu.isChecked()
        self.reset()

    def validate(self) -> bool:
        langs = [s.strip() for s in self.lang_text.split(",") if s.strip()]
        if len(langs) > 1:
            if self._ui:
                QMessageBox.warning(self._ui, "Языки OCR", "Для PaddleOCR можно указать только один язык.")
            return False
        invalid = [lang for lang in langs if lang and lang not in PADDLEOCR_FULL_LANGUAGES]
        if invalid:
            if self._ui:
                QMessageBox.warning(
                    self._ui,
                    "Языки OCR",
                    f"Коды языков отсутствуют в списке PaddleOCR: {', '.join(invalid)}",
                )
            return False
        return True

    def save_settings(self) -> Dict[str, Any]:
        return {
            "langs": self.lang_text,
            "gpu": self.gpu,
            "full_langs": self.show_full_langs,
        }

    def load_settings(self, data: Dict[str, Any]):
        if not isinstance(data, dict):
            return
        lang_raw = data.get("langs", self.lang_text)
        if isinstance(lang_raw, (list, tuple)):
            lang_raw = ", ".join(str(x) for x in lang_raw)
        self.lang_text = str(lang_raw) if lang_raw is not None else self.lang_text
        self.show_full_langs = bool(data.get("full_langs", self.show_full_langs))
        self.gpu = bool(data.get("gpu", self.gpu))
        self.reset()
        self._apply_state_to_ui()

    # --- Загрузка и работа --------------------------------------
    def _load_impl(self) -> bool:
        try:
            from paddleocr import PaddleOCR
            import cv2  # type: ignore
        except Exception as e:
            print("[PaddleOCR] not installed:", e)
            return False

        lang = self._map_lang_for_paddle((self.lang_text or "en").strip() or "en")
        try:
            self._reader = PaddleOCR(
                use_doc_orientation_classify=False,
                use_doc_unwarping=False,
                use_textline_orientation=False,
                lang=lang,
            )
        except Exception as e:
            print("[PaddleOCR] init failed:", e)
            self._reader = None
            return False
        self._cv2 = cv2
        return True

    def warmup(self):
        if not self._reader or not self._cv2:
            return
        try:
            import numpy as np  # type: ignore

            dummy = np.zeros((16, 16, 3), dtype="uint8")
            bgr = self._cv2.cvtColor(dummy, self._cv2.COLOR_RGB2BGR)
            self._reader.predict(input=bgr)
        except Exception:
            traceback.print_exc()

    def recognize(self, qimage, join_newlines: bool, reflect_strings: bool) -> str:
        if not self.ensure_loaded() or not self._reader or not self._cv2:
            return ""
        try:
            import numpy as np  # noqa: F401
        except Exception as e:
            print("[PaddleOCR] numpy is required:", e)
            return ""

        img_np = _numpy_from_qimage(qimage)
        try:
            bgr = self._cv2.cvtColor(img_np, self._cv2.COLOR_RGB2BGR)
            res = self._reader.predict(input=bgr)
            lines: List[str] = []
            for item in (res or []):
                texts = item.get("rec_texts") if isinstance(item, dict) else None
                if isinstance(texts, (list, tuple)):
                    lines.extend([t for t in texts if isinstance(t, str)])
            if reflect_strings:
                lines = list(reversed(lines))
            s = "\n".join(lines) if join_newlines else " ".join(lines)
            return s.strip()
        except Exception:
            traceback.print_exc()
            return ""

    # --- Helpers -------------------------------------------------
    def _map_lang_for_paddle(self, code: str) -> str:
        """
        Простейшая маппа кодов вида 'ko' -> 'korean', 'ja' -> 'japan', 'zh' -> 'ch', иначе — как есть.
        """
        c = (code or "").lower()
        if c in ("ko", "kor", "korean"):
            return "korean"
        if c in ("ja", "jpn", "japanese", "jp"):
            return "japan"
        if c in ("zh", "chi", "ch", "chinese", "cn", "zh-cn", "zh-hans"):
            return "ch"
        return c
