from __future__ import annotations

import math
import os
import time
from typing import List, Optional

from PyQt6.QtCore import Qt, QPointF, QRect, QRectF
from PyQt6.QtGui import QColor, QImage, QPainter, QPainterPath, QPen, QPixmap, QTransform, QBrush
from PyQt6.QtWidgets import (
    QComboBox,
    QHBoxLayout,
    QLabel,
    QSlider,
    QSpinBox,
    QGraphicsPixmapItem,
    QGraphicsRectItem,
)

from .base import BrushBase, MouseEventCtx


class StampTool(BrushBase):
    tool_id = "stamp"
    title = "Штамп"

    def __init__(self):
        super().__init__()
        self._source_subdir: str = ""
        self._source_paths: List[str] = []
        self._source_cache_idx: Optional[int] = None
        self._source_cache_img: Optional[QImage] = None
        self._preview_opacity: int = 160
        self._y_offset: int = 0

        self._cursor_preview = None

        self._rect_item = None
        self._rect_start_scene: Optional[QPointF] = None
        self._rect_erase: bool = False

        self._is_painting = False
        self._last_scene_pt: Optional[QPointF] = None
        self._stroke_idx: Optional[int] = None
        self._stroke_layer: Optional[QImage] = None
        self._stroke_source: Optional[QImage] = None
        self._stroke_erase: bool = False
        self._preview_last_ts: Optional[float] = None
        self._preview_interval_sec: float = 0.02
        self._preview_geom_applied: bool = False

    # ------------- lifecycle -------------
    def activate(self, view) -> None:
        super().activate(view)
        self._clear_view_tool()
        self._update_cursor_visibility()

    def deactivate(self) -> None:
        self._finish_stroke()
        self._remove_rect_item()
        self._remove_preview_item()
        self._clear_view_tool()
        super().deactivate()

    def _clear_view_tool(self) -> None:
        if self.view is None:
            return
        try:
            self.view.set_tool("none")
        except Exception:
            try:
                self.view.set_tool(None)
            except Exception:
                pass

    # ------------- UI -------------
    def build_ui(self, parent_layout: QHBoxLayout) -> None:
        src_lbl = QLabel("Исходник:")
        src_box = QComboBox()
        src_box.setMinimumWidth(150)

        size_lbl = QLabel("Размер:")
        size_sld = QSlider(Qt.Orientation.Horizontal)
        size_sld.setMinimum(1)
        size_sld.setMaximum(200)
        size_sld.setFixedWidth(140)
        size_sld.setValue(getattr(self.view, "brush_radius", 30) if self.view else 30)
        size_sld.valueChanged.connect(self._on_size_changed)

        op_lbl = QLabel("Превью:")
        op_sld = QSlider(Qt.Orientation.Horizontal)
        op_sld.setMinimum(0)
        op_sld.setMaximum(255)
        op_sld.setFixedWidth(140)
        op_sld.setValue(self._preview_opacity)
        op_sld.valueChanged.connect(self._on_preview_opacity_changed)

        off_lbl = QLabel("Смещение Y:")
        off_spin = QSpinBox()
        off_spin.setRange(-10000, 10000)
        off_spin.setSingleStep(1)
        off_spin.setValue(self._y_offset)
        off_spin.valueChanged.connect(self._on_offset_changed)

        parent_layout.addWidget(src_lbl)
        parent_layout.addWidget(src_box)
        parent_layout.addSpacing(8)
        parent_layout.addWidget(size_lbl)
        parent_layout.addWidget(size_sld)
        parent_layout.addSpacing(8)
        parent_layout.addWidget(op_lbl)
        parent_layout.addWidget(op_sld)
        parent_layout.addSpacing(8)
        parent_layout.addWidget(off_lbl)
        parent_layout.addWidget(off_spin)

        self._ui_size = size_sld
        self._source_box = src_box
        self._refresh_source_list()
        src_box.currentIndexChanged.connect(self._on_source_changed)

    # ------------- UI handlers -------------
    def _on_source_changed(self, index: int) -> None:
        if not hasattr(self, "_source_box"):
            return
        name = self._source_box.currentData()
        self._set_source_subdir(name)

    def _on_size_changed(self, value: int) -> None:
        if self.view is None:
            return
        try:
            self.view.set_brush_radius(int(value))
        except Exception:
            pass
        self._refresh_cursor_from_last()

    def _on_preview_opacity_changed(self, value: int) -> None:
        self._preview_opacity = max(0, min(255, int(value)))
        self._refresh_cursor_from_last()

    def _on_offset_changed(self, value: int) -> None:
        self._y_offset = int(value)
        self._refresh_cursor_from_last()

    # ------------- mouse events -------------
    def on_mouse_event(self, ctx: MouseEventCtx) -> bool:
        if ctx.etype == "move":
            if self._rect_item is not None and self._rect_start_scene is not None:
                self._rect_item.setRect(QRectF(self._rect_start_scene, ctx.scene_pos).normalized())
                return True
            if self._is_painting and self._stroke_idx is not None:
                self._stroke_to(ctx.scene_pos)
                return True
            self._update_cursor_position(ctx.scene_pos)
            return False

        if ctx.etype == "press":
            if ctx.button == Qt.MouseButton.LeftButton:
                if ctx.modifiers & Qt.KeyboardModifier.ShiftModifier:
                    self._start_rect(ctx.scene_pos, erase=True)
                    return True
                if ctx.modifiers & Qt.KeyboardModifier.ControlModifier:
                    self._start_rect(ctx.scene_pos, erase=False)
                    return True
                return bool(self._begin_stroke(ctx.scene_pos, erase=False))
            if ctx.button == Qt.MouseButton.RightButton:
                return bool(self._begin_stroke(ctx.scene_pos, erase=True))
            return False

        if ctx.etype == "release":
            if ctx.button in (Qt.MouseButton.LeftButton, Qt.MouseButton.RightButton):
                if ctx.button == Qt.MouseButton.LeftButton and self._rect_item is not None:
                    rect_scene = self._rect_item.rect()
                    self._commit_rect(rect_scene)
                    self._remove_rect_item()
                    self._rect_start_scene = None
                    return True
                self._finish_stroke()
                return True
        return False

    # ------------- stroke -------------
    def _begin_stroke(self, scene_pt: QPointF, *, erase: bool) -> bool:
        if self.view is None or not getattr(self.view, "overlays_model", None):
            return False
        idx = self.view._hit_test_index(scene_pt)
        if idx is None:
            return False
        layer = self.view.overlay_image(idx)
        if layer is None or layer.isNull():
            return False
        source = None
        if not erase:
            source = self._get_source_image(idx, layer.width())
            if source is None or source.isNull():
                return False
        self._lock_model_updates()
        self._stroke_layer = layer.copy()
        self._stroke_idx = idx
        self._stroke_source = source
        self._stroke_erase = erase
        self._is_painting = True
        self._last_scene_pt = scene_pt
        self._preview_last_ts = None
        self._preview_geom_applied = False
        try:
            self.view._begin_undo_capture(idx)
        except Exception:
            pass
        self._stroke_to(scene_pt)
        return True

    def _stroke_to(self, scene_pt: QPointF) -> None:
        if not self._is_painting or self._stroke_layer is None or self._stroke_idx is None:
            return
        idx = self._stroke_idx
        radius = max(1, int(getattr(self.view, "brush_radius", 30)))
        if self._stroke_erase:
            self._erase_segment(idx, self._last_scene_pt, scene_pt, radius)
        else:
            self._stamp_segment(idx, self._last_scene_pt, scene_pt, radius)
        self._last_scene_pt = scene_pt
        self._update_overlay_preview(idx, self._stroke_layer)
        try:
            self._update_cursor_position(scene_pt)
        except Exception:
            pass

    def _erase_segment(self, idx: int, sp0: QPointF, sp1: QPointF, radius: int) -> None:
        layer = self._stroke_layer
        if layer is None:
            return
        x0, y0 = self.view.scene_point_to_overlay_xy(idx, sp0)
        x1, y1 = self.view.scene_point_to_overlay_xy(idx, sp1)
        p = QPainter(layer)
        p.setRenderHints(QPainter.RenderHint.Antialiasing | QPainter.RenderHint.SmoothPixmapTransform)
        p.setCompositionMode(QPainter.CompositionMode.CompositionMode_Clear)
        pen = QPen(QColor(0, 0, 0, 0), radius * 2, Qt.PenStyle.SolidLine,
                   Qt.PenCapStyle.RoundCap, Qt.PenJoinStyle.RoundJoin)
        p.setPen(pen)
        p.drawLine(x0, y0, x1, y1)
        p.end()

    def _stamp_segment(self, idx: int, sp0: QPointF, sp1: QPointF, radius: int) -> None:
        if self._stroke_layer is None or self._stroke_source is None:
            return
        x0, y0 = self.view.scene_point_to_overlay_xy(idx, sp0)
        x1, y1 = self.view.scene_point_to_overlay_xy(idx, sp1)
        dx = x1 - x0
        dy = y1 - y0
        dist = math.hypot(dx, dy)
        spacing = max(1, int(radius * 0.6))
        steps = max(1, int(dist / spacing))
        p = QPainter(self._stroke_layer)
        p.setRenderHints(QPainter.RenderHint.Antialiasing | QPainter.RenderHint.SmoothPixmapTransform)
        p.setCompositionMode(QPainter.CompositionMode.CompositionMode_SourceOver)
        for i in range(steps + 1):
            t = i / steps
            cx = int(round(x0 + dx * t))
            cy = int(round(y0 + dy * t))
            stamp = self._build_stamp_image(self._stroke_source, cx, cy, radius)
            if stamp is None or stamp.isNull():
                continue
            p.drawImage(cx - radius, cy - radius, stamp)
        p.end()

    def _finish_stroke(self) -> None:
        if not self._is_painting or self._stroke_layer is None or self._stroke_idx is None:
            self._reset_stroke_state()
            return
        try:
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

    def _reset_stroke_state(self) -> None:
        self._unlock_model_updates()
        self._is_painting = False
        self._last_scene_pt = None
        self._stroke_idx = None
        self._stroke_layer = None
        self._stroke_source = None
        self._stroke_erase = False
        self._preview_last_ts = None
        self._preview_geom_applied = False

    def _update_overlay_preview(self, idx: int, layer: QImage, *, force: bool = False) -> None:
        try:
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
                    if not self._preview_geom_applied and hasattr(self.view, "_apply_overlay_geom"):
                        self.view._apply_overlay_geom(idx)
                        self._preview_geom_applied = True
        except Exception:
            pass

    # ------------- cursor preview -------------
    def _create_custom_cursor_items(self) -> None:
        if self._cursor_preview is not None or self.view is None:
            return
        self._cursor_preview = QGraphicsPixmapItem()
        self._cursor_preview.setZValue(19999)
        self._cursor_preview.setOpacity(self._preview_opacity / 255.0)
        self._cursor_preview.setVisible(False)
        self.view.scene.addItem(self._cursor_preview)

    def _remove_custom_cursor_items(self) -> None:
        self._remove_preview_item()

    def _remove_preview_item(self) -> None:
        if self.view is None:
            return
        if self._cursor_preview is not None:
            try:
                self.view.scene.removeItem(self._cursor_preview)
            except Exception:
                pass
            self._cursor_preview = None

    def _update_custom_cursor_visibility(self, visible: bool) -> None:
        if self._cursor_preview is not None and not visible:
            self._cursor_preview.setVisible(False)

    def _update_custom_cursor_position(self, scene_pt: QPointF, rect) -> None:
        if self._cursor_preview is None or self.view is None:
            return
        idx = self.view._hit_test_index(scene_pt)
        if idx is None:
            self._cursor_preview.setVisible(False)
            return
        layer = self.view.overlay_image(idx)
        if layer is None or layer.isNull():
            self._cursor_preview.setVisible(False)
            return
        source = self._get_source_image(idx, layer.width())
        if source is None or source.isNull():
            self._cursor_preview.setVisible(False)
            return
        radius = max(1, int(getattr(self.view, "brush_radius", 30)))
        cx, cy = self.view.scene_point_to_overlay_xy(idx, scene_pt)
        stamp = self._build_stamp_image(source, cx, cy, radius)
        if stamp is None or stamp.isNull():
            self._cursor_preview.setVisible(False)
            return
        bbox = self.view._image_bbox(idx)
        if bbox.width() <= 0 or bbox.height() <= 0 or layer.width() <= 0 or layer.height() <= 0:
            self._cursor_preview.setVisible(False)
            return
        sx = bbox.width() / layer.width()
        sy = bbox.height() / layer.height()
        self._cursor_preview.setPixmap(QPixmap.fromImage(stamp))
        self._cursor_preview.setTransform(QTransform().scale(sx, sy))
        self._cursor_preview.setPos(scene_pt.x() - radius * sx, scene_pt.y() - radius * sy)
        self._cursor_preview.setOpacity(self._preview_opacity / 255.0)
        self._cursor_preview.setVisible(True)

    def _refresh_cursor_from_last(self) -> None:
        if self._last_cursor_scene_pt is not None:
            try:
                self._update_cursor_position(self._last_cursor_scene_pt)
            except Exception:
                pass

    # ------------- rect -------------
    def _start_rect(self, scene_pt: QPointF, *, erase: bool) -> None:
        if self.view is None:
            return
        self._remove_rect_item()
        self._rect_start_scene = scene_pt
        self._rect_erase = bool(erase)
        self._rect_item = QGraphicsRectItem(QRectF(scene_pt, scene_pt))
        pen = QPen(QColor(160, 160, 160), 1, Qt.PenStyle.DashLine)
        self._rect_item.setPen(pen)
        self._rect_item.setBrush(QBrush(Qt.BrushStyle.NoBrush))
        self._rect_item.setZValue(19_000)
        self.view.scene.addItem(self._rect_item)

    def _remove_rect_item(self) -> None:
        if self._rect_item is not None and self.view is not None:
            try:
                self.view.scene.removeItem(self._rect_item)
            except Exception:
                pass
        self._rect_item = None

    def _commit_rect(self, rect_scene: "QRectF") -> None:
        if self.view is None or rect_scene.isEmpty():
            return
        idx = self.view._hit_test_index(rect_scene.center())
        if idx is None:
            return
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
            chunk = QImage(r_overlay.size(), QImage.Format.Format_ARGB32_Premultiplied)
            chunk.fill(0)
        else:
            layer = self.view.overlay_image(idx)
            if layer is None or layer.isNull():
                return
            source = self._get_source_image(idx, layer.width())
            if source is None or source.isNull():
                return
            chunk = QImage(r_overlay.size(), QImage.Format.Format_ARGB32_Premultiplied)
            chunk.fill(0)
            src_rect = QRect(r_overlay.x(), r_overlay.y() + self._y_offset, r_overlay.width(), r_overlay.height())
            p = QPainter(chunk)
            p.drawImage(0, 0, source, src_rect.x(), src_rect.y(), src_rect.width(), src_rect.height())
            p.end()
        self.view.paste_chunk_to_overlay(idx, rect_scene, chunk)
        if undo_started:
            try:
                self.view._commit_undo_capture(idx)
            except Exception:
                pass

    # ------------- source handling -------------
    def _refresh_source_list(self) -> None:
        if self.view is None or not hasattr(self, "_source_box"):
            return
        base_dir = getattr(self.view.project, "alt_vers_dir", "")
        folders = []
        if base_dir and os.path.isdir(base_dir):
            for name in os.listdir(base_dir):
                full = os.path.join(base_dir, name)
                if os.path.isdir(full):
                    folders.append(name)
        folders.sort(key=lambda s: s.lower())
        self._source_box.blockSignals(True)
        self._source_box.clear()
        self._source_box.addItem("—", userData=None)
        for name in folders:
            self._source_box.addItem(name, userData=name)
        self._source_box.blockSignals(False)
        self._set_source_subdir(folders[0] if folders else None)
        if folders:
            self._source_box.setCurrentIndex(1)

    def _set_source_subdir(self, name: Optional[str]) -> None:
        self._source_subdir = name or ""
        self._source_paths = self._load_source_paths()
        self._source_cache_idx = None
        self._source_cache_img = None
        self._refresh_cursor_from_last()

    def _load_source_paths(self) -> List[str]:
        base_dir = getattr(self.view.project, "alt_vers_dir", "") if self.view else ""
        if not base_dir or not self._source_subdir:
            return []
        src_dir = os.path.join(base_dir, self._source_subdir)
        if not os.path.isdir(src_dir):
            return []
        entries = []
        for fn in os.listdir(src_dir):
            ext = os.path.splitext(fn)[1].lower()
            if ext in (".png", ".jpg", ".jpeg"):
                entries.append(os.path.join(src_dir, fn))
        entries.sort(key=self._numeric_first_key)
        return entries

    def _numeric_first_key(self, path: str):
        base = os.path.basename(path)
        stem, ext = os.path.splitext(base)
        ext = ext.lower().lstrip(".")
        ext_weight = 0 if ext == "png" else (1 if ext in ("jpg", "jpeg") else 2)
        if stem.isdigit():
            return (0, int(stem), ext_weight, base.lower())
        return (1, stem.lower(), ext_weight, base.lower())

    def _get_source_image(self, idx: int, overlay_width: int) -> Optional[QImage]:
        if idx < 0 or idx >= len(self._source_paths):
            return None
        if self._source_cache_idx == idx and self._source_cache_img is not None:
            if overlay_width <= 0 or self._source_cache_img.width() == overlay_width:
                return self._source_cache_img
            return None
        path = self._source_paths[idx]
        img = QImage(path)
        if img.isNull():
            return None
        if overlay_width > 0 and img.width() != overlay_width:
            return None
        self._source_cache_idx = idx
        self._source_cache_img = img
        return img

    # ------------- stamp building -------------
    def _build_stamp_image(self, source: QImage, cx: int, cy: int, radius: int) -> Optional[QImage]:
        if source is None or source.isNull() or radius <= 0:
            return None
        size = radius * 2
        img = QImage(size, size, QImage.Format.Format_ARGB32_Premultiplied)
        img.fill(0)
        src_cy = cy + self._y_offset
        src_rect = QRect(cx - radius, src_cy - radius, size, size)
        p = QPainter(img)
        p.setRenderHints(QPainter.RenderHint.Antialiasing | QPainter.RenderHint.SmoothPixmapTransform)
        path = QPainterPath()
        path.addEllipse(0, 0, size, size)
        p.setClipPath(path)
        p.drawImage(0, 0, source, src_rect.x(), src_rect.y(), src_rect.width(), src_rect.height())
        p.end()
        return img
