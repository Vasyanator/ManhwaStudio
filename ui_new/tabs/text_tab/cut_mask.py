# ui_new/tabs/text_tab/cut_mask.py
from __future__ import annotations
import os
from PyQt6.QtWidgets import (
    QWidget, QVBoxLayout, QHBoxLayout, QPushButton, QLabel, QFrame, QSpinBox, QButtonGroup
)
from PyQt6.QtCore import Qt, QPropertyAnimation, QRect, QRectF, QEasingCurve, QPoint, QPointF, QTimer, QSize, QObject, QThread, pyqtSignal
from PyQt6.QtGui import QPalette, QColor, QPainter, QPen, QPainterPath, QImage
from typing import List, Dict, Set, Optional, TYPE_CHECKING
from collections import deque
import math

if TYPE_CHECKING:
    from .text_overlay_item import TextOverlayItem

class _MaskSaveWorker(QObject):
    finished = pyqtSignal(bool, str)  # ok, dir_path

    def __init__(self, items: list[tuple[int, QImage]], dir_path: str):
        super().__init__()
        self._items = items
        self._dir = dir_path

    def run(self):
        ok = True
        try:
            os.makedirs(self._dir, exist_ok=True)
            for idx, img in self._items:
                # img — уже .copy() из основного потока
                img.save(os.path.join(self._dir, f"mask_page_{idx}.png"))
        except Exception:
            ok = False
        self.finished.emit(ok, self._dir)

