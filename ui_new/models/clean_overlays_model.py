# ui_new/models/clean_overlays_model.py
from __future__ import annotations
import os
from typing import List, Optional, Tuple
from PyQt6.QtCore import QObject, pyqtSignal
from PyQt6.QtGui import QImage

class CleanOverlaysModel(QObject):
    """
    Памятная модель прозрачных оверлеев (по одному слою на страницу).
    • Быстрая синхронизация между CanvasView через сигналы (без файлов).
    • Ручное сохранение в папку клининга.
    """
    overlayReplaced = pyqtSignal(int)  # idx
    overlayCleared  = pyqtSignal(int)  # idx
    visibilityChanged = pyqtSignal(bool)

    def __init__(self, project):
        super().__init__()
        self.project = project
        self._overlays: List[Optional[QImage]] = []
        self._basenames: List[str] = []
        self._sizes: List[Tuple[int, int]] = []
        self._visible: bool = True
        self._updates_lock: int = 0  # >0 — временно блокируем replace/clear (например, во время мазка)

    # ------ инициализация набора страниц ------
    def init_from_images(self, image_paths: List[str]) -> None:
        """
        Привязывает слои к списку путей изображений (в том же порядке).
        Пытается загрузить готовые PNG из clean_layers_dir (если есть), иначе создаёт прозрачные.
        """
        image_paths = list(image_paths)
        image_paths.sort(key=self._numeric_first_key)
        self._overlays = [None] * len(image_paths)
        self._basenames = [os.path.basename(p) for p in image_paths]
        self._sizes = []

        cldir = getattr(self.project, "clean_layers_dir", None)
        for i, p in enumerate(image_paths):
            img = QImage(p)
            if img.isNull():
                self._sizes.append((0, 0))
                self._overlays[i] = None
                continue
            w, h = img.width(), img.height()
            self._sizes.append((w, h))

            loaded = False
            if cldir and os.path.isdir(cldir):
                # ожидаем точное имя файла (как у исходника) — но PNG
                base, _ = os.path.splitext(self._basenames[i])
                cand = os.path.join(cldir, base + ".png")
                if os.path.isfile(cand):
                    ov = QImage(cand)
                    if not ov.isNull():
                        # если размер отличается (на всякий), аккуратно скейлим в новый слой
                        if ov.size() != img.size():
                            tmp = QImage(img.size(), QImage.Format.Format_ARGB32_Premultiplied)
                            tmp.fill(0)
                            ov = ov.scaled(img.size())
                            # Подменяем ov на tmp, нарисовав в нём ov
                            from PyQt6.QtGui import QPainter
                            pnt = QPainter(tmp); pnt.drawImage(0, 0, ov); pnt.end()
                            ov = tmp
                        self._overlays[i] = ov
                        loaded = True
            if not loaded:
                lay = QImage(img.size(), QImage.Format.Format_ARGB32_Premultiplied)
                lay.fill(0)
                self._overlays[i] = lay

    def _numeric_first_key(self, path: str):
        """Ключ сортировки как в CanvasView: числа вперёд, png приоритетно."""
        base = os.path.basename(path)
        stem, ext = os.path.splitext(base)
        ext = ext.lower().lstrip(".")
        ext_w = 0 if ext == "png" else (1 if ext in ("jpg", "jpeg") else 2)
        if stem.isdigit():
            return (0, int(stem), ext_w, base.lower())
        return (1, stem.lower(), ext_w, base.lower())

    # ------ доступ ------
    def count(self) -> int:
        return len(self._overlays)

    def get(self, idx: int) -> Optional[QImage]:
        if 0 <= idx < len(self._overlays):
            return self._overlays[idx]
        return None

    def replace(self, idx: int, new_image: QImage) -> None:
        """Полная замена слоя idx (например, после инструментов рисования)."""
        if not (0 <= idx < len(self._overlays)) or new_image is None or new_image.isNull():
            return
        w, h = self._sizes[idx] if idx < len(self._sizes) else (0, 0)
        if w > 0 and h > 0 and (new_image.width() != w or new_image.height() != h):
            # подгоним к нужному размеру
            new_image = new_image.scaled(w, h)
        self._overlays[idx] = new_image
        if self._updates_lock > 0:
            return
        self.overlayReplaced.emit(idx)

    def clear(self, idx: int) -> None:
        """Очистка слоя (полная прозрачность)."""
        if not (0 <= idx < len(self._overlays)):
            return
        w, h = self._sizes[idx] if idx < len(self._sizes) else (0, 0)
        if w <= 0 or h <= 0:
            self._overlays[idx] = None
            if self._updates_lock > 0:
                return
            self.overlayCleared.emit(idx)
            return
        lay = QImage(w, h, QImage.Format.Format_ARGB32_Premultiplied)
        lay.fill(0)
        self._overlays[idx] = lay
        if self._updates_lock > 0:
            return
        self.overlayCleared.emit(idx)

    # ------ видимость ------
    def set_visible(self, visible: bool) -> None:
        v = bool(visible)
        if v != self._visible:
            self._visible = v
            self.visibilityChanged.emit(v)

    def is_visible(self) -> bool:
        return self._visible

    # ------ сохранение ------
    def save_all(self) -> None:
        """
        Ручное сохранение всех слоёв в project.clean_layers_dir как PNG (имя = basename страницы + .png).
        """
        out = getattr(self.project, "clean_layers_dir", None)
        if not out:
            return
        os.makedirs(out, exist_ok=True)
        for i, ov in enumerate(self._overlays):
            if ov is None or ov.isNull():
                continue
            base, _ = os.path.splitext(self._basenames[i] if i < len(self._basenames) else f"{i}")
            path = os.path.join(out, base + ".png")
            ov.save(path, "PNG")

    # ------ временная блокировка обновлений (для толстых мазков) ------
    def lock_updates(self) -> None:
        """Увеличить счётчик блокировок replace/clear (без сигналов)."""
        self._updates_lock += 1

    def unlock_updates(self) -> None:
        """Освободить блокировку, если была активна."""
        if self._updates_lock > 0:
            self._updates_lock -= 1

    def updates_locked(self) -> bool:
        return self._updates_lock > 0
