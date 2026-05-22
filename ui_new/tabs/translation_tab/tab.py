from __future__ import annotations
import os, glob, traceback
from typing import Dict, List, Optional

from PyQt6.QtCore import QTimer
from PyQt6.QtWidgets import QWidget, QVBoxLayout, QPushButton

from modules.utils_qt import safe_disconnect
from .utils import _is_deleted, ImageLike
from .canvas import TranslationCanvasView
from .panels.settings import SettingsPanel
from .panels.bubbles import BubblesPanel
from .panels.composition import CompositionPanel
from .panels.text_detector import TextDetectorPanel
from .panels.machine_translation import MachineTranslationPanel

class TranslationTab(QWidget):
    """
    Основной виджет вкладки Translation.

    Организует взаимодействие между Canvas и двумя панелями (настройки OCR и список пузырей).
    После рефакторинга логика разделена на отдельные компоненты, но публичный API не изменился.
    """

    # Инициализация виджета и создание дочерних компонентов
    def __init__(
        self,
        project,
        bubbles_model=None,
        source_images: Optional[List[ImageLike]] = None,
        text_detection_model=None,
        user_config=None,
        ai_device=None,
    ):
        super().__init__()
        self.project = project
        self.model = bubbles_model
        self.text_detection_model = text_detection_model
        self.ai_device = ai_device

        images = source_images if source_images is not None else self._collect_images_from_src(self.project)

        self._root = QVBoxLayout(self)
        self._root.setContentsMargins(0, 0, 0, 0)
        self._root.setSpacing(0)

        self.canvas = TranslationCanvasView(
            self.project,
            images,
            parent=self,
            bubbles_model=self.model,
            text_detection_model=self.text_detection_model,
            user_config=user_config,
        )
        self.canvas.ai_device = ai_device
        self._canvas_ref = self.canvas
        self._root.addWidget(self.canvas)

        # кнопки вызова панелей
        self.btn_bubbles = QPushButton("Пузыри", self)
        self.btn_settings = QPushButton("OCR", self)
        self.btn_composition = QPushButton("Компоновка", self)
        self.btn_machine_translation = QPushButton("Маш. перевод", self)
        self.btn_text_detector = QPushButton("Детектор текста", self)
        self.btn_settings.setFixedSize(55, 25)
        self.btn_bubbles.setFixedSize(75, 25)
        self.btn_composition.setFixedHeight(25)
        self.btn_machine_translation.setFixedHeight(25)
        self.btn_text_detector.setFixedSize(120, 25)

        # панели (выделены в отдельные классы)
        self.panel = SettingsPanel(parent=self, canvas=self.canvas, project_settings=project.settings)
        self.canvas.set_settings_panel(self.panel)
        self._bubbles_panel = BubblesPanel(parent=self, project=self.project, canvas=self.canvas, model=self.model)
        self._composition_panel = CompositionPanel(parent=self, project=self.project, canvas=self.canvas, model=self.model)
        self._text_detector_panel = TextDetectorPanel(parent=self, canvas=self.canvas)
        self._machine_translation_panel = MachineTranslationPanel(parent=self, project=self.project, canvas=self.canvas, model=self.model)

        # toggle
        self.btn_settings.clicked.connect(self._toggle_settings_panel)
        self.btn_bubbles.clicked.connect(self._toggle_bubbles_panel)
        self.btn_composition.clicked.connect(self._toggle_composition_panel)
        self.btn_machine_translation.clicked.connect(self._toggle_machine_translation_panel)
        self.btn_text_detector.clicked.connect(self._toggle_text_detector_panel)

        # синхронизация списка пузырей
        self.canvas.bubblesChanged.connect(self._schedule_rebuild_bubbles_list)

        # автоадаптация геометрии
        self._resize_timer = QTimer(self)
        self._resize_timer.setInterval(50)
        self._resize_timer.setSingleShot(True)
        self._resize_timer.timeout.connect(self._sync_panels_geometry)
        self.resizeEvent = self._wrap_resize(self.resizeEvent)  # type: ignore
        self._position_buttons()

    # Безопасное закрытие с отключением сигналов
    def closeEvent(self, ev):
        try:
            if self.canvas:
                safe_disconnect(self.canvas, "bubblesChanged", self._schedule_rebuild_bubbles_list)
        except Exception:
            traceback.print_exc()
        super().closeEvent(ev)

    # Делегирование обновления списка пузырей в соответствующую панель
    def _schedule_rebuild_bubbles_list(self, *args):
        panel = getattr(self, "_bubbles_panel", None)
        if not panel:
            return
        reason = args[0] if len(args) > 0 else None
        bid = args[1] if len(args) > 1 else None
        if panel.isVisible() and reason in ("place", "unplace") and bid is not None:
            panel.rebuild_card(int(bid))
            return
        panel.schedule_rebuild()

    # Вспомогательные методы
    def _wrap_resize(self, base):
        def _res(ev):
            base(ev)
            self._resize_timer.start()
            self._position_buttons()
        return _res

    def _collect_images_from_src(self, project) -> List[ImageLike]:
        cleaned = getattr(project, "src_dir", None)
        if not cleaned or not os.path.isdir(cleaned):
            return []
        files: List[str] = []
        files.extend(glob.glob(os.path.join(cleaned, "*.png")))
        files.extend(glob.glob(os.path.join(cleaned, "*.jpg")))
        files.extend(glob.glob(os.path.join(cleaned, "*.jpeg")))
        return files
    
    # Управление видимостью и геометрией панелей
    def _toggle_settings_panel(self):
        if self.panel.isVisible():
            self.panel.hide()
        else:
            self.panel.show()
            self._bubbles_panel.hide()
            self._composition_panel.hide()
            self._machine_translation_panel.hide()
            self._text_detector_panel.hide()
            self._sync_panels_geometry()

    def _toggle_bubbles_panel(self):
        if self._bubbles_panel.isVisible():
            self._bubbles_panel.hide()
        else:
            self._bubbles_panel.show()
            self.panel.hide()
            self._composition_panel.hide()
            self._machine_translation_panel.hide()
            self._text_detector_panel.hide()
            self._sync_panels_geometry()
            self._bubbles_panel.rebuild_now()  # немедленно обновим список

    def _toggle_composition_panel(self):
        if self._composition_panel.isVisible():
            self._composition_panel.hide()
        else:
            self._composition_panel.show()
            self.panel.hide()
            self._bubbles_panel.hide()
            self._machine_translation_panel.hide()
            self._text_detector_panel.hide()
            self._sync_panels_geometry()
            self._composition_panel.rebuild_now()  # немедленно обновим список

    def _toggle_machine_translation_panel(self):
        if self._machine_translation_panel.isVisible():
            self._machine_translation_panel.hide()
        else:
            self._machine_translation_panel.show()
            self.panel.hide()
            self._bubbles_panel.hide()
            self._composition_panel.hide()
            self._text_detector_panel.hide()
            self._sync_panels_geometry()

    def _toggle_text_detector_panel(self):
        if self._text_detector_panel.isVisible():
            self._text_detector_panel.hide()
        else:
            self._text_detector_panel.show()
            self.panel.hide()
            self._bubbles_panel.hide()
            self._composition_panel.hide()
            self._machine_translation_panel.hide()
            self._sync_panels_geometry()

    def _sync_panels_geometry(self):
        margin = 12
        w = max(260, int(self.width() * 0.28))
        x = self.width() - w - margin
        y = margin
        h = self.height() - 2 * margin

        self._position_buttons()

        if self.panel.isVisible():
            self.panel.setGeometry(x, y, w, h)
        if self._bubbles_panel.isVisible():
            self._bubbles_panel.setGeometry(x, y, max(300, w), h)
        if self._composition_panel.isVisible():
            self._composition_panel.setGeometry(x, y, max(400, w), h)
        if self._machine_translation_panel.isVisible():
            self._machine_translation_panel.setGeometry(x, y, max(380, w), h)
        if self._text_detector_panel.isVisible():
            self._text_detector_panel.setGeometry(x, y, max(320, w), h)

    def _cv(self):
        cv = getattr(self, "_canvas_ref", None)
        if cv is None or _is_deleted(cv):
            return None
        return cv

    def _position_buttons(self):
        """Размещает кнопки управления панелями в правом нижнем углу canvas."""
        margin = 16
        btn_h = 12

        x = self.canvas.width() - self.btn_settings.width() - margin
        y = self.canvas.height() - btn_h - margin

        self.btn_settings.move(x, y)
        x -= self.btn_composition.width() + 6
        self.btn_composition.move(x, y)
        x -= self.btn_machine_translation.width() + 6
        self.btn_machine_translation.move(x, y)
        x -= self.btn_text_detector.width() + 6
        self.btn_text_detector.move(x, y)
        x -= self.btn_bubbles.width() + 6
        self.btn_bubbles.move(x, y)
