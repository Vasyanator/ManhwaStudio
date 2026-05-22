from __future__ import annotations

import math
import re
import numpy as np
import traceback

from typing import Callable, Iterable, Tuple, List, Union, Optional
from PyQt6.QtCore import Qt, QPointF, QRectF, QRect, QSize, QEvent, QTimer, pyqtSignal
from PyQt6.QtGui import QImage, QPainter, QPen, QColor, QMouseEvent, QPixmap, QCursor, QKeySequence, QBrush
from PyQt6.QtWidgets import (
    QDialog, QWidget, QVBoxLayout, QHBoxLayout, QGraphicsRectItem, QGraphicsLineItem, QScrollArea,
    QPushButton, QLabel, QSizePolicy, QSlider, QMessageBox, QLayout, QGraphicsEllipseItem
)
from modules.utils_qt import qimage_to_numpy_rgb, qimage_alpha_mask, numpy_rgb_to_qimage

ShortcutSpec = Union[str, QKeySequence]  # удобно разрешить строку типа "Ctrl+Shift+R"
class MouseEventCtx:
    """Контекст мыши, который вкладка передаёт инструменту (в сцен-координатах)."""
    __slots__ = ("etype", "button", "buttons", "modifiers", "scene_pos")
    def __init__(self, etype: str, button: Qt.MouseButton, buttons: Qt.MouseButtons,
                 modifiers: Qt.KeyboardModifiers, scene_pos: QPointF):
        self.etype = etype          # 'press' | 'release' | 'move'
        self.button = button
        self.buttons = buttons
        self.modifiers = modifiers
        self.scene_pos = scene_pos
class BaseTool:
    """
    Базовый класс инструмента.
    Каждый инструмент обязан определить:
      • tool_id: str (уникальный ID)
      • title: str (человеческое имя)
    И реализовать activate()/deactivate(). По желанию — build_ui().
    """
    tool_id: str = "base"
    title: str = "Base"

    def __init__(self):
        self.view = None  # будет установлен в activate(view)

    # вызывать из вкладки при переключении инструмента
    def activate(self, view) -> None:
        """Инструмент активирован для данного DrawingCanvasView."""
        self.view = view

    def deactivate(self) -> None:
        """Инструмент деактивирован; очистите временные состояния/ссылки."""
        self.view = None

    def build_ui(self, parent: QWidget | QLayout) -> None:
        """
        Инструмент может добавить сюда свои элементы UI.
        parent — это layout (QHBoxLayout) динамической панели инструментов.
        """
        pass
    def pass_zoom_shortcuts_to_parent(self, event) -> bool:
        """
        Возвращает True, если событие — один из зум-шорткатов (Ctrl+=, Ctrl++,
        Ctrl+-, опционально Ctrl+0) и его нужно пропустить к родителю (CanvasView).
        В этом случае вызывающий eventFilter должен вернуть False.

        Работает и для QEvent.ShortcutOverride, и для QEvent.KeyPress.
        """
        et = event.type()
        if et not in (QEvent.Type.ShortcutOverride, QEvent.Type.KeyPress):
            return False

        try:
            key = event.key()
            mods = event.modifiers()
        except Exception:
            return False

        if not (mods & Qt.KeyboardModifier.ControlModifier):
            return False

        # Плюс часто приходит как '=' + Shift; NumPad тоже обычно мапится в Key_Plus/Key_Minus
        zoom_keys = {
            Qt.Key.Key_Plus,   # '+'
            Qt.Key.Key_Equal,  # '=' (Shift+'=' -> '+')
            Qt.Key.Key_Minus,  # '-'
            Qt.Key.Key_0,      # по желанию — Ctrl+0 (сброс масштаба)
        }

        if key in zoom_keys:
            try:
                # Критично для ShortcutOverride: не помечать как обработанное
                event.ignore()
            except Exception:
                pass
            return True

        return False
    def requested_shortcuts(self) -> Iterable[Tuple[ShortcutSpec, Callable]]:
        """
        Возвращает пары (шорткат, callback). Вкладка сама создаст QShortcut.
        Пример: return [("Ctrl+R", self.on_refresh), (QKeySequence("Shift+X"), self.on_something)]
        callback будет вызван без аргументов.
        """
        return []

    def hotkeys_hint(self) -> str:
        """
        Возвращает строку с подсказкой по хоткеям инструмента.
        Эта строка будет добавлена к _hotkeysLabel вкладки при активации инструмента.
        Пример: "B — кисть, E — ластик, Shift+Колесо — размер"
        """
        return ""

    def wants_raw_keypress(self) -> bool:
        """
        Вернуть True, если инструмент хочет получать KeyPress (редко нужно).
        Даже если True, вкладка всё равно пропустит «зум»-шорткаты к CanvasView.
        """
        return False

    def on_keypress(self, event) -> bool:
        """
        Сырой KeyPress (если wants_raw_keypress=True).
        Вернуть True, если событие обработано инструментом (и его не надо пускать дальше).
        """
        return False

    def on_mouse_event(self, ctx: MouseEventCtx) -> bool:
        """
        Колбэк мыши от вкладки (координаты — сцена).
        Вернуть True, если событие обработано и его не надо пускать дальше в CanvasView.
        """
        return False

    def on_wheel_event(self, steps: int, modifiers):
        """
        Заглушка обработки колёсика мыши.
        Возвращает False — событие не обработано инструментом.
        Дочерние инструменты могут переопределить.
        """
        return False

    def get_ai_device(self):
        """
        Возвращает объект ai_device из view, если он прокинут вкладкой.
        """
        if self.view is None:
            return None
        return getattr(self.view, "ai_device", None)

    def get_ai_device_str(self, fallback: str = "cpu") -> str:
        """
        Возвращает строку устройства для torch: cpu/cuda/cuda:X/mps.
        """
        dev = self.get_ai_device()
        if dev is None:
            return fallback
        value = str(dev).strip().lower()
        if value in {"cpu", "mps", "cuda"}:
            return value
        if re.fullmatch(r"cuda:\d+", value):
            return value
        return fallback