class BarrierMaskManager:
    """
    Менеджер масок-барьеров для всех страниц.
    Каждая маска - это альфа-канал, где 255 = барьер, 0 = прозрачно.
    Упрощенная версия без разделения на линии и заливки - только единая маска для каждой страницы.
    """
    def __init__(self):
        # Единая маска для каждой страницы (Format_Alpha8: 255 = барьер, 0 = прозрачно)
        self.page_masks: Dict[int, QImage] = {}  # img_idx -> маска-барьер (Format_Alpha8)

        # Состояние компонент для каждого оверлея
        self.overlay_components: Dict[int, Dict[int, QImage]] = {}  # id(overlay) -> {component_id -> mask}
        self.hidden_components: Dict[int, Set[int]] = {}  # id(overlay) -> set of hidden component ids
        self.component_cache_hashes: Dict[int, int] = {}  # id(overlay) -> hash для кэширования

        # Анимация для визуализации барьеров
        self.animation_phase: float = 0.0
        self._save_thread: QThread | None = None
        self._save_worker: _MaskSaveWorker | None = None

    def get_animated_color(self) -> QColor:
        """Постоянный полупрозрачный жёлтый цвет"""
        return QColor(255, 230, 0, 160)

    def get_or_create_mask(self, img_idx: int, width: int, height: int) -> QImage:
        """Получить или создать маску для страницы"""
        if img_idx not in self.page_masks:
            mask = QImage(width, height, QImage.Format.Format_Alpha8)
            mask.fill(0)  # Начинаем с полностью прозрачной маски
            self.page_masks[img_idx] = mask
        else:
            # Проверяем размер существующей маски
            mask = self.page_masks[img_idx]
            if mask.width() != width or mask.height() != height:
                # Размер изменился - пересоздаём с масштабированием
                new_mask = QImage(width, height, QImage.Format.Format_Alpha8)
                new_mask.fill(0)
                if not mask.isNull():
                    painter = QPainter(new_mask)
                    painter.drawImage(
                        0, 0, mask.scaled(
                            width, height,
                            Qt.AspectRatioMode.IgnoreAspectRatio,
                            Qt.TransformationMode.SmoothTransformation
                        )
                    )
                    painter.end()
                mask = new_mask
                self.page_masks[img_idx] = mask
        return mask

    def clear_all_masks(self):
        """Очистить все маски барьеров"""
        self.page_masks.clear()
        self.overlay_components.clear()
        self.hidden_components.clear()
        self.component_cache_hashes.clear()

    def clear_mask(self, img_idx: int):
        """Очистить маску конкретной страницы"""
        if img_idx in self.page_masks:
            self.page_masks[img_idx].fill(0)
            # Сбрасываем кэш всех оверлеев на этой странице
            for ov_id in list(self.component_cache_hashes.keys()):
                self.component_cache_hashes[ov_id] = 0

    def invalidate_overlay_cache(self, overlay_item):
        """Сбросить кэш разбиения для конкретного оверлея (при его перемещении)"""
        ov_id = id(overlay_item)
        self.component_cache_hashes[ov_id] = 0

    def draw_on_mask(self, img_idx: int, points: List[QPoint], brush_size: int, mode: str = "add"):
        """
        Рисовать на маске-барьере.

        Args:
            img_idx: индекс страницы
            points: список точек полилинии
            brush_size: размер кисти
            mode: "add" - добавить барьер, "erase" - стереть барьер
        """
        mask = self.page_masks.get(img_idx)
        if not mask or mask.isNull() or len(points) < 2:
            return

        painter = QPainter(mask)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)

        if mode == "add":
            # Рисуем белым (255 = барьер)
            pen = QPen(QColor(255, 255, 255, 255), brush_size, Qt.PenStyle.SolidLine,
                      Qt.PenCapStyle.RoundCap, Qt.PenJoinStyle.RoundJoin)
            painter.setPen(pen)
            painter.setBrush(Qt.GlobalColor.white)
        else:  # erase
            # Стираем (делаем прозрачным)
            painter.setCompositionMode(QPainter.CompositionMode.CompositionMode_Clear)
            pen = QPen(QColor(0, 0, 0, 0), brush_size, Qt.PenStyle.SolidLine,
                      Qt.PenCapStyle.RoundCap, Qt.PenJoinStyle.RoundJoin)
            painter.setPen(pen)
            # Для заливки области используем прозрачную кисть
            painter.setBrush(QColor(0, 0, 0, 0))

        # Создаём замкнутый контур
        path = QPainterPath()
        path.moveTo(QPointF(points[0]))
        for p in points[1:]:
            path.lineTo(QPointF(p))
        path.closeSubpath()  # Замыкаем контур

        # Рисуем контур с заливкой обведённой области
        painter.drawPath(path)

        painter.end()

        # Сбрасываем кэш компонент для всех оверлеев
        self.component_cache_hashes.clear()

    def _color_match(self, c1: QColor, c2: QColor, tolerance: int) -> bool:
        """
        Проверка совпадения цветов с погрешностью.
        tolerance: 0-255, допустимая разница по каждому каналу RGB.
        """
        return (abs(c1.red() - c2.red()) <= tolerance and
                abs(c1.green() - c2.green()) <= tolerance and
                abs(c1.blue() - c2.blue()) <= tolerance)

    def flood_fill_from_point(self, start_scene: QPoint, tolerance: int,
                           source_image: QImage, img_idx: int,
                           page_bbox: QRectF,
                           clip_rect_scene: Optional[QRectF] = None) -> bool:
        """
        Выполняет flood fill заливку начиная с точки start_scene.
        Добавляет залитую область в маску-барьер страницы.

        Args:
            start_scene: точка клика в координатах сцены
            tolerance: погрешность цвета (0-255)
            source_image: исходное изображение страницы (full resolution)
            img_idx: индекс страницы
            page_bbox: bbox страницы в координатах сцены
            clip_rect_scene: опциональная область клиппинга в координатах сцены

        Returns:
            True если заливка успешна, False иначе
        """
        if source_image.isNull() or page_bbox is None:
            return False

        # Преобразуем точку из координат сцены в локальные координаты страницы
        local_x = start_scene.x() - page_bbox.left()
        local_y = start_scene.y() - page_bbox.top()

        # Получаем или создаём маску для этой страницы
        mask_w = int(page_bbox.width())
        mask_h = int(page_bbox.height())
        mask = self.get_or_create_mask(img_idx, mask_w, mask_h)

        # Обрабатываем клиппинг
        clip_local = None
        if clip_rect_scene is not None:
            cx0 = int(clip_rect_scene.left() - page_bbox.left())
            cy0 = int(clip_rect_scene.top() - page_bbox.top())
            cx1 = int(clip_rect_scene.right() - page_bbox.left())
            cy1 = int(clip_rect_scene.bottom() - page_bbox.top())
            clip_local = QRect(
                max(0, min(cx0, cx1)),
                max(0, min(cy0, cy1)),
                0, 0
            )
            clip_local.setRight(min(mask_w - 1, max(cx0, cx1)))
            clip_local.setBottom(min(mask_h - 1, max(cy0, cy1)))
            if clip_local.isEmpty():
                return False

        # Проверяем, попадает ли стартовая точка в клип
        if clip_local is not None:
            if not clip_local.contains(int(local_x), int(local_y)):
                return False

        # Проверяем границы
        if local_x < 0 or local_x >= mask_w or local_y < 0 or local_y >= mask_h:
            return False

        # Вычисляем масштаб для перевода из координат сцены в координаты исходного изображения
        scale_x = float(source_image.width()) / max(1.0, page_bbox.width())
        scale_y = float(source_image.height()) / max(1.0, page_bbox.height())

        orig_x = int(local_x * scale_x)
        orig_y = int(local_y * scale_y)

        if orig_x < 0 or orig_x >= source_image.width() or orig_y < 0 or orig_y >= source_image.height():
            return False

        # Получаем целевой цвет
        target_color = source_image.pixelColor(orig_x, orig_y)

        # BFS flood fill
        w, h = mask.width(), mask.height()
        visited = set()
        queue = deque([(int(local_x), int(local_y))])

        # Прямой доступ к пикселям маски
        mask_bits = mask.bits()
        mask_bits.setsize(mask.height() * mask.bytesPerLine())
        mask_mv = memoryview(mask_bits)

        filled_pixels = 0
        max_pixels = 100000  # Ограничение для предотвращения зависания

        while queue and filled_pixels < max_pixels:
            x, y = queue.popleft()

            if clip_local is not None and not clip_local.contains(x, y):
                continue
            if (x, y) in visited:
                continue
            if x < 0 or x >= w or y < 0 or y >= h:
                continue

            # Проверяем, не является ли пиксель уже барьером
            idx = y * mask.bytesPerLine() + x
            if idx >= len(mask_mv) or mask_mv[idx] > 0:
                continue

            # Проверяем цвет в исходном изображении
            orig_px = int(x * scale_x)
            orig_py = int(y * scale_y)
            if orig_px < 0 or orig_px >= source_image.width() or orig_py < 0 or orig_py >= source_image.height():
                continue

            current_color = source_image.pixelColor(orig_px, orig_py)
            if not self._color_match(current_color, target_color, tolerance):
                continue

            visited.add((x, y))
            filled_pixels += 1

            # Заполняем пиксель (255 = барьер)
            mask_mv[idx] = 255

            # Добавляем соседей (4-связность)
            queue.append((x + 1, y))
            queue.append((x - 1, y))
            queue.append((x, y + 1))
            queue.append((x, y - 1))

        # Сбрасываем кэш компонент после заливки
        if filled_pixels > 0:
            self.component_cache_hashes.clear()

        return filled_pixels > 0

    def _extract_local_barrier(self, page_mask: QImage,
                            overlay_item: "TextOverlayItem",
                            page_bbox: QRectF) -> QImage:
        """
        Извлекает фрагмент барьерной маски в координатах ОВЕРЛЕЯ
        с учётом всех трансформаций (позиция, масштаб, поворот).
        Возвращает QImage(Alpha8) размера boundingRect() оверлея (в item-координатах).
        """
        pixmap = overlay_item.pixmap()
        if pixmap.isNull():
            return QImage()

        ov_w = pixmap.width()
        ov_h = pixmap.height()

        result = QImage(ov_w, ov_h, QImage.Format.Format_Alpha8)
        result.fill(0)
        if page_mask.isNull() or ov_w <= 0 or ov_h <= 0:
            return result

        # Трансформация: (x_mask, y_mask) -> scene -> item
        # mask-пиксели заданы в системе страницы: (0,0) соответствует (page_bbox.left, page_bbox.top) в сцене.
        # Нам нужен transform, который разместит page_mask в item-координатах.
        item_from_scene, ok = overlay_item.sceneTransform().inverted()
        # Сместим маску из её локальной (страничной) системы в систему сцены
        # затем применим обратную трансформацию item.
        t = item_from_scene
        t.translate(page_bbox.left(), page_bbox.top())

        p = QPainter(result)
        p.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        p.setRenderHint(QPainter.RenderHint.SmoothPixmapTransform, True)
        p.setWorldTransform(t, combine=False)

        # Рисуем всю page_mask в item-координаты, обрезка произойдёт границами result
        p.drawImage(0, 0, page_mask)
        p.end()

        return result


    def _compute_barrier_hash(self, page_mask: QImage,
                            overlay_item: "TextOverlayItem",
                            page_bbox: QRectF) -> int:
        """Быстрый хэш локальной маски с учётом трансформаций оверлея."""
        local_barrier = self._extract_local_barrier(page_mask, overlay_item, page_bbox)
        if local_barrier.isNull():
            return 0

        ptr = local_barrier.constBits()
        ptr.setsize(local_barrier.height() * local_barrier.bytesPerLine())
        mv = memoryview(ptr)
        total = 0
        for byte in mv:
            total = (total + byte) & 0xFFFFFFFF

        pm = overlay_item.pixmap()
        w = pm.width()
        h = pm.height()
        # Добавим размер оверлея и немного инфы о трансформации, чтобы чаще инвалидировать
        total = (total + w * 131 + h * 17) & 0xFFFFFFFF
        # Угол и scale берём из item (в градусах и в 1e-4 фиксированном формате)
        ang = int(round(overlay_item.rotation() * 10000))
        sc  = int(round(overlay_item.scale() * 10000))
        total = (total + ang * 7 + sc * 13) & 0xFFFFFFFF

        return int(total)

    def _label_components(self, barrier: QImage) -> tuple[List[List[int]], int]:
        """
        По барьеру вычисляем метки компонент (4-связность) внутри прямоугольника оверлея.
        barrier: Grayscale8, 0 — свободно, >0 — барьер.
        Возврат: grid (h x w), num_labels.
        """
        w = barrier.width()
        h = barrier.height()

        # Быстрый доступ к пикселям
        b_ptr = barrier.constBits()
        b_ptr.setsize(barrier.height() * barrier.bytesPerLine())
        # Преобразуем в удобный для чтения список строк (0/1)
        solid = [[0]*w for _ in range(h)]
        for y in range(h):
            row = memoryview(b_ptr)[y*barrier.bytesPerLine() : y*barrier.bytesPerLine()+w]
            for x in range(w):
                solid[y][x] = 1 if row[x] > 0 else 0

        labels = [[-1]*w for _ in range(h)]
        cur_label = 0
        dirs = ((1,0), (-1,0), (0,1), (0,-1))

        for y in range(h):
            for x in range(w):
                if solid[y][x] == 0 and labels[y][x] == -1:
                    # новая компонента
                    q = deque()
                    q.append((x,y))
                    labels[y][x] = cur_label
                    while q:
                        cx, cy = q.popleft()
                        for dx, dy in dirs:
                            nx, ny = cx+dx, cy+dy
                            if 0 <= nx < w and 0 <= ny < h:
                                if solid[ny][nx] == 0 and labels[ny][nx] == -1:
                                    labels[ny][nx] = cur_label
                                    q.append((nx, ny))
                    cur_label += 1

        return labels, cur_label

    def _build_component_masks(self, labels: List[List[int]], num_labels: int, ov_width: int, ov_height: int) -> Dict[int, QImage]:
        """Строим по меткам alpha-маски (ч/б) размера оверлея для каждого label."""
        w = ov_width
        h = ov_height
        masks: Dict[int, QImage] = {}

        # Создаём пустые маски под каждую компоненту
        for lid in range(num_labels):
            img = QImage(w, h, QImage.Format.Format_Alpha8)
            img.fill(0)
            masks[lid] = img

        # Подготовим ptr для каждой маски
        mask_ptrs: Dict[int, tuple] = {}
        for lid, img in masks.items():
            ptr = img.bits()
            ptr.setsize(img.height() * img.bytesPerLine())
            mv = memoryview(ptr)
            mask_ptrs[lid] = (mv, img.bytesPerLine())

        # Заполнение пикселей соответствующих компонент значением 255
        for y in range(h):
            for x in range(w):
                lid = labels[y][x]
                if lid >= 0:
                    mv, bpl = mask_ptrs[lid]
                    idx = y * bpl + x  # т.к. Format_Alpha8: 1 байт на пиксель
                    mv[idx] = 255

        return masks

    def compute_overlay_components(self, overlay_item: "TextOverlayItem",
                                   page_bbox: QRectF) -> Dict[int, QImage]:
        """
        Разбить оверлей на компоненты, используя маску-барьер.
        Возвращает словарь {component_id -> component_mask}

        Args:
            overlay_item: текстовый оверлей
            page_bbox: bbox страницы в координатах сцены

        Returns:
            Словарь масок компонент
        """
        ov_id = id(overlay_item)
        img_idx = overlay_item.meta.img_idx

        # Получаем маску-барьер для страницы
        page_mask = self.page_masks.get(img_idx)
        pixmap = overlay_item.pixmap()

        if pixmap.isNull():
            return {}

        overlay_size = pixmap.size()
        overlay_pos = overlay_item.scenePos()

        # Вычисляем хэш для кэширования
        if page_mask and not page_mask.isNull():
            current_hash = self._compute_barrier_hash(page_mask, overlay_item, page_bbox)
        else:
            current_hash = 0

        # Проверяем кэш
        cached_hash = self.component_cache_hashes.get(ov_id, -1)
        if cached_hash == current_hash and ov_id in self.overlay_components:
            # Кэш актуален
            return self.overlay_components[ov_id]

        # Если нет барьера или он пустой - весь оверлей это одна компонента
        if not page_mask or page_mask.isNull():
            full_mask = QImage(overlay_size, QImage.Format.Format_Alpha8)
            full_mask.fill(255)
            components = {0: full_mask}
            self.overlay_components[ov_id] = components
            self.component_cache_hashes[ov_id] = current_hash
            return components

        # Извлекаем локальный барьер
        local_barrier = self._extract_local_barrier(page_mask, overlay_item, page_bbox)

        # Инвертируем барьер чтобы получить видимые области (барьер скрывает)
        visible_mask = QImage(overlay_size, QImage.Format.Format_Alpha8)
        visible_mask.fill(255)

        painter = QPainter(visible_mask)
        painter.setCompositionMode(QPainter.CompositionMode.CompositionMode_DestinationOut)
        painter.drawImage(0, 0, local_barrier)
        painter.end()

        # Находим связные компоненты в visible_mask
        labels_grid, num_labels = self._label_components(visible_mask)
        components = self._build_component_masks(labels_grid, num_labels, overlay_size.width(), overlay_size.height())

        # Сохраняем в кэш
        self.overlay_components[ov_id] = components
        self.component_cache_hashes[ov_id] = current_hash

        # Очищаем скрытые компоненты от несуществующих
        if ov_id in self.hidden_components:
            self.hidden_components[ov_id] = {lid for lid in self.hidden_components[ov_id] if lid in components}

        return components

    def toggle_component_at_pos(self, overlay_item: "TextOverlayItem",
                                pos_scene: QPoint, page_bbox: QRectF) -> bool:
        """
        Переключить видимость компоненты оверлея под курсором.

        Args:
            overlay_item: текстовый оверлей
            pos_scene: позиция клика в координатах сцены
            page_bbox: bbox страницы в координатах сцены

        Returns:
            True, если переключение произошло, False иначе
        """
        if overlay_item is None:
            return False

        # Проверяем, попал ли курсор в оверлей
        local_pos = overlay_item.mapFromScene(QPointF(pos_scene))
        pixmap = overlay_item.pixmap()
        if pixmap.isNull():
            return False

        rect = QRectF(0, 0, pixmap.width(), pixmap.height())
        if not rect.contains(local_pos):
            return False

        # Получаем компоненты
        components = self.compute_overlay_components(overlay_item, page_bbox)
        if not components:
            return False

        # Вычисляем labels_grid для hit-test (находим компоненту под курсором)
        ov_id = id(overlay_item)
        img_idx = overlay_item.meta.img_idx
        page_mask = self.page_masks.get(img_idx)

        if not page_mask or page_mask.isNull():
            # Нет барьера - одна компонента
            component_id = 0
        else:
            # Извлекаем локальный барьер и находим компоненты
            overlay_size = pixmap.size()
            overlay_pos = overlay_item.scenePos()
            local_barrier = self._extract_local_barrier(page_mask, overlay_item, page_bbox)

            # Инвертируем для получения видимых областей
            visible_mask = QImage(overlay_size, QImage.Format.Format_Alpha8)
            visible_mask.fill(255)
            painter = QPainter(visible_mask)
            painter.setCompositionMode(QPainter.CompositionMode.CompositionMode_DestinationOut)
            painter.drawImage(0, 0, local_barrier)
            painter.end()

            # Находим labels
            labels_grid, _ = self._label_components(visible_mask)

            # Определяем компоненту под курсором
            x, y = int(local_pos.x()), int(local_pos.y())
            if 0 <= y < len(labels_grid) and 0 <= x < len(labels_grid[0]):
                component_id = labels_grid[y][x]
                if component_id < 0:
                    return False  # Кликнули на барьер
            else:
                return False

        # Инициализируем множество скрытых компонент если нужно
        if ov_id not in self.hidden_components:
            self.hidden_components[ov_id] = set()

        # Переключаем видимость
        if component_id in self.hidden_components[ov_id]:
            self.hidden_components[ov_id].remove(component_id)
        else:
            self.hidden_components[ov_id].add(component_id)

        return True

    def get_visible_masks(self, overlay_item: "TextOverlayItem", page_bbox: QRectF) -> List[QImage]:
        """
        Возвращает список масок видимых компонент для данного оверлея.

        Args:
            overlay_item: текстовый оверлей
            page_bbox: bbox страницы в координатах сцены

        Returns:
            Список масок видимых компонент
        """
        ov_id = id(overlay_item)

        # Получаем компоненты
        components = self.compute_overlay_components(overlay_item, page_bbox)
        if not components:
            return []

        # Получаем скрытые компоненты
        hidden = self.hidden_components.get(ov_id, set())

        # Возвращаем только видимые
        visible_masks = []
        for lid, mask in components.items():
            if lid not in hidden:
                visible_masks.append(mask)

        return visible_masks

    # ---------- SAVE / LOAD ----------
    def _mask_filename(self, page_idx: int) -> str:
        """Единый формат имени файла маски страницы."""
        return f"mask_page_{page_idx}.png"

    def save_page_masks(self, dir_path: str) -> None:
        """Сохранить все имеющиеся маски страниц в dir_path."""
        try:
            os.makedirs(dir_path, exist_ok=True)
        except Exception:
            return
        for idx, mask in self.page_masks.items():
            if mask and not mask.isNull():
                mask.save(os.path.join(dir_path, self._mask_filename(idx)))

    def load_page_masks(self, dir_path: str) -> None:
        """Загрузить маски страниц из dir_path (перезаполняет self.page_masks)."""
        self.page_masks.clear()
        try:
            for fn in os.listdir(dir_path):
                if fn.startswith("mask_page_") and fn.endswith(".png"):
                    try:
                        idx = int(fn[len("mask_page_"):-4])
                    except Exception:
                        continue
                    full = os.path.join(dir_path, fn)
                    img = QImage(full)
                    if not img.isNull():
                        if img.format() != QImage.Format.Format_Alpha8:
                            img = img.convertToFormat(QImage.Format.Format_Alpha8)
                        self.page_masks[idx] = img
            # Маски могли поменяться → инвалидируем кэш компонентов
            self.component_cache_hashes.clear()
        except Exception:
            pass

    def ensure_all_masks_sizes(self, page_bboxes) -> None:
        """
        Привести размеры загруженных масок к текущим размерам страниц,
        основываясь на page_bboxes (QRectF по индексам страниц).
        """
        changed = False
        for idx, mask in list(self.page_masks.items()):
            if 0 <= idx < len(page_bboxes):
                w = max(1, int(page_bboxes[idx].width()))
                h = max(1, int(page_bboxes[idx].height()))
                if mask.width() != w or mask.height() != h:
                    scaled = mask.scaled(
                        w, h,
                        Qt.AspectRatioMode.IgnoreAspectRatio,
                        Qt.TransformationMode.SmoothTransformation
                    )
                    if scaled.format() != QImage.Format.Format_Alpha8:
                        scaled = scaled.convertToFormat(QImage.Format.Format_Alpha8)
                    self.page_masks[idx] = scaled
                    changed = True
        if changed:
            # размеры изменились — сбрасываем кэш
            self.component_cache_hashes.clear()

    def async_save_page_masks(self, dir_path: str, on_finished=None):
        """
        Асинхронно сохраняет все маски страниц в dir_path.
        Делает снепшот (copy) масок в главном потоке и сохраняет их в QThread.
        """
        # Если предыдущий сейв ещё идет — не плодим потоки
        if self._save_thread is not None:
            return

        # Снепшот масок
        items: list[tuple[int, QImage]] = []
        for idx, mask in self.page_masks.items():
            if mask and not mask.isNull():
                items.append((idx, mask.copy()))
        if not items:
            if on_finished:
                on_finished(True, dir_path)
            return

        th = QThread()
        worker = _MaskSaveWorker(items, dir_path)
        worker.moveToThread(th)

        th.started.connect(worker.run)

        def _cleanup(ok: bool, path: str):
            try:
                th.quit()
                th.wait()
            except Exception:
                pass
            try:
                worker.deleteLater()
                th.deleteLater()
            except Exception:
                pass
            self._save_thread = None
            self._save_worker = None
            if on_finished:
                on_finished(ok, path)

        worker.finished.connect(_cleanup)

        self._save_thread = th
        self._save_worker = worker
        th.start()


