from __future__ import annotations

import os
import shutil
from dataclasses import dataclass
from typing import Dict, List, Optional, Tuple

import cv2
import numpy as np
from PyQt6.QtCore import QPointF, QRect, Qt
from PyQt6.QtGui import (
    QColor,
    QImage,
    QKeySequence,
    QPainter,
    QPen,
    QPixmap,
    QPolygonF,
    QShortcut,
    QTransform,
)
from PyQt6.QtWidgets import (
    QApplication,
    QMessageBox,
    QWidget,
    QGraphicsItemGroup,
    QGraphicsPixmapItem,
    QGraphicsPolygonItem,
)

from ui_new.canvas_view import CanvasView

MAX_UNDO_PATCHES = 10


@dataclass
class _UndoPatch:
    rect: QRect
    before: QImage
    after: QImage


class DrawingCanvasView(CanvasView):
    """
    Рисование поверх прозрачных оверлеев (слои в CleanOverlaysModel).
    Показ/синхронизация слоёв делает базовый CanvasView; здесь только ввод/undo/сохранение.
    """

    def __init__(
        self,
        project,
        images: List[str],
        parent: Optional[QWidget] = None,
        bubbles_model=None,
        overlays_model=None,
        text_detection_model=None,
        user_config=None,
    ):
        # editable=False — чтобы не создавать/править пузыри хоткеем T
        super().__init__(
            project,
            images=images,
            editable=False,
            parent=parent,
            bubbles_model=bubbles_model,
            overlays_model=overlays_model,
            user_config=user_config,
        )

        # === Параметры рисования ===
        self.brush_radius = 30
        self.brush_color = QColor(255, 0, 0, 255)
        self._is_painting = False
        self._erasing_mode = False
        self._last_scene_pt: Optional[QPointF] = None
        self.current_tool = "brush"
        self._tool_enabled = True

        # текстовый детектор: отображение масок/линий/блоков
        self._textdetector_results: Dict[int, dict] = {}
        self._textdetector_groups: List[Optional[QGraphicsItemGroup]] = []
        self._textdetector_group_sizes: List[Optional[Tuple[int, int]]] = []
        self._textdetector_mask_alpha: int = 90
        self._textdetector_draw_lines: bool = True
        self._textdetector_draw_blocks: bool = True
        self._textdetector_draw_mask: bool = True
        self._textdetector_visible: bool = False
        self._textdetector_block_expand_px: int = 0
        self._textdetector_merge_gap_px: int = 5
        self._textdetector_merge_nearby: bool = True
        self._textdet_model = None
        self._attach_textdet_model(text_detection_model)

        # Undo-стеки: храним патчи для каждой страницы
        n = len(self.images)
        self._undo_stacks: List[list[_UndoPatch]] = [[] for _ in range(n)]
        self._redo_stacks: List[list[_UndoPatch]] = [[] for _ in range(n)]
        self._pending_undo_before: List[Optional[QImage]] = [None for _ in range(n)]
        self._painting_idx: Optional[int] = None
        self._overlay_updates_locked = False

        self._disable_base_zoom_shortcuts()
        self._install_extra_shortcuts()

    def _reflow_after_resize(self):
        super()._reflow_after_resize()
        self._sync_textdetector_geom()

    # -------- отображение результатов детектора текста --------
    def _attach_textdet_model(self, model):
        self._textdet_model = model
        if model is None:
            return
        try:
            model.resultChanged.connect(self._on_textdet_model_changed)
            model.cleared.connect(self._on_textdet_model_cleared)
            model.reset.connect(self._on_textdet_model_reset)
            model.optionsChanged.connect(
                lambda _: self._sync_options_from_model()
                or self._rebuild_textdetector_items()
                or self._sync_textdetector_geom()
            )
        except Exception:
            pass
        self._on_textdet_model_reset()

    def _on_textdet_model_changed(self, idx: int):
        if self._textdet_model is None:
            return
        res = self._textdet_model.get(idx)
        if res is None:
            self._textdetector_results.pop(idx, None)
        else:
            self._textdetector_results[idx] = res
        self._sync_options_from_model()
        self._rebuild_textdetector_items()
        self._sync_textdetector_geom()

    def _on_textdet_model_cleared(self, idx: int):
        self._textdetector_results.pop(idx, None)
        self._rebuild_textdetector_items()
        self._sync_textdetector_geom()

    def _on_textdet_model_reset(self):
        if self._textdet_model is None:
            return
        self._textdetector_results = self._textdet_model.as_dict()
        self._sync_options_from_model()
        self._rebuild_textdetector_items()
        self._sync_textdetector_geom()

    def _sync_options_from_model(self):
        if self._textdet_model is None:
            return
        try:
            opts = self._textdet_model.get_options()
            self._textdetector_block_expand_px = int(opts.get("block_expand_px", 0))
            self._textdetector_merge_gap_px = int(opts.get("merge_gap_px", 0))
            self._textdetector_merge_nearby = bool(opts.get("merge_nearby", False))
        except Exception:
            pass

    def _clear_textdetector_items(self):
        for grp in self._textdetector_groups:
            if grp is not None:
                try:
                    self.scene.removeItem(grp)
                except Exception:
                    pass
        self._textdetector_groups = []
        self._textdetector_group_sizes = []

    def _rebuild_textdetector_items(self):
        self._clear_textdetector_items()
        if not self._textdetector_visible:
            return
        if not self._textdetector_results:
            return

        total = len(self.images)
        self._textdetector_groups = [None] * total
        self._textdetector_group_sizes = [None] * total

        for raw_idx, data in self._textdetector_results.items():
            try:
                idx = int(raw_idx)
            except Exception:
                continue
            if not (0 <= idx < total):
                continue

            mask = data.get("mask") if isinstance(data, dict) else None
            blocks = data.get("blocks") if isinstance(data, dict) else None
            base_size = None
            if isinstance(data, dict):
                sz = data.get("size")
                if isinstance(sz, tuple) or isinstance(sz, list):
                    if len(sz) == 2:
                        base_size = (int(sz[0]), int(sz[1]))
            if base_size is None:
                base_size = self._size_from_mask(mask)

            grp = QGraphicsItemGroup()
            try:
                grp.setHandlesChildEvents(False)
            except AttributeError:
                try:
                    grp.setFiltersChildEvents(True)
                except Exception:
                    pass
            grp.setAcceptedMouseButtons(Qt.MouseButton.NoButton)
            grp.setZValue(150.0)
            self.scene.addItem(grp)
            self._textdetector_groups[idx] = grp
            self._textdetector_group_sizes[idx] = base_size

            qmask = self._mask_to_qimage(mask, self._textdetector_mask_alpha, dilate_px=0)
            if qmask is not None and self._textdetector_draw_mask:
                mask_item = QGraphicsPixmapItem(QPixmap.fromImage(qmask), grp)
                mask_item.setOpacity(0.35)
                mask_item.setAcceptedMouseButtons(Qt.MouseButton.NoButton)
                mask_item.setZValue(151.0)

            for poly_item in self._make_block_polygons(blocks):
                poly_item.setParentItem(grp)

        self._sync_textdetector_geom()

    def _apply_textdetector_geom(self, idx: int):
        if not (0 <= idx < len(self._textdetector_groups)):
            return
        grp = self._textdetector_groups[idx]
        if grp is None or idx >= len(self.image_bboxes):
            return
        size = self._textdetector_group_sizes[idx] if idx < len(self._textdetector_group_sizes) else None
        if not size or len(size) != 2:
            return
        bw, bh = size
        bbox = self.image_bboxes[idx]
        grp.setPos(bbox.left(), bbox.top())
        if bw <= 0 or bh <= 0 or bbox.width() <= 0 or bbox.height() <= 0:
            grp.setTransform(QTransform())
            return
        sx = bbox.width() / bw
        sy = bbox.height() / bh
        grp.setTransform(QTransform().scale(sx, sy))

    def _sync_textdetector_geom(self):
        if not self._textdetector_groups:
            return
        for i in range(min(len(self._textdetector_groups), len(self.image_bboxes))):
            self._apply_textdetector_geom(i)

    def _mask_to_qimage(self, mask, alpha: int, *, dilate_px: int = 0):
        if mask is None:
            return None
        arr = np.asarray(mask)
        if arr.size == 0:
            return None
        if arr.ndim == 3:
            arr = arr[..., 0]
        if dilate_px and dilate_px > 0:
            try:
                kernel = cv2.getStructuringElement(
                    cv2.MORPH_ELLIPSE,
                    (max(1, 2 * dilate_px + 1),) * 2,
                )
                arr = cv2.dilate(arr, kernel)
            except Exception:
                pass
        overlay = np.zeros((arr.shape[0], arr.shape[1], 4), dtype=np.uint8)
        overlay[..., 0] = 255
        overlay[..., 3] = (arr > 0).astype(np.uint8) * np.clip(alpha, 0, 255)
        h, w, _ = overlay.shape
        return QImage(overlay.data, w, h, 4 * w, QImage.Format.Format_RGBA8888).copy()

    def _size_from_mask(self, mask) -> Optional[Tuple[int, int]]:
        if mask is None:
            return None
        arr = np.asarray(mask)
        if arr.ndim == 3:
            arr = arr[..., 0]
        if arr.ndim != 2 or arr.size == 0:
            return None
        h, w = arr.shape[:2]
        return (w, h)

    def _make_block_polygons(self, blocks) -> List[QGraphicsPolygonItem]:
        items: List[QGraphicsPolygonItem] = []
        if not blocks:
            return items

        def merge_rects(rects, gap: float = 4.0):
            merged = []
            for r in rects:
                x1, y1, x2, y2 = r
                merged_rect = [x1, y1, x2, y2]
                i = 0
                while i < len(merged):
                    mx1, my1, mx2, my2 = merged[i]
                    if not (x2 + gap < mx1 or x1 - gap > mx2 or y2 + gap < my1 or y1 - gap > my2):
                        merged_rect = [
                            min(merged_rect[0], mx1),
                            min(merged_rect[1], my1),
                            max(merged_rect[2], mx2),
                            max(merged_rect[3], my2),
                        ]
                        merged.pop(i)
                        x1, y1, x2, y2 = merged_rect
                        continue
                    i += 1
                merged.append(merged_rect)
            return merged

        pen = QPen(QColor(0, 255, 0))
        pen.setWidth(2)
        rect_pen = QPen(QColor(0, 160, 255))
        rect_pen.setWidth(3)
        rect_pen.setStyle(Qt.PenStyle.DashLine)
        rects = []
        for blk in blocks:
            all_pts: List[Tuple[float, float]] = []
            line_list = getattr(blk, "lines", None) or []
            if line_list:
                for line in line_list:
                    try:
                        poly = QPolygonF([QPointF(float(x), float(y)) for x, y in line])
                    except Exception:
                        continue
                    if self._textdetector_draw_lines:
                        it = QGraphicsPolygonItem(poly)
                        it.setBrush(QColor(0, 0, 0, 0))
                        it.setPen(pen)
                        it.setAcceptedMouseButtons(Qt.MouseButton.NoButton)
                        it.setZValue(152.0)
                        items.append(it)
                    all_pts.extend([(float(x), float(y)) for x, y in line])
            xyxy = getattr(blk, "xyxy", None)
            if xyxy and len(xyxy) == 4:
                try:
                    x1, y1, x2, y2 = [float(v) for v in xyxy]
                except Exception:
                    continue
                poly = QPolygonF(
                    [
                        QPointF(x1, y1),
                        QPointF(x2, y1),
                        QPointF(x2, y2),
                        QPointF(x1, y2),
                    ]
                )
                if self._textdetector_draw_lines:
                    it = QGraphicsPolygonItem(poly)
                    it.setBrush(QColor(0, 0, 0, 0))
                    it.setPen(pen)
                    it.setAcceptedMouseButtons(Qt.MouseButton.NoButton)
                    it.setZValue(152.0)
                    items.append(it)
                all_pts.extend([(x1, y1), (x2, y2)])
            if all_pts:
                xs = [p[0] for p in all_pts]
                ys = [p[1] for p in all_pts]
                ax1, ax2 = min(xs), max(xs)
                ay1, ay2 = min(ys), max(ys)
                if ax2 <= ax1 or ay2 <= ay1:
                    continue
                if self._textdetector_block_expand_px > 0:
                    exp = float(self._textdetector_block_expand_px)
                    ax1 -= exp
                    ay1 -= exp
                    ax2 += exp
                    ay2 += exp
                rects.append((ax1, ay1, ax2, ay2))

        gap = float(self._textdetector_merge_gap_px if self._textdetector_merge_nearby else 0)
        if self._textdetector_draw_blocks:
            merged_rects = merge_rects(rects, gap=gap)
            for ax1, ay1, ax2, ay2 in merged_rects:
                rect_poly = QPolygonF(
                    [
                        QPointF(ax1, ay1),
                        QPointF(ax2, ay1),
                        QPointF(ax2, ay2),
                        QPointF(ax1, ay2),
                    ]
                )
                rect_item = QGraphicsPolygonItem(rect_poly)
                rect_item.setBrush(QColor(0, 0, 0, 0))
                rect_item.setPen(rect_pen)
                rect_item.setAcceptedMouseButtons(Qt.MouseButton.NoButton)
                rect_item.setZValue(153.0)
                items.append(rect_item)
        return items

    def set_textdetector_visibility(
        self,
        visible: bool,
        *,
        show_mask: Optional[bool] = None,
        show_lines: Optional[bool] = None,
        show_blocks: Optional[bool] = None,
    ) -> None:
        self._textdetector_visible = bool(visible)
        if show_mask is not None:
            self._textdetector_draw_mask = bool(show_mask)
        if show_lines is not None:
            self._textdetector_draw_lines = bool(show_lines)
        if show_blocks is not None:
            self._textdetector_draw_blocks = bool(show_blocks)
        self._rebuild_textdetector_items()

    def set_textdetector_mask_visible(self, visible: bool) -> None:
        if self._textdetector_draw_mask == bool(visible):
            return
        self._textdetector_draw_mask = bool(visible)
        if self._textdetector_visible:
            self._rebuild_textdetector_items()

    def set_textdetector_lines_visible(self, visible: bool) -> None:
        if self._textdetector_draw_lines == bool(visible):
            return
        self._textdetector_draw_lines = bool(visible)
        if self._textdetector_visible:
            self._rebuild_textdetector_items()

    def set_textdetector_blocks_visible(self, visible: bool) -> None:
        if self._textdetector_draw_blocks == bool(visible):
            return
        self._textdetector_draw_blocks = bool(visible)
        if self._textdetector_visible:
            self._rebuild_textdetector_items()

    def _disable_base_zoom_shortcuts(self) -> None:
        zoom_keys = [
            QKeySequence("Ctrl++"),
            QKeySequence("Ctrl+="),
            QKeySequence("Ctrl+-"),
            QKeySequence("Ctrl+0"),
        ]
        for action in self.actions():
            try:
                shots = (
                    action.shortcuts()
                    if hasattr(action, "shortcuts") and callable(action.shortcuts)
                    else [action.shortcut()]
                )
                for sc in shots:
                    if any(
                        sc.matches(zk) == QKeySequence.SequenceMatch.ExactMatch for zk in zoom_keys
                    ):
                        self.removeAction(action)
                        break
            except Exception:
                pass

    def _install_extra_shortcuts(self) -> None:
        self._sc_undo = QShortcut(QKeySequence("Ctrl+Z"), self)
        self._sc_undo.setContext(Qt.ShortcutContext.WidgetWithChildrenShortcut)
        self._sc_undo.activated.connect(self.undo_current_page)
        self._sc_redo = QShortcut(QKeySequence("Ctrl+Shift+Z"), self)
        self._sc_redo.setContext(Qt.ShortcutContext.WidgetWithChildrenShortcut)
        self._sc_redo.activated.connect(self.redo_current_page)
        self._sc_save = QShortcut(QKeySequence.StandardKey.Save, self)
        self._sc_save.setContext(Qt.ShortcutContext.WidgetWithChildrenShortcut)
        self._sc_save.activated.connect(self._on_quick_save)

        # Переназначим T: вместо создания пузыря — показать/скрыть пузыри
        key_t_str = QKeySequence(Qt.Key.Key_T).toString()
        for act in self.actions():
            try:
                seqs = (
                    [s.toString() for s in act.shortcuts()]
                    if hasattr(act, "shortcuts") and callable(act.shortcuts)
                    else [act.shortcut().toString()]
                )
            except Exception:
                continue
            if key_t_str in seqs:
                try:
                    act.triggered.disconnect(self._on_add_bubble_shortcut)
                except Exception:
                    pass
                act.triggered.connect(self.toggle_bubbles)
                break

    def _on_quick_save(self) -> None:
        self.save_all_to_cleaned_dir()
        try:
            QMessageBox.information(self, "Готово", "Слои сохранены в папку clean_layers.")
        except Exception:
            pass

    # ------------------------ рисование ------------------------
    def mousePressEvent(self, e):
        if e.button() == Qt.MouseButton.LeftButton and getattr(self, "_tool_enabled", True):
            scene_pt = self.mapToScene(e.pos())
            idx = self._hit_test_index(scene_pt)
            if idx is not None and self.overlays_model is not None:
                if not self._overlay_updates_locked:
                    self.overlays_model.lock_updates()
                    self._overlay_updates_locked = True
                self._is_painting = True
                self._painting_idx = idx
                shift_erase = bool(e.modifiers() & Qt.KeyboardModifier.ShiftModifier)
                self._erasing_mode = shift_erase or getattr(self, "current_tool", "brush") == "eraser"
                self._last_scene_pt = scene_pt
                self._begin_undo_capture(idx)
                # первый мазок
                self.paint_overlay_segment(
                    idx,
                    scene_pt,
                    scene_pt,
                    color=self.brush_color,
                    radius=self.brush_radius,
                    erase=self._erasing_mode,
                )
                e.accept()
                return
        super().mousePressEvent(e)

    def mouseMoveEvent(self, e):
        if self._is_painting and self._last_scene_pt is not None and self.overlays_model is not None:
            scene_pt = self.mapToScene(e.pos())
            idx = self._hit_test_index(scene_pt)
            if idx is not None:
                self.paint_overlay_segment(
                    idx,
                    self._last_scene_pt,
                    scene_pt,
                    color=self.brush_color,
                    radius=self.brush_radius,
                    erase=self._erasing_mode,
                )
                self._last_scene_pt = scene_pt
                e.accept()
                return
        super().mouseMoveEvent(e)

    def mouseReleaseEvent(self, e):
        if e.button() == Qt.MouseButton.LeftButton and self._is_painting:
            self._is_painting = False
            self._last_scene_pt = None
            if self._painting_idx is not None:
                self._commit_undo_capture(self._painting_idx)
                if self.overlays_model and self._overlay_updates_locked:
                    self.overlays_model.unlock_updates()
                    self._overlay_updates_locked = False
                    self.overlays_model.overlayReplaced.emit(self._painting_idx)
                self._painting_idx = None
            e.accept()
            return
        super().mouseReleaseEvent(e)

    def _hit_test_index(self, scene_pt: QPointF) -> Optional[int]:
        for i, r in enumerate(self.image_bboxes):
            if r.contains(scene_pt):
                return i
        return None

    # ------------------------ undo ------------------------
    def _begin_undo_capture(self, idx: int) -> None:
        if not self.overlays_model or not (0 <= idx < len(self._pending_undo_before)):
            return
        if self._pending_undo_before[idx] is not None:
            return
        ov = self.overlays_model.get(idx)
        if ov is not None and not ov.isNull():
            self._pending_undo_before[idx] = ov.copy()

    def _commit_undo_capture(self, idx: int) -> None:
        if not self.overlays_model or not (0 <= idx < len(self._pending_undo_before)):
            return
        before = self._pending_undo_before[idx]
        self._pending_undo_before[idx] = None
        if before is None or before.isNull():
            return
        after = self.overlays_model.get(idx)
        patch = self._build_undo_patch(before, after)
        if patch is None:
            return
        # Новое действие инвалидирует redo для текущей страницы.
        self._redo_stacks[idx].clear()
        self._undo_stacks[idx].append(patch)
        if len(self._undo_stacks[idx]) > MAX_UNDO_PATCHES:
            self._undo_stacks[idx].pop(0)

    def _qimage_to_array(self, img: QImage) -> Tuple[np.ndarray, QImage]:
        if img.format() not in (
            QImage.Format.Format_ARGB32,
            QImage.Format.Format_ARGB32_Premultiplied,
        ):
            img = img.convertToFormat(QImage.Format.Format_ARGB32_Premultiplied)
        ptr = img.bits()
        ptr.setsize(img.bytesPerLine() * img.height())
        arr = np.frombuffer(ptr, np.uint8).reshape((img.height(), img.bytesPerLine() // 4, 4))
        return arr[:, : img.width(), :], img

    def _build_undo_patch(self, before: QImage, after: Optional[QImage]) -> Optional[_UndoPatch]:
        if after is None or after.isNull() or before.isNull():
            return None
        if before.size() != after.size():
            return None
        arr_before, before_img = self._qimage_to_array(before)
        arr_after, _ = self._qimage_to_array(after)
        diff = np.any(arr_before != arr_after, axis=2)
        ys, xs = np.where(diff)
        if ys.size == 0:
            return None
        x0, x1 = int(xs.min()), int(xs.max())
        y0, y1 = int(ys.min()), int(ys.max())
        rect = QRect(x0, y0, x1 - x0 + 1, y1 - y0 + 1)
        patch_before = before_img.copy(rect)
        patch_after = after.copy(rect)
        return _UndoPatch(rect=rect, before=patch_before, after=patch_after)

    def _apply_patch_image(self, idx: int, rect: QRect, patch_img: QImage) -> None:
        if not self.overlays_model:
            return
        current = self.overlays_model.get(idx)
        if current is None or current.isNull() or patch_img is None or patch_img.isNull():
            return
        layer = current.copy()
        p = QPainter(layer)
        p.setCompositionMode(QPainter.CompositionMode.CompositionMode_Source)
        p.drawImage(rect.topLeft(), patch_img)
        p.end()
        self.overlays_model.replace(idx, layer)

    def undo_current_page(self):
        idx = self._current_page_idx()
        if not (0 <= idx < len(self.images)) or not self.overlays_model:
            return
        if self._undo_stacks[idx]:
            patch = self._undo_stacks[idx].pop()
            self._apply_patch_image(idx, patch.rect, patch.before)
            self._redo_stacks[idx].append(patch)
            if len(self._redo_stacks[idx]) > MAX_UNDO_PATCHES:
                self._redo_stacks[idx].pop(0)

    def redo_current_page(self):
        idx = self._current_page_idx()
        if not (0 <= idx < len(self.images)) or not self.overlays_model:
            return
        if self._redo_stacks[idx]:
            patch = self._redo_stacks[idx].pop()
            self._apply_patch_image(idx, patch.rect, patch.after)
            self._undo_stacks[idx].append(patch)
            if len(self._undo_stacks[idx]) > MAX_UNDO_PATCHES:
                self._undo_stacks[idx].pop(0)

    # ------------------------ API инструментов ------------------------
    def set_tool(self, tool: Optional[str]) -> None:
        t = (tool or "").lower()
        if t in ("brush", "eraser"):
            self.current_tool = t
            self._tool_enabled = True
            self._erasing_mode = t == "eraser"
        else:
            self.current_tool = "none"
            self._tool_enabled = False
            self._is_painting = False
            self._last_scene_pt = None
            self._erasing_mode = False

    def set_brush_radius(self, r: int) -> None:
        self.brush_radius = max(1, int(r))

    def set_brush_color(self, color) -> None:
        if color is not None:
            self.brush_color = color

    def set_brush_opacity(self, alpha: int) -> None:
        a = max(0, min(255, int(alpha)))
        c = self.brush_color
        c.setAlpha(a)
        self.brush_color = c

    def clear_current_overlay(self) -> None:
        idx = self._current_page_idx()
        if self.overlays_model and 0 <= idx < len(self.images):
            self._begin_undo_capture(idx)
            self.clear_overlay_index(idx)
            self._commit_undo_capture(idx)

    def set_overlay_visible(self, visible: bool) -> None:
        self.set_clean_overlays_visible(visible)

    # ------------------------ сохранение ------------------------
    def save_all_to_cleaned_dir(self) -> None:
        """
        Сохраняет ТОЛЬКО слои в project.clean_layers_dir (без изменения cleaned_dir).
        """
        if self.overlays_model:
            self.overlays_model.save_all()

    # ------------------------ служебное ------------------------
    def restore_current_page_from_src(self) -> None:
        """
        Копирует исходник поверх файла в cleaned_dir. Слой не удаляем, но он может стать несоответствующего размера —
        пользователь при рисовании увидит «подгон» через трансформацию item (это ок).
        """
        idx = self._current_page_idx()
        if not (0 <= idx < len(self.images)):
            QMessageBox.critical(self, "Ошибка", "Текущая страница не определена.")
            return
        cleaned_path = self.images[idx]
        basename = os.path.basename(cleaned_path)
        src_dir = getattr(self.project, "src_dir", None)
        if not src_dir:
            QMessageBox.critical(self, "Не найден оригинал", "В проекте не указан src_dir.")
            return
        original_path = os.path.join(src_dir, basename)
        if not os.path.exists(original_path):
            QMessageBox.critical(self, "Не найден оригинал", f"Файл не найден:\n{original_path}")
            return
        # сохраним undo для слоя, но сам слой не трогаем (его карта соответствия сцены останется корректной)
        if self.overlays_model:
            self._begin_undo_capture(idx)
        shutil.copy2(original_path, cleaned_path)
        # обновим базовые картинки во view
        self._display_images()
        self._sync_all_overlays_geom()  # геометрия айтемов привязана к bbox
        if self.overlays_model:
            self._commit_undo_capture(idx)

    def finish_interaction(self) -> None:
        self._is_painting = False
        self._last_scene_pt = None
        self._erasing_mode = getattr(self, "current_tool", "brush") == "eraser"
        if self._painting_idx is not None:
            self._commit_undo_capture(self._painting_idx)
            if self.overlays_model and self._overlay_updates_locked:
                self.overlays_model.unlock_updates()
                self._overlay_updates_locked = False
                self.overlays_model.overlayReplaced.emit(self._painting_idx)
            self._painting_idx = None

    def teardown_shortcuts(self) -> None:
        for sc in ("_sc_undo", "_sc_redo", "_sc_save"):
            obj = getattr(self, sc, None)
            if obj is not None:
                try:
                    obj.activated.disconnect()
                except Exception:
                    pass
                obj.setParent(None)
                obj.deleteLater()
                setattr(self, sc, None)

    def set_tool_by_id(self, tool_id: str) -> None:
        """
        Совместимость с CleaningTab: запоминаем id выбранного инструмента.
        Логику выбора actual-инструмента управляет сама вкладка.
        """
        self.current_tool_id = (tool_id or "").lower()