class BrushBase(BaseTool):
    """
    Базовый класс для кистевых инструментов:
    - рисует ч/б кольцо курсора с учётом текущего радиуса;
    - блокирует обновления модели при мазке.
    """

    def __init__(self):
        super().__init__()
        self._cursor_ring_white = None
        self._cursor_ring_black = None
        self._last_cursor_scene_pt = None
        self._model_updates_locked = False

    def activate(self, view) -> None:
        super().activate(view)
        if self.view is None:
            return
        self.view.setCursor(Qt.CursorShape.BlankCursor)
        self._ensure_cursor_item()
        self._update_cursor_visibility()
        self.view.setMouseTracking(True)
        self.view.viewport().setMouseTracking(True)

    def deactivate(self) -> None:
        if self.view is not None:
            self.view.setMouseTracking(False)
            self.view.viewport().setMouseTracking(False)
            self.view.setCursor(Qt.CursorShape.ArrowCursor)
        self._remove_cursor_item()
        self._unlock_model_updates()
        self._last_cursor_scene_pt = None
        super().deactivate()

    def on_wheel_event(self, steps: int, modifiers):
        """
        Shift + колесо — изменение размера кисти (общая логика для кистевых инструментов).
        """
        if self.view is None:
            return False
        if not (modifiers & Qt.KeyboardModifier.ShiftModifier):
            return False
        if steps == 0:
            return True

        cur_radius = int(getattr(self.view, "brush_radius", 30))
        min_radius = 1
        max_radius = 200
        step = 2
        new_radius = cur_radius + steps * step
        if new_radius < min_radius:
            new_radius = min_radius
        elif new_radius > max_radius:
            new_radius = max_radius
        if new_radius == cur_radius:
            return True
        try:
            self.view.set_brush_radius(new_radius)
        except Exception:
            return False

        ui_size = getattr(self, "_ui_size", None)
        if ui_size is not None:
            try:
                ui_size.blockSignals(True)
                ui_size.setValue(new_radius)
                ui_size.blockSignals(False)
            except Exception:
                pass
        if self._last_cursor_scene_pt is not None:
            try:
                self._update_cursor_position(self._last_cursor_scene_pt)
            except Exception:
                pass
        return True

    def _is_brush_cursor_visible(self) -> bool:
        return True

    def _ensure_cursor_item(self) -> None:
        if self._cursor_ring_white is not None:
            return
        self._cursor_ring_white = QGraphicsEllipseItem()
        pen_w = QPen(QColor(255, 255, 255), 2)
        pen_w.setCosmetic(True)
        pen_w.setStyle(Qt.PenStyle.DashLine)
        self._cursor_ring_white.setPen(pen_w)
        self._cursor_ring_white.setBrush(QBrush(Qt.BrushStyle.NoBrush))
        self._cursor_ring_white.setZValue(20000)
        self.view.scene.addItem(self._cursor_ring_white)

        self._cursor_ring_black = QGraphicsEllipseItem()
        pen_b = QPen(QColor(0, 0, 0), 1)
        pen_b.setCosmetic(True)
        pen_b.setStyle(Qt.PenStyle.DashLine)
        self._cursor_ring_black.setPen(pen_b)
        self._cursor_ring_black.setBrush(QBrush(Qt.BrushStyle.NoBrush))
        self._cursor_ring_black.setZValue(20001)
        self.view.scene.addItem(self._cursor_ring_black)
        self._create_custom_cursor_items()

    def _remove_cursor_item(self) -> None:
        if self.view is None:
            return
        self._remove_custom_cursor_items()
        for it_attr in ("_cursor_ring_white", "_cursor_ring_black"):
            it = getattr(self, it_attr)
            if it is not None:
                try:
                    self.view.scene.removeItem(it)
                except Exception:
                    pass
                setattr(self, it_attr, None)

    def _update_cursor_visibility(self) -> None:
        visible = self._is_brush_cursor_visible()
        for it in (self._cursor_ring_white, self._cursor_ring_black):
            if it is not None:
                it.setVisible(visible)
        self._update_custom_cursor_visibility(visible)

    def _update_cursor_position(self, scene_pt: QPointF) -> None:
        self._last_cursor_scene_pt = scene_pt
        if self.view is None:
            return
        idx = self.view._hit_test_index(scene_pt)
        cursor_visible = self._is_brush_cursor_visible() and idx is not None
        try:
            self.view.setCursor(
                Qt.CursorShape.BlankCursor if cursor_visible else Qt.CursorShape.ArrowCursor
            )
        except Exception:
            pass
        if not cursor_visible:
            for it in (self._cursor_ring_white, self._cursor_ring_black):
                if it is not None:
                    it.setVisible(False)
            self._update_custom_cursor_visibility(False)
            return
        for it in (self._cursor_ring_white, self._cursor_ring_black):
            if it is not None:
                it.setVisible(True)
        self._update_custom_cursor_visibility(True)

        bbox = self.view._image_bbox(idx)
        layer = self.view._overlay_images[idx] if 0 <= idx < len(self.view._overlay_images) else None
        if layer is None or bbox.width() <= 0 or layer.width() <= 0:
            return

        sx = bbox.width() / layer.width()
        sy = bbox.height() / layer.height()
        r_scene_x = getattr(self.view, "brush_radius", 30) * sx
        r_scene_y = getattr(self.view, "brush_radius", 30) * sy

        rect = QRectF(
            scene_pt.x() - r_scene_x,
            scene_pt.y() - r_scene_y,
            r_scene_x * 2,
            r_scene_y * 2,
        )
        if self._cursor_ring_white:
            self._cursor_ring_white.setRect(rect)
        if self._cursor_ring_black:
            self._cursor_ring_black.setRect(rect)
        self._update_custom_cursor_position(scene_pt, rect)

    def _create_custom_cursor_items(self) -> None:
        """Переопределите, чтобы добавить дополнительные элементы курсора."""
        return

    def _remove_custom_cursor_items(self) -> None:
        """Переопределите, чтобы убрать дополнительные элементы курсора."""
        return

    def _update_custom_cursor_visibility(self, visible: bool) -> None:
        """Переопределите, чтобы синхронизировать видимость кастомных элементов."""
        return

    def _update_custom_cursor_position(self, scene_pt: QPointF, rect: QRectF) -> None:
        """Переопределите, чтобы позиционировать кастомные элементы курсора."""
        return

    def _lock_model_updates(self) -> None:
        model = getattr(self.view, "overlays_model", None)
        if model is None or self._model_updates_locked:
            return
        locker = getattr(model, "lock_updates", None)
        if callable(locker):
            try:
                locker()
                self._model_updates_locked = True
            except Exception:
                self._model_updates_locked = False

    def _unlock_model_updates(self) -> None:
        model = getattr(self.view, "overlays_model", None)
        if not self._model_updates_locked or model is None:
            self._model_updates_locked = False
            return
        unlocker = getattr(model, "unlock_updates", None)
        if callable(unlocker):
            try:
                unlocker()
            except Exception:
                pass
        self._model_updates_locked = False

