from __future__ import annotations

from typing import Optional

from PyQt6.QtCore import Qt
from PyQt6.QtGui import QImage, QPixmap, QGuiApplication
from PyQt6.QtWidgets import QDialog, QVBoxLayout, QHBoxLayout, QLabel, QPushButton, QScrollArea, QMessageBox, QWidget

from .base import RegionEditTool


class RegionClipboardDialog(QDialog):
    def __init__(self, image: QImage, parent: Optional[QWidget] = None):
        super().__init__(parent)
        self.setWindowTitle("Область → буфер")
        self.setModal(True)

        self._accepted = False
        self._image = image.copy()

        self._image_label = QLabel()
        self._image_label.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self._set_image(self._image)

        scroll = QScrollArea()
        scroll.setWidget(self._image_label)
        scroll.setWidgetResizable(False)

        btn_copy = QPushButton("Копировать")
        btn_replace = QPushButton("Заменить")
        btn_cancel = QPushButton("Отмена")
        btn_apply = QPushButton("Применить")

        btn_copy.clicked.connect(self._copy_to_clipboard)
        btn_replace.clicked.connect(self._replace_from_clipboard)
        btn_cancel.clicked.connect(self.reject)
        btn_apply.clicked.connect(self._apply)

        bottom = QHBoxLayout()
        bottom.addStretch(1)
        bottom.addWidget(btn_copy)
        bottom.addWidget(btn_replace)
        bottom.addSpacing(12)
        bottom.addWidget(btn_cancel)
        bottom.addWidget(btn_apply)

        layout = QVBoxLayout(self)
        layout.addWidget(scroll)
        layout.addLayout(bottom)

        w = min(1000, self._image.width() + 40)
        h = min(900, self._image.height() + 80)
        self.resize(w, h)

    def was_accepted(self) -> bool:
        return self._accepted

    def edited_image(self) -> QImage:
        return self._image

    def _set_image(self, img: QImage) -> None:
        pix = QPixmap.fromImage(img)
        self._image_label.setPixmap(pix)
        self._image_label.setFixedSize(pix.size())

    def _copy_to_clipboard(self) -> None:
        QGuiApplication.clipboard().setImage(self._image)

    def _replace_from_clipboard(self) -> None:
        cb = QGuiApplication.clipboard()
        img = cb.image()
        if img is None or img.isNull():
            QMessageBox.warning(self, "Буфер обмена", "В буфере обмена нет изображения.")
            return

        if img.size() != self._image.size():
            img = img.scaled(
                self._image.size(),
                Qt.AspectRatioMode.IgnoreAspectRatio,
                Qt.TransformationMode.SmoothTransformation,
            )

        self._image = img
        self._set_image(self._image)

    def _apply(self) -> None:
        self._accepted = True
        self.accept()


class RegionClipboardTool(RegionEditTool):
    """
    Инструмент: выделение области и обмен через буфер.

    - Shift+ЛКМ: выделить область.
    - Окно показывает выделенный фрагмент.
    - "Копировать": копирует фрагмент в буфер.
    - "Заменить": подставляет изображение из буфера.
    - "Применить": вставляет текущий фрагмент в оверлей.
    """
    tool_id = "region_clipboard"
    title = "Буфер области"

    def __init__(self):
        super().__init__()
        self.selection_multiple = 8

    def create_editor_dialog(self, image: QImage, parent=None) -> RegionClipboardDialog:
        return RegionClipboardDialog(image, parent)

    def is_editor_accepted(self, dialog: RegionClipboardDialog) -> bool:
        return dialog.was_accepted()

    def editor_result_image(self, dialog: RegionClipboardDialog) -> QImage:
        return dialog.edited_image()

    def build_ui(self, parent_layout) -> None:
        parent_layout.addWidget(QLabel("Выделение: Shift + ЛКМ (прямоугольник)"))
