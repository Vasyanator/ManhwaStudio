from __future__ import annotations

import cv2
import numpy as np
from dataclasses import dataclass
from typing import Dict, Iterable, List, Optional, Tuple

from PyQt6.QtCore import Qt, pyqtSignal
from PyQt6.QtGui import QImage
from PyQt6.QtWidgets import QCheckBox, QComboBox, QDoubleSpinBox, QFrame, QHBoxLayout, QLabel, QMessageBox, QPushButton, QVBoxLayout, QWidget
from modules.utils_qt import qimage_to_numpy_rgba


class FastCleanPanel(QFrame):
    """
    Плавающая панель быстрого клина. Управляет видимостью маски/контуров детектора.
    По стилю повторяет панели из Translation, но прижимается к левой стороне canvas.
    """

    visibilityChanged = pyqtSignal(bool)
    maskVisibilityChanged = pyqtSignal(bool)
    linesVisibilityChanged = pyqtSignal(bool)
    blocksVisibilityChanged = pyqtSignal(bool)
    applyRequested = pyqtSignal(str)
    applyAllRequested = pyqtSignal(str)
    closed = pyqtSignal()
    uniformityToleranceChanged = pyqtSignal(float)

    def __init__(self, parent: QWidget | None = None):
        super().__init__(parent)
        # не даём панели схлопнуться в 0px (как на скрине)
        self.setMinimumWidth(420)
        self.setMinimumHeight(800)
        self.setFrameShape(QFrame.Shape.StyledPanel)
        self.setAttribute(Qt.WidgetAttribute.WA_StyledBackground, True)
        self.setWindowFlag(Qt.WindowType.SubWindow, False)
        self.setObjectName("fastCleanPanel")
        self.setStyleSheet(
            """
            QFrame#fastCleanPanel {
                background: #202020;
                border: 1px solid #444;
                color: #ddd;
            }
            QFrame#fastCleanPanel QLabel { color: #ddd; }
            QFrame#fastCleanPanel QCheckBox {
                color: #ddd;
                background: transparent;
            }
            QFrame#fastCleanPanel QPushButton {
                background: #2b2b2b;
                color: #eee;
                border: 1px solid #555;
                padding: 4px 8px;
            }
            QFrame#fastCleanPanel QPushButton:hover { background: #333; }
            """
        )

        layout = QVBoxLayout(self)
        layout.setContentsMargins(10, 8, 10, 10)
        layout.setSpacing(10)

        header = QHBoxLayout()
        header.setContentsMargins(0, 0, 0, 0)
        header.setSpacing(8)
        title = QLabel("Быстрый клин", self)
        title.setStyleSheet("font-weight:700;color:#fff;")
        close_btn = QPushButton("✕", self)
        close_btn.setFixedWidth(26)
        close_btn.setToolTip("Закрыть панель")
        close_btn.clicked.connect(self._on_close_clicked)
        header.addWidget(title)
        header.addStretch(1)
        header.addWidget(close_btn)
        layout.addLayout(header)

        info = QLabel(
            "Быстрый просмотр результатов детектора текста.\n"
            "Оверлеи прижаты слева, чтобы не закрывать рабочую область."
        )
        info.setStyleSheet("color:#888;font-size:11px;")
        layout.addWidget(info)

        self._detector_hint = QLabel("Сначала выполните детект текста во вкладке перевода", self)
        self._detector_hint.setStyleSheet("color:#e55;font-weight:600;")
        self._detector_hint.setVisible(False)
        layout.addWidget(self._detector_hint)

        controls_box = QFrame(self)
        controls_box.setFrameShape(QFrame.Shape.StyledPanel)
        controls_box.setStyleSheet("QFrame { border: 1px solid #333; }")
        controls_layout = QVBoxLayout(controls_box)
        controls_layout.setContentsMargins(8, 6, 8, 6)
        controls_layout.setSpacing(6)

        controls_title = QLabel("Отображение детектора", self)
        controls_title.setStyleSheet("font-weight:600;")
        controls_layout.addWidget(controls_title)

        self._mask_chk = QCheckBox("Показывать маску", self)
        self._mask_chk.setToolTip("Полупрозрачная маска над найденным текстом")
        self._mask_chk.setChecked(True)
        self._mask_chk.toggled.connect(self.maskVisibilityChanged)
        controls_layout.addWidget(self._mask_chk)

        self._lines_chk = QCheckBox("Показывать строки (зелёные)", self)
        self._lines_chk.setToolTip("Контуры обнаруженных строк")
        self._lines_chk.setChecked(True)
        self._lines_chk.toggled.connect(self.linesVisibilityChanged)
        controls_layout.addWidget(self._lines_chk)

        self._blocks_chk = QCheckBox("Показывать блоки (синие)", self)
        self._blocks_chk.setToolTip("Группы строк, удобно для крупных пузырей")
        self._blocks_chk.setChecked(True)
        self._blocks_chk.toggled.connect(self.blocksVisibilityChanged)
        controls_layout.addWidget(self._blocks_chk)

        controls_box.setContentsMargins(0, 0, 0, 0)
        layout.addWidget(controls_box)

        # Источник маски для авто-замазки
        source_box = QFrame(self)
        source_box.setFrameShape(QFrame.Shape.StyledPanel)
        source_box.setStyleSheet("QFrame { border: 1px solid #333; }")
        src_layout = QVBoxLayout(source_box)
        src_layout.setContentsMargins(8, 6, 8, 6)
        src_layout.setSpacing(6)

        src_title = QLabel("Быстрая замазка", self)
        src_title.setStyleSheet("font-weight:600;")
        src_layout.addWidget(src_title)

        src_row = QHBoxLayout()
        src_row.setContentsMargins(0, 0, 0, 0)
        src_row.setSpacing(6)
        src_row.addWidget(QLabel("Источник:", self))
        self._source_combo = QComboBox(self)
        self._source_combo.addItem("Маска", userData="mask")
        self._source_combo.addItem("Линии", userData="lines")
        self._source_combo.addItem("Блоки", userData="blocks")
        src_row.addWidget(self._source_combo, 1)
        src_layout.addLayout(src_row)

        tol_row = QHBoxLayout()
        tol_row.setContentsMargins(0, 0, 0, 0)
        tol_row.setSpacing(6)
        tol_row.addWidget(QLabel("Допуск однородности:", self))
        self._uniformity_spin = QDoubleSpinBox(self)
        self._uniformity_spin.setRange(1.0, 80.0)
        self._uniformity_spin.setDecimals(1)
        self._uniformity_spin.setSingleStep(0.5)
        self._uniformity_spin.setToolTip("Максимально допустимое σ яркости по границе маски. Уменьшите, если замазываются неоднородные области.")
        self._uniformity_spin.setValue(1.0)
        self._uniformity_spin.valueChanged.connect(self._on_uniformity_changed)
        tol_row.addWidget(self._uniformity_spin, 1)
        src_layout.addLayout(tol_row)

        run_btn = QPushButton("Замазать текущую", self)
        run_btn.setToolTip("Заполнить найденный текст цветом границы страницы")
        run_btn.clicked.connect(self._emit_apply)
        src_layout.addWidget(run_btn)

        run_all_btn = QPushButton("Замазать все", self)
        run_all_btn.setToolTip("Пройти по всем страницам с результатами детектора")
        run_all_btn.clicked.connect(self._emit_apply_all)
        src_layout.addWidget(run_all_btn)

        source_box.setContentsMargins(0, 0, 0, 0)
        layout.addWidget(source_box)

        tip = QLabel("Можно временно свернуть панель кнопкой ✕.\n"
                     "Переключить — кнопкой «Быстрый клин» на тулбаре.")
        tip.setStyleSheet("color:#888;font-size:11px;")
        layout.addWidget(tip)

        layout.addStretch(1)
        self.hide()

    # API -----------------------------------------------------
    def set_panel_visible(self, visible: bool) -> None:
        if visible:
            if not self.isVisible():
                # на всякий случай подгоняем размер по sizeHint, чтобы влезли чекбоксы
                self.resize(self.sizeHint())
                self.show()
                self.raise_()
        else:
            if self.isVisible():
                self.hide()
        self.visibilityChanged.emit(self.isVisible())

    def set_mask_checked(self, checked: bool) -> None:
        if self._mask_chk.isChecked() != bool(checked):
            self._mask_chk.blockSignals(True)
            self._mask_chk.setChecked(bool(checked))
            self._mask_chk.blockSignals(False)

    def set_lines_checked(self, checked: bool) -> None:
        if self._lines_chk.isChecked() != bool(checked):
            self._lines_chk.blockSignals(True)
            self._lines_chk.setChecked(bool(checked))
            self._lines_chk.blockSignals(False)

    def set_blocks_checked(self, checked: bool) -> None:
        if self._blocks_chk.isChecked() != bool(checked):
            self._blocks_chk.blockSignals(True)
            self._blocks_chk.setChecked(bool(checked))
            self._blocks_chk.blockSignals(False)

    def mask_checked(self) -> bool:
        return self._mask_chk.isChecked()

    def lines_checked(self) -> bool:
        return self._lines_chk.isChecked()

    def blocks_checked(self) -> bool:
        return self._blocks_chk.isChecked()

    def selected_source(self) -> str:
        idx = self._source_combo.currentIndex()
        return str(self._source_combo.itemData(idx) or "mask")

    def set_textdetector_empty(self, empty: bool) -> None:
        self._detector_hint.setVisible(bool(empty))

    def set_uniformity_tolerance(self, value: float) -> None:
        try:
            v = float(value)
        except Exception:
            return
        if hasattr(self, "_uniformity_spin"):
            self._uniformity_spin.blockSignals(True)
            self._uniformity_spin.setValue(v)
            self._uniformity_spin.blockSignals(False)

    # Internal ------------------------------------------------
    def _on_close_clicked(self):
        self.hide()
        self.visibilityChanged.emit(False)
        self.closed.emit()

    def _emit_apply(self) -> None:
        src = self.selected_source()
        self.applyRequested.emit(src)

    def _emit_apply_all(self) -> None:
        src = self.selected_source()
        self.applyAllRequested.emit(src)

    def _on_uniformity_changed(self, value: float) -> None:
        self.uniformityToleranceChanged.emit(float(value))


