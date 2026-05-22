# ui_new/tabs/text_tab/text_overlay_item.py
from __future__ import annotations
from dataclasses import dataclass, replace, field
from typing import Optional, Callable, List
from PyQt6.QtCore import Qt, QPointF, QRectF
from PyQt6.QtGui import (
    QPixmap,
    QImage,
    QPainter,
    QPainterPath,
    QPen,
    QBrush,
    QColor,
    QPolygonF,
    QTransform,
)
from PyQt6.QtWidgets import (
    QGraphicsPixmapItem,
    QGraphicsItem,
    QGraphicsEllipseItem,
    QStyleOptionGraphicsItem,
    QWidget,
)
import math
import traceback
from .text_style import TextStyle

DEBUG = False
HANDLE_RADIUS = 7.0
GRID_SPLITS = 4


@dataclass
class _DragState:
    active: bool = False
    press_scene_pos: QPointF = field(default_factory=QPointF)
    press_quad_scene: Optional[list[QPointF]] = None
    handle_index: int = -1
    body_drag: bool = False
@dataclass
class TextOverlayMeta:
    img_idx: int
    u: float
    v: float
    w_frac: float       # ширина оверлея как доля ширины страницы
    user_scale: float = 1.0
    angle: float = 0.0
    file: str = ""      # имя png текста в project.text_images
    text: str = ""      # исходный текст (для повторного редактирования)
    style: TextStyle = field(default_factory=TextStyle)
    cut_enabled: bool = True
    transform_uv: Optional[list[tuple[float, float]]] = None  # uv координаты углов (TL,TR,BR,BL)

    _STYLE_FIELDS = {
        "font_family", "font_size", "font_color_rgba", "color_rgba", "align", "line_spacing",
        "line_spacing_percent", "extra_vpadding", "reflect",
        "stroke_width", "stroke_color_rgba",
        "glow_radius", "glow_softness", "glow_color_rgba",
        "shadow_dx", "shadow_dy", "shadow_color_rgba",
        "grad2_c1_rgba", "grad2_c2_rgba", "grad_angle_deg",
        "grad4_tl_rgba", "grad4_tr_rgba", "grad4_bl_rgba", "grad4_br_rgba",
        "text_shape", "shake_enabled", "shake_angle_deg", "shake_up",
        "shake_down", "shake_steps", "shake_base_fade", "shake_decay", "shake_blur",
    }

    def update_style(self, patch: dict):
        """Обновить вложенный TextStyle, не трогая геометрию."""
        self.style = self.style.with_updates(**patch)

    def __getattr__(self, name):
        if name == "color_rgba":
            name = "font_color_rgba"
        if name in self._STYLE_FIELDS:
            return getattr(self.style, name)
        raise AttributeError(name)

    def __setattr__(self, name, value):
        if name == "color_rgba":
            name = "font_color_rgba"
        if name in TextOverlayMeta.__dict__.get("_STYLE_FIELDS", set()):
            if "style" in self.__dict__:
                object.__setattr__(self, "style", replace(self.style, **{name: value}))
                return
        object.__setattr__(self, name, value)
