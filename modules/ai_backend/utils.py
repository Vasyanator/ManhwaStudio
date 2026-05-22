from __future__ import annotations
from typing import Union
import traceback

from PyQt6.QtGui import QImage, QPixmap
from PyQt6.QtCore import QRectF
from modules.utils_qt import qimage_to_numpy_rgb

ImageLike = Union[str, QImage, QPixmap]

def _qimage_from_any(it: ImageLike) -> QImage:
    if isinstance(it, QImage):
        return it
    if isinstance(it, QPixmap):
        return it.toImage()
    return QImage(it)

def _numpy_from_qimage(img: QImage):
    """
    Преобразует QImage в numpy.ndarray (RGB) для передачи в OCR движки.
    Использует прямой доступ к буферу изображения для максимальной производительности.
    """
    return qimage_to_numpy_rgb(img)

def _rect_union(a: QRectF, b: QRectF) -> QRectF:
    if a.isNull():
        return QRectF(b)
    if b.isNull():
        return QRectF(a)
    r = QRectF(a)
    r = r.united(b)
    return r

# Проверка валидности Qt объектов (совместимость с PySide6/PyQt6)
try:
    import shiboken6  # PySide6
    def _is_deleted(w):
        try:
            return not shiboken6.isValid(w)
        except Exception:
            return False
except Exception:
    traceback.print_exc()
    # Fallback для PyQt6 (не имеет аналога shiboken6)
    def _is_deleted(_):
        return False
