from __future__ import annotations
import json
import os
import traceback
from typing import Callable, Dict, Iterable, List, Optional, Tuple

import cv2
from PyQt6.QtCore import Qt, QRectF, QPointF, QTimer
from PyQt6.QtGui import QImage, QPixmap, QGuiApplication, QColor, QPen, QPolygonF, QTransform
from PyQt6.QtWidgets import (
    QGraphicsItemGroup, QGraphicsPixmapItem, QGraphicsPolygonItem, QGraphicsRectItem,
    QWidget, QHBoxLayout, QPushButton, QCheckBox, QComboBox, QLineEdit, QSpinBox, QLabel
)
import numpy as np

from ui_new.canvas_view import CanvasView
from modules.utils_qt import qobj_alive

# Утилиты для работы с изображениями
from .utils import _qimage_from_any, _is_deleted, ImageLike
from .ocr import create_engines
additional_names = ["Подпись","Звук", "ГГ", "Мысли ГГ",
                    "Непонятно", "Кто-то", "Кто-то из них",
                    "Какая-то девочка", "Какой-то мальчик", "Какой-то парень", "Какая-то девушка", "Какая-то женщина", "Какой-то мужчина"]


class CharacterComboBox(QComboBox):
    """QComboBox, который уведомляет Canvas при открытии/закрытии popup."""

    def __init__(self, bid: int, on_popup_toggled: Callable[[int, bool], None], parent: Optional[QWidget] = None):
        super().__init__(parent)
        self._bid = int(bid)
        self._on_popup_toggled = on_popup_toggled

    def showPopup(self) -> None:
        try:
            self._on_popup_toggled(self._bid, True)
        except RuntimeError:
            return
        except Exception:
            return
        super().showPopup()

    def hidePopup(self) -> None:
        super().hidePopup()
        try:
            self._on_popup_toggled(self._bid, False)
        except Exception:
            pass


