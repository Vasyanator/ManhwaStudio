from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/stitching.py
Stitch/split and "cut like chapter" tools for New Project window.

Main items:
- `on_stitch_split`: merge current pages and re-split with configurable params.
- `on_cut_take_chapter` / `on_cut_pick_folder`: use external chapter layout as cut reference.
- Project chapter lookup uses `user_config.json` projects folder (`General.projects_dir`).
"""

import traceback
from pathlib import Path
from typing import List

import numpy as np
from PIL import Image
from PyQt6 import QtCore, QtGui, QtWidgets

from config import SRC_DIR, get_projects_root
from modules.manhwa_merge import (
    build_candidates,
    greedy_cut_positions,
    refine_cuts_to_uniform_bands,
    stitch_sequence,
    unify_widths,
)

from . import save_ops


class StitchAlignDialog(QtWidgets.QDialog):
    def __init__(self, ref_pil: Image.Image, tape_bgr: np.ndarray, parent=None):
        super().__init__(parent)
        self.setWindowTitle("Стыковка")
        self._crop_px = 0
        self._max_crop = max(0, int(tape_bgr.shape[0]) - 1)

        ref_pix = self._pil_to_pixmap(ref_pil)
        tape_pix = self._bgr_to_pixmap(tape_bgr)

        self._scene = QtWidgets.QGraphicsScene(self)
        self._ref_item = self._scene.addPixmap(ref_pix)
        self._tape_item = self._scene.addPixmap(tape_pix)
        self._tape_item.setOpacity(0.6)
        self._tape_item.setPos(0, 0)

        scene_w = max(ref_pix.width(), tape_pix.width())
        scene_h = max(ref_pix.height(), tape_pix.height())
        self._scene.setSceneRect(0, 0, scene_w, scene_h)

        self._view = QtWidgets.QGraphicsView(self._scene)
        self._view.setDragMode(QtWidgets.QGraphicsView.DragMode.ScrollHandDrag)
        self._view.setVerticalScrollBarPolicy(QtCore.Qt.ScrollBarPolicy.ScrollBarAsNeeded)
        self._view.setHorizontalScrollBarPolicy(QtCore.Qt.ScrollBarPolicy.ScrollBarAsNeeded)
        self._view.setFocusPolicy(QtCore.Qt.FocusPolicy.StrongFocus)
        self._view.installEventFilter(self)

        self._lblCrop = QtWidgets.QLabel("Смещение вверх: 0 px")
        self._lblCropHint = QtWidgets.QLabel("Обрезка: всё, что выше исходной картинки, будет удалено")

        self._opacity = QtWidgets.QSlider(QtCore.Qt.Orientation.Horizontal)
        self._opacity.setRange(20, 100)
        self._opacity.setValue(60)
        self._opacity.valueChanged.connect(self._on_opacity_changed)

        btnCancel = QtWidgets.QPushButton("Отмена")
        btnOk = QtWidgets.QPushButton("Подтвердить смещение")
        btnMinus5 = QtWidgets.QPushButton("-5")
        btnMinus1 = QtWidgets.QPushButton("-1")
        btnPlus1 = QtWidgets.QPushButton("+1")
        btnPlus5 = QtWidgets.QPushButton("+5")
        btnMinus5.clicked.connect(lambda: self._adjust_crop(-5))
        btnMinus1.clicked.connect(lambda: self._adjust_crop(-1))
        btnPlus1.clicked.connect(lambda: self._adjust_crop(1))
        btnPlus5.clicked.connect(lambda: self._adjust_crop(5))
        btnCancel.clicked.connect(self.reject)
        btnOk.clicked.connect(self.accept)

        btns = QtWidgets.QHBoxLayout()
        btns.addWidget(btnMinus5)
        btns.addWidget(btnMinus1)
        btns.addWidget(btnPlus1)
        btns.addWidget(btnPlus5)
        btns.addStretch(1)
        btns.addWidget(btnCancel)
        btns.addWidget(btnOk)

        layout = QtWidgets.QVBoxLayout(self)
        layout.addWidget(self._view, 1)
        layout.addWidget(self._lblCrop)
        layout.addWidget(self._lblCropHint)
        layout.addWidget(QtWidgets.QLabel("Прозрачность ленты:"))
        layout.addWidget(self._opacity)
        layout.addLayout(btns)

        self.resize(900, 700)
        self._update_tape_pos()

    def showEvent(self, ev: QtGui.QShowEvent) -> None:
        super().showEvent(ev)
        self._view.setFocus()
        self._view.verticalScrollBar().setValue(self._view.verticalScrollBar().minimum())

    def crop_px(self) -> int:
        return int(self._crop_px)

    def keyPressEvent(self, ev: QtGui.QKeyEvent) -> None:
        step = 10 if ev.modifiers() & QtCore.Qt.KeyboardModifier.ShiftModifier else 1
        if ev.key() == QtCore.Qt.Key.Key_Up:
            self._adjust_crop(step)
            return
        if ev.key() == QtCore.Qt.Key.Key_Down:
            self._adjust_crop(-step)
            return
        super().keyPressEvent(ev)

    def eventFilter(self, obj, ev):
        if obj is self._view and ev.type() == QtCore.QEvent.Type.KeyPress:
            step = 10 if ev.modifiers() & QtCore.Qt.KeyboardModifier.ShiftModifier else 1
            if ev.key() == QtCore.Qt.Key.Key_Up:
                self._adjust_crop(step)
                return True
            if ev.key() == QtCore.Qt.Key.Key_Down:
                self._adjust_crop(-step)
                return True
        return super().eventFilter(obj, ev)

    def _adjust_crop(self, delta: int):
        new_val = max(0, min(self._crop_px + delta, self._max_crop))
        if new_val == self._crop_px:
            return
        self._crop_px = new_val
        self._update_tape_pos()

    def _update_tape_pos(self):
        self._tape_item.setPos(0, -self._crop_px)
        self._lblCrop.setText(f"Смещение вверх: {self._crop_px} px")

    def _on_opacity_changed(self, val: int):
        self._tape_item.setOpacity(max(0.05, float(val) / 100.0))

    def _pil_to_pixmap(self, im: Image.Image) -> QtGui.QPixmap:
        rgb = im.convert("RGB")
        data = rgb.tobytes("raw", "RGB")
        qimg = QtGui.QImage(data, rgb.width, rgb.height, rgb.width * 3, QtGui.QImage.Format.Format_RGB888)
        return QtGui.QPixmap.fromImage(qimg)

    def _bgr_to_pixmap(self, arr: np.ndarray) -> QtGui.QPixmap:
        if arr.ndim != 3 or arr.shape[2] != 3:
            raise ValueError("Ожидается BGR изображение (H, W, 3).")
        rgb = arr[:, :, ::-1].copy()
        h, w = rgb.shape[:2]
        qimg = QtGui.QImage(rgb.data, w, h, w * 3, QtGui.QImage.Format.Format_RGB888)
        return QtGui.QPixmap.fromImage(qimg)


def pil_to_bgr(im: Image.Image) -> np.ndarray:
    arr = np.array(im.convert("RGB"), dtype=np.uint8)
    return arr[:, :, ::-1].copy()


def bgr_to_pil(arr: np.ndarray) -> Image.Image:
    rgb = arr[:, :, ::-1]
    return Image.fromarray(rgb, mode="RGB")


def build_stitched_tape_and_cuts(
    images_bgr: List[np.ndarray],
    *,
    K: int | None,
    Hmax: int,
    band_rows: int,
    tol: int,
    search_radius: int,
    prefer_up_first: bool,
) -> tuple[np.ndarray, List[int]]:
    """
    Собирает единую ленту и рассчитывает рекомендуемые места разреза.
    Возвращает `(tape_bgr, cuts)` где `cuts` включает 0 и конец ленты.
    """
    if not images_bgr:
        raise ValueError("Нет изображений для сшивания.")

    normalized, width = unify_widths(images_bgr)
    superframes = stitch_sequence(normalized)
    if not superframes:
        raise RuntimeError("Не удалось сшить изображения.")

    heights = [int(im.shape[0]) for im in superframes]
    total_height = int(sum(heights))
    if total_height <= 0:
        raise RuntimeError("Лента после сшивания получилась пустой.")

    if K is None:
        K = max(1, int(np.ceil(total_height / float(Hmax))))

    min_required = int(np.ceil(total_height / float(Hmax)))
    if K < min_required:
        K = min_required

    positions, _sf_map, cost_map = build_candidates(superframes)
    cuts = greedy_cut_positions(positions, cost_map, total_height, K, Hmax)

    fixed = [cuts[0]]
    for i in range(1, len(cuts)):
        prev = fixed[-1]
        cur = cuts[i]
        is_last = i == len(cuts) - 1
        if (not is_last) and (cur - prev > Hmax):
            hi = prev + Hmax
            idxs = np.where((positions > prev) & (positions <= hi))[0]
            if idxs.size:
                best_j = idxs[np.argmin([cost_map.get(int(positions[j]), 0.0) for j in idxs])]
                cur = int(positions[int(best_j)])
            else:
                cur = hi
        fixed.append(cur)
    cuts = refine_cuts_to_uniform_bands(
        superframes,
        fixed,
        Hmax=Hmax,
        band_rows=band_rows,
        tol=tol,
        search_radius=search_radius,
        prefer_up_first=prefer_up_first,
    )

    tape = superframes[0] if len(superframes) == 1 else np.vstack(superframes)
    if tape.shape[1] != width:
        raise RuntimeError("Ширина ленты не совпала с ожидаемой после нормализации.")
    return tape, cuts


def split_tape_by_cuts(tape: np.ndarray, cuts: List[int]) -> List[np.ndarray]:
    """
    Нарезает уже собранную ленту по списку границ `cuts`, включая 0 и конец.
    """
    if tape.ndim != 3 or tape.shape[2] != 3:
        raise ValueError("Ожидается BGR-лента формата (H, W, 3).")

    tape_height = int(tape.shape[0])
    normalized = sorted({int(v) for v in cuts if 0 <= int(v) <= tape_height})
    if not normalized or normalized[0] != 0:
        normalized = [0, *normalized]
    if normalized[-1] != tape_height:
        normalized.append(tape_height)

    segments: List[np.ndarray] = []
    for y0, y1 in zip(normalized, normalized[1:]):
        if y1 <= y0:
            continue
        segments.append(tape[y0:y1, :, :].copy())
    return segments


def on_stitch_split(window) -> None:
    if not window._current_images_pil and window._opened_images_pil:
        window._current_images_pil = list(window._opened_images_pil)
    if not window._current_images_pil:
        QtWidgets.QMessageBox.warning(window, "Нет данных", "Сначала откройте папку или скачайте главу.")
        return

    K = None
    txtK = (window.edK.text() or "").strip()
    if txtK:
        try:
            K = int(txtK)
            assert K > 0
        except Exception:
            QtWidgets.QMessageBox.critical(
                window, "Параметры", "K должно быть положительным целым или пусто (авто)."
            )
            return
    try:
        Hmax = int(window.edHmax.text())
        assert Hmax > 0
        band_rows = int(window.edBand.text())
        assert band_rows > 0
        tol = int(window.edTol.text())
        assert tol > 0
        search_radius = int(window.edR.text())
        assert search_radius > 0
    except Exception:
        QtWidgets.QMessageBox.critical(
            window, "Параметры", "Hmax/band_rows/tol/search_radius должны быть > 0."
        )
        return

    prefer_up_first = window.chkPreferUp.isChecked()

    window.setCursor(QtCore.Qt.CursorShape.BusyCursor)
    window._set_progress("Сшивание…", 0, 0, pulse=True)
    QtWidgets.QApplication.processEvents()

    try:
        bgr_list = [pil_to_bgr(im) for im in window._current_images_pil]
        tape_bgr, cuts = build_stitched_tape_and_cuts(
            bgr_list,
            K=K,
            Hmax=Hmax,
            band_rows=band_rows,
            tol=tol,
            search_radius=search_radius,
            prefer_up_first=prefer_up_first,
        )
        if window.chkAutoCut.isChecked():
            segments_bgr = split_tape_by_cuts(tape_bgr, cuts)
            new_pil = [bgr_to_pil(a) for a in segments_bgr]
            if not new_pil:
                QtWidgets.QMessageBox.information(window, "Результат", "Сегменты не получены.")
                return
            window._clear_stitch_preview()
            window._current_images_pil = new_pil
            window.viewer.set_images(window._current_images_pil)
        else:
            window._set_stitch_preview(bgr_to_pil(tape_bgr), cuts[1:-1])
    except Exception as e:
        QtWidgets.QMessageBox.critical(window, "Ошибка сшивания", str(e))
        traceback.print_exc()
    finally:
        window._set_progress("Готово", 1, 1)
        window.unsetCursor()


def on_revert_original(window) -> None:
    if not window._opened_images_pil:
        QtWidgets.QMessageBox.information(window, "Нет исходных", "Исходные изображения ещё не загружены.")
        return
    window._clear_stitch_preview()
    window._current_images_pil = list(window._opened_images_pil)
    window.viewer.set_images(window._current_images_pil)


def apply_manual_cuts(window) -> None:
    if not window._current_images_pil or len(window._current_images_pil) != 1:
        QtWidgets.QMessageBox.warning(window, "Нет ленты", "Для ручной нарезки нужна одна склеенная лента.")
        return

    tape_bgr = pil_to_bgr(window._current_images_pil[0])
    cuts = [0, *window.viewer.current_cut_guides(), int(tape_bgr.shape[0])]
    segments_bgr = split_tape_by_cuts(tape_bgr, cuts)
    if not segments_bgr:
        QtWidgets.QMessageBox.warning(window, "Нет сегментов", "Не удалось получить сегменты по текущим разрезам.")
        return

    window._clear_stitch_preview()
    window._current_images_pil = [bgr_to_pil(seg) for seg in segments_bgr]
    window.viewer.set_images(window._current_images_pil)


def on_cut_take_chapter(window) -> None:
    title = (window.cmbCutTitle.currentText() or "").strip()
    chapter = (window.cmbCutChapter.currentText() or "").strip()
    if not title or not chapter:
        QtWidgets.QMessageBox.warning(window, "Внимание", "Выберите тайтл и главу.")
        return
    projects_root = Path(get_projects_root())
    src_dir = projects_root / title / chapter / SRC_DIR
    if not src_dir.exists():
        src_dir = projects_root / title / chapter / "scr"
        if not src_dir.exists():
            QtWidgets.QMessageBox.warning(window, "Нет данных", "Папка главы не найдена.")
            return
    ref_images = save_ops.load_images_from_dir(src_dir)
    if not ref_images:
        QtWidgets.QMessageBox.warning(window, "Нет данных", "В папке главы нет изображений.")
        return
    cut_like_chapter(window, ref_images)


def on_cut_pick_folder(window) -> None:
    folder = QtWidgets.QFileDialog.getExistingDirectory(window, "Выберите папку с изображениями")
    if not folder:
        return
    ref_images = save_ops.load_images_from_dir(Path(folder))
    if not ref_images:
        QtWidgets.QMessageBox.warning(window, "Нет данных", "В выбранной папке нет изображений.")
        return
    cut_like_chapter(window, ref_images)


def cut_like_chapter(window, ref_images: List[Image.Image]) -> None:
    if not window._current_images_pil and window._opened_images_pil:
        window._current_images_pil = list(window._opened_images_pil)
    if not window._current_images_pil:
        QtWidgets.QMessageBox.warning(window, "Нет данных", "Сначала откройте папку или скачайте главу.")
        return

    widths = {im.width for im in ref_images if getattr(im, "width", None)}
    if len(widths) > 1:
        QtWidgets.QMessageBox.critical(window, "Ошибка", "Картинки должны иметь одинаковую ширину")
        return

    ref_heights = [int(im.height) for im in ref_images]
    ref_total = sum(ref_heights)
    cur_total = sum(int(im.height) for im in window._current_images_pil)

    window.setCursor(QtCore.Qt.CursorShape.BusyCursor)
    window._set_progress("Сшивание…", 0, 0, pulse=True)
    try:
        bgr_list = [pil_to_bgr(im) for im in window._current_images_pil]
        bgr_list, _ = unify_widths(bgr_list)
        superframes = stitch_sequence(bgr_list)
        if not superframes:
            QtWidgets.QMessageBox.information(window, "Результат", "Не удалось сшить изображения.")
            return
        tape = superframes[0] if len(superframes) == 1 else np.vstack(superframes)

        crop_px = 0
        if cur_total != ref_total:
            diff = cur_total - ref_total
            sign = "больше" if diff > 0 else "меньше"
            QtWidgets.QMessageBox.warning(
                window,
                "Высота отличается",
                f"Суммарная высота текущих изображений {sign} на {abs(diff)} px.\n"
                "Открою окно стыковки для подгонки.",
            )

        if tape.shape[0] != ref_total:
            window.unsetCursor()
            window._set_progress("", 0, 0)
            dlg = StitchAlignDialog(ref_images[0], tape, window)
            if dlg.exec() != QtWidgets.QDialog.DialogCode.Accepted:
                return
            crop_px = dlg.crop_px()
            window.setCursor(QtCore.Qt.CursorShape.BusyCursor)
            window._set_progress("Обрезка…", 0, 0, pulse=True)

        if crop_px:
            if crop_px >= tape.shape[0]:
                QtWidgets.QMessageBox.critical(window, "Ошибка", "Обрезка превышает высоту ленты.")
                return
            tape = tape[crop_px:, :, :]

        tape_height = int(tape.shape[0])
        if sum(ref_heights) > tape_height:
            QtWidgets.QMessageBox.warning(
                window,
                "Высота отличается",
                "Суммарная высота примера больше высоты ленты.\n"
                "Результат может быть неполным.",
            )

        segments = []
        y = 0
        cut_incomplete = False
        for h in ref_heights:
            y1 = y + h
            if y1 > tape_height:
                cut_incomplete = True
                break
            seg = tape[y:y1, :, :].copy()
            segments.append(seg)
            y = y1

        if len(segments) != len(ref_heights):
            if cut_incomplete:
                avg_ref_height = int(sum(ref_heights) / len(ref_heights)) if ref_heights else 0
                short_by = ref_total - tape_height
                significant_short = short_by >= avg_ref_height if avg_ref_height else short_by > 0
                if significant_short:
                    QtWidgets.QMessageBox.warning(
                        window,
                        "Результат",
                        "Глава оказалась сильно короче, отсутствуют последние страницы.",
                    )
            else:
                QtWidgets.QMessageBox.critical(window, "Ошибка", "Не удалось разрезать ленту по высотам примера.")
                return

        new_pil = [bgr_to_pil(a) for a in segments]
        window._current_images_pil = new_pil
        window.viewer.set_images(window._current_images_pil)
    except Exception as e:
        QtWidgets.QMessageBox.critical(window, "Ошибка нарезки", str(e))
        traceback.print_exc()
    finally:
        window._set_progress("Готово", 1, 1)
        window.unsetCursor()
