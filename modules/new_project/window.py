from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/window.py
Main Qt dialog for creating/editing chapter source images.

Main items:
- `NewProjectWindow`: import/download/stitch/waifu2x pipeline and save actions.
- `BatchProcessingNodesWindow`: отдельное окно c тестовым node-холстом для будущей массовой обработки.
- Title/chapter selectors for save/cut/alt-version operations.
- Project lists are loaded from `user_config.json` projects folder (`General.projects_dir`).
"""

import math
import os
import sys
import traceback
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Tuple

import numpy as np
from PIL import Image
from PyQt6 import QtCore, QtGui, QtWidgets

from config import get_projects_root, program_dir, UserConfig
from ui_new.tabs.wiki import create_markdown_widget
from ui_new.theme import apply_theme

from .batch_nodes_window import BatchProcessingNodesWindow
from . import downloaders, import_ops, save_ops, stitching, waifu2x
from .downloaders import SUPPORTED_SITES, _DEFAULT_LINK_PREFIX, detect_available_browsers
from .waifu2x import _HAS_W2X_PY

_DEFAULT_LINK_PREFIX = "https://page-edge.kakao.com/sdownload/resource"

# ---- PyQt6
def _resolve_qt_runner(qt_entry: str) -> str:
    """
    Возвращает абсолютный путь к qt-раннеру.
    По умолчанию ожидаем, что файл лежит рядом с корнем проекта (на уровень выше modules/).
    Можно передать абсолютный путь или относительный к корню проекта.
    """
    if os.path.isabs(qt_entry):
        return qt_entry
    # этот файл: .../modules/new_project_qt/window.py -> корень: на два уровня выше
    project_root = Path(__file__).resolve().parents[2]
    return os.path.join(project_root, qt_entry)


class CutMarkerScrollBar(QtWidgets.QScrollBar):
    """
    Вертикальный scrollbar с красными отметками мест разреза для режима ручной нарезки.
    """

    def __init__(self, parent=None):
        super().__init__(QtCore.Qt.Orientation.Vertical, parent)
        self._markers: List[int] = []
        self._content_height = 0

    def set_cut_markers(self, markers: List[int], content_height: int) -> None:
        self._markers = [int(v) for v in markers if int(v) >= 0]
        self._content_height = max(0, int(content_height))
        self.update()

    def paintEvent(self, event: QtGui.QPaintEvent) -> None:
        super().paintEvent(event)
        if not self._markers or self._content_height <= 0:
            return

        option = QtWidgets.QStyleOptionSlider()
        self.initStyleOption(option)
        groove = self.style().subControlRect(
            QtWidgets.QStyle.ComplexControl.CC_ScrollBar,
            option,
            QtWidgets.QStyle.SubControl.SC_ScrollBarGroove,
            self,
        )
        if groove.height() <= 0:
            return

        painter = QtGui.QPainter(self)
        pen = QtGui.QPen(QtGui.QColor("#ff3b30"))
        pen.setWidth(2)
        painter.setPen(pen)

        denom = max(1, self._content_height)
        for marker in self._markers:
            ratio = max(0.0, min(float(marker) / float(denom), 1.0))
            y = groove.top() + int(round(ratio * groove.height()))
            painter.drawLine(groove.left() + 1, y, groove.right() - 1, y)


class CutGuidesOverlay(QtWidgets.QWidget):
    """
    Слой поверх ленты: рисует линии разреза и позволяет перетаскивать их за центральную ручку.
    """

    def __init__(self, viewer: "VirtualizedImageView"):
        super().__init__(viewer._canvas)
        self._viewer = viewer
        self._drag_index: Optional[int] = None
        self.setAttribute(QtCore.Qt.WidgetAttribute.WA_NoSystemBackground, True)
        self.setMouseTracking(True)
        self.hide()

    def paintEvent(self, event: QtGui.QPaintEvent) -> None:
        del event
        guides = self._viewer.cut_guide_geometries()
        if not guides:
            return

        painter = QtGui.QPainter(self)
        painter.setRenderHint(QtGui.QPainter.RenderHint.Antialiasing, True)
        line_pen = QtGui.QPen(QtGui.QColor("#ff3b30"))
        line_pen.setWidth(2)

        for guide in guides:
            painter.setPen(line_pen)
            painter.setBrush(QtGui.QColor("#ff3b30"))
            y = guide["canvas_y"]
            image_rect = guide["image_rect"]
            handle_rect = guide["handle_rect"]
            painter.drawLine(image_rect.left(), y, image_rect.right(), y)
            painter.drawRoundedRect(handle_rect, 7, 7)
            self._draw_handle_arrows(painter, handle_rect)

    def mousePressEvent(self, event: QtGui.QMouseEvent) -> None:
        if event.button() != QtCore.Qt.MouseButton.LeftButton:
            event.ignore()
            return
        idx = self._guide_at_pos(event.position().toPoint())
        if idx is None:
            event.ignore()
            return
        self._drag_index = idx
        self.setCursor(QtCore.Qt.CursorShape.SizeVerCursor)
        event.accept()

    def mouseMoveEvent(self, event: QtGui.QMouseEvent) -> None:
        point = event.position().toPoint()
        if self._drag_index is not None:
            self._viewer.move_cut_guide(self._drag_index, point.y())
            event.accept()
            return

        if self._guide_at_pos(point) is not None:
            self.setCursor(QtCore.Qt.CursorShape.SizeVerCursor)
        else:
            self.unsetCursor()
        event.accept()

    def mouseReleaseEvent(self, event: QtGui.QMouseEvent) -> None:
        if event.button() == QtCore.Qt.MouseButton.LeftButton:
            self._drag_index = None
            self.unsetCursor()
            event.accept()
            return
        event.ignore()

    def leaveEvent(self, event: QtCore.QEvent) -> None:
        if self._drag_index is None:
            self.unsetCursor()
        super().leaveEvent(event)

    def _guide_at_pos(self, point: QtCore.QPoint) -> Optional[int]:
        for idx, guide in enumerate(self._viewer.cut_guide_geometries()):
            if guide["handle_rect"].contains(point):
                return idx
        return None

    def _draw_handle_arrows(self, painter: QtGui.QPainter, handle_rect: QtCore.QRect) -> None:
        center_x = handle_rect.center().x()
        center_y = handle_rect.center().y()
        painter.setBrush(QtGui.QColor("#ffffff"))
        painter.setPen(QtCore.Qt.PenStyle.NoPen)
        top = QtGui.QPolygon(
            [
                QtCore.QPoint(center_x, center_y - 8),
                QtCore.QPoint(center_x - 5, center_y - 2),
                QtCore.QPoint(center_x + 5, center_y - 2),
            ]
        )
        bottom = QtGui.QPolygon(
            [
                QtCore.QPoint(center_x, center_y + 8),
                QtCore.QPoint(center_x - 5, center_y + 2),
                QtCore.QPoint(center_x + 5, center_y + 2),
            ]
        )
        painter.drawPolygon(top)
        painter.drawPolygon(bottom)


# ------------- Виртуализированный просмотрщик (безопасные тайлы) -------------
class VirtualizedImageView(QtWidgets.QScrollArea):
    """
    Простой виртуализированный viewer для очень длинных картинок.
    Делит каждую PIL-картинку на «плитки» по высоте, держит в памяти только видимые.
    """
    tile_height_default = 512
    cache_limit_default = 256

    def __init__(self, parent=None, tile_height: int = None, cache_limit: int = None, bg="#202020"):
        super().__init__(parent)
        self.setWidgetResizable(True)
        self.setVerticalScrollBar(CutMarkerScrollBar(self))
        self._bg = QtGui.QColor(bg)
        self._tile_h = max(64, tile_height or self.tile_height_default)
        self._cache_limit = max(64, cache_limit or self.cache_limit_default)
        self._images: List[Image.Image] = []
        self._scaled: Dict[int, Image.Image] = {}
        self._tiles: List[Tuple[int, int, int, int]] = []  # (img_idx, y0, y1, out_y)
        self._live: Dict[int, QtWidgets.QLabel] = {}
        self._gutter = 8
        self._content_w = 1200

        w = QtWidgets.QWidget()
        w.setAutoFillBackground(True)
        pal = w.palette()
        pal.setColor(w.backgroundRole(), self._bg)
        w.setPalette(pal)
        self._content = w
        self.setWidget(self._content)

        self._layout = QtWidgets.QVBoxLayout(self._content)
        self._layout.setContentsMargins(0, 0, 0, 0)
        self._layout.setSpacing(0)

        # холдер под canvas-подобный layout
        self._canvas = QtWidgets.QWidget(self._content)
        self._canvas.setMinimumWidth(self._content_w + 2*self._gutter)
        self._layout.addWidget(self._canvas)
        self._canvas.installEventFilter(self)
        self.verticalScrollBar().valueChanged.connect(self._render_visible)
        self._cut_guides: List[int] = []
        self._cut_guides_changed: Optional[callable] = None
        self._cut_requested: Optional[callable] = None
        self._cut_overlay = CutGuidesOverlay(self)
        self._cut_button = QtWidgets.QPushButton("Нарезать", self.viewport())
        self._cut_button.setCursor(QtCore.Qt.CursorShape.PointingHandCursor)
        self._cut_button.setStyleSheet(
            "QPushButton{background:#b71c1c;color:white;border:1px solid #ff6b6b;"
            "border-radius:12px;padding:6px 18px;font-weight:bold;}"
            "QPushButton:hover{background:#d32f2f;}"
        )
        self._cut_button.clicked.connect(self._emit_cut_requested)
        self._cut_button.hide()

        self.on_delete: Optional[callable] = None  # внешний колбэк удаления странички

    def set_images(self, images: Iterable[Image.Image | str]):
        self._images = []
        for it in images or []:
            if isinstance(it, Image.Image):
                self._images.append(it.convert("RGB"))
            else:
                try:
                    self._images.append(Image.open(it).convert("RGB"))
                except Exception:
                    pass
        self._scaled.clear()
        if len(self._images) != 1:
            self.clear_cut_guides()
        self._reflow()

    def _scaled_pil(self, idx: int) -> Image.Image:
        im = self._scaled.get(idx)
        if im is not None:
            return im
        src = self._images[idx]
        w, h = src.size
        if w != self._content_w:
            s = self._content_w / float(w if w else 1)
            im = src.resize((self._content_w, max(1, int(round(h*s)))), Image.LANCZOS)
        else:
            im = src
        self._scaled[idx] = im
        return im

    def _reflow(self):
        # убрать все живые плитки
        for lab in self._live.values():
            lab.setParent(None)
        self._live.clear()
        self._tiles.clear()

        y = self._gutter
        for i, _ in enumerate(self._images):
            im = self._scaled_pil(i)
            w, h = im.size
            ntiles = max(1, math.ceil(h / self._tile_h))
            for t in range(ntiles):
                top = t*self._tile_h
                bottom = min(h, (t+1)*self._tile_h)
                self._tiles.append((i, top, bottom, y+top))
            y += h + self._gutter

        self._canvas.setMinimumHeight(y + self._gutter)
        self._cut_overlay.setGeometry(self._canvas.rect())
        self._cut_overlay.raise_()
        self._render_visible()
        self._update_cut_ui()

    def resizeEvent(self, e: QtGui.QResizeEvent) -> None:
        new_w = max(200, min(self.viewport().width() - 2*self._gutter, 2000))
        if new_w != self._content_w:
            self._content_w = new_w
            self._scaled.clear()
            self._reflow()
        self._position_cut_button()
        super().resizeEvent(e)

    def eventFilter(self, obj, ev):
        # _canvas может ещё не существовать, если фильтр словил событие во время инициализации
        canvas = getattr(self, "_canvas", None)
        if canvas is not None and obj is canvas and ev.type() in (
            QtCore.QEvent.Type.Show,
            QtCore.QEvent.Type.Resize,
        ):
            QtCore.QTimer.singleShot(0, self._render_visible)
            QtCore.QTimer.singleShot(0, self._update_cut_ui)
        return super().eventFilter(obj, ev)

    def _render_visible(self, *_):
        if not self._tiles:
            return
        vy0 = self.verticalScrollBar().value()
        vy1 = vy0 + self.viewport().height()
        buf = 3*self._tile_h
        need_lo, need_hi = vy0 - buf, vy1 + buf

        # создать недостающие
        created = 0
        for (idx, y0, y1, out_y) in self._tiles:
            if out_y + (y1 - y0) < need_lo or out_y > need_hi:
                continue
            key = (idx << 32) ^ (y0 << 1) ^ y1
            if key in self._live:
                continue
            # собрать QPixmap из куска PIL
            pil = self._scaled_pil(idx).crop((0, y0, self._content_w, y1))
            data = pil.tobytes("raw", "RGB")
            qimg = QtGui.QImage(data, pil.width, pil.height, pil.width*3, QtGui.QImage.Format.Format_RGB888)
            pix = QtGui.QPixmap.fromImage(qimg)

            lbl = QtWidgets.QLabel(self._canvas)
            lbl.setPixmap(pix)
            lbl.move(self._gutter, out_y)
            lbl.resize(pix.width(), pix.height())
            lbl.show()
            self._live[key] = lbl

            # кнопка удаления только на первой плитке
            if y0 == 0:
                btn = QtWidgets.QToolButton(self._canvas)
                btn.setText("✕")
                btn.setStyleSheet("QToolButton{background:#cc1a1a;color:white;border:1px solid #660000;border-radius:9px;font-weight:bold;}")
                btn.resize(18, 18)
                btn.move(self._gutter + pix.width() - 24, out_y + 6)
                btn.show()
                # Храним кнопку вместе с тайлом для корректной очистки
                btn_key = ("btn", idx)
                self._live[btn_key] = btn

                def _cb(_=None, img_idx=idx):
                    self._delete_image(img_idx)
                btn.clicked.connect(_cb)

            created += 1
            if created > 128:
                break

        # подчистить из кэша
        self._shrink(need_lo - 4*self._tile_h, need_hi + 4*self._tile_h)
        self._cut_overlay.raise_()

    def _delete_image(self, idx: int):
        """Удаляет изображение по индексу и обновляет холст."""
        if idx < 0 or idx >= len(self._images):
            return
        # Сохраняем позицию скролла относительно удаляемого изображения
        scroll_pos = self.verticalScrollBar().value()

        # Вычисляем высоту удаляемого изображения для корректировки скролла
        removed_height = 0
        removed_y = 0
        y = self._gutter
        for i, _ in enumerate(self._images):
            im = self._scaled_pil(i)
            h = im.size[1]
            if i == idx:
                removed_height = h + self._gutter
                removed_y = y
                break
            y += h + self._gutter

        # Вызываем внешний колбэк если есть
        if callable(self.on_delete):
            self.on_delete(idx)

        # Удаляем изображение из списка
        del self._images[idx]

        # Очищаем кэш масштабированных изображений (индексы сдвинулись)
        self._scaled.clear()

        # Корректируем позицию скролла если удаляем изображение выше текущей позиции
        if scroll_pos > removed_y:
            new_scroll = max(0, scroll_pos - removed_height)
        else:
            new_scroll = scroll_pos

        # Перестраиваем холст
        self._reflow()

        # Восстанавливаем позицию скролла
        self.verticalScrollBar().setValue(new_scroll)

    def _shrink(self, keep_lo: int, keep_hi: int):
        for key, lab in list(self._live.items()):
            y = lab.y()
            h = lab.height()
            if y + h < keep_lo or y > keep_hi:
                lab.setParent(None)
                self._live.pop(key, None)
        if len(self._live) > self._cache_limit:
            # грубая эвикция
            center = (keep_lo + keep_hi)//2
            rest = sorted(self._live.items(), key=lambda kv: abs((kv[1].y()+kv[1].height()//2) - center), reverse=True)
            for key, lab in rest[self._cache_limit:]:
                lab.setParent(None)
                self._live.pop(key, None)

    def set_cut_guides(
        self,
        cut_positions: Iterable[int],
        *,
        on_positions_changed=None,
        on_cut_requested=None,
    ) -> None:
        self._cut_guides_changed = on_positions_changed
        self._cut_requested = on_cut_requested
        self._cut_guides = self._normalize_cut_guides(list(cut_positions))
        self._update_cut_ui()

    def clear_cut_guides(self) -> None:
        self._cut_guides = []
        self._cut_guides_changed = None
        self._cut_requested = None
        self._update_cut_ui()

    def cut_guide_geometries(self) -> List[dict]:
        if len(self._images) != 1 or not self._cut_guides:
            return []
        image_rect = self._single_image_display_rect()
        if image_rect is None:
            return []
        image_height = max(1, self._images[0].height)
        scale = image_rect.height() / float(image_height)
        handle_width = min(96, max(64, image_rect.width() // 5))
        handle_height = 26
        center_x = image_rect.center().x()
        geometries = []
        for cut in self._cut_guides:
            canvas_y = image_rect.top() + int(round(cut * scale))
            handle_rect = QtCore.QRect(
                int(center_x - handle_width / 2),
                int(canvas_y - handle_height / 2),
                handle_width,
                handle_height,
            )
            geometries.append(
                {
                    "cut": cut,
                    "canvas_y": canvas_y,
                    "image_rect": image_rect,
                    "handle_rect": handle_rect,
                }
            )
        return geometries

    def move_cut_guide(self, index: int, canvas_y: int) -> None:
        if len(self._images) != 1 or index < 0 or index >= len(self._cut_guides):
            return
        image_rect = self._single_image_display_rect()
        if image_rect is None or image_rect.height() <= 0:
            return
        image_height = max(1, self._images[0].height)
        scale = image_rect.height() / float(image_height)
        raw_cut = int(round((canvas_y - image_rect.top()) / max(scale, 1e-6)))

        lower_bound = 1 if index == 0 else self._cut_guides[index - 1] + 1
        upper_bound = image_height - 1 if index == len(self._cut_guides) - 1 else self._cut_guides[index + 1] - 1
        new_cut = max(lower_bound, min(raw_cut, upper_bound))
        if new_cut == self._cut_guides[index]:
            return

        self._cut_guides[index] = new_cut
        self._update_cut_ui()
        if callable(self._cut_guides_changed):
            self._cut_guides_changed(list(self._cut_guides))

    def current_cut_guides(self) -> List[int]:
        return list(self._cut_guides)

    def _normalize_cut_guides(self, cut_positions: List[int]) -> List[int]:
        if len(self._images) != 1:
            return []
        max_height = max(1, self._images[0].height)
        unique = sorted({int(v) for v in cut_positions if 0 < int(v) < max_height})
        out: List[int] = []
        prev = 0
        for cut in unique:
            if cut <= prev:
                continue
            if cut >= max_height:
                continue
            out.append(cut)
            prev = cut
        return out

    def _single_image_display_rect(self) -> Optional[QtCore.QRect]:
        if len(self._images) != 1:
            return None
        if not self._tiles:
            return None
        scaled = self._scaled_pil(0)
        return QtCore.QRect(self._gutter, self._gutter, scaled.width, scaled.height)

    def _guide_canvas_positions(self) -> List[int]:
        return [guide["canvas_y"] for guide in self.cut_guide_geometries()]

    def _position_cut_button(self) -> None:
        if not self._cut_button.isVisible():
            return
        hint = self._cut_button.sizeHint()
        x = max(8, int((self.viewport().width() - hint.width()) / 2))
        self._cut_button.resize(hint)
        self._cut_button.move(x, 8)
        self._cut_button.raise_()

    def _update_cut_ui(self) -> None:
        has_guides = len(self._images) == 1 and bool(self._cut_guides)
        self._cut_overlay.setVisible(has_guides)
        self._cut_overlay.raise_()
        self._cut_overlay.update()
        self._cut_button.setVisible(len(self._images) == 1 and callable(self._cut_requested))
        self._position_cut_button()
        scrollbar = self.verticalScrollBar()
        if isinstance(scrollbar, CutMarkerScrollBar):
            scrollbar.set_cut_markers(self._guide_canvas_positions(), self._canvas.minimumHeight())

    def _emit_cut_requested(self) -> None:
        if callable(self._cut_requested):
            self._cut_requested()


# ------------- Главное окно -------------
class NewProjectWindow(QtWidgets.QDialog):
    def __init__(
        self,
        parent=None,
        *,
        qt_entry: str = "qt_runner.py",
        on_open_project=None,
    ):
        super().__init__(parent)
        # Делаем окно полноценным (свернуть/развернуть), иначе на Windows остаётся "диалог" только с крестиком
        self.setWindowFlag(QtCore.Qt.WindowType.Window, True)
        self.setWindowFlag(QtCore.Qt.WindowType.WindowSystemMenuHint, True)
        self.setWindowFlag(QtCore.Qt.WindowType.WindowMinimizeButtonHint, True)
        self.setWindowFlag(QtCore.Qt.WindowType.WindowMaximizeButtonHint, True)
        self.setWindowTitle("Новый проект")
        self.resize(1100, 720)
        self._qt_entry = qt_entry
        self._on_open_project = on_open_project

        # состояния
        self._program_dir = program_dir
        self._waifu_base = self._program_dir / "waifu2x" / "temp_images"
        self._waifu_in = self._waifu_base / "in"
        self._waifu_out = self._waifu_base / "out"

        self._opened_images_pil: list[Image.Image] = []
        self._current_images_pil: list[Image.Image] = []
        self._stitch_cut_positions: list[int] = []
        self._batch_nodes_window: Optional[BatchProcessingNodesWindow] = None

        self._driver = None
        self._tmp_profile_dir = None
        # Сразу стартуем в максимизированном состоянии, чтобы не зависеть от менеджера окон
        self.setWindowState(self.windowState() | QtCore.Qt.WindowState.WindowMaximized)
        self._build_ui()

        # Применяем тему из настроек
        apply_theme(self)

    # ---------- UI ----------
    def _build_ui(self):
        root = QtWidgets.QHBoxLayout(self)

        # === левая панель-обёртка (прогресс + скролл) ===
        leftPanel = QtWidgets.QWidget(self)
        leftPanelLay = QtWidgets.QVBoxLayout(leftPanel)
        leftPanelLay.setContentsMargins(0, 0, 0, 0)

        # === Общий прогресс (в самом верху, ВНЕ scroll) ===
        self.lblProgress = QtWidgets.QLabel("", leftPanel)
        leftPanelLay.addWidget(self.lblProgress)

        # === прогресс + кнопка "Гайд" в одной строке ===
        pbRow = QtWidgets.QHBoxLayout()

        self.pb = QtWidgets.QProgressBar(leftPanel)
        # чуть пониже/аккуратнее
        self.pb.setMaximumHeight(18)
        self.pb.setSizePolicy(
            QtWidgets.QSizePolicy.Policy.Expanding,
            QtWidgets.QSizePolicy.Policy.Fixed,
        )
        pbRow.addWidget(self.pb, 1)  # берёт всё доступное место

        self.btnGuide = QtWidgets.QPushButton("Гайд", leftPanel)
        # узкая кнопка, но по высоте как прогресс
        self.btnGuide.setMaximumWidth(60)
        self.btnGuide.setMaximumHeight(18)
        self.btnGuide.clicked.connect(self._on_guide_clicked)

        pbRow.addWidget(self.btnGuide)

        leftPanelLay.addLayout(pbRow)

        self.btnBatchProcessing = QtWidgets.QPushButton("Массовая обработка", leftPanel)
        self.btnBatchProcessing.setToolTip("Открыть тестовое окно массовой обработки на основе узлов")
        self.btnBatchProcessing.clicked.connect(self._open_batch_processing_window)
        leftPanelLay.addWidget(self.btnBatchProcessing)

        # === левая панель (scroll) ===
        leftScroll = QtWidgets.QScrollArea(leftPanel)   # имя то же самое
        leftScroll.setWidgetResizable(True)
        leftPanelLay.addWidget(leftScroll, 1)           # scroll под прогрессом

        left = QtWidgets.QWidget()
        leftScroll.setWidget(left)
        leftLay = QtWidgets.QVBoxLayout(left)
        leftLay.setContentsMargins(8, 8, 8, 8)

        # === Группа Импорт (открытие папки) ===
        grpImport = QtWidgets.QGroupBox("Импорт", left)
        gL = QtWidgets.QVBoxLayout(grpImport)

        btnRow = QtWidgets.QHBoxLayout()
        btnOpen = QtWidgets.QPushButton("Открыть папку…", grpImport)
        btnOpen.setToolTip('''
            - Папка с упорядоченными картинками (0.png, 1.png, ...)
            - Папка сохраненной веб-страницы с главой
        ''')
        btnOpen.clicked.connect(self._on_open_folder)
        btnRow.addWidget(btnOpen)

        btnOpenFile = QtWidgets.QPushButton("Открыть файл…", grpImport)
        btnOpenFile.setToolTip('''
            - Изображения (*.png *.jpg *.jpeg *.bmp *.webp *.tif *.tiff)
            - HTML файлы сохраненной страницы (*.html *.htm)        
            - Архивы с главой (*.zip *.rar *.7z *.tar *.tar.gz *.tgz)   
        ''')
        btnOpenFile.clicked.connect(self._on_open_file)
        btnRow.addWidget(btnOpenFile)

        gL.addLayout(btnRow)

        # — новые параметры открытия папки —
        self.chkSameWidth = QtWidgets.QCheckBox("Фильтровать по одинаковой ширине (±50%)", grpImport)
        self.chkSameWidth.setChecked(True)  # как раньше — фильтр включён по умолчанию
        gL.addWidget(self.chkSameWidth)

        labExtra = QtWidgets.QLabel("Доп. имена файлов (маски * и ? поддерживаются):", grpImport)
        gL.addWidget(labExtra)
        self.edExtraNames = QtWidgets.QLineEdit(grpImport)
        self.edExtraNames.setPlaceholderText("например: resource, resource(*), scan*.*, page????, img[0-9]*.dat")
        gL.addWidget(self.edExtraNames)

        leftLay.addWidget(grpImport)

        # === Быстрый выкачиватель ===
        grpQuick = QtWidgets.QGroupBox("Быстрый выкачиватель", left)
        grpQuick.setToolTip(SUPPORTED_SITES)
        qL = QtWidgets.QVBoxLayout(grpQuick)

        self.edNaver = QtWidgets.QLineEdit(grpQuick)
        self.edNaver.setPlaceholderText("Вставьте ссылку на главу, если сайт поддерживается")
        self.edNaver.setToolTip(SUPPORTED_SITES)
        qL.addWidget(self.edNaver)

        self.btnDownload = QtWidgets.QPushButton("Загрузить главы из ссылки", grpQuick)
        self.btnDownload.setToolTip(SUPPORTED_SITES)
        self.btnDownload.clicked.connect(self._on_download)
        qL.addWidget(self.btnDownload)

        leftLay.addWidget(grpQuick)

        # === Продвинутый выкачиватель ===
        grpAdv = QtWidgets.QGroupBox("Продвинутый выкачиватель", left)
        grpAdv.setToolTip("Если глава откроется в этом браузере, значит её можно выкачать.")
        aL = QtWidgets.QGridLayout(grpAdv)
        row = 0

        # Ссылка на страницу
        aL.addWidget(QtWidgets.QLabel("Ссылка на страницу:"), row, 0, 1, 2); row += 1
        self.edAdvUrl = QtWidgets.QLineEdit(grpAdv); aL.addWidget(self.edAdvUrl, row, 0, 1, 2); row += 1

        # Выбор браузера
        aL.addWidget(QtWidgets.QLabel("Браузер:"), row, 0)
        self.cmbBrowser = QtWidgets.QComboBox(grpAdv)
        browsers = detect_available_browsers()
        self.cmbBrowser.addItems(browsers)
        aL.addWidget(self.cmbBrowser, row, 1); row += 1

        btnOpenBrowser = QtWidgets.QPushButton("Открыть в браузере", grpAdv)
        btnOpenBrowser.clicked.connect(self._adv_open_in_browser)
        aL.addWidget(btnOpenBrowser, row, 0, 1, 2); row += 1

        aL.addWidget(QtWidgets.QLabel("Убедитесь, что все картинки на сайте прогружены"), row, 0, 1, 2); row += 1

        # Разделитель для префиксов
        line = QtWidgets.QFrame(grpAdv)
        line.setFrameShape(QtWidgets.QFrame.Shape.HLine)
        line.setFrameShadow(QtWidgets.QFrame.Shadow.Sunken)
        aL.addWidget(line, row, 0, 1, 2); row += 1

        # Префиксы ссылок
        aL.addWidget(QtWidgets.QLabel("Префиксы ссылок (* — любая последовательность, ? — символ)"), row, 0, 1, 2); row += 1

        aL.addWidget(QtWidgets.QLabel("Сайт (пресет):"), row, 0)
        self.cmbSite = QtWidgets.QComboBox(grpAdv)
        self._reload_site_presets()
        aL.addWidget(self.cmbSite, row, 1); row += 1

        aL.addWidget(QtWidgets.QLabel("Префикс:"), row, 0, 1, 2); row += 1
        self.edAdvPat = QtWidgets.QLineEdit(grpAdv)
        self.edAdvPat.setText(_DEFAULT_LINK_PREFIX)
        aL.addWidget(self.edAdvPat, row, 0, 1, 2); row += 1

        # Сохранение нового префикса
        aL.addWidget(QtWidgets.QLabel("Название нового сайта:"), row, 0)
        self.edNewSiteName = QtWidgets.QLineEdit(grpAdv)
        self.edNewSiteName.setPlaceholderText("название для сохранения")
        aL.addWidget(self.edNewSiteName, row, 1); row += 1

        btnSavePrefix = QtWidgets.QPushButton("Сохранить префикс", grpAdv)
        btnSavePrefix.clicked.connect(self._save_new_prefix)
        aL.addWidget(btnSavePrefix, row, 0, 1, 2); row += 1

        # Кнопка выкачивания
        self.btnAdvFetch = QtWidgets.QPushButton("Выкачать", grpAdv)
        self.btnAdvFetch.clicked.connect(self._adv_fetch_start)
        aL.addWidget(self.btnAdvFetch, row, 0, 1, 2); row += 1

        # Обработчик изменения выбора сайта
        self.cmbSite.currentTextChanged.connect(self._on_site_preset_changed)

        leftLay.addWidget(grpAdv)

        # === Сшивание ===
        grpStitch = QtWidgets.QGroupBox("Сшивание/Нарезка", left)
        grpStitch.setToolTip("Только для вертикальных вебтунов. На постраничных (например манга) не применять.")
        sL = QtWidgets.QGridLayout(grpStitch)
        r = 0
        self.edK = QtWidgets.QLineEdit(); self.edK.setPlaceholderText("пусто = авто")
        sL.addWidget(QtWidgets.QLabel("K (кол-во частей, пусто = авто)"), r, 0); sL.addWidget(self.edK, r, 1); r += 1

        self.edHmax = QtWidgets.QLineEdit("19000")
        sL.addWidget(QtWidgets.QLabel("Hmax (лимит высоты, px)"), r, 0); sL.addWidget(self.edHmax, r, 1); r += 1

        self.edBand = QtWidgets.QLineEdit("4")
        sL.addWidget(QtWidgets.QLabel("Белая полоса: band_rows"), r, 0); sL.addWidget(self.edBand, r, 1); r += 1

        self.edTol = QtWidgets.QLineEdit("15")
        sL.addWidget(QtWidgets.QLabel("tol (допуск одноцветности)"), r, 0); sL.addWidget(self.edTol, r, 1); r += 1

        self.edR = QtWidgets.QLineEdit("5500")
        sL.addWidget(QtWidgets.QLabel("search_radius (px)"), r, 0); sL.addWidget(self.edR, r, 1); r += 1

        self.chkPreferUp = QtWidgets.QCheckBox("Сначала вверх при refine"); self.chkPreferUp.setChecked(True)
        sL.addWidget(self.chkPreferUp, r, 0, 1, 2); r += 1
        self.chkAutoCut = QtWidgets.QCheckBox("Автоматическая резка")
        self.chkAutoCut.setChecked(False)
        sL.addWidget(self.chkAutoCut, r, 0, 1, 2); r += 1

        hBtns = QtWidgets.QHBoxLayout()
        btnStitch = QtWidgets.QPushButton("Сшить/разбить"); btnStitch.clicked.connect(self._on_stitch_split)
        btnReset = QtWidgets.QPushButton("Вернуть исходное"); btnReset.clicked.connect(self._on_revert_original)
        hBtns.addWidget(btnStitch); hBtns.addWidget(btnReset)
        sL.addLayout(hBtns, r, 0, 1, 2); r += 1

        self.grpCutAsChapter = QtWidgets.QGroupBox("Нарезать как главу", grpStitch)
        self.grpCutAsChapter.setToolTip("Нарезать точно так же, как был нарезан исходник существующей главы. Например, чтобы взять звуки.")
        self.grpCutAsChapter.setCheckable(True)
        self.grpCutAsChapter.setChecked(False)
        cutOuter = QtWidgets.QVBoxLayout(self.grpCutAsChapter)
        self._cutAsChapterBody = QtWidgets.QWidget(self.grpCutAsChapter)
        cutL = QtWidgets.QGridLayout(self._cutAsChapterBody)

        row = 0
        self.cmbCutTitle = QtWidgets.QComboBox(self._cutAsChapterBody)
        self.cmbCutChapter = QtWidgets.QComboBox(self._cutAsChapterBody)
        btnCutReload = QtWidgets.QPushButton("Обновить", self._cutAsChapterBody)
        btnCutReload.setFixedWidth(90)
        btnCutReload.clicked.connect(self._reload_cut_titles)

        cutL.addWidget(QtWidgets.QLabel("Тайтл:"), row, 0)
        cutL.addWidget(self.cmbCutTitle, row, 1)
        cutL.addWidget(btnCutReload, row, 2)
        row += 1

        cutL.addWidget(QtWidgets.QLabel("Глава:"), row, 0)
        cutL.addWidget(self.cmbCutChapter, row, 1, 1, 2)
        row += 1

        btnTakeChapter = QtWidgets.QPushButton("Взять эту главу", self._cutAsChapterBody)
        btnPickFolder = QtWidgets.QPushButton("Выбрать папку", self._cutAsChapterBody)
        btnTakeChapter.clicked.connect(self._on_cut_take_chapter)
        btnPickFolder.clicked.connect(self._on_cut_pick_folder)
        cutL.addWidget(btnTakeChapter, row, 0, 1, 2)
        cutL.addWidget(btnPickFolder, row, 2)

        cutOuter.addWidget(self._cutAsChapterBody)
        self._cutAsChapterBody.setVisible(False)
        self.grpCutAsChapter.toggled.connect(self._cutAsChapterBody.setVisible)

        self._reload_cut_titles()
        self.cmbCutTitle.currentTextChanged.connect(self._reload_cut_chapters)

        sL.addWidget(self.grpCutAsChapter, r, 0, 1, 2); r += 1
        leftLay.addWidget(grpStitch)

        # === waifu2x ===
        grpW2x = QtWidgets.QGroupBox("waifu2x", left)
        grpW2x.setToolTip("Помогает убрать шум и исправить шакальное качество.")
        wL = QtWidgets.QGridLayout(grpW2x)
        r = 0
        self.edW2xPath = QtWidgets.QLineEdit()
        self.edW2xPath.setReadOnly(True)
        if _HAS_W2X_PY:
            self.edW2xPath.setText("waifu2x-ncnn-py (Python) — путь не требуется")
            self.edW2xPath.setEnabled(False)
        else:
            self.edW2xPath.setText(str(self._waifu2x_exec_path()))
        wL.addWidget(QtWidgets.QLabel("Бэкенд / путь:"), r, 0); wL.addWidget(self.edW2xPath, r, 1); r += 1

        self.cmbW2xN = QtWidgets.QComboBox(); self.cmbW2xN.addItems(["-1","0","1","2","3"]); self.cmbW2xN.setCurrentText("3")
        wL.addWidget(QtWidgets.QLabel("Шумоподавление -n"), r, 0); wL.addWidget(self.cmbW2xN, r, 1); r += 1

        self.cmbW2xS = QtWidgets.QComboBox(); self.cmbW2xS.addItems(["1","2","4","8","16","32"]); self.cmbW2xS.setCurrentText("1")
        wL.addWidget(QtWidgets.QLabel("Масштаб -s"), r, 0); wL.addWidget(self.cmbW2xS, r, 1); r += 1

        self.edW2xT = QtWidgets.QLineEdit("384")
        wL.addWidget(QtWidgets.QLabel("Tile size -t (>=32, 0=auto)"), r, 0); wL.addWidget(self.edW2xT, r, 1); r += 1

        btnRunW2x = QtWidgets.QPushButton("Прогнать через waifu2x")
        btnRunW2x.clicked.connect(self._on_run_waifu2x)
        wL.addWidget(btnRunW2x, r, 0, 1, 2); r += 1
        leftLay.addWidget(grpW2x)

        # === Сохранение ===
        grpSave = QtWidgets.QGroupBox("Сохранение", left)
        svL = QtWidgets.QGridLayout(grpSave)
        r = 0
        grpSaveBase = QtWidgets.QGroupBox("Сохранить как основу проекта", grpSave)
        grpSaveBase.setToolTip("Сохранить как главный исходник. На нем будет перевод и клин, без этого глава не откроется.")
        baseL = QtWidgets.QGridLayout(grpSaveBase)
        br = 0
        self.cmbTitles = QtWidgets.QComboBox(); self.cmbTitles.setEditable(True); self._reload_titles()
        baseL.addWidget(QtWidgets.QLabel("Тайтл:"), br, 0); baseL.addWidget(self.cmbTitles, br, 1)
        btnRe = QtWidgets.QPushButton("Обновить"); btnRe.clicked.connect(self._reload_titles)
        baseL.addWidget(btnRe, br, 2); br += 1

        self.edChapter = QtWidgets.QLineEdit()
        baseL.addWidget(QtWidgets.QLabel("Название главы:"), br, 0); baseL.addWidget(self.edChapter, br, 1, 1, 2); br += 1

        btnSaveOpen = QtWidgets.QPushButton("Сохранить и открыть"); btnSaveOpen.clicked.connect(self._on_save_and_open)
        btnSaveOpen.setToolTip("Сохранить в папку проектов как тайтл и главу, и сразу открыть в студии")
        baseL.addWidget(btnSaveOpen, br, 0, 1, 3); br += 1

        btnSaveProject = QtWidgets.QPushButton("Сохранить в проект"); btnSaveProject.clicked.connect(self._on_save_to_project)
        btnSaveProject.setToolTip("Сохранить в папку проектов как тайтл и главу")
        baseL.addWidget(btnSaveProject, br, 0, 1, 3); br += 1

        svL.addWidget(grpSaveBase, r, 0, 1, 3); r += 1

        grpSaveAlt = QtWidgets.QGroupBox("Сохранить в проект как альтернативную версию", grpSave)
        grpSaveAlt.setToolTip("Сохранить другую версию (например английскую), чтобы взять из нее что-то при клине. Например, звуки или места без водяных знаков.")
        altL = QtWidgets.QGridLayout(grpSaveAlt)
        ar = 0
        self.cmbAltTitle = QtWidgets.QComboBox(); self.cmbAltTitle.setEditable(True)
        self.cmbAltChapter = QtWidgets.QComboBox(); self.cmbAltChapter.setEditable(True)
        btnAltReTitles = QtWidgets.QPushButton("Обновить"); btnAltReTitles.clicked.connect(self._reload_alt_titles)
        btnAltReChapters = QtWidgets.QPushButton("Обновить"); btnAltReChapters.clicked.connect(self._reload_alt_chapters)

        altL.addWidget(QtWidgets.QLabel("Тайтл:"), ar, 0); altL.addWidget(self.cmbAltTitle, ar, 1)
        altL.addWidget(btnAltReTitles, ar, 2); ar += 1

        altL.addWidget(QtWidgets.QLabel("Глава:"), ar, 0); altL.addWidget(self.cmbAltChapter, ar, 1)
        altL.addWidget(btnAltReChapters, ar, 2); ar += 1

        self.edAltName = QtWidgets.QLineEdit()
        altL.addWidget(QtWidgets.QLabel("Название альтер-версии:"), ar, 0)
        altL.addWidget(self.edAltName, ar, 1, 1, 2); ar += 1

        btnSaveAlt = QtWidgets.QPushButton("Сохранить как альтер-версию")
        btnSaveAlt.clicked.connect(self._on_save_alt_version)
        altL.addWidget(btnSaveAlt, ar, 0, 1, 3); ar += 1

        self._reload_alt_titles()
        self.cmbAltTitle.currentTextChanged.connect(self._reload_alt_chapters)

        svL.addWidget(grpSaveAlt, r, 0, 1, 3); r += 1

        grpSaveInd = QtWidgets.QGroupBox("Независимое сохранение", grpSave)
        grpSaveInd.setToolTip("Просто сохранить в выбранную папку")
        indL = QtWidgets.QGridLayout(grpSaveInd)
        btnSaveFolder = QtWidgets.QPushButton("Сохранить в папку"); btnSaveFolder.clicked.connect(self._on_save_to_folder)
        btnSaveFolder.setToolTip("Просто сохранить в выбранную папку")
        indL.addWidget(btnSaveFolder, 0, 0, 1, 3)

        svL.addWidget(grpSaveInd, r, 0, 1, 3); r += 1
        leftLay.addWidget(grpSave)

        leftLay.addStretch(1)

        # правая часть — viewer
        self.viewer = VirtualizedImageView(self, tile_height=512, cache_limit=256)
        self.viewer.on_delete = self._on_delete_page

        root.addWidget(leftPanel, 1)
        root.addWidget(self.viewer, 2)

    # ---------- Бизнес-логика / обработчики ----------
    def _reload_site_presets(self):
        """Загрузить пресеты префиксов из UserConfig"""
        try:
            nested = UserConfig.NewProjectWindow.ImageUrlPrefs
        except Exception:
            nested = None

        self.cmbSite.clear()
        self.cmbSite.addItem("")  # пустой элемент (ничего не выбрано)

        if not nested:
            return

        keys = sorted(self._cfg_keys(nested))
        if keys:
            self.cmbSite.addItems(keys)

    def _on_site_preset_changed(self, site_name: str):
        """При выборе сайта подставлять его префикс в поле"""
        if not site_name:
            return
        try:
            nested = UserConfig.NewProjectWindow.ImageUrlPrefs
            pref = self._cfg_get(nested, site_name, "")
            if pref:
                self.edAdvPat.setText(pref)
        except Exception:
            # молча игнорируем — не критично для UX
            pass

    def _save_new_prefix(self):
        """Добавить/обновить префикс в UserConfig

        Логика:
        - Если поле 'Название нового сайта' НЕ пустое → создаём/перезаписываем этот сайт.
        - ИНАЧЕ, если в комбобоксе выбран существующий сайт → обновляем его префикс.
        - ИНАЧЕ просим пользователя ввести название.
        """
        prefix = (self.edAdvPat.text() or "").strip()
        new_name = (self.edNewSiteName.text() or "").strip()
        selected = (self.cmbSite.currentText() or "").strip()

        if not prefix:
            QtWidgets.QMessageBox.warning(self, "Внимание", "Введите префикс URL")
            return

        # кого сохраняем — новый или выбранный?
        target_name = new_name if new_name else selected
        if not target_name:
            QtWidgets.QMessageBox.warning(self, "Внимание",
                                          "Введите название сайта (или выберите существующий в списке).")
            return

        try:
            nested = UserConfig.NewProjectWindow.ImageUrlPrefs
            self._cfg_set(nested, target_name, prefix)
            # на некоторых реализациях NestedConfig автосохранение есть, но явный save надёжнее
            try:
                UserConfig.save()
            except Exception:
                pass

            # обновляем UI
            self._reload_site_presets()
            self.cmbSite.setCurrentText(target_name)
            if new_name:  # если добавляли новый — очистим поле ввода названия
                self.edNewSiteName.clear()

            QtWidgets.QMessageBox.information(self, "Готово",
                                              f"Префикс для сайта «{target_name}» сохранён.")
        except Exception as e:
            traceback.print_exc()
            QtWidgets.QMessageBox.critical(self, "Ошибка сохранения", str(e))

    @QtCore.pyqtSlot(str, int, int, bool)
    def _set_progress(self, text: str, cur: int, total: int, pulse: bool=False):
        self.lblProgress.setText(text)
        if pulse:
            self.pb.setRange(0, 0)
        else:
            if total > 0:
                self.pb.setRange(0, total)
                self.pb.setValue(cur)
            else:
                self.pb.setRange(0, 1)
                self.pb.setValue(0)

    # --- слоты завершения ---
    @QtCore.pyqtSlot(object)
    def _finish_download(self, pil_list):
        self.btnDownload.setEnabled(True)
        if not pil_list:
            self._set_progress("Ничего не получено", 0, 0)
        else:
            self._set_images(pil_list)
            self._set_progress("Готово", 1, 1)

    @QtCore.pyqtSlot(object, object)
    def _finish_adv(self, pil_list, err_text):
        self.btnAdvFetch.setEnabled(True)
        self.unsetCursor()
        if err_text:
            QtWidgets.QMessageBox.critical(self, "Выкачиватель", str(err_text))
        elif not pil_list:
            QtWidgets.QMessageBox.information(self, "Результат", "Подходящих ссылок не найдено или ничего не скачалось.")
        else:
            self._set_images(pil_list)
        self._set_progress("Готово", 1, 1)

    def _reload_titles(self):
        try:
            root = Path(get_projects_root())
            titles = sorted([p.name for p in root.iterdir() if p.is_dir()]) if root.exists() else []
        except Exception:
            titles = []
        self.cmbTitles.clear()
        self.cmbTitles.addItems(titles)

    def _reload_cut_titles(self):
        try:
            root = Path(get_projects_root())
            titles = sorted([p.name for p in root.iterdir() if p.is_dir()]) if root.exists() else []
        except Exception:
            titles = []
        self.cmbCutTitle.clear()
        self.cmbCutTitle.addItems(titles)
        self._reload_cut_chapters()

    def _reload_cut_chapters(self):
        title = (self.cmbCutTitle.currentText() or "").strip()
        chapters = []
        if title:
            try:
                title_dir = Path(get_projects_root()) / title
                if title_dir.exists():
                    chapters = sorted([p.name for p in title_dir.iterdir() if p.is_dir()])
            except Exception:
                chapters = []
        self.cmbCutChapter.clear()
        self.cmbCutChapter.addItems(chapters)

    def _reload_alt_titles(self):
        try:
            root = Path(get_projects_root())
            titles = sorted([p.name for p in root.iterdir() if p.is_dir()]) if root.exists() else []
        except Exception:
            titles = []
        self.cmbAltTitle.clear()
        self.cmbAltTitle.addItems(titles)
        self._reload_alt_chapters()

    def _reload_alt_chapters(self):
        title = (self.cmbAltTitle.currentText() or "").strip()
        chapters = []
        if title:
            try:
                title_dir = Path(get_projects_root()) / title
                if title_dir.exists():
                    chapters = sorted([p.name for p in title_dir.iterdir() if p.is_dir()])
            except Exception:
                chapters = []
        self.cmbAltChapter.clear()
        self.cmbAltChapter.addItems(chapters)

    def _on_open_folder(self):
        import_ops.on_open_folder(self)

    def _on_open_file(self):
        import_ops.on_open_file(self)

    def _filter_width_outliers(self, pil_list, tolerance: float = 0.5):
        return import_ops.filter_width_outliers(pil_list, tolerance=tolerance)

    def _set_images(self, images: List[Image.Image | str]):
        pil_list: List[Image.Image] = []
        for it in images or []:
            if isinstance(it, Image.Image):
                pil_list.append(it.convert('RGB'))
            else:
                try:
                    pil_list.append(Image.open(it).convert('RGB'))
                except Exception:
                    pass
        if not pil_list:
            self._opened_images_pil = []
            self._current_images_pil = []
            self._stitch_cut_positions = []
            self.viewer.set_images([])
            self._set_progress('', 0, 0)
            return

        if getattr(self, "chkSameWidth", None) is not None and self.chkSameWidth.isChecked():
            filtered, removed, bounds = self._filter_width_outliers(pil_list, tolerance=0.5)
            # 👉 если фильтр всё снес — откатываемся к исходным
            if not filtered:
                filtered, removed, bounds = pil_list, 0, None
            if removed and bounds:
                med, lo, hi = bounds
                self._set_progress(f"Отфильтровано по ширине: −{removed} (медиана={med}px, допуск [{lo}; {hi}])", 0, 0)
        else:
            filtered, removed, bounds = pil_list, 0, None
        # 👉 если фильтр всё снес — откатываемся к исходным
        if not filtered:
            filtered, removed, bounds = pil_list, 0, None

        if removed and bounds:
            med, lo, hi = bounds
            self._set_progress(f"Отфильтровано по ширине: −{removed} (медиана={med}px, допуск [{lo}; {hi}])", 0, 0)

        self._opened_images_pil = list(filtered)
        self._current_images_pil = list(filtered)
        self._stitch_cut_positions = []
        self.viewer.set_images(self._current_images_pil)
        self._set_progress('', 0, 0)


    def _on_delete_page(self, idx: int):
        """Колбэк удаления страницы - синхронизирует _current_images_pil с viewer."""
        if 0 <= idx < len(self._current_images_pil):
            self._current_images_pil.pop(idx)
            if len(self._current_images_pil) != 1:
                self._stitch_cut_positions = []
                self.viewer.clear_cut_guides()
            # НЕ вызываем set_images - viewer уже обновился сам

    # --- downloader (naver bestfree) ---
    def _on_download(self):
        downloaders.on_download(self)

    def _to_pil_list(self, obj):
        return downloaders.to_pil_list(obj)

    # --- продвинутый выкачиватель ---
    def _ensure_browser(self):
        return downloaders.ensure_browser(self)

    def _adv_open_in_browser(self):
        return downloaders.adv_open_in_browser(self)

    def _adv_fetch_start(self):
        return downloaders.adv_fetch_start(self)

    # --- waifu2x ---
    def _waifu2x_exec_path(self) -> Path:
        return waifu2x.waifu2x_exec_path(self)

    def _ensure_waifu_temp_dirs(self, clean: bool=True):
        return waifu2x.ensure_waifu_temp_dirs(self, clean=clean)

    def _save_canvas_pngs(self, dst_dir: Path) -> int:
        return save_ops.save_canvas_pngs(self, dst_dir)

    def _load_images_from_dir(self, dirpath: Path):
        return save_ops.load_images_from_dir(dirpath)

    def _on_run_waifu2x(self):
        return waifu2x.on_run_waifu2x(self)


    # --- сшивание ---
    def _pil_to_bgr(self, im: Image.Image) -> np.ndarray:
        return stitching.pil_to_bgr(im)

    def _bgr_to_pil(self, arr: np.ndarray) -> Image.Image:
        return stitching.bgr_to_pil(arr)

    def _on_stitch_split(self):
        return stitching.on_stitch_split(self)

    def _set_stitch_preview(self, tape_image: Image.Image, cut_positions: List[int]) -> None:
        self._current_images_pil = [tape_image.convert("RGB")]
        self._stitch_cut_positions = [int(v) for v in cut_positions]
        self.viewer.set_images(self._current_images_pil)
        self.viewer.set_cut_guides(
            self._stitch_cut_positions,
            on_positions_changed=self._update_stitch_cut_positions,
            on_cut_requested=self._apply_manual_stitch_cuts,
        )

    def _update_stitch_cut_positions(self, cut_positions: List[int]) -> None:
        self._stitch_cut_positions = [int(v) for v in cut_positions]

    def _clear_stitch_preview(self) -> None:
        self._stitch_cut_positions = []
        self.viewer.clear_cut_guides()

    def _apply_manual_stitch_cuts(self) -> None:
        return stitching.apply_manual_cuts(self)

    def _on_revert_original(self):
        return stitching.on_revert_original(self)

    def _on_cut_take_chapter(self):
        return stitching.on_cut_take_chapter(self)

    def _on_cut_pick_folder(self):
        return stitching.on_cut_pick_folder(self)

    def _cut_like_chapter(self, ref_images: list[Image.Image]):
        return stitching.cut_like_chapter(self, ref_images)

    # --- сохранение ---
    def _prepare_project_dirs(self, title: str, chapter: str) -> tuple[Path, Path]:
        return save_ops.prepare_project_dirs(title, chapter)

    def _clear_dir_contents(self, dst_dir: Path) -> None:
        return save_ops.clear_dir_contents(dst_dir)

    def _confirm_overwrite_nonempty(self, dst_dir: Path) -> bool:
        return save_ops.confirm_overwrite_nonempty(self, dst_dir)

    def _save_project(self, open_after: bool):
        return save_ops.save_project(self, open_after=open_after)

    def _on_save_and_open(self):
        return save_ops.on_save_and_open(self)

    def _on_save_to_project(self):
        return save_ops.on_save_to_project(self)

    def _on_save_alt_version(self):
        return save_ops.on_save_alt_version(self)

    def _on_save_to_folder(self):
        return save_ops.on_save_to_folder(self)

    # --------- helpers для безопасной работы с UserConfig ---------
    def _cfg_keys(self, nested) -> list[str]:
        try:
            if hasattr(nested, "_data") and isinstance(nested._data, dict):
                return list(nested._data.keys())
        except Exception:
            pass
        # запасной путь
        try:
            return [k for k in dir(nested) if not k.startswith("_") and isinstance(getattr(nested, k), (str, bytes))]
        except Exception:
            return []

    def _cfg_get(self, nested, key: str, default: str | None = None) -> str | None:
        try:
            if hasattr(nested, "_data") and isinstance(nested._data, dict):
                return nested._data.get(key, default)
        except Exception:
            pass
        try:
            # пробуем как атрибут
            if hasattr(nested, key):
                return getattr(nested, key)
        except Exception:
            pass
        try:
            # пробуем как маппинг
            return nested[key]  # type: ignore[index]
        except Exception:
            return default

    def _cfg_set(self, nested, key: str, value: str) -> None:
        # предпочитаем публичный путь (атрибут/маппинг); на _data не полагаемся для записи
        try:
            setattr(nested, key, value)
        except Exception:
            try:
                nested[key] = value  # type: ignore[index]
            except Exception:
                # самый последний шанс — прямо в _data
                if hasattr(nested, "_data") and isinstance(nested._data, dict):
                    nested._data[key] = value
                else:
                    raise

    def _on_guide_clicked(self):
        # Например, wiki/Guide.md рядом с основным скриптом
        guide_path = os.path.join(program_dir, "wiki", "Окно-Новый-проект.md")
        # или просто:
        # guide_path = r"D:\path\to\your\guide.md"

        widget, title = create_markdown_widget(guide_path)

        # Создаём отдельное окно для гайда
        self._guide_window = QtWidgets.QMainWindow(self)
        self._guide_window.setWindowFlag(QtCore.Qt.WindowType.Window, True)
        self._guide_window.setAttribute(QtCore.Qt.WidgetAttribute.WA_DeleteOnClose)
        self._guide_window.setWindowTitle(f"Гайд — {title}")
        self._guide_window.setCentralWidget(widget)
        self._guide_window.showMaximized()
        self._guide_window.show()

    def _open_batch_processing_window(self):
        window = self._batch_nodes_window
        if window is None or not window.isVisible():
            window = BatchProcessingNodesWindow(self)
            window.setAttribute(QtCore.Qt.WidgetAttribute.WA_DeleteOnClose, True)
            window.destroyed.connect(lambda *_: setattr(self, "_batch_nodes_window", None))
            self._batch_nodes_window = window
            window.show()
        window.raise_()
        window.activateWindow()


# ---------- Публичная точка входа ----------
def show_new_project_full(
    parent: QtWidgets.QWidget | None = None,
    *,
    qt_entry: str = "qt_runner.py",
    on_open_project=None,
) -> NewProjectWindow:
    dlg = NewProjectWindow(
        parent,
        qt_entry=qt_entry,
        on_open_project=on_open_project,
    )
    dlg.setModal(True)
    dlg.show()
    dlg.raise_()
    dlg.activateWindow()
    return dlg

# ---------- Самостоятельный запуск ----------
def main():
    app = QtWidgets.QApplication.instance() or QtWidgets.QApplication(sys.argv)
    dlg = NewProjectWindow(
        None,
        qt_entry="qt_runner.py",
        on_open_project=None,
    )
    dlg.show()
    sys.exit(app.exec())

if __name__ == "__main__":
    main()