class TranslationCanvasView(CanvasView):
    """
    Canvas для вкладки Translation с поддержкой OCR и создания пузырей.

    Расширяет базовый CanvasView функциональностью:
    - Выделение областей через Shift+drag для OCR
    - Интеграция с EasyOCR, PaddleOCR и MangaOCR
    - Автоматическое создание пузырей с распознанным текстом
    """

    HOTKEYS = "Распознавание: Shift+ЛКМ • Пузырь: T • Удалить: Del • Зум: Ctrl±, Ctrl0"


    def __init__(
        self,
        project,
        images,
        parent=None,
        bubbles_model=None,
        text_detection_model=None,
        user_config=None,
    ):
        # ВАЖНО: Загрузка списка персонажей ДО вызова super().__init__,
        # т.к. базовый класс вызывает _load_bubbles_from_project -> _create_bubble_widget -> build_bubble_footer
        self._character_names = self._load_character_names_static(project)

        # Отслеживание последних значений роли и уточнения для новых пузырей
        self._last_is_known_character: bool = True
        self._last_character_name: str = ''
        self._last_clarification: str = ''
        # Отслеживание последней страницы и номера реплики для автонумерации
        self._last_page_idx: int = -1
        self._last_bubble_order: int = -1

        super().__init__(
            project,
            images,
            editable=True,
            parent=parent,
            bubbles_model=bubbles_model,
            user_config=user_config,
        )

        # визуальная подсказка снизу
        if hasattr(self, "_hotkeysLabel"):
            self._hotkeysLabel.setText(self.HOTKEYS)

        # результаты детектора текста (маски/полигоны) для отрисовки поверх страниц
        self._textdetector_results: Dict[int, dict] = {}
        self._textdetector_groups: List[Optional[QGraphicsItemGroup]] = []
        self._textdetector_group_sizes: List[Optional[Tuple[int, int]]] = []
        self._textdetector_mask_alpha: int = 90  # прозрачно-красная маска поверх текста
        self._textdetector_draw_lines: bool = True
        self._textdetector_draw_mask: bool = True
        self._textdetector_block_expand_px: int = 0
        self._textdetector_merge_gap_px: int = 0
        self._textdetector_merge_nearby: bool = False
        self._textdet_model = None
        self._textdet_model_updating = False

        # прямоугольник выделения
        self._sel_active: bool = False
        self._sel_origin: QPointF = QPointF()
        self._sel_item = None  # QGraphicsRectItem (создадим по требованию)
        self._sel_last_rect: QRectF = QRectF()

        # настройки OCR (будут прокинуты из панели)
        self.ocr_engine: str = "none"        # 'none' | ключ из движков
        self.join_newlines: bool = True
        self.post_copy: bool = True
        self.post_bubble: bool = True
        self.post_reflect_strings: bool = False

        # Движки OCR (lazy init внутри каждого)
        self.ocr_engines = create_engines(self)
        self._settings_panel = None
        self._last_ocr_notice_state: str | None = None
        self._ocr_notice = QLabel(self.viewport())
        self._ocr_notice.setVisible(False)
        self._ocr_notice.setWordWrap(True)
        self._ocr_notice.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self._ocr_notice.setStyleSheet(
            "QLabel {"
            "background: #f2c94c;"
            "color: #1f1f1f;"
            "border: 2px solid #a67c00;"
            "border-radius: 10px;"
            "padding: 12px 18px;"
            "font-weight: 700;"
            "font-size: 14px;"
            "}"
        )
        self._ocr_notice.setAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents, True)
        self._ocr_notice_timer = QTimer(self)
        self._ocr_notice_timer.setSingleShot(True)
        self._ocr_notice_timer.timeout.connect(self._hide_ocr_notice)
        self._pending_ocr_selection_rect: QRectF | None = None

        # Инициализируем последние значения из существующих пузырей
        self._init_last_bubble_values()
        self._attach_textdet_model(text_detection_model)

    def set_settings_panel(self, panel):
        self._settings_panel = panel
        if panel is not None and hasattr(panel, "ocrStateChanged"):
            try:
                panel.ocrStateChanged.connect(self._on_ocr_state_changed)
            except Exception:
                traceback.print_exc()

    def resizeEvent(self, e):
        super().resizeEvent(e)
        self._position_ocr_notice()

    def _reflow_after_resize(self):
        super()._reflow_after_resize()
        self._sync_textdetector_geom()

    # -------- интеграция с моделью результатов детекции --------
    def _attach_textdet_model(self, model):
        self._textdet_model = model
        if model is None:
            return
        try:
            model.resultChanged.connect(self._on_textdet_model_changed)
            model.cleared.connect(self._on_textdet_model_cleared)
            model.reset.connect(self._on_textdet_model_reset)
            model.optionsChanged.connect(lambda _: self._sync_options_from_model() or self._rebuild_textdetector_items() or self._sync_textdetector_geom())
        except Exception:
            traceback.print_exc()
        self._on_textdet_model_reset()

    def _on_textdet_model_changed(self, idx: int):
        if self._textdet_model_updating:
            return
        if self._textdet_model is None:
            return
        res = self._textdet_model.get(idx)
        if res is None:
            self._textdetector_results.pop(idx, None)
        else:
            self._textdetector_results[idx] = res
        self._rebuild_textdetector_items()
        self._sync_textdetector_geom()

    def _on_textdet_model_cleared(self, idx: int):
        if self._textdet_model_updating:
            return
        self._textdetector_results.pop(idx, None)
        self._rebuild_textdetector_items()
        self._sync_textdetector_geom()

    def _on_textdet_model_reset(self):
        if self._textdet_model_updating or self._textdet_model is None:
            return
        self._textdetector_results = self._textdet_model.as_dict()
        self._rebuild_textdetector_items()
        self._sync_textdetector_geom()
        self._sync_options_from_model()

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

    def current_page_index(self) -> int:
        """Публичная обертка над расчётом видимого изображения."""
        return super()._current_page_idx()

    def mousePressEvent(self, e):
        if e.modifiers() & Qt.KeyboardModifier.ShiftModifier:
            # старт выделения, только если клик по картинке
            scene_pt = self.mapToScene(e.pos())
            img_idx = self._find_image_at_point(scene_pt)
            if img_idx is not None:
                self._sel_active = True
                self._sel_origin = scene_pt
                self._ensure_sel_item()
                self._update_selection_rect(self._sel_origin, self._sel_origin)
                e.accept()
                return
        # иначе — обычная логика (включая перенос пузыря)
        super().mousePressEvent(e)

    def mouseMoveEvent(self, e):
        if self._sel_active:
            scene_pt = self.mapToScene(e.pos())
            self._update_selection_rect(self._sel_origin, scene_pt)
            e.accept()
            return
        super().mouseMoveEvent(e)

    def mouseReleaseEvent(self, e):
        if self._sel_active:
            self._sel_active = False
            rect = QRectF(self._sel_item.rect())
            self._sel_item.setVisible(False)
            # запускаем OCR по выделению
            self._run_ocr_on_selection(rect)
            e.accept()
            return
        super().mouseReleaseEvent(e)

    def _update_selection_rect(self, p1: QPointF, p2: QPointF):
        r = QRectF(p1, p2).normalized()
        self._sel_item.setRect(r)
        self._sel_last_rect = r

    def _find_image_at_point(self, pt: QPointF) -> Optional[int]:
        for i, r in enumerate(getattr(self, "image_bboxes", [])):
            if r.contains(pt):
                return i
        return None

    # Создание графического элемента для отображения выделения
    def _ensure_sel_item(self):
        if getattr(self, "_sel_item", None) is None:
            self._sel_item = QGraphicsRectItem()
            pen = QPen(QColor(0, 160, 255), 2, Qt.PenStyle.DashLine)
            self._sel_item.setPen(pen)
            self._sel_item.setBrush(QColor(0, 160, 255, 60))
            self._sel_item.setZValue(9999.0)
            self.scene.addItem(self._sel_item)
        self._sel_item.setVisible(True)

    def _build_ocr_task_for_scene_rect(self, scene_rect: QRectF) -> Optional[dict]:
        """Готовит задачу OCR для области в координатах сцены."""
        target_idx = None
        best_area = 0.0
        for i, rb in enumerate(self.image_bboxes):
            inter = rb.intersected(scene_rect)
            area = max(0.0, inter.width() * inter.height())
            if area > best_area:
                best_area = area
                target_idx = i
        if target_idx is None or best_area <= 0.0:
            return None

        img_bbox = self.image_bboxes[target_idx]
        crop_scene = img_bbox.intersected(scene_rect)
        if crop_scene.isNull():
            return None

        src_item = self.images[target_idx]
        qimg = _qimage_from_any(src_item)
        if qimg.isNull():
            return None

        u1 = (crop_scene.left() - img_bbox.left()) / max(1.0, img_bbox.width())
        v1 = (crop_scene.top() - img_bbox.top()) / max(1.0, img_bbox.height())
        u2 = (crop_scene.right() - img_bbox.left()) / max(1.0, img_bbox.width())
        v2 = (crop_scene.bottom() - img_bbox.top()) / max(1.0, img_bbox.height())

        x1 = int(round(self._clip01(u1) * (qimg.width())))
        y1 = int(round(self._clip01(v1) * (qimg.height())))
        x2 = int(round(self._clip01(u2) * (qimg.width())))
        y2 = int(round(self._clip01(v2) * (qimg.height())))

        if x2 <= x1 or y2 <= y1:
            return None

        crop = qimg.copy(x1, y1, x2 - x1, y2 - y1)
        if crop.isNull():
            return None

        return {
            "target_idx": int(target_idx),
            "crop_scene": QRectF(crop_scene),
            "crop": crop,
        }

    def _apply_ocr_text_to_scene_rect(self, target_idx: int, crop_scene: QRectF, text: str):
        """Применяет OCR-результат к UI (буфер, пузырь, текст)."""
        if not text:
            return

        if self.post_copy:
            try:
                QGuiApplication.clipboard().setText(text)
            except Exception:
                traceback.print_exc()

        if self.post_bubble:
            cx = crop_scene.center().x()
            cy = crop_scene.center().y()
            bid = self.create_bubble(target_idx, float(cx), float(cy))
            try:
                u1, v1 = self._uv_from_scene(target_idx, crop_scene.left(), crop_scene.top())
                u2, v2 = self._uv_from_scene(target_idx, crop_scene.right(), crop_scene.bottom())
                rect_coords = {
                    'p1': {'img_u': min(u1, u2), 'img_v': min(v1, v2)},
                    'p2': {'img_u': max(u1, u2), 'img_v': max(v1, v2)},
                }
                self._set_bubble_rect_coords(bid, rect_coords, update_model=True)
            except Exception:
                traceback.print_exc()
            b = self.bubbles.get(bid)
            if b and b.original_text_widget:
                b.original_text_widget.blockSignals(True)
                b.original_text_widget.setPlainText(text)
                b.original_text_widget.blockSignals(False)
                try:
                    self._adjust_box(bid, update_model=True)
                except Exception:
                    traceback.print_exc()

    def collect_ocr_tasks_for_detected_blocks(self, indices: Iterable[int]) -> List[dict]:
        """Собирает OCR-задачи для найденных блоков на указанных страницах."""
        tasks: List[dict] = []
        if not self.is_ocr_ready():
            return tasks
        for idx in indices:
            try:
                rects = self.get_detected_block_rects(int(idx))
            except Exception:
                traceback.print_exc()
                continue
            if not rects:
                continue
            for rect in rects:
                task = self._build_ocr_task_for_scene_rect(rect)
                if task:
                    tasks.append(task)
        return tasks

    # Выполнение OCR на выделенной области
    def _run_ocr_on_selection(self, scene_rect: QRectF):
        if not self._ensure_ocr_ready_for_selection():
            panel = getattr(self, "_settings_panel", None)
            state = str(panel.ocr_state()) if panel is not None and hasattr(panel, "ocr_state") else ""
            if state == "loading":
                # OCR загружается в фоне: запомним выделение и повторим после готовности.
                self._pending_ocr_selection_rect = QRectF(scene_rect)
            return
        self._pending_ocr_selection_rect = None

        task = self._build_ocr_task_for_scene_rect(scene_rect)
        if task is None:
            return
        text = self._perform_ocr(task["crop"])

        if not text:
            return
        self._apply_ocr_text_to_scene_rect(
            int(task["target_idx"]),
            QRectF(task["crop_scene"]),
            text,
        )

    def _ensure_ocr_ready_for_selection(self) -> bool:
        if self.is_ocr_ready():
            self._last_ocr_notice_state = None
            return True

        state = "error"
        panel = getattr(self, "_settings_panel", None)
        if panel is not None and hasattr(panel, "ensure_ocr_from_config"):
            try:
                state = str(panel.ensure_ocr_from_config())
            except Exception:
                traceback.print_exc()
                state = "error"
        else:
            state = "none" if self.ocr_engine == "none" else "error"

        if state == "ok" and self.is_ocr_ready():
            self._last_ocr_notice_state = None
            return True

        self._notify_ocr_state(state)
        return False

    def _notify_ocr_state(self, state: str):
        if self._last_ocr_notice_state == state:
            return
        self._last_ocr_notice_state = state

        if state == "loading":
            self._show_ocr_notice("OCR загружается...\nПодождите завершения и попробуйте снова.", keep_visible=True)
            return

        if state in ("none", "idle"):
            self._show_ocr_notice("Нужно настроить OCR", keep_visible=False, timeout_ms=2200)
            return

        self._show_ocr_notice("Ошибка загрузки OCR", keep_visible=False, timeout_ms=2500)

    def _on_ocr_state_changed(self, state: str):
        if state == "ok":
            self._last_ocr_notice_state = None
            self._hide_ocr_notice()
            pending_rect = self._pending_ocr_selection_rect
            self._pending_ocr_selection_rect = None
            if pending_rect is not None and not pending_rect.isNull():
                QTimer.singleShot(0, lambda rect=QRectF(pending_rect): self._run_ocr_on_selection(rect))
            return
        if state == "loading":
            self._notify_ocr_state("loading")
            return
        if state == "error":
            self._pending_ocr_selection_rect = None
            self._notify_ocr_state("error")
            return
        if state == "idle" and self.ocr_engine == "none":
            self._pending_ocr_selection_rect = None
            self._notify_ocr_state("none")

    def _show_ocr_notice(self, text: str, keep_visible: bool, timeout_ms: int = 0):
        self._ocr_notice_timer.stop()
        self._ocr_notice.setText(text)
        self._ocr_notice.adjustSize()
        self._position_ocr_notice()
        self._ocr_notice.show()
        self._ocr_notice.raise_()
        if not keep_visible:
            self._ocr_notice_timer.start(max(1, int(timeout_ms or 2000)))

    def _hide_ocr_notice(self):
        self._ocr_notice_timer.stop()
        self._ocr_notice.hide()

    def _position_ocr_notice(self):
        if not self._ocr_notice:
            return
        vp = self.viewport()
        size = self._ocr_notice.sizeHint()
        w = min(max(size.width(), 240), max(240, vp.width() - 40))
        self._ocr_notice.setFixedWidth(w)
        self._ocr_notice.adjustSize()
        x = max(12, (vp.width() - self._ocr_notice.width()) // 2)
        y = max(12, (vp.height() - self._ocr_notice.height()) // 2)
        self._ocr_notice.move(x, y)

    # Инициализация и управление OCR движками
    def _current_engine_obj(self):
        return self.ocr_engines.get(self.ocr_engine)

    def _perform_ocr(self, crop_qimage: QImage) -> str:
        if self.ocr_engine == "none":
            return ""

        engine = self._current_engine_obj()
        if engine is None:
            return ""

        try:
            if not engine.ensure_loaded():
                return ""
            return engine.recognize(
                crop_qimage,
                join_newlines=self.join_newlines,
                reflect_strings=self.post_reflect_strings,
            )
        except Exception:
            traceback.print_exc()
            return ""

    def is_ocr_ready(self) -> bool:
        """Проверяет, выбран ли OCR и был ли он загружен."""
        engine = self._current_engine_obj()
        if engine is None:
            return False
        if getattr(engine, "_loaded", False):
            return True
        return False

    # Вспомогательные методы для работы с координатами
    @staticmethod
    def _clip01(v: float) -> float:
        return 0.0 if v < 0.0 else (1.0 if v > 1.0 else v)

    def _init_last_bubble_values(self):
        """
        Инициализирует последние значения роли и уточнения из существующих пузырей.
        Берет значения из последнего созданного пузыря (с максимальным id).
        """
        if not self.project.bubbles:
            return

        try:
            # Находим пузырь с максимальным id
            last_bubble = max(self.project.bubbles, key=lambda b: int(b.get('id', 0)))

            # Инициализируем последние значения
            self._last_is_known_character = last_bubble.get('is_known_character', True)
            self._last_character_name = last_bubble.get('character_name', '')
            self._last_clarification = last_bubble.get('clarification', '')

            # Инициализируем автонумерацию
            self._last_page_idx = int(last_bubble.get('img_idx', -1))
            self._last_bubble_order = int(last_bubble.get('bubble_order', -1))

        except Exception as e:
            print(f"[TranslationCanvasView] Не удалось инициализировать последние значения: {e}")
            traceback.print_exc()

    def create_bubble(self, img_idx: int, x: float, y: float) -> int:
        """
        Переопределенный метод создания пузыря с использованием последних значений
        роли и уточнения из предыдущего созданного пузыря.
        """
        bid = self.bubble_count + 1
        r = self.image_bboxes[img_idx]
        side = "left" if x < (r.left()+r.right())/2.0 else "right"
        u, v = self._uv_from_scene(img_idx, x, y)
        if self._last_page_idx == img_idx and self._last_bubble_order >= 0:
            bubble_order = self._last_bubble_order + 1
        else:
            bubble_order = 0
        # Используем сохраненные последние значения вместо дефолтных
        rec = {
            'id': bid,
            'img_idx': img_idx,
            'img_u': float(u),
            'img_v': float(v),
            'side': side,
            'text': '',
            'original_text': '',
            'translation_status': 'untranslated',
            'is_known_character': self._last_is_known_character,
            'character_name': self._last_character_name,
            'clarification': self._last_clarification,
            'bubble_order': bubble_order,
        }

        # 1) сначала модель (разошлёт сигнал всем вкладкам)
        if self.model:
            self.model.create(rec, self.uid)
        else:
            # fallback к старому поведению (одиночный CanvasView)
            self.project.bubbles.append(rec)

        # 2) локально тоже создаём (если мы источник, то on_model_created проигнорирует)
        self._create_bubble_widget(rec)
        self._last_page_idx = img_idx
        self._last_bubble_order = bubble_order
        self.bubble_count = bid
        return bid

    # Загрузка списка персонажей из characters.json
    @staticmethod
    def _load_character_names_static(project) -> List[str]:
        """
        Загружает имена персонажей из {project.char_dir}/characters.json
        и сортирует их по первой букве второго слова (если оно есть).

        Статический метод, чтобы можно было вызвать ДО super().__init__.
        """
        try:
            char_file = os.path.join(project.char_dir, "characters.json")
            if not os.path.isfile(char_file):
                return []

            with open(char_file, 'r', encoding='utf-8') as f:
                data = json.load(f)

            # Извлекаем имена
            names = [item.get("name", "") for item in data if isinstance(item, dict)]
            names = [n.strip() for n in names if n.strip()]

            # Сортируем по первой букве второго слова (если есть)
            def sort_key(name: str) -> str:
                words = name.split()
                if len(words) >= 2:
                    # Берём первую букву второго слова
                    return words[1][0].lower() if words[1] else name.lower()
                # Если второго слова нет, сортируем по первой букве первого слова
                return name[0].lower() if name else ""

            names.sort()

            return ["(не указан)"] + names + additional_names

        except Exception as e:
            print(f"[TranslationCanvasView] Не удалось загрузить персонажей: {e}")
            traceback.print_exc()
            return []

    # Расширение пузырей дополнительными элементами управления
    def build_bubble_footer(self, bid: int) -> List[QWidget]:
        """
        Добавляет дополнительную линию управления в пузырь:
        - Номер реплики
        - Чекбокс "Известный персонаж"
        - Выпадающий список (если чекбокс включен)
        - Строка ввода (если чекбокс выключен)
        """
        # Загружаем сохраненные данные из project.bubbles
        rec = next((e for e in self.project.bubbles if int(e.get('id')) == bid), None)

        # Значения по умолчанию или из записи
        is_known_character = rec.get('is_known_character', True) if rec else True
        character_name = rec.get('character_name', '') if rec else ''

        row = QWidget()
        layout = QHBoxLayout(row)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(4)

        bubble_order = int(rec.get('bubble_order', 0)) if rec else 0
        spin_order = QSpinBox()
        spin_order.setObjectName(f"BubbleOrder_{bid}")
        spin_order.setRange(0, 100000)
        spin_order.setValue(bubble_order)
        spin_order.setToolTip("Номер реплики для упорядочивания")
        spin_order.setFixedHeight(28)
        spin_order.setMinimumWidth(60)
        spin_order.valueChanged.connect(lambda val, b=bid: self._on_bubble_order_changed(b, val))
        layout.addWidget(spin_order)

        # Чекбокс "Известный персонаж" (восстанавливаем состояние)
        chk_character = QCheckBox("И.П.")
        chk_character.setObjectName(f"KnownCharacter_{bid}")
        chk_character.setChecked(is_known_character)
        chk_character.setToolTip("Использовать ли готовые имена персонажей, или ввести своё.")
        chk_character.stateChanged.connect(lambda state, b=bid: self._on_character_check_changed(b, state))
        layout.addWidget(chk_character)

        # Выпадающий список персонажей
        combo_character = CharacterComboBox(bid, self._on_character_popup_toggled)
        combo_character.setObjectName(f"CharacterCombo_{bid}")

        # Заполняем реальными персонажами из characters.json
        if self._character_names:
            combo_character.addItems(self._character_names)
            # Восстанавливаем выбранное имя персонажа (если это известный персонаж)
            if is_known_character and character_name:
                idx = combo_character.findText(character_name)
                if idx >= 0:
                    combo_character.setCurrentIndex(idx)
        else:
            combo_character.addItem("(нет персонажей)")
        combo_character.setMaxVisibleItems(7)
        combo_character.setVisible(is_known_character)
        combo_character.setMinimumWidth(120)
        combo_character.currentTextChanged.connect(lambda text, b=bid: self._on_character_combo_changed(b, text))
        layout.addWidget(combo_character)

        # Кнопка обновления списка персонажей
        btn_refresh_chars = QPushButton("↻")
        btn_refresh_chars.setObjectName(f"RefreshCharacters_{bid}")
        btn_refresh_chars.setToolTip("Обновить список персонажей из characters.json")
        btn_refresh_chars.setFixedSize(28, 28)
        btn_refresh_chars.clicked.connect(lambda checked=False, b=bid: self._on_refresh_characters(b))
        btn_refresh_chars.setVisible(is_known_character) 
        layout.addWidget(btn_refresh_chars)

        # Строка ввода для неизвестного персонажа
        edit_character = QLineEdit()
        edit_character.setObjectName(f"CharacterEdit_{bid}")
        edit_character.setPlaceholderText("Имя персонажа...")
        # Восстанавливаем введенное имя (если это неизвестный персонаж)
        if not is_known_character and character_name:
            edit_character.setText(character_name)
        edit_character.setVisible(not is_known_character)
        edit_character.setMinimumWidth(120)
        edit_character.textChanged.connect(lambda text, b=bid: self._on_character_edit_changed(b, text))
        layout.addWidget(edit_character)

        # Строка ввода "Уточнение" (показывается только для известных персонажей)
        clarification = rec.get('clarification', '') if rec else ''
        edit_clarification = QLineEdit()
        edit_clarification.setObjectName(f"ClarificationEdit_{bid}")
        edit_clarification.setPlaceholderText("Уточнение...")
        if clarification:
            edit_clarification.setText(clarification)
        edit_clarification.setVisible(is_known_character)
        edit_clarification.setMinimumWidth(100)
        edit_clarification.textChanged.connect(lambda text, b=bid: self._on_clarification_changed(b, text))
        layout.addWidget(edit_clarification)

        layout.addStretch()

        return [row]

    def _on_character_popup_toggled(self, bid: int, opened: bool) -> None:
        def _set_z(item, z: float) -> None:
            if not qobj_alive(item):
                return
            try:
                item.setZValue(float(z))
            except Exception:
                pass

        b = self.bubbles.get(int(bid))
        if not b:
            return
        if opened:
            top = 5000.0
            _set_z(b.line_item, top)
            _set_z(b.proxy_widget, top + 1.0)
            _set_z(b.header_proxy, top + 2.0)
            _set_z(b.original_proxy, top + 2.0)
            _set_z(b.footer_proxy, top + 2.0)
            return

        if self._bubble_type() == "on_top":
            try:
                self._layout_on_top_bubble(int(bid))
            except Exception:
                pass
            return

        base = 1000.0
        _set_z(b.line_item, base)
        _set_z(b.proxy_widget, base)
        _set_z(b.header_proxy, base)
        _set_z(b.original_proxy, base)
        _set_z(b.footer_proxy, base)

    def _on_character_check_changed(self, bid: int, state: int):
        """Обработчик изменения чекбокса "Известный персонаж" с сохранением."""
        b = self.bubbles.get(bid)
        if not b or not b.container_widget:
            return

        combo = b.container_widget.findChild(QComboBox, f"CharacterCombo_{bid}")
        edit = b.container_widget.findChild(QLineEdit, f"CharacterEdit_{bid}")
        edit_clarification = b.container_widget.findChild(QLineEdit, f"ClarificationEdit_{bid}")
        btn_refresh = b.container_widget.findChild(QPushButton, f"RefreshCharacters_{bid}")
        if not combo or not edit:
            return

        # Если чекбокс включен - показываем combo и clarification, скрываем edit
        # Если выключен - показываем edit, скрываем combo и clarification
        is_checked = (state == Qt.CheckState.Checked.value)
        combo.setVisible(is_checked)
        edit.setVisible(not is_checked)
        if edit_clarification:
            edit_clarification.setVisible(is_checked)
        # Кнопка обновления — показываем только для известных персонажей

        if btn_refresh:
            btn_refresh.setVisible(is_checked)
        # Сохраняем состояние в JSON
        self._save_bubble_field(bid, 'is_known_character', is_checked)

        # Обновляем character_name в зависимости от выбранного виджета
        if is_checked:
            # Если включен чекбокс, берем значение из combo
            character_name = combo.currentText()
            if character_name == "(нет персонажей)":
                character_name = ''
            self._save_bubble_field(bid, 'character_name', character_name)
        else:
            # Если выключен, берем значение из edit
            self._save_bubble_field(bid, 'character_name', edit.text())

        # Обновляем последние значения
        self._last_is_known_character = is_checked
        if is_checked:
            self._last_character_name = combo.currentText()
        else:
            self._last_character_name = edit.text()

    def _on_character_combo_changed(self, bid: int, text: str):
        """Обработчик изменения выбранного персонажа в комбобоксе."""
        character_name = text if text != "(нет персонажей)" else ''
        self._save_bubble_field(bid, 'character_name', character_name)

        # Найти и очистить поле "Уточнение" у этого пузыря
        b = self.bubbles.get(bid)
        if b and b.container_widget:
            edit_clarification = b.container_widget.findChild(QLineEdit, f"ClarificationEdit_{bid}")
            if edit_clarification:
                edit_clarification.blockSignals(True)  # чтобы не триггерить textChanged
                edit_clarification.clear()
                edit_clarification.blockSignals(False)

        # Сохраняем пустое уточнение в JSON
        self._save_bubble_field(bid, 'clarification', "")

        # Обновляем последние значения
        self._last_character_name = text
        self._last_clarification = ""

    def _on_character_edit_changed(self, bid: int, text: str):
        """Обработчик изменения введенного имени персонажа."""
        self._save_bubble_field(bid, 'character_name', text)
        # Обновляем последние значения
        self._last_character_name = text

    def _on_clarification_changed(self, bid: int, text: str):
        """Обработчик изменения поля 'Уточнение'."""
        self._save_bubble_field(bid, 'clarification', text)
        # Обновляем последнее значение уточнения
        self._last_clarification = text

    def _on_bubble_order_changed(self, bid: int, value: int):
        """Обработчик изменения номера реплики."""
        self._save_bubble_field(bid, 'bubble_order', int(value))

    def _save_bubble_field(self, bid: int, field: str, value):
        """
        Сохраняет поле пузыря в project.bubbles и синхронизирует с моделью.
        """
        # Обновляем в project.bubbles
        for e in self.project.bubbles:
            try:
                if int(e.get('id')) == bid:
                    e[field] = value
                    break
            except Exception:
                traceback.print_exc()
                continue

        # Автосохранение
        if hasattr(self.project, "autosave"):
            try:
                self.project.autosave()
            except Exception as e:
                print(f"[TranslationCanvasView] autosave failed: {e}")
                traceback.print_exc()

        # Синхронизация с моделью (если используется)
        if self.model:
            b = self.bubbles.get(bid)
            if b:
                rec = {
                    'id': bid,
                    field: value,
                    'img_idx': b.img_idx,
                    'img_u': b.img_u,
                    'img_v': b.img_v,
                    'side': b.side
                }
                self.model.update(rec, self.uid)

    def _machine_translation_panel(self):
        parent = self.parentWidget()
        panel = getattr(parent, "_machine_translation_panel", None) if parent else None
        if panel is None or _is_deleted(panel):
            return None
        return panel

    def _on_translate_bubble(self, bid: int):
        b = self.bubbles.get(bid)
        if not b:
            return

        original_text = ""
        if b.original_text_widget:
            original_text = b.original_text_widget.toPlainText()
        if not original_text.strip():
            return

        mt_panel = self._machine_translation_panel()
        if mt_panel is None:
            return

        rec = None
        for e in getattr(self.project, "bubbles", []):
            try:
                if int(e.get("id")) == int(bid):
                    rec = e
                    break
            except Exception:
                continue
        if rec is None:
            rec = {"id": bid, "original_text": original_text}
        if hasattr(mt_panel, "_start_translation_for_records"):
            mt_panel._start_translation_for_records([rec])
        else:
            mt_panel._append_log("Асинхронный перевод одного пузыря недоступен.")


    # ------------------- детектор текста (оверлеи) -------------------
    def clear_text_detections(self):
        """Полностью удалить визуализацию текстовых блоков."""
        self._textdetector_results.clear()
        self._clear_textdetector_items()

    def set_textdetector_options(self, *, draw_lines: Optional[bool] = None, draw_mask: Optional[bool] = None,
                                 block_expand_px: Optional[int] = None,
                                 merge_gap_px: Optional[int] = None):
        """
        Обновляет параметры отрисовки детектора текста.
        """
        if draw_lines is not None:
            self._textdetector_draw_lines = bool(draw_lines)
        if draw_mask is not None:
            self._textdetector_draw_mask = bool(draw_mask)
        if block_expand_px is not None:
            try:
                self._textdetector_block_expand_px = max(0, int(block_expand_px))
            except Exception:
                pass
        if merge_gap_px is not None:
            try:
                self._textdetector_merge_gap_px = max(0, int(merge_gap_px))
            except Exception:
                pass
            self._textdetector_merge_nearby = self._textdetector_merge_gap_px > 0
        if self._textdet_model is not None and not getattr(self, "_textdet_model_updating", False):
            try:
                self._textdet_model_updating = True
                self._textdet_model.set_options(
                    block_expand_px=self._textdetector_block_expand_px,
                    merge_gap_px=self._textdetector_merge_gap_px,
                    merge_nearby=bool(self._textdetector_merge_nearby),
                )
            finally:
                self._textdet_model_updating = False
        self._rebuild_textdetector_items()
        self._sync_textdetector_geom()

    def set_text_detection_results(self, results: Dict[int, dict], *, replace: bool = True):
        """Обновляет/перерисовывает оверлеи детектора текста."""
        if replace:
            self._textdetector_results.clear()
            if self._textdet_model is not None and not self._textdet_model_updating:
                self._textdet_model_updating = True
                try:
                    self._textdet_model.clear_all()
                finally:
                    self._textdet_model_updating = False
        if results:
            proc: Dict[int, dict] = {}
            for k, v in results.items():
                proc[k] = v
            # сначала — в модель, чтобы она стала источником правды
            if self._textdet_model is not None and not self._textdet_model_updating:
                self._textdet_model_updating = True
                try:
                    for idx, data in proc.items():
                        mask = data.get("mask") if isinstance(data, dict) else None
                        blocks = data.get("blocks") if isinstance(data, dict) else None
                        size = data.get("size") if isinstance(data, dict) else None
                        self._textdet_model.set_result(int(idx), mask, blocks, size)
                finally:
                    self._textdet_model_updating = False
            self._textdetector_results.update(proc)
        self._rebuild_textdetector_items()
        self._sync_textdetector_geom()

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
        """Создает графические элементы из сохранённых результатов."""
        self._clear_textdetector_items()
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
                grp.setHandlesChildEvents(False)  # Qt5 API; в Qt6 может отсутствовать
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

    def _mask_to_qimage(self, mask, alpha: int, *, dilate_px: int = 0) -> Optional[QImage]:
        if mask is None:
            return None
        arr = np.asarray(mask)
        if arr.size == 0:
            return None
        if arr.ndim == 3:
            arr = arr[..., 0]
        if dilate_px and dilate_px > 0:
            try:
                kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (max(1, 2 * dilate_px + 1),) * 2)
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

    def _merge_rects(self, rects, gap: float = 4.0):
        """Greedily merge overlapping or near-touching rectangles (within `gap` px)."""
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

    def _extract_block_rects(self, blocks):
        """Возвращает объединённые прямоугольники блоков с учётом настроек расширения/объединения."""
        rects = []
        for blk in blocks or []:
            all_pts: List[Tuple[float, float]] = []
            line_list = getattr(blk, "lines", None) or []
            for line in line_list:
                try:
                    all_pts.extend([(float(x), float(y)) for x, y in line])
                except Exception:
                    continue

            xyxy = getattr(blk, "xyxy", None)
            if xyxy and len(xyxy) == 4:
                try:
                    x1, y1, x2, y2 = [float(v) for v in xyxy]
                    all_pts.extend([(x1, y1), (x2, y2)])
                except Exception:
                    pass

            if not all_pts:
                continue

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
        return self._merge_rects(rects, gap=gap)

    def get_detected_block_rects(self, page_idx: int) -> List[QRectF]:
        """
        Возвращает прямоугольники блоков детекта на странице в координатах сцены.
        Используется автоконвейером OCR.
        """
        if not (0 <= page_idx < len(self.images)):
            return []
        data = self._textdetector_results.get(page_idx)
        if not isinstance(data, dict):
            return []
        blocks = data.get("blocks") if isinstance(data, dict) else None
        if not blocks:
            return []

        base_size = None
        sz = data.get("size") if isinstance(data, dict) else None
        if isinstance(sz, (list, tuple)) and len(sz) == 2:
            try:
                base_size = (int(sz[0]), int(sz[1]))
            except Exception:
                base_size = None
        if base_size is None:
            base_size = self._size_from_mask(data.get("mask") if isinstance(data, dict) else None)
        if base_size is None:
            return []

        bw, bh = base_size
        if bw <= 0 or bh <= 0:
            return []
        if page_idx >= len(self.image_bboxes):
            return []
        bbox = self.image_bboxes[page_idx]
        if bbox.isNull():
            return []

        rects = self._extract_block_rects(blocks)
        if not rects:
            return []

        sx = bbox.width() / float(bw)
        sy = bbox.height() / float(bh)
        scene_rects: List[QRectF] = []
        for ax1, ay1, ax2, ay2 in rects:
            x1 = bbox.left() + max(0.0, min(ax1, float(bw))) * sx
            y1 = bbox.top() + max(0.0, min(ay1, float(bh))) * sy
            x2 = bbox.left() + max(0.0, min(ax2, float(bw))) * sx
            y2 = bbox.top() + max(0.0, min(ay2, float(bh))) * sy
            if x2 <= x1 or y2 <= y1:
                continue
            scene_rects.append(QRectF(QPointF(x1, y1), QPointF(x2, y2)))
        return scene_rects

    def run_ocr_on_detected_blocks(self, indices: Iterable[int], on_progress=None) -> int:
        """Запускает OCR по всем найденным блокам на указанных страницах. Возвращает кол-во обработанных блоков."""
        if not self.is_ocr_ready():
            return 0
        tasks = self.collect_ocr_tasks_for_detected_blocks(indices)
        total = 0
        processed = 0
        total = len(tasks)
        for task in tasks:
            text = self._perform_ocr(task["crop"])
            if text:
                self._apply_ocr_text_to_scene_rect(
                    int(task["target_idx"]),
                    QRectF(task["crop_scene"]),
                    text,
                )
            processed += 1
            if on_progress:
                try:
                    on_progress(processed, total)
                except Exception:
                    traceback.print_exc()
        return processed

    def _make_block_polygons(self, blocks) -> List[QGraphicsPolygonItem]:
        items: List[QGraphicsPolygonItem] = []
        if not blocks:
            return items

        pen = QPen(QColor(0, 255, 0))
        pen.setWidth(2)
        rect_pen = QPen(QColor(0, 160, 255))
        rect_pen.setWidth(3)
        rect_pen.setStyle(Qt.PenStyle.DashLine)
        for blk in blocks:
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
            xyxy = getattr(blk, "xyxy", None)
            if xyxy and len(xyxy) == 4:
                try:
                    x1, y1, x2, y2 = [float(v) for v in xyxy]
                except Exception:
                    continue
                poly = QPolygonF([QPointF(x1, y1), QPointF(x2, y1), QPointF(x2, y2), QPointF(x1, y2)])
                if self._textdetector_draw_lines:
                    it = QGraphicsPolygonItem(poly)
                    it.setBrush(QColor(0, 0, 0, 0))
                    it.setPen(pen)
                    it.setAcceptedMouseButtons(Qt.MouseButton.NoButton)
                    it.setZValue(152.0)
                    items.append(it)

        merged_rects = self._extract_block_rects(blocks)
        for ax1, ay1, ax2, ay2 in merged_rects:
            rect_poly = QPolygonF([
                QPointF(ax1, ay1),
                QPointF(ax2, ay1),
                QPointF(ax2, ay2),
                QPointF(ax1, ay2),
            ])
            rect_item = QGraphicsPolygonItem(rect_poly)
            rect_item.setBrush(QColor(0, 0, 0, 0))
            rect_item.setPen(rect_pen)
            rect_item.setAcceptedMouseButtons(Qt.MouseButton.NoButton)
            rect_item.setZValue(153.0)
            items.append(rect_item)
        return items

    def _on_refresh_characters(self, bid: int):
        """
        Перечитывает список персонажей из characters.json и обновляет все комбобоксы.
        Текущий выбор в каждом пузыре сохраняется, если он есть в новом списке.
        """
        try:
            # 1) Обновляем кэш имен
            self._character_names = self._load_character_names_static(self.project)

            # Если список пуст (например, файла нет) — показываем заглушку
            items = self._character_names if self._character_names else ["(нет персонажей)"]

            # 2) Пробегаем по всем пузырям и перезаполняем их комбобоксы
            for rec in getattr(self.project, "bubbles", []):
                try:
                    rid = int(rec.get('id'))
                except Exception:
                    continue

                b = self.bubbles.get(rid)
                if not b or not b.container_widget:
                    continue

                combo = b.container_widget.findChild(QComboBox, f"CharacterCombo_{rid}")
                if not combo:
                    continue

                # Текущие значения из записи
                is_known = rec.get('is_known_character', True)
                current_name = rec.get('character_name', '') or ''

                # Перезаполняем комбобокс
                combo.blockSignals(True)
                combo.clear()
                combo.addItems(items)
                if is_known and current_name:
                    idx = combo.findText(current_name)
                    if idx >= 0:
                        combo.setCurrentIndex(idx)
                combo.blockSignals(False)

            # Обновляем "последние значения" для новых пузырей (чтобы T/создание тянуло свежие имена)
            # Смысл: если последний выбор был известным персонажем — оставим его как есть,
            # но он мог исчезнуть. В таком случае просто не трогаем self._last_character_name.
            if self._last_is_known_character and self._character_names:
                # Если текущего имени нет в списке — не переустанавливаем, чтобы пользователь видел проблему.
                pass

        except Exception:
            traceback.print_exc()
