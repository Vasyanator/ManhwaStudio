# utils_qt.py (или в вашем классе)

from typing import Any, Optional

import numpy as np
from PyQt6.QtGui import QImage
from PyQt6.sip import isdeleted # есть и в PyQt6

def qobj_alive(obj: Optional[object]) -> bool:
    """Надёжно проверяем, не удалён ли Python-обёрткой C++-объект."""
    if obj is None:
        return False
    try:
        return not isdeleted(obj)
    except Exception:
        # На всякий пожарный: любые странности считаем как 'не жив'
        return False

def safe_disconnect(obj: Optional[object], signal_name: str, slot: Any) -> None:
    """
    Безопасно дисконнектим сигнал, не разыменовывая его до проверок.
    signal_name — строка, ровно как атрибут у obj (например, 'bubblesChanged').
    """
    if not qobj_alive(obj):
        return

    sig = getattr(obj, signal_name, None)
    if sig is None:
        return

    try:
        sig.disconnect(slot)
    except (TypeError, RuntimeError):
        # Уже был отключён, подпись не совпала, перегруженный сигнал и т.п. — тихо выходим
        pass


def qimage_to_numpy_rgba(img: QImage) -> np.ndarray:
    """QImage -> np.ndarray HxWx4 (RGBA, uint8)."""
    if img.isNull():
        return np.zeros((0, 0, 4), dtype=np.uint8)
    rgba = img.convertToFormat(QImage.Format.Format_RGBA8888)
    w, h = rgba.width(), rgba.height()
    ptr = rgba.bits()
    ptr.setsize(rgba.sizeInBytes())
    arr = np.frombuffer(ptr, dtype=np.uint8).reshape((h, rgba.bytesPerLine()))
    arr = arr[:, : w * 4].reshape((h, w, 4))
    return arr.copy()


def qimage_to_numpy_rgb(img: QImage) -> np.ndarray:
    """QImage -> np.ndarray HxWx3 (RGB, uint8)."""
    rgba = qimage_to_numpy_rgba(img)
    return rgba[..., :3].copy()


def qimage_to_numpy_bgr(img: QImage) -> np.ndarray:
    """QImage -> np.ndarray HxWx3 (BGR, uint8)."""
    rgb = qimage_to_numpy_rgb(img)
    return rgb[..., ::-1].copy()


def qimage_alpha_mask(img: QImage) -> np.ndarray:
    """Возвращает HxW (0..255) из альфа-канала RGBA QImage."""
    rgba = qimage_to_numpy_rgba(img)
    return rgba[..., 3].copy()


def numpy_rgb_to_qimage(rgb: np.ndarray) -> QImage:
    """np.ndarray HxWx3 (uint8 RGB) -> QImage RGBA8888 (alpha=255)."""
    if rgb.size == 0:
        return QImage()
    h, w, _ = rgb.shape
    out = np.zeros((h, w, 4), dtype=np.uint8)
    out[..., :3] = rgb
    out[..., 3] = 255
    qimg = QImage(out.data, w, h, w * 4, QImage.Format.Format_RGBA8888)
    return qimg.copy()