# ---------------------- Холст маски ----------------------
class MaskCanvas(QWidget):
    """
    Слой маски поверх базовой картинки.
    - ЛКМ — рисовать (белое), ПКМ — стирать (прозрачное)
    - Shift+колесо — размер кисти
    - Курсор-кружок всегда виден.
    """
    changed = pyqtSignal()

    def __init__(self, base: QImage, parent: Optional[QWidget] = None):
        super().__init__(parent)
        if base.format() != QImage.Format.Format_RGBA8888:
            base = base.convertToFormat(QImage.Format.Format_RGBA8888)
        self.base = base.copy()

        self.mask = QImage(self.base.size(), QImage.Format.Format_RGBA8888)
        self.mask.fill(QColor(0, 0, 0, 0))

        self._last_pos: Optional[QPointF] = None
        self.brush_radius = 30
        self._erasing = False
        self._scale = 1.0

        self.setSizePolicy(QSizePolicy.Policy.Fixed, QSizePolicy.Policy.Fixed)
        self.setMinimumSize(self.base.size())
        self.setMouseTracking(True)

        self._cursor_timer = QTimer(self)
        self._cursor_timer.setInterval(33)  # ~30 fps
        self._cursor_timer.timeout.connect(self._on_cursor_tick)
        self._cursor_timer.start()

    def sizeHint(self):
        return self._scaled_size()

    # --- публичное API ---
    def clear_mask(self):
        self.mask.fill(QColor(0, 0, 0, 0))
        self.changed.emit()
        self.update()

    def invert_mask(self):
        if self.mask.isNull():
            return
        if self.mask.format() != QImage.Format.Format_RGBA8888:
            self.mask = self.mask.convertToFormat(QImage.Format.Format_RGBA8888)
        w, h = self.mask.width(), self.mask.height()
        ptr = self.mask.bits()
        ptr.setsize(h * self.mask.bytesPerLine())
        arr = np.frombuffer(ptr, dtype=np.uint8).reshape((h, self.mask.bytesPerLine()))
        rgba = arr[:, : w * 4].reshape((h, w, 4)).copy()
        rgba[..., 3] = 255 - rgba[..., 3]
        out = QImage(rgba.data, w, h, w * 4, QImage.Format.Format_RGBA8888)
        self.mask = out.copy()
        self.changed.emit()
        self.update()

    def set_brush_radius(self, r: int):
        self.brush_radius = int(max(5, min(200, r)))
        self.update()

    def set_scale(self, scale: float):
        self._scale = max(0.05, float(scale))
        scaled = self._scaled_size()
        self.setMinimumSize(scaled)
        self.resize(scaled)
        self.update()

    def scale_value(self) -> float:
        return self._scale

    def _scaled_size(self):
        if self._scale == 1.0:
            return self.base.size()
        return QSize(
            max(1, int(round(self.base.width() * self._scale))),
            max(1, int(round(self.base.height() * self._scale)))
        )

    # --- события ---
    def showEvent(self, e):
        self._cursor_timer.start()
        return super().showEvent(e)

    def hideEvent(self, e):
        self._cursor_timer.stop()
        return super().hideEvent(e)

    def wheelEvent(self, e):
        if e.modifiers() & Qt.KeyboardModifier.ShiftModifier:
            delta = 1 if e.angleDelta().y() > 0 else -1
            self.set_brush_radius(self.brush_radius + delta * 2)
            e.accept()
            return
        super().wheelEvent(e)

    def mousePressEvent(self, e: QMouseEvent):
        if e.button() in (Qt.MouseButton.LeftButton, Qt.MouseButton.RightButton):
            self._erasing = (e.button() == Qt.MouseButton.RightButton)
            pos = self._widget_to_image(e.position())
            self._last_pos = pos
            self._draw_to(pos)
            e.accept()
            return
        super().mousePressEvent(e)

    def mouseMoveEvent(self, e: QMouseEvent):
        if self._last_pos is not None:
            self._draw_to(self._widget_to_image(e.position()))
            e.accept()
            return
        self.update()
        super().mouseMoveEvent(e)

    def mouseReleaseEvent(self, e: QMouseEvent):
        if e.button() in (Qt.MouseButton.LeftButton, Qt.MouseButton.RightButton) and self._last_pos is not None:
            self._draw_to(self._widget_to_image(e.position()))
            self._last_pos = None
            e.accept()
            return
        super().mouseReleaseEvent(e)

    def _on_cursor_tick(self):
        if self.isVisible():
            self.update()

    # --- отрисовка ---
    def paintEvent(self, _):
        p = QPainter(self)
        p.scale(self._scale, self._scale)
        p.drawImage(0, 0, self.base)
        p.setOpacity(0.35)
        p.drawImage(0, 0, self._mask_visual())
        p.setOpacity(1.0)
        mouse_widget = self.mapFromGlobal(QCursor.pos())
        mx = max(0, min(mouse_widget.x(), self.width()))
        my = max(0, min(mouse_widget.y(), self.height()))
        mouse = self._widget_to_image(QPointF(mx, my))
        r = self.brush_radius
        p.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        pen_w = 2
        p.setPen(QPen(QColor(255, 255, 255), pen_w))
        p.drawEllipse(mouse, r, r)
        p.setPen(QPen(QColor(0, 0, 0), pen_w))
        p.drawEllipse(mouse, r - 1, r - 1)
        half = max(6, r // 2)
        p.drawLine(int(mouse.x() - half), int(mouse.y()), int(mouse.x() + half), int(mouse.y()))
        p.drawLine(int(mouse.x()), int(mouse.y() - half), int(mouse.x()), int(mouse.y() + half))
        p.end()

    # --- рисование по маске ---
    def _draw_to(self, posf: QPointF):
        p = QPainter(self.mask)
        p.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        if self._erasing:
            p.setCompositionMode(QPainter.CompositionMode.CompositionMode_Clear)
            pen = QPen(QColor(0, 0, 0, 0), self.brush_radius * 2, Qt.PenStyle.SolidLine, Qt.PenCapStyle.RoundCap)
        else:
            pen = QPen(QColor(255, 255, 255, 255), self.brush_radius * 2, Qt.PenStyle.SolidLine, Qt.PenCapStyle.RoundCap)
        p.setPen(pen)
        if self._last_pos is None:
            p.drawPoint(int(posf.x()), int(posf.y()))
        else:
            p.drawLine(self._last_pos, posf)
        p.end()
        self._last_pos = QPointF(posf)
        self.changed.emit()
        self.update()

    def _mask_visual(self) -> QImage:
        viz = self.mask.copy()
        p = QPainter(viz)
        p.setCompositionMode(QPainter.CompositionMode.CompositionMode_SourceIn)
        p.fillRect(viz.rect(), QColor(255, 215, 0, 200))
        p.end()
        return viz

    def _widget_to_image(self, posf: QPointF) -> QPointF:
        if self._scale <= 0:
            return posf
        return QPointF(posf.x() / self._scale, posf.y() / self._scale)


# ---------------------- Базовый диалог редактирования региона ----------------------
class RegionEditorDialog(QDialog):
    """
    Базовый диалог редактирования: холст, кисть, зум, кнопки и статус.
    Наследники переопределяют run(base_rgb, mask_a) и build_params_block().
    """
    def __init__(self, image: QImage, parent: Optional[QWidget] = None):
        super().__init__(parent)
        self.setWindowTitle(self.dialog_title())
        self.setModal(True)

        self._accepted = False
        self._processing = False
        self._process_src_img: Optional[QImage] = None
        self._process_src_mask: Optional[QImage] = None
        self._min_zoom = 0.25
        self._max_zoom = 6.0

        # Верхняя панель
        top = QHBoxLayout()
        self.hint = QLabel(self.hint_text())
        self.hint.setStyleSheet("color:#666;")

        self.slider = QSlider(Qt.Orientation.Horizontal)
        self.slider.setRange(5, 200)
        self.slider.setValue(self.default_brush_radius())
        self.slider.setMaximumWidth(150)

        top.addWidget(self.hint)
        top.addStretch(1)
        top.addWidget(QLabel("Кисть:"))
        top.addWidget(self.slider, 0)

        # Холст
        self.canvas = MaskCanvas(image, self)
        self.canvas.set_brush_radius(self.default_brush_radius())
        self.slider.valueChanged.connect(self.canvas.set_brush_radius)

        scroll = QScrollArea()
        scroll.setWidget(self.canvas)
        scroll.setWidgetResizable(False)
        self._install_zoom_support(scroll, self.canvas)

        # Параметры (переопределяются наследниками)
        params_block = self.build_params_block()

        # Кнопки
        self.btn_process = QPushButton(self.process_button_text())
        self.btn_redo = QPushButton("Переделать")
        self.btn_cancelp = QPushButton("Отменить")
        self.btn_apply = QPushButton("Применить")
        self.btn_close = QPushButton("Закрыть")

        self.btn_redo.setEnabled(False)
        self.btn_cancelp.setEnabled(False)

        self.btn_process.clicked.connect(self._on_process)
        self.btn_redo.clicked.connect(self._on_redo)
        self.btn_cancelp.clicked.connect(self._on_cancel_process)
        self.btn_apply.clicked.connect(self._apply)
        self.btn_close.clicked.connect(self.reject)

        # Статус под подсказкой
        self.status_label = QLabel("")
        self.status_label.setStyleSheet("color:#888;")
        self.status_label.setWordWrap(True)

        # Нижняя панель
        bottom = QHBoxLayout()
        left = QVBoxLayout()

        self.info_label = QLabel(self.info_text())
        self.info_label.setStyleSheet("color:#666;")
        self.info_label.setVisible(bool(self.info_text()))

        if self.info_label.isVisible():
            left.addWidget(self.info_label)
        bottom.addLayout(left)
        bottom.addStretch(1)
        bottom.addWidget(self.btn_cancelp)
        bottom.addWidget(self.btn_redo)
        bottom.addWidget(self.btn_process)
        bottom.addSpacing(12)
        bottom.addWidget(self.btn_close)
        bottom.addWidget(self.btn_apply)

        # Основной layout
        layout = QVBoxLayout(self)
        layout.addLayout(top)
        layout.addWidget(self.status_label)
        layout.addWidget(scroll)
        if params_block is not None:
            layout.addWidget(params_block)
        layout.addLayout(bottom)

        w = min(1000, self.canvas.base.width() + 40)
        h = min(900, self.canvas.base.height() + 200)
        self.resize(w, h)

        self.set_status("Нарисуйте маску и нажмите «{}».".format(self.process_button_text()))

    # ---- Настраиваемые части ----
    def dialog_title(self) -> str:
        return "Редактор области"

    def hint_text(self) -> str:
        return "ЛКМ — рисовать маску, ПКМ — стирать • Shift+Колесо — размер кисти"

    def process_button_text(self) -> str:
        return "Обработать"

    def info_text(self) -> str:
        return ""

    def default_brush_radius(self) -> int:
        return 30

    def build_params_block(self) -> Optional[QWidget]:
        """Наследники возвращают QWidget/GroupBox с параметрами или None."""
        return None

    def run(self, base_rgb: np.ndarray, mask_a: np.ndarray):
        """Наследники реализуют обработку. Возвращают np.ndarray RGB или QImage. None = async."""
        raise NotImplementedError

    # ---- Служебные хелперы ----
    def set_status(self, text: str):
        self.status_label.setText(text)

    def set_info_text(self, text: str):
        self.info_label.setText(text)
        self.info_label.setVisible(bool(text))

    def _install_zoom_support(self, scroll: QScrollArea, canvas: QWidget):
        if not hasattr(self, "_min_zoom"):
            self._min_zoom = 0.25
        if not hasattr(self, "_max_zoom"):
            self._max_zoom = 6.0
        self._zoom_canvas = canvas
        self._zoom_scroll = scroll
        for obj in (scroll, scroll.viewport(), canvas):
            obj.installEventFilter(self)

    # --- масштабирование ---
    def eventFilter(self, obj, event):
        if event.type() == QEvent.Type.Wheel and (event.modifiers() & Qt.KeyboardModifier.ControlModifier):
            self._zoom_from_wheel(event)
            return True
        return super().eventFilter(obj, event)

    def keyPressEvent(self, event):
        if event.modifiers() & Qt.KeyboardModifier.ControlModifier:
            if event.key() in (Qt.Key.Key_Plus, Qt.Key.Key_Equal):
                self._adjust_zoom(1)
                event.accept()
                return
            if event.key() == Qt.Key.Key_Minus:
                self._adjust_zoom(-1)
                event.accept()
                return
        super().keyPressEvent(event)

    def _zoom_from_wheel(self, event):
        delta = event.angleDelta().y() / 120.0
        if delta:
            self._adjust_zoom(delta)
        event.accept()

    def _adjust_zoom(self, steps: float):
        factor = 1.1 ** steps
        if getattr(self, "_zoom_canvas", None) is None:
            return
        new_scale = max(self._min_zoom, min(self._max_zoom, self._zoom_canvas.scale_value() * factor))
        self._zoom_canvas.set_scale(new_scale)
        if getattr(self, "_zoom_scroll", None) is not None and self._zoom_scroll.widget() is not None:
            self._zoom_scroll.widget().adjustSize()

    # ---- Кнопки ----
    def _apply(self):
        self._accepted = True
        self.accept()

    def _on_cancel_process(self):
        if self._process_src_img is None or self._process_src_mask is None:
            return
        self.canvas.base = self._process_src_img.copy()
        self.canvas.mask = self._process_src_mask.copy()
        self.canvas.update()
        self.btn_process.setEnabled(True)
        self.btn_process.setText(self.process_button_text())
        self.set_status("Отменено.")

    def _on_redo(self):
        if self._process_src_img is None or self._process_src_mask is None:
            return
        self.canvas.base = self._process_src_img.copy()
        self.canvas.mask = self._process_src_mask.copy()
        self.canvas.update()
        self._on_process()

    def _on_process(self):
        if self._processing:
            return

        base_rgb = qimage_to_numpy_rgb(self.canvas.base)
        mask_a = qimage_alpha_mask(self.canvas.mask)

        if np.count_nonzero(mask_a) == 0:
            self.set_status("⚠️ Маска пуста — нечего обрабатывать.")
            return

        self._ensure_process_snapshots()
        self._enter_processing_state()

        try:
            result = self.run(base_rgb, mask_a)
            if result is not None:
                self.finish_processing(result, None)
        except Exception as e:
            traceback.print_exc()
            self.finish_processing(None, e)

    def _enter_processing_state(self):
        self._processing = True
        self.btn_process.setEnabled(False)
        self.btn_process.setText("⏳ Обработка…")
        has_snap = self._process_src_img is not None and self._process_src_mask is not None
        self.btn_redo.setEnabled(has_snap)
        self.btn_cancelp.setEnabled(has_snap)
        self.set_status("⏳ Обработка...")

    def _ensure_process_snapshots(self):
        def img_bytes(q: QImage) -> bytes:
            if q.format() != QImage.Format.Format_RGBA8888:
                q = q.convertToFormat(QImage.Format.Format_RGBA8888)
            ptr = q.bits(); ptr.setsize(q.height() * q.bytesPerLine())
            return bytes(ptr)

        if (self._process_src_img is None
                or self._process_src_mask is None
                or img_bytes(self._process_src_img) != img_bytes(self.canvas.base)
                or img_bytes(self._process_src_mask) != img_bytes(self.canvas.mask)):
            self._process_src_img = self.canvas.base.copy()
            self._process_src_mask = self.canvas.mask.copy()

    def finish_processing(self, result_rgb_or_qimg, err: Optional[Exception]):
        self._processing = False
        self.btn_process.setEnabled(True)
        self.btn_process.setText(self.process_button_text())

        if err is not None:
            self.set_status(f"❌ Ошибка обработки: {type(err).__name__}: {err}")
            return

        if result_rgb_or_qimg is None:
            self.set_status("⚠️ Обработка не вернула результат.")
            return

        if isinstance(result_rgb_or_qimg, QImage):
            out_qimg = result_rgb_or_qimg
        else:
            out_qimg = numpy_rgb_to_qimage(np.asarray(result_rgb_or_qimg))

        self.canvas.base = out_qimg
        self.canvas.clear_mask()
        self.canvas.update()

        self.btn_redo.setEnabled(True)
        self.btn_cancelp.setEnabled(True)
        self.set_status("✅ Готово. Можно дорисовать маску и нажать «Переделать» или «Применить».")

    # ---- API для RegionEditTool ----
    def edited_image(self) -> QImage:
        return self.canvas.base

    def was_accepted(self) -> bool:
        return self._accepted


# ---------------------- Каркас инструмента редактирования области ----------------------
class RegionEditTool(BaseTool):
    """
    Инструмент редактирования области.

    Жест:
      • Shift + ЛКМ — прямоугольное выделение на одной картинке.
      • При отпускании ЛКМ откроется редактор. «Применить» вставит результат
        на оверлей ровно в ту же сцену-область.
    """
    tool_id = "region_edit_base"
    title = "Редакт. области"

    def __init__(self):
        super().__init__()
        # Если задано, ширина/высота выделения кратны этому числу (например 8)
        self.selection_multiple: Optional[int] = None
        # Прямоугольник выделения (белый и чёрный для контраста)
        self._rect_item_white: Optional[QGraphicsRectItem] = None
        self._rect_item_black: Optional[QGraphicsRectItem] = None
        self._rect_start_scene: Optional[QPointF] = None
        # Контекст текущего выделения
        self._sel_idx: Optional[int] = None
        self._sel_scene_rect: Optional[QRectF] = None
        # Графический курсор (белый и чёрный слои для контраста)
        self._cursor_cross_h_white = None
        self._cursor_cross_v_white = None
        self._cursor_cross_h_black = None
        self._cursor_cross_v_black = None

    # ---------- lifecycle ----------
    def activate(self, view) -> None:
        # ничего не перехватываем; вкладка будет звать on_mouse_event()
        super().activate(view)
        # Устанавливаем пустой курсор и создаём графический
        if self.view:
            self.view.setCursor(Qt.CursorShape.BlankCursor)
            self._ensure_cursor_items()
            # Включаем отслеживание мыши для постоянного обновления курсора
            self.view.setMouseTracking(True)
            self.view.viewport().setMouseTracking(True)

    def deactivate(self) -> None:
        self._remove_rect_item()
        self._rect_start_scene = None
        self._sel_idx = None
        self._sel_scene_rect = None
        # Убираем графический курсор и восстанавливаем системный
        self._remove_cursor_items()
        if self.view:
            # Отключаем отслеживание мыши
            self.view.setMouseTracking(False)
            self.view.viewport().setMouseTracking(False)
            self.view.setCursor(Qt.CursorShape.ArrowCursor)
            # Отключаем режим кисти в DrawingCanvasView
            try:
                self.view.set_tool("none")
            except Exception:
                pass
        super().deactivate()

    # ---------- UI ----------
    def build_ui(self, parent_layout) -> None:
        parent_layout.addWidget(QLabel("Выделение: Shift + ЛКМ (прямоугольник)"))

    # ---------- shortcuts ----------
    def requested_shortcuts(self):
        # Esc — отменить текущую рамку (если тянем)
        return [("Esc", self._cancel_selection)]

    def hotkeys_hint(self) -> str:
        """Возвращает строку с подсказкой по хоткеям инструмента."""
        return "Shift+ЛКМ — выделить область, Esc — отмена"

    # ---------- event routing (from CleaningTab) ----------
    def on_mouse_event(self, ctx: MouseEventCtx) -> bool:
        """
        Возвращает True, если событие обработано инструментом и НЕ должно
        идти дальше в CanvasView. Иначе False — CanvasView обработает сам.
        """
        if self.view is None:
            return False

        # --- PRESS ---
        if ctx.etype == "press":
            if (ctx.button == Qt.MouseButton.LeftButton
                and (ctx.modifiers & Qt.KeyboardModifier.ShiftModifier)):
                # начинаем выделение только при Shift+ЛКМ
                sp = ctx.scene_pos
                idx = self.view._hit_test_index(sp)
                if idx is None:
                    return True  # съели клик с Shift, чтобы не рисовала кисть
                self._sel_idx = idx
                self._rect_start_scene = sp
                self._start_rect(sp)
                return True
            # любые другие нажатия — не блокируем
            return False

        # --- MOVE ---
        if ctx.etype == "move":
            # Обновляем позицию графического курсора
            self._update_cursor_position(ctx.scene_pos)

            if self._rect_item_white is not None and self._rect_start_scene is not None:
                r = self._snap_rect_to_multiple(self._rect_start_scene, ctx.scene_pos)
                self._rect_item_white.setRect(r)
                if self._rect_item_black is not None:
                    self._rect_item_black.setRect(r)
            # движение мыши не блокируем
            return False

        # --- RELEASE ---
        if ctx.etype == "release":
            if ctx.button == Qt.MouseButton.LeftButton and self._rect_item_white is not None:
                rect_scene = self._snap_rect_to_multiple(self._rect_start_scene, ctx.scene_pos)
                ok = self._validate_selection(self._sel_idx, rect_scene)
                if ok:
                    self._sel_scene_rect = rect_scene
                    self._open_editor_for_selection()
                # очистка рамки
                self._remove_rect_item()
                self._rect_start_scene = None
                return True
            return False

        return False

    # ---------- selection visuals ----------
    def _start_rect(self, scene_pt: QPointF) -> None:
        self._remove_rect_item()

        # Чёрный прямоугольник (фон для контраста)
        self._rect_item_black = QGraphicsRectItem(QRectF(scene_pt, scene_pt))
        pen_black = QPen(QColor(0, 0, 0), 3, Qt.PenStyle.DashLine)
        pen_black.setCosmetic(True)
        self._rect_item_black.setPen(pen_black)
        self._rect_item_black.setZValue(19000)
        self.view.scene.addItem(self._rect_item_black)

        # Белый прямоугольник (сверху)
        self._rect_item_white = QGraphicsRectItem(QRectF(scene_pt, scene_pt))
        pen_white = QPen(QColor(255, 255, 255), 1, Qt.PenStyle.DashLine)
        pen_white.setCosmetic(True)
        self._rect_item_white.setPen(pen_white)
        self._rect_item_white.setZValue(19001)
        self.view.scene.addItem(self._rect_item_white)

    def _remove_rect_item(self) -> None:
        for rect_item in (self._rect_item_white, self._rect_item_black):
            if rect_item is not None:
                try:
                    self.view.scene.removeItem(rect_item)
                except Exception:
                    pass
        self._rect_item_white = None
        self._rect_item_black = None

    def _cancel_selection(self):
        self._remove_rect_item()
        self._rect_start_scene = None
        self._sel_idx = None
        self._sel_scene_rect = None

    def _snap_rect_to_multiple(self, start_scene: QPointF, cur_scene: QPointF) -> QRectF:
        """Возвращает прямоугольник с кратными сторонами (если задано)."""
        rect = QRectF(start_scene, cur_scene).normalized()
        mult = self.selection_multiple or 0
        if mult <= 1:
            return rect

        dx = cur_scene.x() - start_scene.x()
        dy = cur_scene.y() - start_scene.y()
        if dx == 0 or dy == 0:
            return rect

        width = abs(dx)
        height = abs(dy)
        snapped_w = math.ceil(width / mult) * mult
        snapped_h = math.ceil(height / mult) * mult

        x0 = start_scene.x()
        y0 = start_scene.y()
        x1 = x0 + snapped_w if dx > 0 else x0 - snapped_w
        y1 = y0 + snapped_h if dy > 0 else y0 - snapped_h

        return QRectF(QPointF(x0, y0), QPointF(x1, y1)).normalized()

    # ---------- selection validation ----------
    def _validate_selection(self, idx: Optional[int], rect_scene: QRectF) -> bool:
        """Прямоугольник не пустой и целиком внутри bbox одной картинки idx."""
        if idx is None or rect_scene.isEmpty():
            return False

        rect_scene = QRectF(rect_scene).normalized()
        bbox = self.view._image_bbox(idx)
        if bbox.isEmpty():
            return False

        if self._selection_touches_multiple_pages(rect_scene):
            self._show_multi_page_warning()
            return False

        return bbox.contains(rect_scene)

    def _selection_touches_multiple_pages(self, rect_scene: QRectF) -> bool:
        """Возвращает True, если прямоугольник пересекает больше одной страницы."""
        if self.view is None:
            return False
        hits = 0
        for bbox in getattr(self.view, "image_bboxes", []):
            if rect_scene.intersects(bbox):
                hits += 1
                if hits > 1:
                    return True
        return False

    def _show_multi_page_warning(self):
        """Показывает предупреждение, что выделение вышло за пределы одной страницы."""
        parent = self.view.window() if self.view else None
        try:
            QMessageBox.warning(
                parent,
                "Выделение",
                "Выделение пересекает несколько страниц. Сделайте рамку внутри одной страницы.",
            )
        except Exception:
            # QMessageBox может не сработать, если контекст не готов — в этом случае тихо игнорируем
            pass

    # ---------- editor ----------
    def _open_editor_for_selection(self) -> None:
        if self._sel_idx is None or self._sel_scene_rect is None:
            return

        # Получаем композитное изображение (основная картинка + оверлей)
        chunk: QImage = self._get_composited_chunk(self._sel_idx, self._sel_scene_rect)
        if chunk is None or chunk.isNull() or chunk.width() == 0 or chunk.height() == 0:
            # сброс контекста
            self._sel_idx = None
            self._sel_scene_rect = None
            return

        dlg = self.create_editor_dialog(chunk, parent=self.view.window())
        if dlg.exec() == QDialog.DialogCode.Accepted and self.is_editor_accepted(dlg):
            edited = self.editor_result_image(dlg)
            if edited is not None and not edited.isNull():
                try:
                    self.view._begin_undo_capture(self._sel_idx)
                except Exception:
                    pass
                self.view.paste_chunk_to_overlay(self._sel_idx, self._sel_scene_rect, edited)
                try:
                    self.view._commit_undo_capture(self._sel_idx)
                except Exception:
                    pass

        # сброс контекста после диалога
        self._sel_idx = None
        self._sel_scene_rect = None

    def _get_composited_chunk(self, idx: int, scene_rect) -> QImage:
        """
        Возвращает композитное изображение (основа + оверлей) для региона.
        Берёт оригинальную картинку и накладывает на неё оверлей.
        """
        # Получаем основную картинку
        base_chunk = self.view.get_original_chunk(idx, scene_rect)
        if base_chunk is None or base_chunk.isNull():
            return QImage()

        # Проверяем, есть ли оверлей для этой страницы
        if not hasattr(self.view, '_overlay_images'):
            return base_chunk  # нет оверлея - возвращаем как есть

        overlay_images = getattr(self.view, '_overlay_images', [])
        if not (0 <= idx < len(overlay_images)):
            return base_chunk

        overlay = overlay_images[idx]
        if overlay is None or overlay.isNull():
            return base_chunk  # оверлей пустой - возвращаем базу

        # Вычисляем координаты региона в оверлее
        overlay_rect = self.view.scene_rect_to_overlay_rect(idx, scene_rect)
        if overlay_rect.isEmpty():
            return base_chunk

        # Вырезаем кусок оверлея
        overlay_chunk = overlay.copy(overlay_rect)
        if overlay_chunk.isNull():
            return base_chunk

        # Композитим: base + overlay
        # Убеждаемся, что размеры совпадают
        if overlay_chunk.size() != base_chunk.size():
            overlay_chunk = overlay_chunk.scaled(
                base_chunk.size(),
                Qt.AspectRatioMode.IgnoreAspectRatio,
                Qt.TransformationMode.SmoothTransformation
            )

        # Создаём результат и композитим
        result = QImage(base_chunk.size(), QImage.Format.Format_ARGB32_Premultiplied)
        painter = QPainter(result)
        painter.setCompositionMode(QPainter.CompositionMode.CompositionMode_Source)
        painter.drawImage(0, 0, base_chunk)
        painter.setCompositionMode(QPainter.CompositionMode.CompositionMode_SourceOver)
        painter.drawImage(0, 0, overlay_chunk)
        painter.end()

        return result

    # ---- overridable hooks (оставлены без изменений) ----
    def create_editor_dialog(self, image: QImage, parent: Optional[QWidget] = None) -> "RegionEditorDialog":
        return RegionEditorDialog(image, parent)

    def is_editor_accepted(self, dialog: "RegionEditorDialog") -> bool:
        return dialog.was_accepted()

    def editor_result_image(self, dialog: "RegionEditorDialog") -> QImage:
        return dialog.edited_image()

    # ---------- графический курсор ----------
    def _ensure_cursor_items(self) -> None:
        """Создаёт графические элементы курсора (крестик) если их ещё нет."""
        if self._cursor_cross_h_white is not None:
            return  # уже созданы

        # Чёрная горизонтальная линия (фон для контраста)
        self._cursor_cross_h_black = QGraphicsLineItem()
        pen_h_black = QPen(QColor(0, 0, 0), 3)
        pen_h_black.setCosmetic(True)
        self._cursor_cross_h_black.setPen(pen_h_black)
        self._cursor_cross_h_black.setZValue(20000)
        self.view.scene.addItem(self._cursor_cross_h_black)

        # Белая горизонтальная линия (сверху)
        self._cursor_cross_h_white = QGraphicsLineItem()
        pen_h_white = QPen(QColor(255, 255, 255), 1)
        pen_h_white.setCosmetic(True)
        self._cursor_cross_h_white.setPen(pen_h_white)
        self._cursor_cross_h_white.setZValue(20001)
        self.view.scene.addItem(self._cursor_cross_h_white)

        # Чёрная вертикальная линия (фон для контраста)
        self._cursor_cross_v_black = QGraphicsLineItem()
        pen_v_black = QPen(QColor(0, 0, 0), 3)
        pen_v_black.setCosmetic(True)
        self._cursor_cross_v_black.setPen(pen_v_black)
        self._cursor_cross_v_black.setZValue(20000)
        self.view.scene.addItem(self._cursor_cross_v_black)

        # Белая вертикальная линия (сверху)
        self._cursor_cross_v_white = QGraphicsLineItem()
        pen_v_white = QPen(QColor(255, 255, 255), 1)
        pen_v_white.setCosmetic(True)
        self._cursor_cross_v_white.setPen(pen_v_white)
        self._cursor_cross_v_white.setZValue(20001)
        self.view.scene.addItem(self._cursor_cross_v_white)

    def _remove_cursor_items(self) -> None:
        """Удаляет графические элементы курсора со сцены."""
        for it_attr in ("_cursor_cross_h_white", "_cursor_cross_v_white",
                        "_cursor_cross_h_black", "_cursor_cross_v_black"):
            it = getattr(self, it_attr, None)
            if it is not None:
                try:
                    self.view.scene.removeItem(it)
                except Exception:
                    pass
                setattr(self, it_attr, None)

    def _update_cursor_position(self, scene_pt: QPointF) -> None:
        """Обновляет позицию графического курсора."""
        if (self._cursor_cross_h_white is None or self._cursor_cross_v_white is None or
            self._cursor_cross_h_black is None or self._cursor_cross_v_black is None):
            return

        # Проверяем, находится ли курсор над изображением
        idx = self.view._hit_test_index(scene_pt)
        if idx is None:
            try:
                self.view.setCursor(Qt.CursorShape.ArrowCursor)
            except Exception:
                pass
            # Курсор вне изображений - скрываем
            self._cursor_cross_h_white.setVisible(False)
            self._cursor_cross_v_white.setVisible(False)
            self._cursor_cross_h_black.setVisible(False)
            self._cursor_cross_v_black.setVisible(False)
            return

        try:
            self.view.setCursor(Qt.CursorShape.BlankCursor)
        except Exception:
            pass
        # Курсор внутри изображения - показываем
        self._cursor_cross_h_white.setVisible(True)
        self._cursor_cross_v_white.setVisible(True)
        self._cursor_cross_h_black.setVisible(True)
        self._cursor_cross_v_black.setVisible(True)

        # Размер крестика (в пикселях сцены)
        cross_size = 15

        # Горизонтальные линии (чёрная и белая)
        self._cursor_cross_h_black.setLine(
            scene_pt.x() - cross_size, scene_pt.y(),
            scene_pt.x() + cross_size, scene_pt.y()
        )
        self._cursor_cross_h_white.setLine(
            scene_pt.x() - cross_size, scene_pt.y(),
            scene_pt.x() + cross_size, scene_pt.y()
        )

        # Вертикальные линии (чёрная и белая)
        self._cursor_cross_v_black.setLine(
            scene_pt.x(), scene_pt.y() - cross_size,
            scene_pt.x(), scene_pt.y() + cross_size
        )
        self._cursor_cross_v_white.setLine(
            scene_pt.x(), scene_pt.y() - cross_size,
            scene_pt.x(), scene_pt.y() + cross_size
        )
