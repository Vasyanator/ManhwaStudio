from __future__ import annotations

from typing import Dict, Any
import traceback

from PyQt6.QtWidgets import QLabel, QFrame, QVBoxLayout, QWidget

from ..utils import _numpy_from_qimage
from .base import OcrEngineBase


class MangaOcrEngine(OcrEngineBase):
    key = "mangaocr"
    title = "MangaOCR"
    checkbox_label = "MangaOCR"

    def __init__(self, canvas):
        super().__init__(canvas)
        self._ocr = None

    # --- UI -----------------------------------------------------
    def build_ui(self, parent: QWidget, on_change) -> QWidget:
        self._on_change = on_change

        root = QFrame(parent)
        lay = QVBoxLayout(root)
        lay.setContentsMargins(0, 0, 0, 0)

        desc = QLabel("Японский OCR без настроек. При первом запуске может скачать модель.")
        desc.setWordWrap(True)
        lay.addWidget(desc)
        lay.addStretch(1)
        return root

    def read_ui_state(self):
        # Нет параметров, но соблюдаем интерфейс базового класса.
        self.reset()

    def validate(self) -> bool:
        return True

    def save_settings(self) -> Dict[str, Any]:
        return {}

    def load_settings(self, data: Dict[str, Any]):
        self.reset()

    # --- Загрузка и работа --------------------------------------
    def _load_impl(self) -> bool:
        try:
            from manga_ocr import MangaOcr
        except Exception as e:
            print("[MangaOCR] not installed:", e)
            return False

        try:
            self._ocr = MangaOcr()
        except Exception as e:
            print("[MangaOCR] init failed:", e)
            self._ocr = None
            return False
        return True

    def warmup(self):
        if not self._ocr:
            return
        try:
            from PIL import Image  # type: ignore

            dummy = Image.new("RGB", (8, 8), (0, 0, 0))
            self._ocr(dummy)
        except Exception:
            traceback.print_exc()

    def recognize(self, qimage, join_newlines: bool, reflect_strings: bool) -> str:
        if not self.ensure_loaded() or not self._ocr:
            return ""
        try:
            from PIL import Image  # type: ignore
        except Exception as e:
            print("[MangaOCR] pillow is required:", e)
            return ""

        img_np = _numpy_from_qimage(qimage)
        try:
            pil_img = Image.fromarray(img_np)
            text_raw = self._ocr(pil_img)
            text = str(text_raw or "")

            lines = text.splitlines()
            if not lines:
                lines = [text]
            if reflect_strings:
                lines = list(reversed(lines))
            s = "\n".join(lines) if join_newlines else " ".join(lines)
            return s.strip()
        except Exception:
            traceback.print_exc()
            return ""