class BarrierPanel(QFrame):
    """
    Упрощенная панель управления маской-барьером.
    Единый инструмент без разделения на линии и заливки.
    """

    def __init__(self, parent=None):
        super().__init__(parent)
        self.setObjectName("BarrierPanel")

        # Стиль панели
        self.setStyleSheet("""
            QFrame#BarrierPanel {
                background-color: #2b2b2b;
                border: 1px solid #444;
                border-radius: 8px;
            }
        """)
        self.setFrameShape(QFrame.Shape.StyledPanel)
        self.setFrameShadow(QFrame.Shadow.Raised)

        # Основной layout
        main_layout = QVBoxLayout(self)
        main_layout.setContentsMargins(12, 12, 12, 12)
        main_layout.setSpacing(8)

        # Заголовок
        header_layout = QHBoxLayout()
        title_label = QLabel("Маска-Барьер")
        title_label.setStyleSheet("font-weight: bold; font-size: 14px; color: white;")
        header_layout.addWidget(title_label)
        header_layout.addStretch()

        # Кнопка закрытия
        close_btn = QPushButton("✕")
        close_btn.setFixedSize(24, 24)
        close_btn.setStyleSheet("""
            QPushButton {
                background-color: #f44336;
                color: white;
                border: none;
                border-radius: 12px;
                font-weight: bold;
            }
            QPushButton:hover {
                background-color: #d32f2f;
            }
        """)
        close_btn.clicked.connect(self.hide)
        header_layout.addWidget(close_btn)

        main_layout.addLayout(header_layout)

        # Separator
        separator = QFrame()
        separator.setFrameShape(QFrame.Shape.HLine)
        separator.setStyleSheet("background-color: #444;")
        separator.setFixedHeight(1)
        main_layout.addWidget(separator)

        # Группа инструментов
        tools_label = QLabel("Инструменты:")
        tools_label.setStyleSheet("color: #bbb; font-size: 11px; font-weight: bold;")
        main_layout.addWidget(tools_label)

        self.tool_group = QButtonGroup(self)
        self.tool_group.setExclusive(True)

        # Кисть
        self.brush_tool = QPushButton("Кисть")
        self.brush_tool.setCheckable(True)
        self.brush_tool.setChecked(True)
        self.brush_tool.setStyleSheet("""
            QPushButton {
                background-color: #444;
                color: white;
                border: none;
                border-radius: 4px;
                padding: 8px;
                font-weight: bold;
            }
            QPushButton:hover {
                background-color: #555;
            }
            QPushButton:checked {
                background-color: #4CAF50;
            }
        """)
        self.tool_group.addButton(self.brush_tool, 0)
        main_layout.addWidget(self.brush_tool)

        # Ластик
        self.eraser_tool = QPushButton("Ластик")
        self.eraser_tool.setCheckable(True)
        self.eraser_tool.setStyleSheet("""
            QPushButton {
                background-color: #444;
                color: white;
                border: none;
                border-radius: 4px;
                padding: 8px;
                font-weight: bold;
            }
            QPushButton:hover {
                background-color: #555;
            }
            QPushButton:checked {
                background-color: #2196F3;
            }
        """)
        self.tool_group.addButton(self.eraser_tool, 1)
        main_layout.addWidget(self.eraser_tool)

        # Заливка
        self.fill_tool = QPushButton("Заливка")
        self.fill_tool.setCheckable(True)
        self.fill_tool.setStyleSheet("""
            QPushButton {
                background-color: #444;
                color: white;
                border: none;
                border-radius: 4px;
                padding: 8px;
                font-weight: bold;
            }
            QPushButton:hover {
                background-color: #555;
            }
            QPushButton:checked {
                background-color: #FF9800;
            }
        """)
        self.tool_group.addButton(self.fill_tool, 2)
        main_layout.addWidget(self.fill_tool)

        # Separator
        separator2 = QFrame()
        separator2.setFrameShape(QFrame.Shape.HLine)
        separator2.setStyleSheet("background-color: #444;")
        separator2.setFixedHeight(1)
        main_layout.addWidget(separator2)

        # Размер кисти/ластика
        size_row = QHBoxLayout()
        size_label = QLabel("Размер кисти:")
        size_label.setStyleSheet("color: #bbb; font-size: 11px;")
        self.brush_size_spinbox = QSpinBox()
        self.brush_size_spinbox.setRange(1, 50)
        self.brush_size_spinbox.setValue(4)
        self.brush_size_spinbox.setFixedWidth(60)
        self.brush_size_spinbox.setStyleSheet("""
            QSpinBox {
                background-color: #555;
                color: white;
                border: 1px solid #666;
                border-radius: 4px;
                padding: 4px;
            }
        """)
        size_row.addWidget(size_label)
        size_row.addWidget(self.brush_size_spinbox)
        size_row.addStretch()
        main_layout.addLayout(size_row)

        # Превью цвета под курсором (для заливки)
        color_row = QHBoxLayout()
        color_label = QLabel("Цвет под курсором:")
        color_label.setStyleSheet("color: #bbb; font-size: 11px;")
        self.color_preview = QLabel()
        self.color_preview.setFixedSize(40, 20)
        self.color_preview.setStyleSheet("background-color: #000; border: 1px solid #666;")
        color_row.addWidget(color_label)
        color_row.addWidget(self.color_preview)
        color_row.addStretch()
        main_layout.addLayout(color_row)

        # Погрешность для заливки
        tolerance_row = QHBoxLayout()
        tolerance_label = QLabel("Погрешность:")
        tolerance_label.setStyleSheet("color: #bbb; font-size: 11px;")
        self.tolerance_spinbox = QSpinBox()
        self.tolerance_spinbox.setRange(0, 255)
        self.tolerance_spinbox.setValue(10)
        self.tolerance_spinbox.setFixedWidth(60)
        self.tolerance_spinbox.setStyleSheet("""
            QSpinBox {
                background-color: #555;
                color: white;
                border: 1px solid #666;
                border-radius: 4px;
                padding: 4px;
            }
        """)
        tolerance_row.addWidget(tolerance_label)
        tolerance_row.addWidget(self.tolerance_spinbox)
        tolerance_row.addStretch()
        main_layout.addLayout(tolerance_row)

        # Separator
        separator3 = QFrame()
        separator3.setFrameShape(QFrame.Shape.HLine)
        separator3.setStyleSheet("background-color: #444;")
        separator3.setFixedHeight(1)
        main_layout.addWidget(separator3)

        # Кнопка очистки маски
        self.clear_button = QPushButton("Очистить маску страницы")
        self.clear_button.setStyleSheet("""
            QPushButton {
                background-color: #f44336;
                color: white;
                border: none;
                border-radius: 4px;
                padding: 8px;
            }
            QPushButton:hover {
                background-color: #d32f2f;
            }
        """)
        main_layout.addWidget(self.clear_button)

        # Информационная метка
        info_label = QLabel("• ПКМ (зажать) — рисовать кистью/ластиком\n• ПКМ (заливка) — залить область\n• Ctrl+ПКМ — скрыть/показать часть текста")
        info_label.setStyleSheet("color: #bbb; padding: 8px; font-size: 11px;")
        info_label.setWordWrap(True)
        main_layout.addWidget(info_label)

        main_layout.addStretch()

        # Размеры панели
        self.setFixedWidth(300)
        self.setMinimumHeight(200)

        # Скрываем по умолчанию
        self.hide()

        # Устанавливаем поверх других виджетов
        self.raise_()

    def get_current_tool(self) -> str:
        """Получить текущий выбранный инструмент"""
        if self.brush_tool.isChecked():
            return "brush"
        elif self.eraser_tool.isChecked():
            return "eraser"
        elif self.fill_tool.isChecked():
            return "fill"
        return "brush"

    def update_color_preview(self, color: QColor):
        """Обновить превью цвета под курсором"""
        if color and color.isValid():
            self.color_preview.setStyleSheet(
                f"background-color: rgb({color.red()}, {color.green()}, {color.blue()}); "
                f"border: 1px solid #666;"
            )
        else:
            self.color_preview.setStyleSheet("background-color: #000; border: 1px solid #666;")

    def position_in_parent(self):
        """Позиционирует панель в правой части родителя"""
        if not self.parent():
            return

        parent = self.parent()
        parent_width = parent.width()
        parent_height = parent.height()

        # Размещаем справа с небольшим отступом
        x = parent_width - self.width() - 20
        y = 60  # Отступ сверху

        self.move(x, y)

        # Устанавливаем высоту с учетом отступов
        max_height = parent_height - y - 20
        if self.height() > max_height:
            self.setFixedHeight(max_height)

    def showEvent(self, event):
        """При показе позиционируем панель"""
        super().showEvent(event)
        self.position_in_parent()
        self.raise_()
        if self.parent() and hasattr(self.parent(), 'viewport'):
            self.parent().viewport().update()

    def hideEvent(self, event):
        """При скрытии обновляем viewport"""
        super().hideEvent(event)
        if self.parent() and hasattr(self.parent(), 'viewport'):
            self.parent().viewport().update()