# === Алгоритм быстрой замазки ===


@dataclass
class _EdgeParams:
    ring_thickness_px: int = 6
    sample_step_px: int = 2
    std_threshold: float = 8.0
    min_samples: int = 30


def _qimage_to_rgba_array(img: QImage) -> np.ndarray:
    """Безопасно конвертирует QImage в копию np.ndarray HxWx4 (RGBA)."""
    return qimage_to_numpy_rgba(img)


class FastCleanProcessor:
    """Собирает маску из результатов детектора и заливает clean_overlay однородным цветом границы."""

    def __init__(self, view):
        self.view = view
        self._edge_params = _EdgeParams()

    def uniformity_tolerance(self) -> float:
        return float(self._edge_params.std_threshold)

    def set_uniformity_tolerance(self, value: float) -> None:
        try:
            val = float(value)
        except Exception:
            return
        # clamp to sane range to avoid missing all components или заливки шума
        self._edge_params.std_threshold = max(0.5, min(val, 128.0))

    # --- публичное API ---
    def apply_current_page(self, source: str) -> Tuple[bool, str]:
        if self.view is None:
            return False, "Вид не готов"
        idx_fn = getattr(self.view, "_current_page_idx", None)
        try:
            idx = int(idx_fn()) if callable(idx_fn) else 0
        except Exception:
            idx = 0
        return self.apply_for_index(idx, source)

    def apply_for_index(self, idx: int, source: str) -> Tuple[bool, str]:
        ok, filled, total, msg = self._apply_single(idx, source)
        if not ok:
            return False, msg
        return True, f"Замазано областей: {filled} / {total}"

    def apply_all(self, source: str) -> Tuple[bool, str]:
        ov_model = getattr(self.view, "overlays_model", None)
        imgs = getattr(self.view, "images", []) or []
        total_pages = len(imgs)
        if ov_model is None:
            return False, "Модель clean_overlay недоступна"
        if total_pages <= 0:
            return False, "Нет страниц для обработки"
        applied = 0
        filled_sum = 0
        total_sum = 0
        for idx in range(total_pages):
            det = self._textdet_result(idx)
            if not det:
                continue
            ok, filled, total, _ = self._apply_single(idx, source)
            if ok:
                applied += 1
                filled_sum += filled
                total_sum += total
        if applied == 0:
            return False, "Нет подходящих страниц с результатами детектора"
        return True, f"Обработано страниц: {applied}, областей: {filled_sum} / {total_sum or filled_sum}"

    # --- вспомогательное ---
    def _textdet_result(self, idx: int) -> Optional[Dict]:
        try:
            data = getattr(self.view, "_textdetector_results", {})
            if isinstance(data, dict):
                return data.get(int(idx))
        except Exception:
            pass
        return None

    def _base_image(self, idx: int) -> Optional[QImage]:
        if not hasattr(self.view, "images"):
            return None
        images = getattr(self.view, "images", [])
        if not (0 <= idx < len(images)):
            return None
        try:
            qimg_fn = getattr(self.view, "_qimage_from", None)
            if callable(qimg_fn):
                return qimg_fn(images[idx])
        except Exception:
            pass
        img = QImage(images[idx])
        return img if not img.isNull() else None

    def _apply_single(self, idx: int, source: str) -> Tuple[bool, int, int, str]:
        ov_model = getattr(self.view, "overlays_model", None)
        if ov_model is None:
            return False, 0, 0, "Модель clean_overlay недоступна"
        ov = ov_model.get(idx)
        if ov is None or ov.isNull():
            return False, 0, 0, "Слой клина пустой или не найден"

        base_img = self._base_image(idx)
        if base_img is None or base_img.isNull():
            return False, 0, 0, "Не удалось загрузить базовое изображение"

        target_size = (base_img.width(), base_img.height())
        det = self._textdet_result(idx)
        if not det:
            return False, 0, 0, "Для страницы нет результатов детектора"

        mask = self._compose_mask(det, source, target_size)
        if mask is None or mask.size == 0 or mask.max() == 0:
            return False, 0, 0, "Не удалось построить маску для замазки"

        ok, filled, total, new_overlay = self._fill_components(mask, base_img, ov)
        if not ok or new_overlay is None:
            return False, filled, total, "Замазка не выполнена"
        try:
            self.view._begin_undo_capture(idx)
        except Exception:
            pass
        ov_model.replace(idx, new_overlay)
        try:
            self.view._commit_undo_capture(idx)
        except Exception:
            pass
        return True, filled, total, ""

    def _compose_mask(self, det: Dict, source: str, target_size: Tuple[int, int]) -> Optional[np.ndarray]:
        src = (source or "mask").lower()
        base_size = None
        if isinstance(det, dict):
            sz = det.get("size")
            if isinstance(sz, (tuple, list)) and len(sz) == 2:
                base_size = (int(sz[0]), int(sz[1]))
        if src == "mask":
            mask = det.get("mask") if isinstance(det, dict) else None
            return self._normalize_mask(mask, target_size)

        blocks = det.get("blocks") if isinstance(det, dict) else None
        if src == "lines":
            return self._mask_from_lines(blocks, base_size, target_size)
        if src == "blocks":
            return self._mask_from_blocks(blocks, base_size, target_size)
        return None

    def _normalize_mask(self, mask, target_size: Tuple[int, int]) -> Optional[np.ndarray]:
        if mask is None:
            return None
        arr = np.asarray(mask)
        if arr.size == 0:
            return None
        if arr.ndim == 3:
            arr = arr[..., 0]
        arr = (arr > 0).astype(np.uint8)
        tw, th = target_size
        if arr.shape[1] == tw and arr.shape[0] == th:
            return arr
        try:
            resized = cv2.resize(arr, (tw, th), interpolation=cv2.INTER_NEAREST)
            return (resized > 0).astype(np.uint8)
        except Exception:
            return None

    def _mask_from_lines(self, blocks, base_size: Optional[Tuple[int, int]], target_size: Tuple[int, int]) -> Optional[np.ndarray]:
        w, h = target_size
        if base_size and len(base_size) == 2:
            w, h = int(base_size[0]), int(base_size[1])
        mask = np.zeros((h, w), dtype=np.uint8)
        if blocks:
            for blk in blocks:
                for line in getattr(blk, "lines", []) or []:
                    pts = np.asarray(line, dtype=np.float32).reshape(-1, 2)
                    if pts.size == 0:
                        continue
                    cv2.fillPoly(mask, [np.round(pts).astype(np.int32)], 255)
        if mask.max() == 0:
            return self._mask_from_blocks(blocks, base_size, target_size)
        return self._normalize_mask(mask, target_size)

    def _mask_from_blocks(self, blocks, base_size: Optional[Tuple[int, int]], target_size: Tuple[int, int]) -> Optional[np.ndarray]:
        w, h = target_size
        if base_size and len(base_size) == 2:
            w, h = int(base_size[0]), int(base_size[1])
        mask = np.zeros((h, w), dtype=np.uint8)
        rects: List[Tuple[float, float, float, float]] = []
        if blocks:
            expand_px = max(0, int(getattr(self.view, "_textdetector_block_expand_px", 0)))
            merge_gap = float(getattr(self.view, "_textdetector_merge_gap_px", 0)) if getattr(self.view, "_textdetector_merge_nearby", False) else 0.0
            for blk in blocks:
                xyxy = getattr(blk, "xyxy", None)
                rect = None
                if xyxy and len(xyxy) == 4:
                    try:
                        x1, y1, x2, y2 = [float(v) for v in xyxy]
                        rect = (x1, y1, x2, y2)
                    except Exception:
                        rect = None
                if rect is None:
                    pts = []
                    for line in getattr(blk, "lines", []) or []:
                        pts.extend(line)
                    if pts:
                        arr = np.asarray(pts, dtype=np.float32).reshape(-1, 2)
                        xs, ys = arr[:, 0], arr[:, 1]
                        rect = (float(xs.min()), float(ys.min()), float(xs.max()), float(ys.max()))
                if rect is None:
                    continue
                x1, y1, x2, y2 = rect
                if expand_px > 0:
                    x1 -= expand_px
                    y1 -= expand_px
                    x2 += expand_px
                    y2 += expand_px
                rects.append((x1, y1, x2, y2))

            rects = self._merge_rects(rects, gap=merge_gap) if rects else rects
        for x1, y1, x2, y2 in rects:
            if x2 <= x1 or y2 <= y1:
                continue
            cv2.rectangle(
                mask,
                (int(round(x1)), int(round(y1))),
                (int(round(x2)), int(round(y2))),
                255,
                thickness=-1,
            )
        return self._normalize_mask(mask, target_size)

    def _merge_rects(self, rects: Iterable[Tuple[float, float, float, float]], *, gap: float = 0.0) -> List[Tuple[float, float, float, float]]:
        merged: List[Tuple[float, float, float, float]] = []
        g = float(gap)
        for rect in rects:
            x1, y1, x2, y2 = rect
            cur = [x1, y1, x2, y2]
            i = 0
            while i < len(merged):
                mx1, my1, mx2, my2 = merged[i]
                if not (x2 + g < mx1 or x1 - g > mx2 or y2 + g < my1 or y1 - g > my2):
                    cur = [min(cur[0], mx1), min(cur[1], my1), max(cur[2], mx2), max(cur[3], my2)]
                    merged.pop(i)
                    x1, y1, x2, y2 = cur
                    continue
                i += 1
            merged.append(tuple(cur))
        return merged

    def _fill_components(self, mask: np.ndarray, base_img: QImage, overlay: QImage) -> Tuple[bool, int, int, Optional[QImage]]:
        mask_bin = (mask > 0).astype(np.uint8)
        if mask_bin.size == 0:
            return False, 0, 0, None
        base_rgba = _qimage_to_rgba_array(base_img)
        if base_rgba.shape[:2] != mask_bin.shape:
            return False, 0, 0, None
        overlay_rgba = _qimage_to_rgba_array(overlay)
        # растягиваем слой если его размер отличается (на всякий случай)
        if overlay_rgba.shape[:2] != mask_bin.shape:
            try:
                overlay_rgba = cv2.resize(overlay_rgba, (mask_bin.shape[1], mask_bin.shape[0]), interpolation=cv2.INTER_NEAREST)
            except Exception:
                return False, 0, 0, None

        num, labels = cv2.connectedComponents(mask_bin, connectivity=8)
        if num <= 1:
            return False, 0, 0, None

        params = self._edge_params
        ring = max(1, int(params.ring_thickness_px))
        step = max(1, int(params.sample_step_px))
        kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (ring * 2 + 1, ring * 2 + 1))

        filled = 0
        total = num - 1
        for lbl in range(1, num):
            comp = (labels == lbl)
            if not comp.any():
                continue
            comp_u8 = comp.astype(np.uint8)
            outer = cv2.dilate(comp_u8, kernel, iterations=1)
            inner = cv2.erode(comp_u8, kernel, iterations=1)
            ring_mask = cv2.subtract(outer, inner)
            ys, xs = np.nonzero(ring_mask)
            if step > 1:
                ys = ys[::step]
                xs = xs[::step]
            if len(ys) < params.min_samples:
                continue
            samples = base_rgba[ys, xs, :3].astype(np.float32)
            if samples.size == 0:
                continue
            luma = 0.299 * samples[:, 0] + 0.587 * samples[:, 1] + 0.114 * samples[:, 2]
            if float(luma.std()) > float(params.std_threshold):
                continue
            mean_col = samples.mean(axis=0)
            overlay_rgba[comp, 0] = np.clip(mean_col[0], 0, 255)
            overlay_rgba[comp, 1] = np.clip(mean_col[1], 0, 255)
            overlay_rgba[comp, 2] = np.clip(mean_col[2], 0, 255)
            overlay_rgba[comp, 3] = 255
            filled += 1

        if filled <= 0:
            return False, filled, total, None

        h, w, _ = overlay_rgba.shape
        qimg = QImage(overlay_rgba.data, w, h, w * 4, QImage.Format.Format_RGBA8888).copy()
        qimg = qimg.convertToFormat(QImage.Format.Format_ARGB32_Premultiplied)
        return True, filled, total, qimg