class TextOverlayItem(QGraphicsPixmapItem):
    """Перемещаемый/вращаемый/масштабируемый текстовый оверлей."""
    def __init__(
        self,
        meta: TextOverlayMeta,
        base_image: QImage,
        parent: Optional[QGraphicsItem] = None,
        on_changed: Optional[Callable[[str, "TextOverlayItem"], None]] = None,
        on_drag_state_changed: Optional[Callable[[bool, "TextOverlayItem"], None]] = None,
    ):
        super().__init__(parent)
        self.meta = meta
        self._on_changed = on_changed
        self._on_drag_state_changed = on_drag_state_changed
        self._get_masks_callback: Optional[Callable[["TextOverlayItem"], List[QImage]]] = None
        self.setZValue(20_000)
        # перемещение/выделение/фокус + уведомления об изменениях геометрии
        self.setFlags(
            self.GraphicsItemFlag.ItemIsMovable
            | self.GraphicsItemFlag.ItemIsSelectable
            | self.GraphicsItemFlag.ItemIsFocusable
            | self.GraphicsItemFlag.ItemSendsGeometryChanges
        )
        self.setAcceptHoverEvents(True)
        self.setAcceptedMouseButtons(Qt.MouseButton.LeftButton)
        self.setTransformationMode(Qt.TransformationMode.SmoothTransformation)
        self.setCacheMode(QGraphicsItem.CacheMode.NoCache)

        # ----- Ручка поворота (скрыта по умолчанию, видна только при выделении)
        self._rotating = False
        self._rotate_handle_radius = 8
        self._rotate_handle = QGraphicsEllipseItem(self)
        d = self._rotate_handle_radius * 2
        self._rotate_handle.setRect(-self._rotate_handle_radius, -self._rotate_handle_radius, d, d)
        self._rotate_handle.setBrush(Qt.GlobalColor.white)
        self._rotate_handle.setPen(Qt.GlobalColor.black)
        self._rotate_handle.setZValue(10_000)
        self._rotate_handle.setVisible(False)
        self._rotate_handle.setAcceptedMouseButtons(Qt.MouseButton.NoButton)
        # Геометрия задаётся сразу, чтобы ручка не падала на _rect=None
        self._rect = QRectF(0, 0, base_image.width(), base_image.height())
        self._update_rotate_handle_pos()
        self.setAcceptHoverEvents(True)
        self._base = base_image    # RGBA-снимок текста (без масштаба)
        self._base_scale = 1.0
        self._transform_mode = False
        self._drag = _DragState()
        self._dragging_overlay = False
        self._apply_pixmap()

    # ---------- перспективная трансформация ----------
    def setTransformMode(self, enabled: bool):
        if self._transform_mode == enabled:
            return
        self._transform_mode = enabled
        try:
            self.prepareGeometryChange()
        except Exception:
            pass
        if enabled:
            quad_scene = self._current_quad_scene()
            quad_parent = [p - self.pos() for p in quad_scene]
            self._ensure_transform_from_quad(quad_parent)
        self._rotate_handle.setVisible(self.isSelected() and not enabled)
        self.update()

    def isTransformMode(self) -> bool:
        return bool(self._transform_mode)

    def apply_parent_quad(self, quad_parent: list[QPointF]):
        """Устанавливает projective-трансформацию по углам (в coords родителя, без self.pos())."""
        self._ensure_transform_from_quad(quad_parent)

    def _local_corners(self) -> list[QPointF]:
        r = self._rect
        return [
            QPointF(r.left(), r.top()),         # TL
            QPointF(r.right(), r.top()),        # TR
            QPointF(r.right(), r.bottom()),     # BR
            QPointF(r.left(), r.bottom()),      # BL
        ]

    def _current_quad_scene(self) -> list[QPointF]:
        return [self.mapToScene(p) for p in self._local_corners()]

    def _ensure_transform_from_quad(self, quad_parent: list[QPointF]):
        """quad_parent: 4 точки в координатах родителя (без self.pos())."""
        src = QPolygonF(self._local_corners())
        dst = QPolygonF(quad_parent)
        m = QTransform()
        ok = QTransform.quadToQuad(src, dst, m)
        if not ok:
            return
        base = self._base_affine_transform()
        inv, inv_ok = base.inverted()
        if inv_ok:
            m = m * inv
        self.setTransform(m)

    def _base_affine_transform(self) -> QTransform:
        """Матрица, описывающая rotation/scale (без projective)."""
        t = QTransform()
        origin = self.transformOriginPoint()
        if not origin.isNull():
            t.translate(origin.x(), origin.y())
        angle = float(self.rotation())
        if abs(angle) > 1e-6:
            t.rotate(angle)
        sc = float(self.scale())
        if abs(sc - 1.0) > 1e-6:
            t.scale(sc, sc)
        if not origin.isNull():
            t.translate(-origin.x(), -origin.y())
        return t


    def _apply_pixmap(self, target_width_px: Optional[int] = None):
        img = self._base
        if img.isNull():
            return

        qimg = img
        if target_width_px is not None and target_width_px > 0:
            ratio = target_width_px / qimg.width()
            qimg = qimg.scaled(
                int(qimg.width() * ratio),
                int(qimg.height() * ratio),
                Qt.AspectRatioMode.KeepAspectRatio,
                Qt.TransformationMode.SmoothTransformation,
            )

        try:
            self.prepareGeometryChange()
        except Exception:
            pass
        self.setPixmap(QPixmap.fromImage(qimg))
        self._rect = QRectF(0, 0, qimg.width(), qimg.height())
        self.setTransformOriginPoint(self._rect.center())
        self.setRotation(self.meta.angle)
    # Alt+колёсико вращает оверлей; Ctrl+колёсико оставляем CanvasView для зума холста
    def wheelEvent(self, e):
        if e.modifiers() & Qt.KeyboardModifier.AltModifier:
            dy = e.delta() if hasattr(e, "delta") else e.angleDelta().y()
            delta = 2.0 if dy > 0 else -2.0
            self.meta.angle = (self.meta.angle + delta) % 360.0
            self.setRotation(self.meta.angle)
            e.accept()
            if self._on_changed:
                self._on_changed("angle", self)
            return
        # всё остальное игнорируем
        e.ignore()

    # ---------- выбор с первого клика и перетаскивание ручки поворота ----------
    def mousePressEvent(self, e):
        if e.button() == Qt.MouseButton.LeftButton and self._transform_mode:
            if not self.isSelected():
                self.setSelected(True)
            self.setFocus(Qt.FocusReason.MouseFocusReason)
            self._drag = _DragState(
                active=True,
                press_scene_pos=e.scenePos(),
                press_quad_scene=self._current_quad_scene(),
                handle_index=-1,
                body_drag=False,
            )
            self._emit_drag_state(True)
            hi = self._hit_test_handle(e.scenePos())
            if hi >= 0:
                self._drag.handle_index = hi
            else:
                self._drag.body_drag = True
            e.accept()
            return

        # гарантируем выделение с первого клика
        if not self.isSelected():
            self.setSelected(True)
        # проверим, попали ли в ручку
        if self._rotate_handle.isVisible():
            local = self.mapFromScene(e.scenePos())
            if self._rotate_handle.contains(self._rotate_handle.mapFromParent(local)):
                self._rotating = True
                self.setCursor(Qt.CursorShape.ClosedHandCursor)
                # запомним вектор «центр -> мышь» в начале
                self._rot_start_vec = (local - self._item_center())
                e.accept()
                return
        if e.button() == Qt.MouseButton.LeftButton:
            self._emit_drag_state(True)
        super().mousePressEvent(e)

    def mouseMoveEvent(self, e):
        if self._transform_mode and self._drag.active:
            delta_scene = e.scenePos() - self._drag.press_scene_pos
            quad_scene = list(self._drag.press_quad_scene) if self._drag.press_quad_scene else self._current_quad_scene()
            if self._drag.handle_index >= 0:
                quad_scene[self._drag.handle_index] = quad_scene[self._drag.handle_index] + delta_scene
            elif self._drag.body_drag:
                quad_scene = [p + delta_scene for p in quad_scene]

            quad_parent = [p - self.pos() for p in quad_scene]
            self._ensure_transform_from_quad(quad_parent)
            self.update()
            e.accept()
            return

        if self._rotating:
            local = self.mapFromScene(e.scenePos())
            v0 = self._rot_start_vec
            v1 = (local - self._item_center())
            if v0.manhattanLength() > 0.1 and v1.manhattanLength() > 0.1:
                a0 = math.degrees(math.atan2(v0.y(), v0.x()))
                a1 = math.degrees(math.atan2(v1.y(), v1.x()))
                da = (a1 - a0)
                new_angle = (self.meta.angle + da) % 360.0
                self.setRotation(new_angle)
            e.accept()
            return
        super().mouseMoveEvent(e)

    def mouseReleaseEvent(self, e):
        if self._transform_mode and self._drag.active:
            self._drag.active = False
            self._emit_drag_state(False)
            e.accept()
            return
        if self._rotating:
            # зафиксируем угол в метаданных
            self.meta.angle = self.rotation() % 360.0
            if self._on_changed:
                self._on_changed("angle", self)
            self._rotating = False
            self.setCursor(Qt.CursorShape.ArrowCursor)
            e.accept()
            return
        super().mouseReleaseEvent(e)
        self._emit_drag_state(False)

    def hoverMoveEvent(self, e):
        if self._transform_mode and self.isSelected():
            hi = self._hit_test_handle(e.scenePos())
            if hi >= 0:
                self.setCursor(Qt.CursorShape.SizeAllCursor)
            else:
                self.setCursor(Qt.CursorShape.OpenHandCursor)
            return
        # курсор «рука», если навели на ручку
        if self._rotate_handle.isVisible():
            local = self.mapFromScene(e.scenePos())
            if self._rotate_handle.contains(self._rotate_handle.mapFromParent(local)):
                self.setCursor(Qt.CursorShape.OpenHandCursor)
                return
        self.setCursor(Qt.CursorShape.ArrowCursor)

    def itemChange(self, change, value):
        if change == self.GraphicsItemChange.ItemSelectedHasChanged:
            self._rotate_handle.setVisible(bool(value) and not self._transform_mode)
            self._update_rotate_handle_pos()
            # === Новое: делаем выбор эксклюзивным ===
            try:
                if bool(value):  # этот item стал выделенным
                    sc = self.scene()
                    if sc:
                        for other in sc.selectedItems():
                            if other is not self:
                                other.setSelected(False)
            except Exception:
                pass
        elif change in (
            self.GraphicsItemChange.ItemPositionChange,
            self.GraphicsItemChange.ItemPositionHasChanged,
            self.GraphicsItemChange.ItemTransformHasChanged,
            self.GraphicsItemChange.ItemScaleHasChanged,
        ):
            self._update_rotate_handle_pos()
            if self._on_changed:
                if change in (self.GraphicsItemChange.ItemPositionChange,
                            self.GraphicsItemChange.ItemPositionHasChanged):
                    if DEBUG: print("reason - pos")
                    reason = "pos"
                elif change == self.GraphicsItemChange.ItemScaleHasChanged:
                    reason = "scale"
                else:
                    reason = "transform"
                self._on_changed(reason, self)
        return super().itemChange(change, value)

    def _update_rotate_handle_pos(self):
        # ставим ручку в правый верх видимой рамки предмета
        br: QRectF = self._rect
        top_right = br.topRight()
        # отступ чуть наружу, чтобы не перекрывала контент
        offset = QPointF(self._rotate_handle_radius + 2, -self._rotate_handle_radius - 2)
        self._rotate_handle.setPos(top_right + offset)

    def _item_center(self) -> QPointF:
        return self._rect.center()
    

    # ---------- расширяем хит-область на область ручки ----------
    def _handle_center_in_item_coords(self) -> QPointF:
        # Позиция ручки задана в координатах item: setPos(top_right + offset)
        return self._rotate_handle.pos()

    def _handle_path(self) -> QPainterPath:
        r = float(self._rotate_handle_radius)
        c = self._handle_center_in_item_coords()
        rect = QRectF(c.x() - r, c.y() - r, 2 * r, 2 * r)
        p = QPainterPath()
        p.addEllipse(rect)
        return p

    def shape(self) -> QPainterPath:
        # Базовая форма + область ручки, чтобы клики по ней считались "по элементу"
        p = QPainterPath()
        p.addRect(self.boundingRect())
        if self._rotate_handle.isVisible():
            p.addPath(self._handle_path())
        return p

    def contains(self, point: QPointF) -> bool:
        # Дублируем логику для contains на всякий случай
        if QRectF(self.boundingRect()).contains(point):
            return True
        if self._rotate_handle.isVisible() and self._handle_path().contains(point):
            return True
        return False

    def paint(self, painter: QPainter, option: QStyleOptionGraphicsItem, widget: Optional[QWidget] = None):
        """
        Переопределённая отрисовка с поддержкой масок компонент от линий обрезки.
        """
        # Если есть callback для получения масок и есть маски - рисуем с применением масок
        if self._get_masks_callback is not None:
            masks = self._get_masks_callback(self)
            if masks:
                pixmap = self.pixmap()
                if not pixmap.isNull():
                    ov_img = pixmap.toImage().convertToFormat(QImage.Format.Format_ARGB32_Premultiplied)

                    # ИСПРАВЛЕНИЕ: Объединяем все видимые маски в одну перед применением
                    # Создаём объединённую маску размером с оверлеем
                    combined_mask = QImage(ov_img.size(), QImage.Format.Format_Alpha8)
                    combined_mask.fill(0)  # Начинаем с полностью прозрачной маски

                    # Рисуем все видимые маски в объединённую маску
                    mask_painter = QPainter(combined_mask)
                    mask_painter.setCompositionMode(QPainter.CompositionMode.CompositionMode_SourceOver)
                    for mask in masks:
                        # Каждая маска добавляется к объединённой (логическое ИЛИ)
                        # Масштабируем маску до размера оверлея, если размеры не совпадают
                        if mask.size() != ov_img.size():
                            scaled_mask = mask.scaled(
                                ov_img.size(),
                                Qt.AspectRatioMode.IgnoreAspectRatio,
                                Qt.TransformationMode.SmoothTransformation
                            )
                            mask_painter.drawImage(0, 0, scaled_mask)
                        else:
                            mask_painter.drawImage(0, 0, mask)
                    mask_painter.end()

                    # Применяем объединённую маску к изображению оверлея
                    result = QImage(ov_img)
                    qp = QPainter(result)
                    qp.setRenderHint(QPainter.RenderHint.SmoothPixmapTransform)
                    qp.setCompositionMode(QPainter.CompositionMode.CompositionMode_DestinationIn)
                    qp.drawImage(0, 0, combined_mask)
                    qp.end()

                    # Рисуем финальный результат один раз
                    painter.drawImage(0, 0, result)

                    # Рисуем пунктирную рамку выделения, если элемент выделен
                    if self.isSelected():
                        painter.save()
                        pen = QPen(Qt.GlobalColor.cyan, 2, Qt.PenStyle.DashLine)
                        painter.setPen(pen)
                        painter.drawRect(self._rect)
                        painter.restore()
                    if self._transform_mode and self.isSelected():
                        self._draw_grid(painter)
                        self._draw_handles(painter)
                    return

        # Стандартная отрисовка (без масок)
        super().paint(painter, option, widget)

        # Рисуем пунктирную рамку выделения, если элемент выделен
        if self.isSelected():
            painter.save()
            pen = QPen(Qt.GlobalColor.cyan, 2, Qt.PenStyle.DashLine)
            painter.setPen(pen)
            painter.drawRect(self._rect)
            painter.restore()

        if self._transform_mode and self.isSelected():
            self._draw_grid(painter)
            self._draw_handles(painter)

    def boundingRect(self) -> QRectF:
        base = self._rect if self._rect is not None else super().boundingRect()
        margin = 0.0
        if self._transform_mode:
            margin = HANDLE_RADIUS * 2 + 4
        else:
            margin = float(self._rotate_handle_radius + 2)
        return base.adjusted(-margin, -margin, margin, margin)

    # ---------- transform UI ----------
    def _draw_grid(self, painter: QPainter):
        painter.save()
        painter.setBrush(Qt.BrushStyle.NoBrush)
        painter.setPen(QPen(QColor(255, 255, 255, 170), 1))
        painter.drawRect(self._rect)

        w = self._rect.width()
        h = self._rect.height()
        for i in range(1, GRID_SPLITS):
            x = w * i / GRID_SPLITS
            y = h * i / GRID_SPLITS
            painter.drawLine(QPointF(x, 0), QPointF(x, h))
            painter.drawLine(QPointF(0, y), QPointF(w, y))
        painter.restore()

    def _draw_handles(self, painter: QPainter):
        painter.save()
        painter.setBrush(QBrush(QColor(255, 80, 80, 230)))
        painter.setPen(QPen(QColor(0, 0, 0, 200), 1))

        world = painter.worldTransform()
        painter.setWorldTransform(QTransform())
        for pt in self._local_corners():
            device_pt = world.map(pt)
            painter.drawEllipse(device_pt, HANDLE_RADIUS, HANDLE_RADIUS)
        painter.restore()

    def _hit_test_handle(self, scene_pos: QPointF) -> int:
        corners_scene = self._current_quad_scene()
        for i, p in enumerate(corners_scene):
            if (p - scene_pos).manhattanLength() <= HANDLE_RADIUS * 2.0:
                return i
        return -1

    def _emit_drag_state(self, active: bool):
        """Уведомляем вьюху о старте/окончании перетаскивания."""
        if self._dragging_overlay == active:
            return
        self._dragging_overlay = active
        cb = self._on_drag_state_changed
        if cb:
            try:
                cb(active, self)
            except Exception:
                pass