class BarrierButton(QPushButton):
    """
    Кнопка для открытия панели маски-барьера.
    Будет размещена рядом со счетчиком страниц и масштабом.
    """

    def __init__(self, parent=None):
        super().__init__("Маска-Барьер", parent)
        self.setObjectName("BarrierButton")
        self.setStyleSheet("""
            QPushButton#BarrierButton {
                background: #555;
                color: white;
                padding: 3px 8px;
                font-weight: 600;
                border-radius: 4px;
                border: none;
            }
            QPushButton#BarrierButton:hover {
                background: #666;
            }
            QPushButton#BarrierButton:pressed {
                background: #444;
            }
        """)
        self.setFixedHeight(26)
        self.setCursor(Qt.CursorShape.PointingHandCursor)

        # Панель будет установлена извне
        self._panel = None

    def set_panel(self, panel: BarrierPanel):
        """Установить связанную панель"""
        self._panel = panel

    def toggle_panel(self):
        """Открыть/закрыть панель маски-барьера"""
        if self._panel is None:
            return

        if self._panel.isVisible():
            self._panel.hide()
        else:
            self._panel.show()
            self._panel.raise_()


# Алиасы для обратной совместимости
CutLinesPanel = BarrierPanel
CutLinesButton = BarrierButton
CutLinesManager = BarrierMaskManager