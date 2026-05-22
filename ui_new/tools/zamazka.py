# zamazka.py
from PyQt6.QtCore import Qt, QPointF, QRectF, QRect
from PyQt6.QtGui import QPen, QBrush, QColor, QImage, QPainter, QPixmap
from PyQt6.QtWidgets import (
    QHBoxLayout, QLabel, QComboBox, QSlider, QPushButton,
    QGraphicsRectItem, QColorDialog
)
from .base import BrushBase, MouseEventCtx  # MouseEventCtx — новый тип из BaseTool
from typing import Optional
import time

class ZamazkaTool(BrushBase):
    """
    Инструмент "Замазка" для вкладки клининга (PyQt6).
    Режимы: кисть, ластик, пипетка, прямоугольник (заливка цветом или "стирание").
    """
    tool_id = "zamazka"
    title = "Замазка"

    def __init__(self):
        super().__init__()

        self.mode = "brush"
        self._cursor_cross_h = None
        self._cursor_cross_v = None

        self._rect_item = None
        self._rect_start_scene = None
        self._rect_erase = False

        self._ui_mode = None
        self._ui_size = None
        self._ui_opacity = None
        self._ui_color_btn = None
        self._ui_rect_erase_btn = None
        self._is_painting = False
        self._last_scene_pt: Optional[QPointF] = None
        self._stroke_idx: Optional[int] = None
        self._stroke_layer: Optional[QImage] = None
        self._stroke_erase: bool = False
        self._preview_last_ts: Optional[float] = None  # дебаунс обновления pixmap
        self._preview_interval_sec: float = 0.02       # ~50fps достаточно для плавности
        self._preview_geom_applied: bool = False

    # ------------- жизненный цикл -------------
    def activate(self, view) -> None:
        super().activate(view)
        # активный режим кисти/ластика
        self._apply_view_tool()
        self._update_cursor_visibility()

    def deactivate(self) -> None:
        self._remove_rect_item()
        self._reset_stroke_state()

        # перевести вью в нейтральный инструмент
        self._clear_view_tool()

        # сброс внутреннего состояния
        try:
            self._is_painting = False
            self._last_scene_pt = None
        except Exception:
            pass

        super().deactivate()



    def _clear_view_tool(self) -> None:
        """
        Переводим DrawingCanvasView в нейтральный режим, чтобы она
        не обрабатывала рисование после деактивации инструмента.
        """
        if self.view is None:
            return
        # Пытаемся установить явный "none"/None — на случай разных реализаций set_tool
        try:
            self.view.set_tool("none")
        except Exception:
            try:
                self.view.set_tool(None)
            except Exception:
                pass

    # ------------- UI инструмента -------------
    def build_ui(self, parent_layout: QHBoxLayout) -> None:
        # Режим
        mode_lbl = QLabel("Режим:")
        mode_box = QComboBox()
        mode_box.addItem("Кисть", userData="brush")
        mode_box.addItem("Ластик", userData="eraser")
        mode_box.addItem("Пипетка", userData="eyedropper")
        mode_box.addItem("Прямоуг.", userData="rect")
        mode_box.currentIndexChanged.connect(self._on_mode_changed)

        # Размер
        size_lbl = QLabel("Размер:")
        size_sld = QSlider(Qt.Orientation.Horizontal)
        size_sld.setMinimum(1)
        size_sld.setMaximum(200)
        size_sld.setFixedWidth(140)
        # если view уже готов — возьмём стартовое значение
        start_radius = getattr(self.view, "brush_radius", 30) if self.view else 30
        size_sld.setValue(start_radius)
        size_sld.valueChanged.connect(self._on_size_changed)

        # Непрозрачность
        op_lbl = QLabel("Непрозр.:")
        op_sld = QSlider(Qt.Orientation.Horizontal)
        op_sld.setMinimum(0)
        op_sld.setMaximum(255)
        op_sld.setFixedWidth(140)
        start_alpha = getattr(self.view, "brush_color", QColor(255, 0, 0, 255)).alpha() if self.view else 255
        op_sld.setValue(start_alpha)
        op_sld.valueChanged.connect(self._on_opacity_changed)

        # Цвет
        color_btn = QPushButton("Цвет…")
        color_btn.clicked.connect(self._pick_color)

        # Прямоугольник: как ластик?
        rect_erase_btn = QPushButton("Прямоуг.: стирать")
        rect_erase_btn.setCheckable(True)
        rect_erase_btn.setChecked(False)
        rect_erase_btn.toggled.connect(self._on_rect_erase_toggled)

        # Порядок
        parent_layout.addWidget(mode_lbl)
        parent_layout.addWidget(mode_box)
        parent_layout.addSpacing(8)
        parent_layout.addWidget(size_lbl)
        parent_layout.addWidget(size_sld)
        parent_layout.addSpacing(8)
        parent_layout.addWidget(op_lbl)
        parent_layout.addWidget(op_sld)
        parent_layout.addWidget(color_btn)
        parent_layout.addSpacing(8)
        parent_layout.addWidget(rect_erase_btn)

        # ссылки
        self._ui_mode = mode_box
        self._ui_size = size_sld
        self._ui_opacity = op_sld
        self._ui_color_btn = color_btn
        self._ui_rect_erase_btn = rect_erase_btn

        self._refresh_ui_visibility()

    def paint_click_at_scene(self, scene_pt: QPointF, modifiers: Qt.KeyboardModifier = Qt.KeyboardModifier.NoModifier) -> None:
        """
        Принудительно рисует «точку» кистью в указанной координате сцены и подготавливает
        состояние для последующего рисования при движении (как если бы был обычный mousePress).
        Используется инструментом Замазка, чтобы рисовать уже при первом клике даже без движения.
        """
        idx = self._hit_test_index(scene_pt)
        if idx is None:
            return
        self._begin_stroke(idx, scene_pt, modifiers)

    def _use_brush(self):
        self.mode = "brush"
        self._apply_view_tool()
        self._update_cursor_visibility()

    def _use_eraser(self):
        self.mode = "eraser"
        self._apply_view_tool()
        self._update_cursor_visibility()
    # --- Новая схема: бинды/шорткаты инструмента ---
    def requested_shortcuts(self):
        # Вкладка создаст QShortcut'ы на эти комбинации.
        return [
            ("B", self._use_brush),
            ("E", self._use_eraser),
        ]

    def hotkeys_hint(self) -> str:
        """Возвращает строку с подсказкой по хоткеям инструмента."""
        return "B — кисть, E — ластик, Ctrl+ЛКМ — прямоуг., ПКМ — пипетка"

    def wants_raw_keypress(self) -> bool:
        return False  # не нужен сырой KeyPress

    # --- Новая схема: мышь через вкладку (MouseEventCtx) ---
    def on_mouse_event(self, ctx: MouseEventCtx) -> bool:
        """
        Вернуть True, если инструмент обработал событие и его НЕ надо пускать дальше
        в CanvasView; иначе False — CanvasView нарисует кистью/ластиком сам.
        """
        # Двигаем курсор/превью прямоугольника
        if ctx.etype == "move":
            # если идёт мазок кисти/ластика — рисуем на временном слое и блокируем CanvasView
            if self._is_painting and self.mode in ("brush", "eraser") and self._stroke_idx is not None:
                self._stroke_to(ctx.scene_pos)
                return True
            self._update_cursor_position(ctx.scene_pos)
            if self.mode == "eyedropper":
                col = self._sample_color_at_scene(ctx.scene_pos)
                if col is not None:
                    self._set_cursor_pen_color(col)
            # тянем прямоугольник, если он начат
            if self._rect_item is not None and self._rect_start_scene is not None:
                r = QRectF(self._rect_start_scene, ctx.scene_pos).normalized()
                self._rect_item.setRect(r)
            return False  # движение мыши не блокирует CanvasView

        # Нажатие
        if ctx.etype == "press":
            if ctx.button == Qt.MouseButton.LeftButton:
                # Ctrl + ЛКМ — временный прямоугольник независимо от режима
                if ctx.modifiers & Qt.KeyboardModifier.ControlModifier:
                    idx = self.view._hit_test_index(ctx.scene_pos)
                    if idx is None:
                        return True  # «съели» событие
                    self._start_rect(ctx.scene_pos)
                    return True

                if self.mode == "eyedropper":
                    col = self._sample_color_at_scene(ctx.scene_pos)
                    if col is not None:
                        self.view.set_brush_color(col)
                        self._set_cursor_pen_color(col)
                    return True

                if self.mode == "rect":
                    idx = self.view._hit_test_index(ctx.scene_pos)
                    if idx is None:
                        return True
                    self._start_rect(ctx.scene_pos)
                    return True

                # режимы кисти/ластика — рисуем сами, чтобы не спамить модель при движении
                if self.mode in ("brush", "eraser"):
                    ok = self._begin_stroke_at_point(ctx.scene_pos, ctx.modifiers)
                    return True if ok else False
                self._update_cursor_position(ctx.scene_pos)
                return False

            elif ctx.button == Qt.MouseButton.RightButton:
                # Пипетка «под правую»
                col = self._sample_color_at_scene(ctx.scene_pos)
                if col is not None:
                    self.view.set_brush_color(col)
                    self._set_cursor_pen_color(col)
                return True

            return False

        # Отпускание
        if ctx.etype == "release":
            if ctx.button == Qt.MouseButton.LeftButton and self._rect_item is not None:
                end_pt = ctx.scene_pos
                rect_scene = QRectF(self._rect_start_scene, end_pt).normalized()
                self._commit_rect(rect_scene)
                self._remove_rect_item()
                self._rect_start_scene = None
                return True
            if ctx.button == Qt.MouseButton.LeftButton and self._is_painting:
                self._finish_stroke()
                return True
            return False

        return False

    def on_wheel_event(self, steps: int, modifiers: Qt.KeyboardModifier) -> bool:
        """
        Обработка колесика мыши.
        Shift + колесо — изменение размера кисти.
        steps > 0 — прокрутка «вверх», steps < 0 — «вниз».
        """
        if self.view is None:
            return False

        # Нас интересует только Shift + колесо
        if not (modifiers & Qt.KeyboardModifier.ShiftModifier):
            return False

        if steps == 0:
            return True  # событие считаем обработанным, но менять нечего

        # Текущий радиус и границы такие же, как у слайдера
        cur_radius = int(getattr(self.view, "brush_radius", 30))
        min_radius = 1
        max_radius = 200
        step = 2  # на сколько менять за один «щелчок» колеса

        new_radius = cur_radius + steps * step
        if new_radius < min_radius:
            new_radius = min_radius
        elif new_radius > max_radius:
            new_radius = max_radius

        if new_radius == cur_radius:
            return True  # обработали, но размер не изменился (упёрлись в границу)

        # Обновляем view
        try:
            self.view.set_brush_radius(new_radius)
        except Exception:
            return False

        # Синхронизируем слайдер размера, если он уже создан
        if self._ui_size is not None:
            try:
                self._ui_size.blockSignals(True)
                self._ui_size.setValue(new_radius)
                self._ui_size.blockSignals(False)
            except Exception:
                pass

        # Обновляем курсор в последней позиции
        if self._last_cursor_scene_pt is not None:
            try:
                self._update_cursor_position(self._last_cursor_scene_pt)
            except Exception:
                pass

        return True

    # --------- рисование мазка без постоянных обновлений модели ---------
    def _begin_stroke_at_point(self, scene_pt: QPointF, modifiers) -> bool:
        idx = self.view._hit_test_index(scene_pt)
        if idx is None:
            return False
        return self._begin_stroke(idx, scene_pt, modifiers)

    def _begin_stroke(self, idx: int, scene_pt: QPointF, modifiers) -> bool:
        if self.view is None or not getattr(self.view, "overlays_model", None):
            return False
        layer = self.view.overlay_image(idx)
        if layer is None or layer.isNull():
            return False
        self._lock_model_updates()
        self._stroke_layer = layer.copy()
        self._stroke_idx = idx
        self._is_painting = True
        self._last_scene_pt = scene_pt
        self._stroke_erase = bool(modifiers & Qt.KeyboardModifier.ShiftModifier) or (self.mode == "eraser")
        self._preview_last_ts = None
        self._preview_geom_applied = False
        try:
            self.view._begin_undo_capture(idx)
        except Exception:
            pass
        self._update_cursor_position(scene_pt)
        self._stroke_to(scene_pt)
        return True

    def _stroke_to(self, scene_pt: QPointF) -> None:
        if not self._is_painting or self._stroke_layer is None or self._stroke_idx is None:
            return
        idx = self._stroke_idx
        layer = self._stroke_layer
        x0, y0 = self.view.scene_point_to_overlay_xy(idx, self._last_scene_pt)
        x1, y1 = self.view.scene_point_to_overlay_xy(idx, scene_pt)
        p = QPainter(layer)
        p.setRenderHints(QPainter.RenderHint.Antialiasing | QPainter.RenderHint.SmoothPixmapTransform)
        radius = max(1, int(getattr(self.view, "brush_radius", 30)))
        if self._stroke_erase:
            p.setCompositionMode(QPainter.CompositionMode.CompositionMode_Clear)
            pen = QPen(QColor(0, 0, 0, 0), radius * 2, Qt.PenStyle.SolidLine, Qt.PenCapStyle.RoundCap, Qt.PenJoinStyle.RoundJoin)
        else:
            p.setCompositionMode(QPainter.CompositionMode.CompositionMode_SourceOver)
            color = getattr(self.view, "brush_color", QColor(255, 0, 0, 255))
            pen = QPen(color, radius * 2, Qt.PenStyle.SolidLine, Qt.PenCapStyle.RoundCap, Qt.PenJoinStyle.RoundJoin)
        p.setPen(pen)
        p.drawLine(x0, y0, x1, y1)
        p.end()
        self._last_scene_pt = scene_pt
        self._update_overlay_preview(idx, layer)
        # поддерживаем живой курсор в процессе мазка
        try:
            self._update_cursor_position(scene_pt)
        except Exception:
            pass

    def _update_overlay_preview(self, idx: int, layer: QImage, *, force: bool = False) -> None:
        """Обновить только локальный QGraphicsPixmapItem без рассылки модели."""
        try:
            # троттлинг частоты setPixmap на крупных слоях
            if not force:
                now = time.monotonic()
                if self._preview_last_ts is not None and (now - self._preview_last_ts) < self._preview_interval_sec:
                    return
                self._preview_last_ts = now
            items = getattr(self.view, "_overlay_items", [])
            if 0 <= idx < len(items):
                it = items[idx]
                if it is not None:
                    it.setPixmap(QPixmap.fromImage(layer))
                    # геометрия слоя во время мазка не меняется — применяем один раз
                    if not self._preview_geom_applied and hasattr(self.view, "_apply_overlay_geom"):
                        self.view._apply_overlay_geom(idx)
                        self._preview_geom_applied = True
        except Exception:
            pass

    def _finish_stroke(self) -> None:
        if not self._is_painting or self._stroke_layer is None or self._stroke_idx is None:
            self._reset_stroke_state()
            return
        try:
            # финальный прогон превью без троттлинга, чтобы не потерять последние пиксели
            self._update_overlay_preview(self._stroke_idx, self._stroke_layer, force=True)
            self._unlock_model_updates()
            self.view.overlays_model.replace(self._stroke_idx, self._stroke_layer)
            try:
                self.view._commit_undo_capture(self._stroke_idx)
            except Exception:
                pass
        except Exception:
            pass
        self._reset_stroke_state()

    def _reset_stroke_state(self):
        self._unlock_model_updates()
        self._is_painting = False
        self._last_scene_pt = None
        self._stroke_idx = None
        self._stroke_layer = None
        self._stroke_erase = False
        self._preview_last_ts = None
        self._preview_geom_applied = False


    # ------------- курсор -------------
    def _is_brush_cursor_visible(self) -> bool:
        return self.mode in ("brush", "eraser", "eyedropper")

    def _create_custom_cursor_items(self) -> None:
        if self._cursor_cross_h is not None:
            return
        from PyQt6.QtWidgets import QGraphicsLineItem

        self._cursor_cross_h = QGraphicsLineItem()
        ph = QPen(getattr(self.view, "brush_color", QColor(255, 0, 0, 255)), 1)
        ph.setCosmetic(True)
        self._cursor_cross_h.setPen(ph)
        self._cursor_cross_h.setZValue(20002)
        self.view.scene.addItem(self._cursor_cross_h)

        self._cursor_cross_v = QGraphicsLineItem()
        pv = QPen(getattr(self.view, "brush_color", QColor(255, 0, 0, 255)), 1)
        pv.setCosmetic(True)
        self._cursor_cross_v.setPen(pv)
        self._cursor_cross_v.setZValue(20002)
        self.view.scene.addItem(self._cursor_cross_v)

    def _remove_custom_cursor_items(self) -> None:
        for it_attr in ("_cursor_cross_h", "_cursor_cross_v"):
            it = getattr(self, it_attr)
            if it is not None:
                try:
                    self.view.scene.removeItem(it)
                except Exception:
                    pass
                setattr(self, it_attr, None)

    def _update_custom_cursor_visibility(self, visible: bool) -> None:
        for it in (self._cursor_cross_h, self._cursor_cross_v):
            if it is not None:
                it.setVisible(visible)

    def _update_custom_cursor_position(self, scene_pt: QPointF, rect: QRectF) -> None:
        if self._cursor_cross_h:
            self._cursor_cross_h.setLine(rect.left(), scene_pt.y(), rect.right(), scene_pt.y())
        if self._cursor_cross_v:
            self._cursor_cross_v.setLine(scene_pt.x(), rect.top(), scene_pt.x(), rect.bottom())

    def _set_cursor_pen_color(self, color: QColor) -> None:
        if self._cursor_cross_h is not None:
            ph = self._cursor_cross_h.pen()
            ph.setColor(color)
            self._cursor_cross_h.setPen(ph)
        if self._cursor_cross_v is not None:
            pv = self._cursor_cross_v.pen()
            pv.setColor(color)
            self._cursor_cross_v.setPen(pv)

    # ------------- прямоугольник -------------
    def _start_rect(self, scene_pt: QPointF) -> None:
        self._remove_rect_item()
        self._rect_start_scene = scene_pt
        self._rect_item = QGraphicsRectItem(QRectF(scene_pt, scene_pt))
        pen = QPen(QColor(160, 160, 160), 1, Qt.PenStyle.DashLine)
        self._rect_item.setPen(pen)
        self._rect_item.setBrush(QBrush(Qt.BrushStyle.NoBrush))
        self._rect_item.setZValue(19_000)
        self.view.scene.addItem(self._rect_item)

    def _remove_rect_item(self) -> None:
        if self._rect_item is not None:
            try:
                self.view.scene.removeItem(self._rect_item)
            except Exception:
                pass
            self._rect_item = None

    def _commit_rect(self, rect_scene: QRectF) -> None:
        if rect_scene.isEmpty():
            return
        idx = self.view._hit_test_index(rect_scene.center())
        if idx is None:
            return

        # Undo снапшот — положимся на DrawingCanvasView
        undo_started = False
        try:
            self.view._begin_undo_capture(idx)
            undo_started = True
        except Exception:
            pass

        r_overlay: QRect = self.view.scene_rect_to_overlay_rect(idx, rect_scene)
        if r_overlay.isEmpty():
            if undo_started:
                try:
                    self.view._commit_undo_capture(idx)
                except Exception:
                    pass
            return

        if self._rect_erase:
            # прозрачная вставка
            chunk = QImage(r_overlay.size(), QImage.Format.Format_ARGB32_Premultiplied)
            chunk.fill(0)
        else:
            # заливка текущим цветом кисти
            chunk = QImage(r_overlay.size(), QImage.Format.Format_ARGB32_Premultiplied)
            chunk.fill(0)
            from PyQt6.QtGui import QPainter
            p = QPainter(chunk)
            p.fillRect(0, 0, chunk.width(), chunk.height(), getattr(self.view, "brush_color", QColor(255, 0, 0, 255)))
            p.end()

        self.view.paste_chunk_to_overlay(idx, rect_scene, chunk)
        if undo_started:
            try:
                self.view._commit_undo_capture(idx)
            except Exception:
                pass

    # ------------- пипетка -------------
    def _sample_color_at_scene(self, scene_pt: QPointF) -> Optional[QColor]:
        idx = self.view._hit_test_index(scene_pt)
        if idx is None:
            return None

        # сначала слой
        ox, oy = self.view._scene_to_overlay_xy(idx, scene_pt)
        layer = self.view._overlay_images[idx] if 0 <= idx < len(self.view._overlay_images) else None
        if layer is not None and 0 <= ox < layer.width() and 0 <= oy < layer.height():
            rgba = QColor(layer.pixelColor(ox, oy))
            if rgba.alpha() > 0:
                return rgba

        # если слой прозрачный — берём из оригинала
        orig_chunk = self.view.get_original_chunk(idx, QRectF(scene_pt, scene_pt).adjusted(-1, -1, 1, 1))
        if not orig_chunk.isNull():
            # возьмём центр
            cx = orig_chunk.width() // 2
            cy = orig_chunk.height() // 2
            return QColor(orig_chunk.pixelColor(max(0, min(cx, orig_chunk.width() - 1)),
                                               max(0, min(cy, orig_chunk.height() - 1))))
        return None

    # ------------- обработчики UI -------------
    def _on_mode_changed(self, _idx: int) -> None:
        if not self._ui_mode:
            return
        self.mode = str(self._ui_mode.currentData()) or "brush"
        self._apply_view_tool()
        self._update_cursor_visibility()

    def _on_size_changed(self, value: int) -> None:
        if self.view is not None:
            self.view.set_brush_radius(int(value))

    def _on_opacity_changed(self, value: int) -> None:
        if self.view is not None:
            self.view.set_brush_opacity(int(value))

    def _pick_color(self) -> None:
        if self.view is None:
            return
        start = getattr(self.view, "brush_color", QColor(255, 0, 0, 255))
        col = QColorDialog.getColor(start, self.view, "Выбор цвета", QColorDialog.ColorDialogOption.ShowAlphaChannel)
        if col.isValid():
            self.view.set_brush_color(col)
            self._set_cursor_pen_color(col)

    def _on_rect_erase_toggled(self, checked: bool) -> None:
        self._rect_erase = bool(checked)

    # ------------- служебные -------------
    def _apply_view_tool(self) -> None:
        """Сообщаем DrawingCanvasView какой инструмент активен (кисть/ластик)."""
        if self.view is None:
            return
        if self.mode == "eraser":
            self.view.set_tool("eraser")
        else:
            # 'brush', 'eyedropper' и 'rect' — базовый инструмент кисти
            self.view.set_tool("brush")

    def _refresh_ui_visibility(self) -> None:
        """Прячем/показываем контролы, зависящие от режима."""
        show_size = self.mode in ("brush", "eraser", "eyedropper")
        show_opacity = self.mode in ("brush", "eyedropper")  # непрозрачность кисти; для ластика не важна
        show_color = self.mode in ("brush", "eyedropper")
        show_rect_toggle = self.mode == "rect"

        if self._ui_size:
            self._ui_size.setVisible(show_size)
        if self._ui_opacity:
            self._ui_opacity.setVisible(show_opacity)
        if self._ui_color_btn:
            self._ui_color_btn.setVisible(show_color)
        if self._ui_rect_erase_btn:
            self._ui_rect_erase_btn.setVisible(show_rect_toggle)
